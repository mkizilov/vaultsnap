# VaultSnap

A reliable encrypted incremental backup utility written in Rust.

## Status

**Pre-release (0.1.0, not yet published).** Every command is implemented and the test suite passes
on macOS (Apple silicon) with Rust 1.94.1; Linux and Windows have not been run. Read
[`docs/RELIABILITY.md`](docs/RELIABILITY.md) for exactly what has and has not been verified before
relying on it for data you cannot lose, and keep a second copy of anything important until you have
restored from VaultSnap yourself ([`docs/DISASTER_RECOVERY.md`](docs/DISASTER_RECOVERY.md)).

| Command     | State                                                                  |
|-------------|------------------------------------------------------------------------|
| `init`      | Implemented: creates a versioned repository (see below)                |
| `backup`    | Implemented: incremental, encrypted, deduplicated (see below)           |
| `snapshots` | Implemented: lists snapshots with sizes, text or JSON                  |
| `show`      | Implemented: everything about one snapshot, text or JSON               |
| `ls`        | Implemented: the files of a snapshot, text or JSON                     |
| `diff`      | Implemented: added, deleted, and modified files between two snapshots   |
| `restore`   | Implemented: safe restore of a snapshot or selected paths (see below)  |
| `check`     | Implemented: quick or full verification; reports, never repairs        |
| `prune`     | Implemented: retention policy plus mark-and-sweep; dry run, confirmation |

Key management, a streaming content-defined chunker, an encrypted, deduplicating chunk store, a
filesystem scanner, encrypted snapshot manifests, the `backup` command that connects them, the
commands that inspect snapshot history, `restore`, `check`, `prune`, local and S3-compatible
storage, release packaging, and scheduler files are in place. `vaultsnap version` shows the build
and the repository formats it reads and writes.

### Documentation

To use it: [`INSTALL.md`](docs/INSTALL.md) (installing and upgrading),
[`GUIDE.md`](docs/GUIDE.md) (initialization, backup schedules, restores, retention, remote
repositories), [`DISASTER_RECOVERY.md`](docs/DISASTER_RECOVERY.md), and
[`LIMITATIONS.md`](docs/LIMITATIONS.md). To maintain it: [`RELIABILITY.md`](docs/RELIABILITY.md)
(what is tested and how, review findings, measurements) and [`RELEASING.md`](docs/RELEASING.md).
The reference documents are listed further down.

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

### `vaultsnap backup`

```sh
vaultsnap --repo REPO --password-file PW backup ~/Documents ~/notes.txt --exclude '*.tmp' --tag laptop
```

Backs up one or more files or directories into one new snapshot, reusing everything already in
the repository:

- **Incremental.** A file whose size, timestamps, inode, and device match the same path in the
  previous snapshot (and which is clearly older than that backup) is not read; its recorded chunks
  are reused if they are all still stored. Every other file is chunked and each chunk that already
  exists is not written again. `--rescan` reads everything.
- **All or nothing.** The snapshot is published in one atomic step after every chunk is stored. A
  failure, Ctrl-C, SIGTERM, or crash at any point leaves no snapshot behind; chunks already stored
  are reused by the next run.
- **Honest about problems.** Files that cannot be read, or that keep changing while being read, are
  listed on `stderr` and recorded in the snapshot, which is then flagged incomplete: exit status 3.
  Exit status 130 means cancelled, 1 means failed, and 0 a complete snapshot.
- **Bounded.** `--jobs` worker threads (default one per CPU, at most 4); memory is bounded by the
  chunk size limits and the number of files in flight.
- **Progress.** A one-line status on a terminal, and a summary of what was read, unchanged,
  stored, and reused when it finishes.

Details, guarantees, and limits: [`docs/BACKUP.md`](docs/BACKUP.md).

### Inspecting history: `snapshots`, `show`, `ls`, `diff`

```sh
vaultsnap --repo REPO snapshots                 # oldest first, with logical / stored / unique sizes
vaultsnap --repo REPO show 3f2a91c4             # a snapshot by the start of its id
vaultsnap --repo REPO ls 3f2a91c4 /home/me/Documents
vaultsnap --repo REPO diff 3f2a91c4 b7e01d52    # added, deleted, modified
vaultsnap --repo REPO diff 3f2a91c4 b7e01d52 --json | jq .summary
```

