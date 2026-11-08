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

Key management, a streaming content-defined chunker, and an encrypted, deduplicating chunk store
are in place, but **nothing yet reads your files into the repository** because `backup` does not
exist. Do not rely on this tool for real data.

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
| `vaultsnap-core`  | Library: repository handle and layout, `config`, local store, locks, keys and crypto, chunking, encrypted chunk store, snapshot ids, retention, operations |
| `vaultsnap-cli`   | Binary `vaultsnap`: `clap` argument parsing and output          |

Libraries: `clap` (CLI), `tracing` + `tracing-subscriber` (diagnostics), `thiserror` (errors),
`tokio` (async runtime; repository operations are `async` and run blocking file I/O on Tokio's
blocking pool), `getrandom` (OS randomness), `argon2`, `hkdf` + `sha2`, `chacha20poly1305`
(RustCrypto key derivation and authenticated encryption), `blake3` (keyed chunk ids), `zstd`
(compression; the C library is compiled from source, so a C compiler is needed to build),
`zeroize` (clearing secrets), and `rpassword` (reading a passphrase without echo).

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

## License

MIT.
