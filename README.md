# VaultSnap

A reliable encrypted incremental backup utility written in Rust.

## Status

**Early development.**

| Command     | State                                                                  |
|-------------|------------------------------------------------------------------------|
| `init`      | Implemented: creates a versioned repository (see below)                |
| `backup`    | Not implemented; validates arguments, then fails                        |
| `snapshots` | Not implemented                                                        |
| `restore`   | Not implemented                                                        |
| `check`     | Not implemented                                                        |
| `prune`     | Not implemented; validates the retention policy first                  |

Key management, a streaming content-defined chunker, an encrypted, deduplicating chunk store, a
filesystem scanner, and encrypted snapshot manifests are in place, but **nothing yet connects them
into a backup**: `backup` and `restore` do not exist. Do not rely on this tool for real data.

### Chunking

`vaultsnap_core::chunking` splits any `std::io::Read` into content-defined chunks (FastCDC with
normalized chunking; default 256 KiB / 1 MiB / 4 MiB minimum / average / maximum). It is
streaming with memory bounded by twice the maximum chunk size, deterministic whatever the read
sizes, binary-safe, and independent of storage:

```rust
use vaultsnap_core::chunking::{Chunker, ChunkerConfig};

let mut chunker = Chunker::new(std::fs::File::open("big.iso")?, ChunkerConfig::default());
while let Some(chunk) = chunker.next_chunk()? {
    // chunk.offset: u64, chunk.data: &[u8] (borrowed until the next call)
}
```

The exact algorithm is specified in [`docs/FORMAT.md`](docs/FORMAT.md).

### Filesystem scanner

`vaultsnap_core::scan::Scanner` walks a directory (or one file) lazily and deterministically
(depth first, names in byte order) and reports each path's type, size, timestamps, permissions,
ownership, link information, and why anything was left out. File content is never read during
the walk; `Scanner::open` returns a streaming `Read` that plugs into the chunker.

Every judgment call is an explicit option with a conservative default: symbolic links are
recorded and **never followed** (following only inside the root, or anywhere, is opt-in); hard
links are stored once; unreadable paths are reported and skipped; paths vanishing mid-scan are
skipped; files that change while being read fail the read; other filesystems are not entered;
FIFOs, sockets, and devices are recorded but never opened. Include/exclude globs are supported.
The policy table, platform differences, and limitations are in
[`docs/SCANNING.md`](docs/SCANNING.md).

```rust
let scanner = Scanner::new("/home/me/Documents", ScanOptions::default())?;
for item in scanner.scan() {
    match item? {
        ScanItem::Entry(entry) if entry.is_file() => {
            let reader = scanner.open(&entry)?;      // streaming, change-checked
            let chunks = Chunker::new(reader, ChunkerConfig::default());
            // ... store each chunk in the ChunkStore
        }
        ScanItem::Skipped(skipped) => eprintln!("skipped {:?}", skipped.path),
        _ => {}
    }
}
```

### Chunk store

`vaultsnap_core::chunk_store::ChunkStore` stores chunks as encrypted objects in `data/<xx>/<id>`:

- **Addressing**: the id is a keyed BLAKE3 hash of the plaintext, keyed with a secret derived from
  the repository key. Identical chunks deduplicate within a repository; without the key nobody
  can test whether a known file is present, or link two repositories.
- **Encryption**: each chunk is compressed with zstd only when that makes it smaller, then sealed
  with XChaCha20-Poly1305 under a fresh random nonce. The object header and the chunk id are
  authenticated, so objects cannot be edited or moved to another id.
- **Integrity**: reads authenticate, decrypt, decompress within the authenticated length, and
  check that the content hashes to the id. Truncated, extended, modified, misplaced, and
  wrong-key objects are all rejected as corrupt.
- **Atomic and concurrency-safe**: objects are published with a single no-overwrite step; when
  several writers store the same chunk exactly one creates it and the rest see a duplicate.