- **Verified.** Every snapshot is read through authenticated decryption and validated before it
  is shown; a damaged or tampered snapshot is reported, never displayed.
- **Sizes.** *Logical* (what the files add up to), *stored* (what the snapshot's distinct chunks
  occupy in the repository), and *unique stored* (the part no other snapshot uses).
- **Text or JSON.** `--json` prints one stable JSON document per command; text output escapes
  hostile file names so they cannot inject terminal control sequences.
- **Read-only**, under a shared lock.

The full command reference with example output and the JSON schema is in
[`docs/CLI.md`](docs/CLI.md).

### Restoring: `restore`

```sh
vaultsnap --repo REPO restore 3f2a91c4 --target /mnt/restore
vaultsnap --repo REPO restore 3f2a91c4 --target /mnt/restore --include /home/me/Documents/taxes
vaultsnap --repo REPO restore 3f2a91c4 --target ~/Documents --on-conflict skip     # or overwrite
vaultsnap --repo REPO restore 3f2a91c4 --target /mnt/restore --symlinks as-recorded
```

- **Authenticated.** The snapshot and every chunk are authenticated and decrypted before use; a
  damaged chunk fails its file only, which is then absent, never wrong or half-written.
- **Contained.** Files go to their original absolute path *below* `--target`. Every path component
  is checked, nothing is written outside the target, and no symbolic link in the target is ever
  followed. A target inside the repository is refused.
- **Atomic files.** Each file is written to a private temporary file and renamed into place.
- **Conflicts are explicit.** The default `fail` restores nothing if anything would be replaced;
  `skip` and `overwrite` are opt-in, and a directory is never replaced or deleted.
- **Links under a policy.** By default only symbolic links that cannot lead out of the restore
  root are restored.
- Permissions and times are restored (without set-user-id/set-group-id); owners and special files
  are not.

