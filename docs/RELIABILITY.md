# Reliability, performance, and verification

What has been verified and how, what the first real build and test run found, what each kind of
test proves (and cannot prove), the review of memory, locking, permissions, integrity, cryptographic
metadata, and error handling, and measurements. Written for people who maintain VaultSnap or who
need to decide how far to trust it. Nothing here is estimated: every number and every result was
observed on the machine described below, and anything that was not run is listed under
[What is not verified](#what-is-not-verified).

## 1. Verification status

**Environment.** macOS 27.2 (build 26B123), Apple silicon (arm64, 10 cores), Rust 1.94.1
(`rust-toolchain.toml`), installed inside the repository (`AGENTS.md`). At most two build jobs and two
test threads. The code was written without a Rust toolchain or network access, so this was the first
time any of it was compiled, and it was done with network access to the Rust, Go, and crates.io
hosts.

| Check | Command | Result |
|-------|---------|--------|
| Formatting | `cargo fmt --all --check` | passes |
| Lints | `cargo clippy --workspace --all-targets --locked -- -D warnings` | passes |
| Lints, without the S3 client | `cargo clippy -p vaultsnap-core --no-default-features --all-targets -- -D warnings` | passes |
| Tests | `cargo test --workspace --locked --no-fail-fast` | **1027 passed, 0 failed, 1 ignored** (a doctest that is not meant to run); re-run after the last dependency change |
| Tests, without the S3 client | `cargo test -p vaultsnap-core --no-default-features --lib` | 633 passed |
| Documentation | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked` | passes |
| S3 tests against a real server | `cargo test -p vaultsnap-core --test s3_minio` with MinIO running | **7 of 7 passed**, including the whole storage contract (22 cases), on the final dependency set; with no server they skip, and with the server pointed at a dead port 5 of them fail, which shows they really use it |
| More random trees | `VAULTSNAP_ROUNDTRIP_SEEDS=60 cargo test -p vaultsnap-core --test roundtrip` | 60 of 60 seeds pass (the default is 10) |
| Dependency audit | `cargo audit` (RustSec database of 1422 advisories) | **no advisories in 288 crates**; there were **7 before** the fix in section 2. Run by hand, not yet a CI gate |
| Signals through the binary | by hand, release binary | SIGINT and SIGTERM during `backup` and `restore` behave as documented (section 5). Manual; not in the test suite |
| TLS to the S3 backend | by hand, MinIO with a generated CA | certificate and hostname verification work as documented (section 5). Manual; not in the test suite |
| Scheduler script | `sh contrib/test-scheduled.sh` | 15 checks pass |
| Packaging script | `sh scripts/test-package.sh` | 15 checks pass (against stand-ins for cargo) |
| Release package | `scripts/package.sh` | builds; the archive's checksum verifies; the packed binary runs and reports its version |
| Benchmarks | `cargo bench -p vaultsnap-core --bench chunking` and `--bench throughput` | run; results in [section 6](#6-performance-and-memory) |
| Documented user flows | restore drill, `jq` recipes, scheduled wrapper, stale lock, lost `config`, file modes | all run against the real binary (section 5) |

The 1027 tests include 7 that need a MinIO server and skip themselves without one: in a plain
`cargo test` they pass without testing anything. The MinIO run above is separate and is the one that
counts. MinIO's own binary downloads were withdrawn when the project was archived, so
`scripts/minio-local.sh` builds the pinned release (`RELEASE.2025-04-22T22-12-26Z`) from source with a
checksum-verified Go toolchain, all inside the repository, and serves it on loopback port 21100
([`S3.md`](S3.md)). Its `install` step was run from scratch (129 s, and the binary is the same size as
the one built by hand); `start`, `env`, `stop`, and `clean` were run.

## 2. What the first real build and test run found

Everything was written without a compiler, so this is the honest account of what that cost:

| Stage | Found | Cause | Where the fault was |
|-------|-------|-------|---------------------|
| compile | 2 type errors in `keys.rs` | `&self.derive(p)?` is checked against `&[u8; 32]`, so the compiler wanted the `?` result to be `[u8; 32]`, not `Zeroizing<..>` | product code (fixed by binding the key first) |
| compile | 1 error in `properties.rs`; 3 in a `chunk_store` test | `prop_assert_eq!` takes no inline `{name}` captures; a fixture type used under an old name | tests |
| rustfmt | 1183 differences | nothing had been formatted | all (mechanical, applied) |
| clippy | 17 errors of 5 kinds, found over several rounds | `from_ref`, `expect_err`, `is_multiple_of`, `repeat_n`, complex types | mostly test code; 2 product lines (`is_multiple_of`) |
| rustdoc | 4 redundant link targets | | product doc comments |
| clippy (no S3 feature) | 3 dead-code warnings | the S3 error tables are used only by the SDK adapter | product (marked as intentionally unused there) |
| tests | **10 failures** in the first full run | see below | **all in the tests, none in the product** |
| real S3 server | 1 of 22 contract cases failed | MinIO creates an object on `If-Match` of a missing one (S3 answers 404) | product assumption, fixed in the connect-time probe |
| dependency audit | **7 advisories** in three crates: `h2` (unbounded empty HTTP/2 DATA frames), `rustls` (secrets exposed by `Debug` output of session values; a key-exchange downgrade by an unauthenticated ServerHello), and `rustls-webpki` (a reachable panic parsing certificate revocation lists, and three certificate-validation flaws: URI and wildcard name constraints, and distrusted intermediates) | a default feature of `aws-sdk-s3` (`rustls`) links the legacy hyper 0.14 / rustls 0.21 client; the SDK's own selection code prefers the current client (`default-https-client`), so by reading the source the vulnerable one is linked but not used by default | product dependencies (`Cargo.toml`): the feature is off; 11 crates left `Cargo.lock`; the audit is clean; the binary is 1.9 MiB smaller; the MinIO suite and a TLS run were repeated |
| helper script | `scripts/minio-local.sh stop` printed success and forgot the pid when its shell was not allowed to signal the server | liveness was judged with `kill -0`, which fails for a process that exists but may not be signalled | the script (written in this project): now judged by the server's health endpoint, the outcome is verified, and it fails loudly; tested with an unsignalable pid and with a server it did not start |
| first signal experiment | a SIGTERM "failure" (the process died from the signal) | my experiment counted objects left by the previous run, so it signalled before the handler existed | the experiment, not the product; redone with a trigger relative to a baseline. It did reveal the window before the handler is installed (roadmap item 3) |
| S3 warning | `failed to load region from IMDS` on every command when no region is configured | the default region chain asks the instance-metadata service first | product behavior; the documentation now says to set a region (any of three ways silences it); the fix is roadmap item 12 |

The ten test failures were each analyzed before anything was changed, and none showed a product
defect:

- **Test data generator** (3 failures): `pseudo_random(len, seed)` started from `seed | 1`, so seeds 2
  and 3, 4 and 5, and so on gave identical bytes. Fixtures that meant "two different chunks" got the
  same chunk twice. Fixed at the source (the seed is multiplied by an odd constant first) and covered
  by a test that 1000 seeds give 1000 different outputs.
- **Wrong expectation about order** (3): the scanner lists names in ascending byte order, as
  documented (`.` sorts before `_`), and the test had expected directories first; two prune tests
  compared sorted file names with ids in creation order, although ids are random.
- **Wrong fixture** (2): skipped records copied onto a manifest with fewer roots, which the
  validator rightly refused; a stray line that wrote a file to a path with a trailing slash.
- **A platform premise** (2): two restore tests assumed that `i64::MIN` seconds cannot be a
  `SystemTime`. On 64-bit systems it can, so no valid timestamp is unrepresentable. The tests now check
  what matters (never wrapped to the wrong side of the epoch; a nanosecond count that overflows is
  reported).

The real-server finding is the kind of thing only a real server shows. The storage contract says an
`If-Match` update of a missing object must be refused. MinIO of that release creates it. Nothing in a
repository depends on this (backups, locks, snapshot commits and chunk publication use only the
exclusive create, `If-None-Match`, which MinIO honors: the contract's 8-way race has exactly one
winner), but the backend had declared a conditional-replace guarantee it had not tested. The connect
probe now also tries an update of an object that does not exist, and a service that creates it is
given no conditional-replace guarantee. A unit test with a fake that behaves that way pins it.

## 3. The tests

| Kind | Where | What it proves |
|------|-------|----------------|
| Unit | beside the code (`src/**`, 633 in the core, 86 in the CLI) | each module's rules, including byte-exact formats, every truncation and bit flip of the codecs, golden command output |
| Property-based | `crates/vaultsnap-core/tests/properties.rs` (17), `tests/chunking.rs` | invariants over generated inputs (below) |
| Randomized round trips | `tests/roundtrip.rs` | restore(backup(tree)) equals the tree for trees nobody wrote by hand |
| Crash recovery | `tests/crash.rs` | a backup and a prune killed at every storage call |
| Integration, library | `tests/backup.rs`, `restore.rs`, `check.rs`, `prune.rs`, `history.rs`, `scan.rs`, `snapshot.rs`, `storage.rs`, `chunk_store.rs` | each command on real directories and real encrypted repositories |
| Integration, binary | `crates/vaultsnap-cli/tests/` (`e2e.rs`, `backup.rs`, `restore.rs`, `check.rs`, `prune.rs`, `inspect.rs`, `cli.rs`, `s3.rs`) | exit statuses, output, and the user-visible behavior of the compiled program |
| Real S3 server | `tests/s3_minio.rs` | the SDK adapter, conditional writes, uploads, resuming, an outage, the storage contract |
| Shell | `contrib/test-scheduled.sh`, `scripts/test-package.sh` | the scheduler wrapper and the packaging script |

### Property-based tests

Run by `proptest` with fixed case counts (24 to 256 per property); a failing case is shrunk and
saved in `proptest-regressions/` (git-ignored scratch; commit a regression file with its fix):

- **Cryptography.** Sealed data opens only under the same key and context; any single change,
  truncation, or extension of sealed bytes is detected; two seals of the same data differ (no nonce
  reuse); purpose keys are deterministic and pairwise different.
- **Retention.** Every snapshot gets exactly one decision; the newest is always kept; the result does
  not depend on the order of the input; `keep-last N` keeps exactly the newest N; a larger count never
  keeps less; the daily rule keeps the newest snapshot of each of at most N days.
- **Parsers.** The `config` parser never panics and reads back what it writes; arbitrary bytes in place
  of a chunk object, a snapshot file, or a key file are never accepted; a name that passes validation
  can never leave the repository.
- **Chunk store.** What is put comes back; a second put is a duplicate; a changed byte anywhere in a
  stored object is never decoded into wrong data; different data has a different id.
- **Backends.** The in-memory and directory backends behave like a map under random sequences of
  create, overwrite, conditional update, delete, get, and list.
- **Round trip.** For any set of files, a restore returns the same bytes.

### Randomized filesystem round trips

`tests/roundtrip.rs` generates two source trees per seed with a deterministic generator: nested
directories with awkward names (spaces, a newline, a leading dash, non-ASCII, long), empty, tiny,
small, large, repeated, zero-filled, and edited-copy files, relative and dangling symbolic links, hard
links, and varied permissions. It backs both up, makes random edits (change, delete, add, rename,
`chmod`), backs up again, and restores both snapshots and the first one again. Every restore must equal
the tree as it was in content, permissions, modification times, links, and the set of paths; a full
`check --read-data` must be clean. Ten seeds run by default; a failure names its seed, and
`VAULTSNAP_ROUNDTRIP_SEED=<n>` replays exactly that one (`VAULTSNAP_ROUNDTRIP_SEEDS=<n>` runs more).

### Crash recovery

The model is a storage that stops answering: after the Nth call every call fails. Each call can fail
the two ways an unreliable network lets it: it never arrived, or it was carried out and the answer was
lost. `tests/crash.rs` first counts the storage calls of an undisturbed run, then repeats the run
killed at **every** call number in both ways, against a copy of the same repository, and checks what a
person would find afterwards:

- a backup: every visible snapshot is complete and restores exactly to what it was made from; the
  earlier snapshot is untouched; a full check finds no errors; and the next backup works with no manual
  step;
- a prune (of three snapshots down to one): the newest snapshot is never lost, everything that remains
  restores exactly and checks clean, and running the prune again finishes the job. The one manual step,
  removing the dead process's lock, is taken the way an operator would;
- a lost `config` file is rewritten from the repository id and everything works again (and an invented
  id unlocks nothing);
- a dead backup's lock blocks `prune` until removed.

What this cannot show: a power cut that loses writes a file system had acknowledged, torn writes
inside one file, or behavior of a real disk. It runs on the in-memory backend, so it tests the
ordering of VaultSnap's own writes (chunks before the snapshot, snapshots removed and synced before
chunks), not the durability of `fsync`.

### End-to-end regressions

`crates/vaultsnap-cli/tests/e2e.rs` drives the binary through a repository's life (three backups with
edits between them, restores of each, `diff`, a full check, a dry run, a real prune, the survivor
restored again), a wrong passphrase refused by every command with the repository unchanged, the
scheduled invocation (configuration from the environment, no terminal, no secret in any argument,
`prune` refusing without `--yes`), the exit statuses a scheduler relies on (1, 2, 3), and the version
output.

## 4. Review: memory, locking, permissions, integrity, cryptographic metadata, errors

Each item says what the code does, how it was checked, and what was found. The review changed one
thing (the S3 probe, above); the rest needed no change.

**Memory.** Backup memory is bounded by construction: a bounded queue of `2 x jobs` and at most
`4 x jobs` files in flight, each chunker buffer at most twice the maximum chunk size (8 MiB by default),
at most 64 workers. Restore, check, and prune read one object at a time (check and prune keep one
small record per object). The unbounded parts are the manifests: one snapshot is decoded in memory
(capped at 1 GiB on read, with counts checked against the bytes left before anything is allocated),
and a backup also holds the previous snapshot of each source. Measured peaks are in section 6: they
did not grow with the amount of data and grew by about 2 KiB per file in the worst case. An S3 object
is read whole, up to its maximum size.

**Locking.** A lock is a file `locks/<random>.lock` created with the no-overwrite primitive; acquiring
one lists the others and fails at once, naming the holder, if a conflicting one exists (it never waits
and never removes anyone's lock). Backups, checks, restores, and listings take shared locks; prune
takes an exclusive lock, and a dry run a shared one. Verified: the lock unit tests refuse a second
exclusive lock and a shared lock next to an exclusive one; the contract's race of eight creators of one
name has exactly one winner on every backend, MinIO included, which is the primitive locks rest on;
against the real binary a dead shared lock does not block backups or checks but blocks `prune`, and a
dead exclusive lock blocks everything (section 5); the backup, restore, and prune tests assert that
the lock is gone after success, failure, and cancellation. **Weak points, by design or by reading
the code, not tested:** a killed process leaves its lock until a person removes it (documented with the
exact steps in [`DISASTER_RECOVERY.md`](DISASTER_RECOVERY.md)); two processes that start at the same
time can both fail instead of one winning, because each sees the other's file; a panic would release
the lock because unwinding is on in release builds, but no test panics a worker.

**File permissions.** Observed on disk with `umask 022`: the repository directory and every directory
in it are `0700`; `config`, key files, lock files, snapshots, and chunk objects are `0600`; a
world-readable password file produces a warning (and is still used); restored files get the recorded
permissions through the open handle, set-user-id and set-group-id bits are never restored, and no
temporary file was left behind. Restore writes through a private temporary file and never follows a
link below the target.

**Object integrity.** Every object is authenticated when read: the chunk object's header and chunk id
are associated data, so an object cannot be edited, truncated, extended, or filed under another id
(property test: a changed byte anywhere is never decoded; tests: swapped ids, wrong key, truncation at
every length). After decryption the content is hashed and compared with its id, and the length with
the recorded length. Snapshots are authenticated with their id and the repository id. One deliberate
limit: a backup treats an existing object as present without re-reading it, so damage is found by
`check --read-data` or by the restore that needs it, not hidden by deduplication
([`CHECK.md`](CHECK.md)).

**Cryptographic metadata.** XChaCha20-Poly1305 with a fresh random 192-bit nonce per object (property
test: two seals of the same data differ); Argon2id parameters, salt, algorithm names, key version, and
the repository id are the associated data of the key file (tests: every ciphertext digit, changed
parameters or salt, a key file moved to another repository, newer versions); parameters above 1 GiB,
16 passes, or 16 lanes are refused on read so a modified file cannot exhaust memory or time; purpose
keys come from HKDF-SHA256 with fixed, versioned labels (property test: independent). Every format
carries an explicit version that is checked before anything else, and a newer one is reported as
unsupported, never guessed at. Not done: there is no lower bound on the Argon2 cost when reading a key
file, because the parameters are authenticated and an attacker cannot change them without breaking
the file.

**Error handling.** No `unwrap()` in non-test code (`clippy::unwrap_used`, enforced with `-D warnings`);
the only `expect` calls in non-test code state an invariant (a hard-coded default that is valid, and
writing to a `String`). Errors are typed in the core and converted at the CLI boundary, with exit status
0 success, 1 failure, 2 usage, 3 the result has a problem, 130 cancelled (130 only through the library,
the others through the binary). Names that come from the backed-up data are escaped in the command's
output and error messages (tested with hostile names); no credential appears in an error or in `Debug`
output (asserted by the S3 tests, including against a real server).

## 5. User flows run against the real binary

The documentation's own instructions were run, with the release binary (the TLS run used the debug
build of the same code), in scratch repositories that were deleted afterwards:

- the restore drill of [`GUIDE.md`](GUIDE.md): backup, restore into a scratch directory, `diff -r`
  reports the trees **identical**, and `check --read-data` exits 0;
- the three `jq` recipes of [`CLI.md`](CLI.md) and the "newest snapshot's creation time" example;
- the scheduled wrapper `contrib/vaultsnap-scheduled.sh` with the real binary: exit 0, the snapshot is
  tagged, retention applied (with every snapshot on one UTC day the daily/weekly/monthly rules keep
  just the newest);
- a dead *exclusive* lock: `prune` and `backup` exit 1 and `check` refuses too; a dead *shared* lock:
  `backup` runs, a dry-run prune runs, a deleting prune refuses, and `check` reports a `lock_present`
  warning; after the file is removed everything works. This corrected the guides, which did not say
  that a dead exclusive lock stops `check` too;
- a lost `config`: the commands report "not a VaultSnap repository"; the four-line file rewritten from
  the repository id restores service and `check --read-data` is clean; a `config` with another id gives
  "incorrect passphrase, or the key file was modified";
- **signals**, delivered to the release binary by a script that waits until work is under way (by
  watching the repository, not by sleeping) and then signals. `backup` of 6,000 small files with
  SIGINT after 209 objects, and of 3,000 with SIGTERM after 202: exit status 130 within 0.02 s, a
  message (`cancelled: no snapshot was created (chunks already stored are kept and will be reused)`),
  **no snapshot, no lock file, no temporary file**, `check` clean, and the next `backup` completes
  (it reused the 210 stored chunks) and passes `check --read-data`. `restore` with SIGINT after 325
  files and with SIGTERM after 347: exit status 130, a message, no partial files, and resuming with
  `--on-conflict skip` yields all files, byte for byte equal to the source. **A signal that arrives
  before the handler is installed** (during key derivation, a fraction of a second after start) kills
  the process by the signal itself; the first run of the experiment hit this with SIGTERM and found
  the repository untouched. `check` and `prune` finish too quickly to be caught and were **not**
  tested; nothing here is in the automated suite (roadmap item 3);
- **TLS**, with MinIO serving HTTPS on loopback behind a generated CA and a leaf certificate that
  names only `127.0.0.1`: with no CA bundle, and with an unrelated CA, every command is refused
  (`invalid peer certificate: UnknownIssuer`); with the right CA, `init`, `backup`,
  `check --read-data`, and `restore` succeed and the restored tree is identical to the source; the
  URL `https://localhost` is refused (`certificate not valid for name "localhost"`); there is no
  option to turn verification off (`--s3-insecure` is an unknown argument); the secret key does not
  appear in the output. This exercised the dependency change above. The case of a certificate from a
  public CA (the common case with AWS) cannot be run on loopback and was **not** tested;
- the region warning: set by `--s3-region`, `AWS_REGION`, or `VAULTSNAP_S3_REGION` it disappears
  (one warning without; about 0.25 s per command against 0.15 s on a loopback server).

## 6. Performance and memory

All numbers are from one machine (above), the release profile (thin LTO, one codegen unit), built
without the S3 client because the benchmarks do not touch it, with cheap key-derivation parameters
for the throughput benchmark (it measures the engine, not Argon2). Compare rows, not machines.
`cargo bench -p vaultsnap-core --bench chunking` and `--bench throughput` reproduce them; the
generated data is removed when the run ends.

### Chunking (the chunker alone, in memory)

About **2.0 to 2.4 GiB/s** for inputs of 1 MiB and larger, whatever the content (random, zeros, text,
counters, low-entropy bytes), the chunk-size configuration (256 KiB/1 MiB/4 MiB or 2 KiB/8 KiB/32 KiB),
and API (streaming or in-memory slice). For 64 MiB inputs:

| Content | Default sizes, stream | Default, slice | Small sizes, stream | Small, slice | Chunks (default / small) |
|---------|----------------------|----------------|---------------------|--------------|--------------------------|
| random | 2095 MiB/s | 2371 | 2267 | 2369 | 53 / 7204 |
| zeros | 1937 | 2011 | 1937 | 2000 | 16 / 2048 |
| text | 2087 | 2341 | 2173 | 2245 | 56 / 7184 |
| counter | 2035 | 2341 | 2165 | 2275 | 56 / 7172 |
| low-entropy | 2076 | 2342 | 2258 | 2360 | 55 / 7153 |

Rows for inputs under one chunk (4 KiB, 64 KiB) are dominated by timer resolution and by the slice API
returning the whole input as one chunk without scanning it; ignore them.

### End to end (64 MiB per dataset, 2 workers, local disk)

| | random, 32 files of 2 MiB | text, 64 files of 1 MiB | small files, 6559 of 4 to 16 KiB |
|---|---|---|---|
| backup, first, 1 worker | 103.8 MiB/s | 74.0 | **1.3** |
| backup, first, 2 workers | 123.8 | 91.6 | **1.3** |
| stored size / data size | 1.00x | 0.19x (zstd) | 1.01x |
| backup, nothing changed | 0.018 s (32 files reused, none read) | 0.022 s | 0.64 s (6559 reused) |
| backup, 1 % of files changed | 0.029 s (read 2.0 MiB) | 0.031 s | 1.16 s |
| restore | 392 MiB/s | 527 | 10.4 |
| check, quick | 0.011 s | 0.114 s | 0.25 s |
| check, full (read and decrypt everything) | 794 MiB/s | 1163 | 151 |

The in-memory chunk store alone: **285 MiB/s** to store new random chunks (380 MiB/s for text, which
compresses to 18 %), **1.8 GiB/s** to recognize chunks that already exist, 356 to 649 MiB/s to read
and verify.

**Why small files are slow, measured and not assumed.** Every chunk object is committed durably before
it is published: the file is synced, then the directory. Rust's `sync_all` on macOS flushes to the disk
(`F_FULLFSYNC`). A 800-file experiment on this machine: 0.05 ms per file with no sync, 0.06 ms with
plain `fsync`, **3.58 ms with `F_FULLFSYNC`**. Two of those per object is about 7 ms, which is what the
backup measured (7.7 ms per file, the same with one worker or two because the disk serializes it).
That is the price of the guarantee that a published object is on disk; Linux `fsync` is cheaper, but
this was not measured there. It is a deliberate trade, documented rather than changed in this
release. The fix is structural: pack many small chunks into one object (a format change), or sync once
per backup before the snapshot is committed.

### Deduplication

64 MiB of random data in 32 files of 2 MiB: 75 chunks stored the first time. A second source with the
same data in which 8 of the files had 8 bytes inserted at the start: **8 new chunks, 67 reused**; the repository
holds 73 MiB for 128 MiB backed up (1.75x). An exact copy at another path: **0 new chunks**, 73 MiB for
192 MiB (2.63x). An insertion changes only the chunks around it, which is the point of content-defined
chunking.

### Memory (peak resident size of the release binary, `/usr/bin/time -l`)

| Operation | 256 MiB in 8 files | 10,000 files of about 150 bytes |
|-----------|--------------------|---------------------------------|
| unlock only (`snapshots` on an empty repository) | 73 MiB | 73 MiB |
| backup, first | 120 MiB | 76 MiB |
| backup, again | 103 MiB | 90 MiB |
| restore | 85 MiB | 93 MiB |
| `check --read-data` | 111 MiB | 81 MiB |
| `snapshots` / `ls` | | 80 MiB |

Almost all of the 73 MiB floor is the Argon2id working set (64 MiB) that every unlock needs. Memory did
not grow with the data (256 MiB peaked at 120 MiB), and 10,000 files and 10,000 objects added 3 to
20 MiB. `--jobs 1`: 80 MiB; `--jobs 4`: 101 MiB.

## What is not verified

- **Linux and Windows.** Nothing was built or run on them. The GitHub workflows (CI on Linux x86-64 and
  arm64 and macOS, a Windows build check, the release workflow for four targets) are written and their
  YAML parses, but none has run on GitHub. Windows is not supported.
- **AWS S3 itself, and everything about the SDK that a local server does not exercise:** a
  certificate from a public CA, named profiles, SSO, role credentials, request signing for a real
  region, throttling. The tested server is a MinIO release built from source, on loopback, over plain
  HTTP in the automated tests and over TLS with a generated CA in a manual run.
- **Signals in the automated suite.** `backup` and `restore` were checked by hand with SIGINT and
  SIGTERM (section 5); `check` and `prune` were not caught mid-flight; none of it is a test that runs
  in CI. The interactive passphrase prompt and the `prune` question need a terminal and have not
  been exercised.
- **Power loss and torn writes**, and anything about real disk failures.
- **Scale.** The largest dataset measured was 256 MiB (and 10,000 files). The `--large` benchmark
  (512 MiB per dataset) was not run, and no multi-gigabyte or million-object repository was tried.
- **Tests that skip themselves** where the platform cannot create the case (non-UTF-8 names on macOS,
  FIFOs and sockets where unavailable, privileged cases as root).
- **Concurrency against a running backup** for `check`: safe by design, not tested with real timing.
- **The Docker Compose file** (`docker/minio/compose.yaml`) was never run; the native script was.