```rust
let keys = repo.unlock(&passphrase).await?;
let chunks = repo.chunk_store(&keys)?;
let stored = chunks.put(b"chunk bytes")?;      // PutOutcome::Stored or ::Duplicate
let bytes = chunks.get(&stored.id)?;           // verified plaintext
```

The byte-level format and the reasoning behind each cryptographic choice are in
[`docs/FORMAT.md`](docs/FORMAT.md) ("Chunk objects").

### Snapshots

`vaultsnap_core::snapshot` records one backup as a single immutable file, `snapshots/<id>`:

- **Contents**: a random unique snapshot id, creation time, host and tags, the backed-up roots,
  each root's directory tree (parent index plus one safe path component per entry), every
  entry's metadata (type, size, nanosecond timestamps, permissions, ownership, link structure),
  for every file the ordered list of chunk references, the backup configuration (chunker sizes,
  scan policies, patterns), a list of skipped paths, totals, and the format version.
- **Encrypted and authenticated**, including every file name and path: zstd-compressed if smaller,
  sealed with XChaCha20-Poly1305 with the header and snapshot id as associated data, so a
  snapshot cannot be edited, truncated, renamed, or swapped undetected.
- **Atomic**: a snapshot is validated, checked for missing chunks, encrypted, and then published
  in one no-overwrite step. An interrupted or incomplete backup leaves no snapshot; a damaged
  one is reported as damaged, never presented as good.
- **Versioned and validated**: the container and the manifest each carry a version; unknown
  versions are reported, historical ones stay readable, and every manifest, even an authentic
  one, must satisfy strict rules before use (no path traversal, no cycles, consistent sizes).

```rust
let snapshots = repo.snapshot_store(&keys).await?;
let mut builder = ManifestBuilder::new(repository_id, BackupConfig::new(chunker, &scan_options));
let root = builder.add_root(scanner.root());
// for each scanned entry: builder.add_entry(root, &entry, chunk_refs)?;
let manifest = builder.finish(Timestamp::now())?;
snapshots.commit(&manifest)?;                 // atomic; refuses if a chunk is missing
let again = snapshots.load(&manifest.id)?;    // authenticated, decrypted, validated
```

Restoring is not implemented; loading only reads and verifies manifests. The byte-level format
and compatibility rules are in [`docs/FORMAT.md`](docs/FORMAT.md) ("Snapshots").

The design is specified before the code that implements it:

- [`docs/FORMAT.md`](docs/FORMAT.md): repository layout, identifiers, key files and encryption,
  chunking, crash safety.
- [`docs/SECURITY.md`](docs/SECURITY.md): threat model, secrets handling, restore safety.

### `vaultsnap init`

Creates, at `--repo`: a `config` file (format version, random repository id, creation time), the
`data/`, `snapshots/`, `locks/`, and `keys/` directories, and one key file, with owner-only
permissions (`0700`/`0600`). `config` is written atomically and last. `init` never overwrites
anything: it fails if a repository already exists there (and reports an unsupported format
version separately), if the path is a file, or if the directory holds unrelated files. Re-running
it after an interrupted `init` is safe.

**Keys.** A random 256-bit master key is generated and stored only encrypted: XChaCha20-Poly1305
under a key derived from your passphrase with Argon2id (64 MiB, 3 passes, 4 lanes, random
per-file salt). Separate keys for content ids, data, and metadata are derived from it with
HKDF-SHA256. Key files are versioned and authenticated, so tampering and wrong passphrases are
detected.

**Passphrase.** Never on the command line. Either an interactive prompt (no echo, asked twice) or
an explicit file for automation:

```sh
vaultsnap --repo /path/to/repo init                              # prompts on the terminal
vaultsnap --repo /path/to/repo --password-file ~/.vaultsnap-pw init   # non-interactive
export VAULTSNAP_PASSWORD_FILE=~/.vaultsnap-pw                    # same, via the environment
```

The file should be readable only by you (`chmod 600`); one trailing newline is ignored. There is
no way to recover a lost passphrase.

## Workspace