Specification and limits: [`docs/RESTORE.md`](docs/RESTORE.md); options and exit statuses:
[`docs/CLI.md`](docs/CLI.md#restore).

### Verifying: `check`

```sh
vaultsnap --repo REPO check                   # quick: seconds to minutes
vaultsnap --repo REPO check --read-data       # full: reads and authenticates every chunk
vaultsnap --repo REPO check --read-data --json | jq '.ok, .affected_snapshots'
```

- **Reports, never repairs.** Nothing is moved, rewritten, or deleted; the only file written is the
  check's own lock. The repository is byte-for-byte the same afterwards.
- **Authenticated.** Every snapshot is authenticated and validated; every chunk a snapshot needs
  must exist; every object in `data/` is examined, referenced or not; unreferenced objects
  (orphans), stray files, leftovers, and lock files are reported.
- **Two depths, honestly labelled.** A quick check examines the structure of each chunk object
  without reading its payload, so it finds missing, truncated, or extended objects but **not**
  flipped bits inside a chunk; `--read-data` reads and authenticates everything. Both say which
  they were.
- **Text or JSON**; exit status 0 (no errors; warnings allowed), 3 (errors found), 1 (could not
  run).
- **What to do about each failure** is documented per kind of finding in
  [`docs/CHECK.md`](docs/CHECK.md), and the report prints a short version of it.

The design is specified before the code that implements it:

- [`docs/FORMAT.md`](docs/FORMAT.md): repository layout, identifiers, key files and encryption,
  chunking, crash safety.
- [`docs/BACKUP.md`](docs/BACKUP.md): the backup pipeline, change detection, failure and
  cancellation behavior.
- [`docs/RESTORE.md`](docs/RESTORE.md): what a restore does and guarantees: paths, conflicts,
  links, atomicity, interruption.
- [`docs/CHECK.md`](docs/CHECK.md): what `check` examines, what each mode can and cannot find,
  the report, and how to respond to each failure.
- [`docs/PRUNE.md`](docs/PRUNE.md): retention rules, what `prune` deletes and in which order, why
  a kept snapshot cannot lose a chunk, confirmation, locking, and interruption.
- [`docs/STORAGE.md`](docs/STORAGE.md): the storage backend interface, what every backend must
  guarantee, retries and conditional writes, the contract tests, and how an S3-compatible backend
  maps onto it.
- [`docs/S3.md`](docs/S3.md): keeping a repository in an S3-compatible bucket: location and
  options, credentials, transport security, conditional-write checks, retries, resuming,
  request volume, and how to run the MinIO tests.
- [`docs/CLI.md`](docs/CLI.md): every command with examples, exit statuses, size definitions,
  and the JSON output.
- [`docs/SECURITY.md`](docs/SECURITY.md): threat model, secrets handling, restore safety.

### Repositories in S3-compatible buckets

`--repo` takes a directory or `s3://BUCKET[/FOLDER]`. Every command works the same on a bucket.
Credentials come only from the standard AWS credential chain (environment, `~/.aws` files and
profiles, SSO, roles); there is no option for them. Certificates are always verified.

```sh
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
vaultsnap --repo s3://my-bucket/laptop init
vaultsnap --repo s3://my-bucket/laptop backup ~/Documents
vaultsnap --repo s3://my-bucket --s3-endpoint http://127.0.0.1:21100 snapshots   # MinIO
```

See [`docs/S3.md`](docs/S3.md) for the options and the guarantees.

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
| `vaultsnap-core`  | Library: repository handle and layout, `config`, the storage backend interface (local directory, S3-compatible bucket, in-memory object store, retries) and its contract tests, locks, keys and crypto, chunking, encrypted chunk store, filesystem scanner, snapshot manifests, the backup engine, retention and garbage collection, operations |
| `vaultsnap-cli`   | Binary `vaultsnap`: `clap` argument parsing and output          |

Libraries: `clap` (CLI), `tracing` + `tracing-subscriber` (diagnostics), `thiserror` (errors),
`tokio` (async runtime; repository operations are `async` and run blocking file I/O on Tokio's
blocking pool), `getrandom` (OS randomness), `argon2`, `hkdf` + `sha2`, `chacha20poly1305`
(RustCrypto key derivation and authenticated encryption), `blake3` (keyed chunk ids), `zstd`
(compression; the C library is compiled from source, so a C compiler is needed to build),
`globset` (include/exclude patterns), `libc` (Unix `O_NOFOLLOW` and friends), `zeroize`
(clearing secrets), `rpassword` (reading a passphrase without echo), and `serde` + `serde_json`
(the `--json` output of the command line; the core library does not use them).

## Usage

```sh
vaultsnap --repo /path/to/repo init
vaultsnap --repo /path/to/repo backup ~/Documents --exclude '*.tmp' --tag laptop
vaultsnap --repo /path/to/repo snapshots
vaultsnap --repo /path/to/repo show <snapshot-id-or-start>
vaultsnap --repo /path/to/repo ls <snapshot-id-or-start> [/path/inside]
vaultsnap --repo /path/to/repo diff <older> <newer> --json
vaultsnap --repo /path/to/repo restore <snapshot-id-or-start> --target /path/to/restore \
    [--include /path/inside]... [--on-conflict fail|skip|overwrite] [--symlinks contained|skip|as-recorded]
vaultsnap --repo /path/to/repo check --read-data
vaultsnap --repo /path/to/repo prune --keep-last 7 --keep-monthly 12 --dry-run   # look first
vaultsnap --repo /path/to/repo prune --keep-last 7 --keep-monthly 12             # asks for "yes"
vaultsnap --repo /path/to/repo prune --keep-last 7 --keep-monthly 12 --yes       # scripts
```

`--repo` can also be set with `VAULTSNAP_REPO`. Use `-v`, `-vv`, `-vvv` for more diagnostics on
stderr, or `RUST_LOG` for fine control; secrets are never logged (see `docs/SECURITY.md`).

## Development

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.94.1 with `rustfmt` and `clippy`; the
AWS SDK needs it). `Cargo.lock` is committed.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
sh contrib/test-scheduled.sh && sh scripts/test-package.sh
```

`AGENTS.md` explains how the toolchain is installed inside the repository, and the two environment
variables the macOS sandbox needs for C dependencies.

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
- Backup tests are in `crates/vaultsnap-core/tests/backup.rs`: repeated, modified, inserted,
  deleted, and added files; unchanged-file skipping and its safety margin; several sources;
  cancellation, failure, and files changing or vanishing mid-backup; bounded and deterministic
  concurrency; simultaneous backups; and a rebuild of every file of every snapshot from its chunk
  list. Unit tests for the engine and the parent comparison sit beside the code.
- History tests (ordering, changes between real backups, unusual file names, sizes checked
  against the object files on disk, tampering, wrong keys) are in
  `crates/vaultsnap-core/tests/history.rs`; the exact text and JSON of every inspection command
  for fixed data, and the escaping of hostile names, are unit tests in
  `crates/vaultsnap-cli/src/inspect/tests.rs`.
- Restore tests: the planner (path rules, selection, the link policy), the file system layer
  (links never followed, atomic publication, attributes), and the engine with hand-built hostile
  manifests, planted links, damaged chunks, and cancellation are unit tests under
  `crates/vaultsnap-core/src/restore/`; restores of real backups compared byte for byte (and by
  permissions and times) with the originals, selected paths, conflicts, damaged and tampered data,
  and interrupted restores are in `crates/vaultsnap-core/tests/restore.rs` and
  `crates/vaultsnap-cli/tests/restore.rs`.
- Check tests damage repositories on purpose: `crates/vaultsnap-core/tests/check.rs` deletes,
  truncates, extends, flips, swaps, and link-replaces chunk objects, tampers with, cuts, empties,
  swaps, and replaces snapshot files with a foreign one, removes directories, and adds strays,
  leftovers, stale locks, and bad key files, then asserts which mode finds what (including what
  the quick mode cannot see), the exact counts, the affected snapshots, determinism across thread
  counts, and that the repository is unchanged afterwards. The same is run through the binary in
  `crates/vaultsnap-cli/tests/check.rs`; the exact text and JSON for fixed reports are unit tests
  in `crates/vaultsnap-cli/src/check/tests.rs`; the keyless object inspection, the directory walk,
  and the verification pool have unit tests beside the code in `crates/vaultsnap-core/src/`.
- Prune tests: the retention rules (calendar boundaries, ISO weeks, ties) and the pipeline (shared
  chunks, refusals, plan digests, cancellation after every possible number of deletions, a
  deletion that fails) are unit tests in `crates/vaultsnap-core/src/retention.rs` and
  `src/prune/engine.rs`; prunes of real backups, with every kept snapshot restored byte for byte
  and fully checked afterwards, dry runs and refusals that change nothing, interrupted prunes
  finished by a rerun, and prunes racing a backup or another prune are in
  `crates/vaultsnap-core/tests/prune.rs`; the binary is exercised in
  `crates/vaultsnap-cli/tests/prune.rs`, and the exact text and the confirmation rules are unit
  tests in `crates/vaultsnap-cli/src/prune/tests.rs`.
- Storage tests: the backend contract (`crates/vaultsnap-core/src/storage/contract.rs`, 22 cases)
  is run against the local directory, the in-memory object store, and the in-memory store behind
  retries over storage that fails every few calls; unit tests sit beside each backend, the retry
  layer, and the fault injector; `crates/vaultsnap-core/tests/storage.rs` runs a whole repository
  (init, backups, restore, check, prune) on each, moves a repository between backends object by
  object, and checks that backends without the needed guarantees are refused.
- S3 tests: the S3 backend passes the same contract against an in-memory fake service
  (`crates/vaultsnap-core/src/storage/s3/`, with tests of paging, conditional writes, error
  mapping, retries, the request limit, and redaction), and `crates/vaultsnap-cli/tests/s3.rs`
  checks the refusals that need no network. `crates/vaultsnap-core/tests/s3_minio.rs` runs a
  repository against a real MinIO server (round trip and restore from a fresh connection,
  nothing readable uploaded, duplicate uploads, a cancelled backup, a network outage in the middle
  of a backup, connection failures, and the whole storage contract). It needs a MinIO server and
  `VAULTSNAP_S3_TEST_ENDPOINT`; `scripts/minio-local.sh` builds and runs one without Docker (see
  `docs/S3.md`). Without a server each test prints `SKIPPED (unverified)` and passes.
- CLI integration tests are in `crates/vaultsnap-cli/tests/`. They run the compiled binary and use
  helpers in `tests/common/` that create scratch directories via `TMPDIR`.
- Reliability tests, described in [`docs/RELIABILITY.md`](docs/RELIABILITY.md): property tests of
  the invariants (`tests/properties.rs`), randomized filesystem round trips (`tests/roundtrip.rs`,
  seeded and replayable), a backup and a prune killed at every storage call (`tests/crash.rs`), and
  end-to-end runs of the binary (`crates/vaultsnap-cli/tests/e2e.rs`).

Benchmarks (see the header of each file for what it covers; results and how to read them are in
[`docs/RELIABILITY.md`](docs/RELIABILITY.md)):

```sh
cargo bench -p vaultsnap-core --bench chunking              # chunker alone, in memory, 4 KiB .. 64 MiB
cargo bench -p vaultsnap-core --bench chunking -- --quick   # 4 KiB .. 1 MiB
cargo bench -p vaultsnap-core --bench throughput            # end to end on real directories, 64 MiB
cargo bench -p vaultsnap-core --bench throughput -- --quick # 8 MiB
```

CI (`.github/workflows/ci.yml`) runs the checks above on Linux (x86-64 and arm64) and macOS, a
build check on Windows that may fail, a smoke run of the benchmarks, and the MinIO tests. Releases
are built by `.github/workflows/release.yml` for four targets ([`docs/RELEASING.md`](docs/RELEASING.md)).
None of these workflows has run on GitHub yet.

## Known gaps

What the tests do not cover, and what is slow or missing by design. [`docs/RELIABILITY.md`](docs/RELIABILITY.md)
has the evidence behind what *is* verified; [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) is the list for
users.

- **Only macOS (Apple silicon) has been built and tested.** Linux and Windows are untested here; the
  GitHub workflows (CI, release) are written and syntax-checked but have never run on GitHub.
  Windows is not supported.
- **S3** is verified against a MinIO server (the April 2025 release, built from source, on loopback)
  through the real AWS SDK adapter, including the whole storage contract. It is not verified against
  AWS S3 itself, nor TLS with a private CA, named profiles, SSO, or role credentials. That server
  creates an object on `If-Match` of a missing one, unlike S3, so it is given no conditional-replace
  guarantee (nothing in a repository uses one). A backup checks each chunk with its own request, one
  after the other, so a very large backup to a distant bucket is slow (`docs/S3.md`).
- **Many small files back up slowly** because every chunk object is committed durably (a file sync
  and a directory sync each; on macOS a full flush to the disk, about 3.6 ms): about 7.7 ms per file,
  1.3 MiB/s, whatever the number of workers. Large files reach 100 MiB/s or more
  (`docs/RELIABILITY.md`). Packing chunks into larger files would fix this and is a format change.
- **Signals through the binary are not tested.** Ctrl-C and SIGTERM handling of `backup`, `restore`,
  `check`, and `prune` is exercised only through the library's cancel token. The interactive `yes`
  question of `prune` needs a terminal, so only its logic is unit tested.
- **Crash tests model a storage that stops answering**, at every call and in both ways a call can
  fail to report back. They do not model a power cut that loses data a file system had acknowledged,
  or torn writes inside one file; the design leans on `fsync` and no-overwrite publication for those
  (`docs/FORMAT.md`).
- **Stale locks** left by a killed process are never removed automatically: a killed backup blocks
  `prune`, and a killed `prune` blocks everything, until a person deletes the lock file
  (`docs/DISASTER_RECOVERY.md`).
- The local backend publishes new files with hard links, so the repository must live on a file
  system that supports them.
- **Restore** is unexercised on case-insensitive file systems beyond what macOS gives by default, on
  file systems without hard links, and under a racing attacker, which it does not defend against (it
  is `unsafe`-free and cannot use `openat`; `docs/RESTORE.md`). There is no `fsync` on restored files,
  no restoration of owners, extended attributes, or ACLs, and no way to strip the leading source path.
- **`check`** next to a *running* backup is safe by design (snapshot files are listed before objects)
  but has no test with real concurrency. A quick check cannot see damage inside a chunk's payload,
  and nothing can see a deleted snapshot or a rollback (`docs/CHECK.md`).
- **`prune`**: the race tests (a prune against a backup, two prunes) assert only that every outcome
  is a success or a lock conflict and that nothing is damaged, since which one wins is not
  deterministic; calendar retention is tested on hand-made times, since a real backup cannot be
  given a time; the test that makes objects undeletable skips itself as root. There is no
  append-only mode and no way to forget one named snapshot (`docs/PRUNE.md`).
- Tests that need files with newlines, escape characters, or non-UTF-8 names, FIFOs, sockets, or
  privileges skip themselves where the platform cannot provide them; a skipped case is unverified.
- Scanner behavior on real mount points, on Windows, and on network file systems is unverified
  (`docs/SCANNING.md`).
- The benchmarks have run only on one machine and only at the default sizes (chunker up to 64 MiB,
  end to end 64 MiB per dataset); the `--large` variants were not run.

## License

MIT.