| Crate             | Purpose                                                        |
|-------------------|----------------------------------------------------------------|
| `vaultsnap-core`  | Library: repository handle and layout, `config`, local store, locks, keys and crypto, chunking, encrypted chunk store, filesystem scanner, snapshot ids, retention, operations |
| `vaultsnap-cli`   | Binary `vaultsnap`: `clap` argument parsing and output          |

Libraries: `clap` (CLI), `tracing` + `tracing-subscriber` (diagnostics), `thiserror` (errors),
`tokio` (async runtime; repository operations are `async` and run blocking file I/O on Tokio's
blocking pool), `getrandom` (OS randomness), `argon2`, `hkdf` + `sha2`, `chacha20poly1305`
(RustCrypto key derivation and authenticated encryption), `blake3` (keyed chunk ids), `zstd`
(compression; the C library is compiled from source, so a C compiler is needed to build),
`globset` (include/exclude patterns), `libc` (Unix `O_NOFOLLOW` and friends), `zeroize`
(clearing secrets), and `rpassword` (reading a passphrase without echo).

## Usage

```sh
vaultsnap --repo /path/to/repo init
vaultsnap --repo /path/to/repo backup ~/Documents --exclude '*.tmp' --tag laptop
vaultsnap --repo /path/to/repo snapshots
vaultsnap --repo /path/to/repo restore <snapshot-id> --target /path/to/restore
vaultsnap --repo /path/to/repo check --read-data
vaultsnap --repo /path/to/repo prune --keep-last 7 --keep-monthly 12 --dry-run
```

`--repo` can also be set with `VAULTSNAP_REPO`. Use `-v`, `-vv`, `-vvv` for more diagnostics on
stderr, or `RUST_LOG` for fine control; secrets are never logged (see `docs/SECURITY.md`).

## Development

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.85.0 with `rustfmt` and `clippy`).

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Tests:

- Unit tests live next to the code in each module.
- Chunking property tests (`proptest`) and statistical tests are in
  `crates/vaultsnap-core/tests/chunking.rs`. A failing property case is saved under
  `crates/vaultsnap-core/proptest-regressions/`; commit that file with the fix.
- Snapshot tests: unit tests for the codec (byte-exact layout, every truncation, lying counts,
  corrupted bytes), the container, validation, the builder, and the store (interrupted writes,
  corruption, concurrency); end-to-end tests with a real repository, scanner, chunker, and
  chunk store are in `crates/vaultsnap-core/tests/snapshot.rs`.
- Scanner tests, which build real directory trees (symlinks, hard links, FIFOs, unreadable
  directories, files changing mid-read), are in `crates/vaultsnap-core/tests/scan.rs`.
- CLI integration tests are in `crates/vaultsnap-cli/tests/`. They run the compiled binary and use
  helpers in `tests/common/` that create scratch directories via `TMPDIR`.

Benchmarks (in-memory, nothing written to disk; see the header of the file for what they cover):

```sh
cargo bench -p vaultsnap-core --bench chunking              # 4 KiB .. 64 MiB inputs
cargo bench -p vaultsnap-core --bench chunking -- --quick   # 4 KiB .. 1 MiB
cargo bench -p vaultsnap-core --bench chunking -- --large   # adds a 256 MiB input
```

CI (`.github/workflows/ci.yml`) runs the four commands above on Linux and macOS (the benchmark
is compiled by `clippy --all-targets` but not run).

## Known gaps

- `Cargo.lock` has not been generated or committed yet; it must be produced by the first build
  with network access and committed. Until then CI does not use `--locked`.
- Formatting, linting, and tests were written without a Rust toolchain being available in the
  authoring environment (no network, no preinstalled `cargo`); nothing has been compiled or run
  yet, and the first CI run is the first verification.
- The local store publishes new files with hard links, so the repository must live on a
  filesystem that supports them.
- The chunking benchmark has never been run, so there are no throughput numbers yet.
- Scanner behavior on real mount points, on Windows, and on network filesystems is unverified;
  see `docs/SCANNING.md`.

## License

MIT.
