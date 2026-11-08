# VaultSnap roadmap

What to do next, in priority order. The order follows one rule: **gaps in what was asked for, and
gaps in correctness, security, and verification, come before new features.** Everything below was
found by reviewing the implementation and by running it; the evidence for each item is named. What
is implemented, verified, partial, and unverified today is in the status table of the
[README](README.md#status) and in detail in [`docs/RELIABILITY.md`](docs/RELIABILITY.md); users'
view of the limits is [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md).

Estimates are in engineer-hours and include the tests and the documentation the project's rules
require ([`AGENTS.md`](AGENTS.md)). See [Assumptions](#assumptions) before relying on them.

| # | Step | Tier | Hours |
|--:|------|------|------:|
| 1 | [Run everything on Linux and on GitHub](#1-run-everything-on-linux-and-on-github) | Verification | 12 |
| 2 | [Gate the supply chain](#2-gate-the-supply-chain) | Security | 6 |
| 3 | [Automate signal and terminal tests, and install the handler first](#3-automate-signal-and-terminal-tests-and-install-the-handler-first) | Verification | 12 |
| 4 | [Handle stale locks](#4-handle-stale-locks) | Correctness | 16 |
| 5 | [Keep secrets out of memory dumps](#5-keep-secrets-out-of-memory-dumps) | Security | 10 |
| 6 | [Fuzz every decoder](#6-fuzz-every-decoder) | Security | 12 |
| 7 | [Detect rollback and deletion](#7-detect-rollback-and-deletion) | Security | 16 |
| 8 | [Harden restore against a racing attacker](#8-harden-restore-against-a-racing-attacker) | Security | 20 |
| 9 | [Independent security review](#9-independent-security-review) | Security | 52 |
| 10 | [Expose the scanner's policies in `backup`](#10-expose-the-scanners-policies-in-backup) | Original requirement | 8 |
| 11 | [Key management commands](#11-key-management-commands) | Original requirement | 14 |
| 12 | [Verify S3 against real services and automate TLS tests](#12-verify-s3-against-real-services-and-automate-tls-tests) | Original requirement | 18 |
| 13 | [Make small files fast: group commit](#13-make-small-files-fast-group-commit) | Performance | 20 |
| 14 | [Cut S3 request volume](#14-cut-s3-request-volume) | Performance | 12 |
| 15 | [Measure and fix scale limits](#15-measure-and-fix-scale-limits) | Performance | 16 |
| 16 | [Capture extended metadata](#16-capture-extended-metadata) | Feature | 28 |
| 17 | [Retention grouping and `forget`](#17-retention-grouping-and-forget) | Feature | 12 |
| 18 | [Distribution polish](#18-distribution-polish) | Feature | 16 |
| 19 | [Pack files, an index, and split manifests (format 2)](#19-pack-files-an-index-and-split-manifests-format-2) | Feature | 60 |
| 20 | [Windows support](#20-windows-support) | Feature | 40 |
|   | **Total** | | **400** |

By tier: verification, correctness, and security (1–9) 156 hours; original requirements (10–12)
40; performance and scale (13–15) 48; features and platforms (16–20) 156.

**Sequencing.** Nothing has been released, so the repository format can still change without a
migration path. That window closes at the first tag. Before tagging 0.1.0: do 1–8, 10, and 11, and
decide explicitly about 16 and 19 (they change the format; if they will happen, doing them before
the tag costs less than a migration afterwards). Item 9 is best done once 5–8 are in, so the
reviewer is not given known problems. Items 13–15 are independent of the rest. Item 19 needs 13 and
15 first, because they say how much it is worth.

---

## 1. Run everything on Linux and on GitHub

**Scope.** Build and test on Linux x86-64 and arm64; get the CI workflow, the release workflow, the
Windows build check, the benchmark smoke job, and the MinIO job to run on GitHub; fix whatever
fails; remove `continue-on-error` from the jobs that pass. Run a release dry run
(`workflow_dispatch`) and check the four archives.

**Rationale.** Only macOS on Apple silicon has ever built or run this code. The workflows have been
parsed as YAML and nothing more. Expect Linux-specific failures: tests that assume macOS behavior
(case-insensitive paths, `F_FULLFSYNC`, `mkfifo`, extended attributes, tmpfs without hard links),
and the `cc`/linker environment for `ring`, `aws-lc-sys`, and `zstd`. A distributable CLI that has
been built on one platform is not yet distributable.

**Dependencies.** The public repository, and a machine or runner for each target. No Docker needed.

**Acceptance criteria.**
- `fmt`, `clippy -D warnings`, `test`, and `doc -D warnings` pass on `ubuntu-latest`,
  `ubuntu-24.04-arm`, and `macos-latest`, with `--locked`.
- The MinIO job passes on Linux using `scripts/minio-local.sh`, and the job fails if its tests
  skip (a skipped S3 test must not count as a pass).
- A manual run of the release workflow produces four archives whose checksums verify, and the Linux
  binaries run `vaultsnap version` on a clean container.
- Every difference found is recorded in `docs/RELIABILITY.md` section 2 with its cause and whether
  the product or the test was wrong.
- The Windows job either passes `cargo check` or the failures are listed in item 20.

**Effort.** 12 hours (the first red run on a new platform is the uncertain part).

## 2. Gate the supply chain

**Scope.** Run `cargo audit` and `cargo deny` (advisories, licenses, banned and duplicate crates,
sources) in CI and on a schedule; automate dependency updates; pin third-party GitHub Actions to
commit hashes; generate a software bill of materials and attach it, with the checksums, to every
release.

**Rationale.** Running `cargo audit` once during this review found **7 advisories** in the shipped
dependency tree (`h2`, `rustls`, `rustls-webpki`, including certificate-verification issues). They
came from a default feature of `aws-sdk-s3` that links an old HTTP and TLS stack the program does not
select. Disabling that feature removed 11 crates and the audit is now clean, but nothing keeps it
clean: a new advisory or a new default feature would go unnoticed. The workflows also use mutable
action tags.

**Dependencies.** Item 1 (a CI that runs).

**Acceptance criteria.**
- CI fails on a RustSec advisory that affects `Cargo.lock`, demonstrated once with a deliberately
  pinned vulnerable crate on a branch.
- `deny.toml` allows only the licenses actually present and `MIT`-compatible, and CI enforces it.
- Dependency-update pull requests are opened automatically and run the full CI.
- Every `uses:` in the workflows is pinned to a full commit hash with the tag in a comment.
- Each release carries an SBOM (CycloneDX or SPDX) generated from `Cargo.lock`.

**Effort.** 6 hours.

## 3. Automate signal and terminal tests, and install the handler first

**Scope.** (a) Tests that start the real binary, deliver SIGINT and SIGTERM mid-command, and check
the exit status, the message, and the state of the repository, for `backup`, `restore`, `check`, and
`prune`. (b) Pseudo-terminal tests of the interactive passphrase prompt (no echo, the two entries
must match) and of the `prune` question (`yes` proceeds, anything else declines and changes nothing).
(c) Install the signal handlers before the repository is unlocked.

**Rationale.** Both signal handlers are currently verified only by hand: this review ran SIGINT and
SIGTERM against `backup` (exit 130, no snapshot, no lock, no temporary file, the next run completes)
and `restore` (exit 130, nothing half-written, resuming with `--on-conflict skip` reproduces the tree
byte for byte), but `check` and `prune` were not caught mid-flight and nothing is in the test suite.
The experiment also found a window: a signal that arrives before the handler is installed (during
the key derivation, a fraction of a second) kills the process by the signal's default action. The
process then dies *from* the signal (a shell reports 143 for SIGTERM) without the program's own
message and exit status 130. Nothing is written by then (the first run of the experiment hit this
window with SIGTERM and found no snapshot, no lock, and no temporary file), so no data is at risk,
but schedulers see a different status. The prompts have never been exercised because tests
have no terminal.

**Dependencies.** A pseudo-terminal dev-dependency for (b) (for example `expectrl` or
`portable-pty`), which the project's dependency rule requires justifying in the commit. Item 1 for
running on Linux.

**Acceptance criteria.**
- A test per command delivers each signal at a point where work is under way (triggered by
  observing the repository, never by a sleep) and asserts status 130, the message, no snapshot or
  half-written file, no lock file, and a clean `check` afterwards; the same for a signal sent
  immediately after start.
- `check` and `prune` have slow enough sources to be caught (generated, bounded in size and time),
  or an explicit test hook is added and documented.
- The prompt tests pass on macOS and Linux and assert that nothing is echoed.
- The README's gap about signals through the binary is deleted.

**Effort.** 12 hours.

## 4. Handle stale locks

**Scope.** Make a killed process's lock recoverable without guessing: record the host in the lock
file (it holds the kind, the process id, and the creation time today), refresh a timestamp while the
command runs, and add
`vaultsnap locks` (list) and `vaultsnap locks --remove <name>` (remove one named lock, with a refusal
to remove a lock that is fresh unless forced). Decide and document what counts as stale.

**Rationale.** Today a killed backup leaves a shared lock that blocks `prune` forever, and a killed
`prune` leaves an exclusive lock that makes **every** command, `check` included, refuse until a
person deletes a file by hand. This was demonstrated against the real binary and is documented in
`docs/DISASTER_RECOVERY.md`, but "delete the right file in a bucket" is a weak recovery procedure for
a tool whose purpose is recovery. Nothing removes a lock automatically, deliberately, because a dead
process cannot be told from a slow one.

**Dependencies.** None for the command. A heartbeat adds one small object write per interval, which
costs requests on S3, so decide the interval with item 14 in mind. The lock file format is part of
the repository format ([`docs/FORMAT.md`](docs/FORMAT.md)) and changes there need a version decision
(item 19's window).

**Acceptance criteria.**
- The crash tests (`tests/crash.rs`) are extended: after a kill at every storage call, `locks`
  lists exactly the dead process's lock and removing it by name restores service, with no other
  manual step.
- A fresh lock is not removed without `--force`; a lock older than the documented threshold with an
  unchanged heartbeat is reported stale; both behaviors are tested.
- Removing a lock that does not exist, or a file that is not a lock, fails without touching anything.
- `docs/DISASTER_RECOVERY.md` replaces the hand-deletion procedure with the command.

**Effort.** 16 hours.

## 5. Keep secrets out of memory dumps

**Scope.** (a) Establish what actually clears key material: `argon2` 0.5 declares no `zeroize`
feature, and whether its 64 MiB working memory is wiped after use has not been established; either
upgrade to a version that wipes it or wipe it ourselves around the call, and prove it. (b) Disable
core dumps for the process (`RLIMIT_CORE` 0, and the platform equivalent of non-dumpable) at start,
without `unsafe` in this project's code (through a vetted crate). (c) Evaluate `mlock` for the master
key and decide with evidence. (d) Write the threat model for memory in `docs/SECURITY.md`.

**Rationale.** The documented limit is "copies inside the crypto crates are not cleared". This review
found that `chacha20poly1305` already enables `zeroize` for its cipher state, and that `argon2` has
no such feature, so the part that holds the most material (the Argon2 matrix, derived from the
passphrase) is the unknown part. A crash with a core dump on a shared machine is the realistic
exposure.

**Dependencies.** None. Possibly a new crate (justified in the commit) for the process limits.

**Acceptance criteria.**
- A test, run in CI, that derives a key from a recognizable passphrase, then scans the process's own
  memory (or a forced core file) for the passphrase and the derived key and fails if either is
  found after the documented point of wiping; the test demonstrably fails with the wiping removed.
- `vaultsnap` starts with core dumps disabled; a test checks the limit through the binary on Unix.
- `docs/SECURITY.md` states exactly what is and is not cleared, with the evidence.

**Effort.** 10 hours.

## 6. Fuzz every decoder

**Scope.** `cargo-fuzz` (libFuzzer) targets for the `config` parser, key-file parser, chunk-object
header and decoder, snapshot container, manifest codec, glob-pattern compiler, and the CLI's
reference parsers. Seed corpora from the existing golden tests. A short run in CI and a long run on a
schedule. Fix what it finds.

**Rationale.** The decoders parse bytes an attacker with write access to the repository controls,
and the manifest codec is hand-written. Today they are covered by byte-exact golden tests, tests
that truncate at every length, and tests that flip every bit and every byte, plus property tests with
arbitrary bytes (`tests/properties.rs`). Those are strong but bounded: they do not explore
structure-aware mutations, and they have never been run for hours.

**Dependencies.** A nightly toolchain for the fuzz crate only (`cargo-fuzz` needs it; the workspace
stays on the pinned stable). Item 1 for the CI job.

**Acceptance criteria.**
- One fuzz target per decoder in a separate `fuzz/` crate that is excluded from the workspace
  build, with a corpus checked in.
- No panic, no out-of-memory, no hang over at least 2 CPU-hours per target before the first tag, with
  the run recorded in `docs/RELIABILITY.md`.
- Every crash found becomes a unit test in the module's tests before the fix.
- CI runs each target for 60 seconds on pull requests.

**Effort.** 12 hours (more if it finds something).

## 7. Detect rollback and deletion

**Scope.** Let a client notice that the repository it reads is not the one it last saw: keep a small
local record, outside the repository, of the snapshot ids and the newest creation time seen, and make
`snapshots`, `check`, `backup`, and `prune` warn (and `prune` refuse) when a snapshot has vanished or
the repository is older than the record. Design what an authenticated record inside the repository
could add, and decide.

**Rationale.** `docs/SECURITY.md` and `docs/LIMITATIONS.md` state the gap: someone with write access
to the storage can delete a snapshot, or restore the whole repository to an older consistent state,
and VaultSnap cannot tell, because every object is authentic and internally consistent. For a backup
tool that is the central attack by ransomware with storage credentials. A local record is cheap and
catches the common case (same client, later), though not a fresh machine.

**Dependencies.** A decision on where the record lives (a state directory, honoring a
project-local override for tests) and what a fresh client does. Touches `prune`'s safety rules.

**Acceptance criteria.**
- Tests: deleting a snapshot file, and replacing the repository with an earlier copy, each produce a
  clear warning on the next `snapshots` and `check` and a refusal in `prune`, and `--accept-changes`
  (or the equivalent) updates the record on purpose.
- The record contains no secret and no file name, is written atomically, and is documented.
- `docs/SECURITY.md` says what this does and does not defend against.

**Effort.** 16 hours.

## 8. Harden restore against a racing attacker

**Scope.** (a) Walk the destination with directory handles (`openat`-style) through a vetted crate,
so a directory swapped for a link between a check and its use cannot redirect a write; no `unsafe`
in this project's code. (b) Optional `fsync` of restored files and directories. (c) Optional
restoration of owners (`--restore-owner`, needs privilege), with a clear failure when not permitted.
(d) A test with a thread that swaps directories for links while a restore runs.

**Rationale.** `docs/RESTORE.md` and `docs/LIMITATIONS.md` list these as known limits: restore uses
path-based operations and probes with `symlink_metadata`, which is safe against a hostile
*snapshot* (that is tested) but not against a hostile *local process* racing the restore in a
directory it can write to. Restored files are not synced, so a power cut shortly after can leave a
short file. Owners are recorded but not restored, which matters for restoring a system.

**Dependencies.** A crate for directory-relative operations (for example `cap-std` or `rustix`),
justified in the commit, and the project's `unsafe` ban preserved. Item 1 for Linux behavior.

**Acceptance criteria.**
- The race test (a swapper thread running for the length of a restore into a directory writable by
  the attacker) never causes a write outside the target, over a documented number of iterations; the
  same test fails on the current code, which proves it can detect the problem.
- `--fsync` makes restored data durable (documented how it was checked) and its cost is measured.
- `--restore-owner` works as root and fails with a clear message as a user; both tested where the
  platform allows.
- The limitation is removed from the README and the docs, or restated accurately.

**Effort.** 20 hours.

## 9. Independent security review

**Scope.** Commission a review of the cryptographic design and the implementation by someone who did
not write it: key hierarchy, file formats and associated data, nonce handling, the Argon2 parameters
and their bounds, chunk-id derivation and what it leaks, the lock and publication protocols, the
restore path checks, and the S3 credential and TLS handling. Triage and fix the findings.

**Rationale.** Everything in this project was designed and reviewed by the same hand, with machine
checks (property tests, golden tests, crash tests, an audit). That finds slips, not shared
misunderstandings. The format uses only established primitives and no hand-written cryptography, but
the *composition* (what is authenticated with what, in which order) is ours, and a backup tool's
guarantees are only as good as that composition.

**Dependencies.** Items 5–8 done first, so the reviewer does not spend time on known problems; item
6's corpus; a stable format (the sequencing note above).

**Acceptance criteria.**
- A written report with findings by severity, and a response to each (fixed, accepted with reason,
  or disputed with reason), published in `docs/SECURITY.md`.
- Every high or medium finding is fixed with a regression test, or recorded as an accepted risk with
  the reviewer's agreement.
- The format documents are updated for anything the review changes, before the first tag.

**Effort.** 52 hours: about 40 of the reviewer's time (not engineering time, and an external cost)
and 12 for triage and fixes.

## 10. Expose the scanner's policies in `backup`

**Scope.** Add the options the library already implements: `--include` (repeatable),
`--exclude-file`, `--one-file-system`, `--follow-symlinks=none|inside|all` (with the safe default
unchanged), `--hard-links=preserve|duplicate`, `--on-error=skip|fail`, and `--special-files`. Update
`docs/CLI.md`, the guide, and the golden help test.

**Rationale.** The scanner was built with explicit policies, and `docs/SCANNING.md` documents every
one, but the command line exposes only `--exclude`, `--tag`,
`--host`, `--jobs`, and `--rescan`. A user who wants the repository on a different file system
excluded, or an include list, cannot ask for it, although the code does it. The default for each
option stays conservative (never follow links outside the root; stay on one file system is opt-in).

**Dependencies.** None.

**Acceptance criteria.**
- Each option has a CLI integration test against a real tree (for `--one-file-system`, a synthetic
  device id in a unit test plus a real mount point where the platform allows, skipped and reported
  otherwise).
- `--follow-symlinks=all` prints a warning that links may leave the tree, and the snapshot records
  the policy (it already does).
- `docs/CLI.md` documents each with an example; the "not exposed" limitation is removed.

**Effort.** 8 hours.

## 11. Key management commands

**Scope.** `vaultsnap key list`, `add` (a second passphrase for the same master key),
`remove`, and `change-passphrase`; an `init --kdf-memory/--kdf-passes` option; a re-calibration
suggestion based on a measured unlock time. Enforce a floor on Argon2 cost when *writing* a key file.
Concurrency rules: these commands take the exclusive lock.

**Rationale.** The key-file format already allows several key files, and `unlock` tries each, but no
command adds, removes, or changes one. Changing a passphrase today means creating a new repository
and backing up again, and a repository with one passphrase has one person who can unlock it
(`docs/LIMITATIONS.md`, `docs/DISASTER_RECOVERY.md`). The original specification asked for key
management with a passphrase-protected master key; the lifecycle of that key is half done.

**Dependencies.** None for the commands. Item 4 if the exclusive lock needs the stale-lock command to
be usable. A format note: key files are already versioned.

**Acceptance criteria.**
- Adding a passphrase lets either unlock the repository; removing one stops it unlocking, and the
  last key file cannot be removed.
- Changing a passphrase never exposes the master key and is atomic: a crash at any storage call
  (the crash-test harness) leaves either the old or the new passphrase working, never neither.
- Tests with the interactive prompt (item 3), with a password file, and with a wrong current
  passphrase.
- `docs/CLI.md`, `docs/GUIDE.md`, and `docs/DISASTER_RECOVERY.md` are updated; the limitation is gone.

**Effort.** 14 hours.

## 12. Verify S3 against real services and automate TLS tests

**Scope.** (a) Run the S3 tests and a full backup/restore/prune cycle against AWS S3 itself and at
least two compatible services (for example Backblaze B2, Cloudflare R2, or Ceph), and record which
need which flags. (b) Document a minimal IAM policy. (c) Test credentials from a named profile, from
SSO, and from an assumed role. (d) Turn the manual TLS run of this review into an automated test: a
generated certificate chain in front of MinIO (`scripts/minio-local.sh start-tls`), with the
unknown-issuer, wrong-CA, hostname-mismatch, and success cases. (e) Stop printing the IMDS warning
when an explicit endpoint is configured without a region, and document `--s3-region`.

**Rationale.** The S3 backend has been verified against exactly one server: a MinIO release from
April 2025, over plain HTTP in the automated tests, and over TLS in a manual run during this review
(unknown issuer, wrong CA, and hostname mismatch are refused; a correct CA works end to end; there is
no option to disable verification). That server deviated from S3 in one way (it creates an object on
`If-Match` of a missing one), which the connect-time probe now detects; other services will deviate
in other ways, and only a real run shows how. "Credentials through established AWS mechanisms" was
tested with static environment credentials only. Without a configured region every command prints a
warning about the instance-metadata service (measured: a warning and about 0.1 s more per command on
a machine with no route to it; the cost on a network where the connection hangs was not measured).

**Dependencies.** An AWS account and the other services' accounts (costs and credentials, which this
project must not store). Item 1 for CI.

**Acceptance criteria.**
- A table in `docs/S3.md` of services tried, the date, the version, what passed, and what flags
  were needed; the contract (22 cases) passes on each, or the deviation is handled in the probe with
  a unit test.
- The TLS cases run in CI on every change to the S3 code.
- Profile, SSO, and role credentials are each exercised once and documented (SSO and roles may stay
  manual with a checklist, stated as such).
- With `--s3-endpoint` and no region, no warning is printed and no metadata request is made.

**Effort.** 18 hours plus service costs, which are small.

## 13. Make small files fast: group commit

**Scope.** Stop committing every chunk object durably one at a time. Write the objects of a backup
without a per-object sync, then make them all durable once before the snapshot is published
(a barrier: sync the files, then the directories), keeping the invariant that **a snapshot file that
exists always has all its chunks, durable**. On S3 this is already the case, so use the time for
bounded parallel uploads instead.

**Rationale.** Measured in this review on macOS: a tree of 6,559 small files backs up at 1.3 MiB/s
(7.7 ms per file) regardless of the number of workers, while large files reach 100–124 MiB/s. The
cause was measured too: `sync_all` costs 3.58 ms per file on this machine (a full flush to the disk),
and each object takes two (file and directory). Restoring the same data is about 8 times faster
than backing it up, and a full check over 100 times faster. A backup of a home directory is mostly small files, so this is the
performance limit users meet first. Linux `fsync` is cheaper; that was not measured.

**Dependencies.** Item 1 (to measure on Linux). Careful with the crash-safety argument in
`docs/FORMAT.md` ("write ordering"), which must be restated and re-proved by the crash tests.

**Acceptance criteria.**
- `tests/crash.rs` still passes with kills at **every** storage call, including new ones introduced by
  the barrier, and a new case kills between the last object and the barrier.
- The throughput benchmark shows at least 10 times the current small-file backup rate on the same
  machine, with the number recorded in `docs/RELIABILITY.md`; large-file throughput does not
  regress by more than 5 %.
- The durability claim is restated precisely in `docs/FORMAT.md` and `docs/SECURITY.md` (what a
  power cut can and cannot lose), and a test shows a snapshot is not published before the barrier.

**Effort.** 20 hours.

## 14. Cut S3 request volume

**Scope.** Replace one existence request per chunk with batched listings or bounded parallel
requests; consider multipart upload above a size threshold; measure requests per backup and per
check on a bucket-shaped test.

**Rationale.** A backup checks each chunk with its own request, one after the other, before it
publishes the snapshot, so a large backup to a distant bucket spends most of its time waiting and a
billed request is spent per chunk (`docs/S3.md`). An experiment during development listed the whole
bucket instead and was reverted: for a large bucket and a small backup, a listing is worse. The
right design chooses by size, and needs measurement against a real latency, which the loopback
server cannot give.

**Dependencies.** Item 12 (a real service to measure latency), item 13 (shared commit-barrier
design).

**Acceptance criteria.**
- A request-count test on the fake service (exact numbers asserted for a backup of N chunks with M
  already present) and a latency-injection run showing the wall-clock improvement.
- The chosen strategy and its crossover are documented, with the measurements.
- `check`, `prune`, and `restore` request counts are recorded in `docs/S3.md`.

**Effort.** 12 hours.

## 15. Measure and fix scale limits

**Scope.** Generate synthetic repositories of 1 million files and 100 GiB (bounded memory, cleaned
up afterwards), and measure `backup`, `restore`, `check`, `prune`, `snapshots`, and `ls` for time and
peak memory. Fix the first bottleneck that appears. Candidates visible now: a snapshot manifest is
one object decoded whole (capped at 1 GiB), `snapshots` decrypts every manifest, and `check` and
`prune` keep a record per object.

**Rationale.** The largest datasets measured are 256 MiB and 10,000 files (peaks of 120 MiB and
93 MiB), and nothing larger has been tried. Memory did not grow with data size at that scale,
but the manifest and per-object records are proportional to the number of files and objects, so the
curve will bend somewhere; the cost of a listing grows with the number of snapshots times their
size. These are claims in the documentation that are reasoned, not measured.

**Dependencies.** A machine with the disk space (not a CI runner). Item 13 first, otherwise a
million-file backup takes hours.

**Acceptance criteria.**
- A table in `docs/RELIABILITY.md` of time and peak memory by operation at 10^4, 10^5, and 10^6
  files, from a script in `scripts/` that reproduces it.
- A stated upper bound of supported size, and a failure that is clear (not an out-of-memory kill)
  beyond it.
- The first bottleneck is fixed or has its own item in this roadmap with the numbers.

**Effort.** 16 hours.

## 16. Capture extended metadata

**Scope.** Capture and restore extended attributes, POSIX ACLs, file flags (including macOS flags and
resource forks), and sparse-file hole maps, as new optional fields of the snapshot manifest (schema
version 2, with the version-1 decoder kept and tested), and restore them where the platform allows.

**Rationale.** `docs/LIMITATIONS.md`: none of these is captured, so a restore loses them, which
matters for macOS applications (resource forks and quarantine flags live in extended attributes),
SELinux labels, and file capabilities; sparse files are expanded on restore. This is the biggest
fidelity gap, but a feature rather than a defect, and it changes the format, hence its position.

**Dependencies.** The format decision (see sequencing). Item 8 for the restore side's handle-based
operations.

**Acceptance criteria.**
- Round trip tests with extended attributes, ACLs, and sparse files on macOS and Linux, including
  the randomized round trip (`tests/roundtrip.rs`) extended to generate them.
- A version-1 snapshot (checked in as a fixture) still loads and restores under the new decoder.
- Restoring to a file system that cannot hold an attribute reports it per path and does not fail the
  whole restore; the documentation says which are restored by default.

**Effort.** 28 hours.

## 17. Retention grouping and `forget`

**Scope.** `vaultsnap forget <snapshot>...` to remove named snapshots (then `prune` reclaims the
space), `prune --group-by host,tags,paths` so each group has its own retention, and `--keep-tag`.

**Rationale.** Retention treats every snapshot as one history, so a repository shared by several
machines applies one machine's schedule to another's, and there is no way to remove one named
snapshot (`docs/PRUNE.md`). Both are routine needs once a repository is shared, and both are safe to
add because the engine is already built around an explicit keep set.

**Dependencies.** None. Item 7 if `forget` should refuse a rolled-back repository.

**Acceptance criteria.**
- Property tests extended: per group, every snapshot gets exactly one decision, the newest of each
  group is kept, and groups do not influence each other.
- `forget` of the newest snapshot of a group is refused unless confirmed, and a dry run shows the plan
  as `prune` does; the crash tests cover it.
- Documented in `docs/PRUNE.md` and `docs/CLI.md` with examples.

**Effort.** 12 hours.

## 18. Distribution polish

**Scope.** A man page and shell completions generated from the CLI definition; a Homebrew formula and
`.deb`/`.rpm` packages built in the release workflow; a signed and notarized macOS binary; signed
checksums (for example minisign or cosign); a reproducibility check (two builds, same hash).

**Rationale.** The release today is a tarball with a checksum file and unsigned binaries; macOS will
quarantine a downloaded unsigned binary, and a checksum from the same page protects against
corruption, not against a compromised page (`docs/RELEASING.md` says so). Package-manager install and
completions are what make a CLI usable day to day.

**Dependencies.** Item 1 (a working release workflow) and item 2 (SBOM); signing identities and
keys, which are secrets this repository must not hold.

**Acceptance criteria.**
- `brew install` from the tap and `dpkg -i` of the `.deb` each install a binary that runs
  `vaultsnap version` and has working completions, tested in a clean container or VM.
- The macOS binary passes `spctl --assess` after notarization.
- A release's checksum file is signed and the verification command is in `docs/INSTALL.md`.
- Two independent builds of a tag produce the same archive hash, or the differences are explained.

**Effort.** 16 hours.

## 19. Pack files, an index, and split manifests (format 2)

**Scope.** Group chunks into larger pack objects with an index, so a repository holds thousands of
objects instead of millions, and split a snapshot's tree into shared sub-trees so a manifest is no
longer one object proportional to the whole backup. A new format version with the version-1 reader
kept, an explicit upgrade command, and the crash and contract tests extended to the new layout.

**Rationale.** One chunk per object is the simplest correct unit and was a deliberate decision
(`docs/FORMAT.md` says packs are planned), but it costs: millions of small files to list, copy, and
check; one request per chunk on S3; a manifest decoded whole. Items 13–15 reduce those costs without
changing the format; this item removes them. It is the largest change in the roadmap and should be
done only if the measurements of items 13 and 15 say the simpler fixes are not enough.

**Dependencies.** Items 13 and 15 (their numbers decide whether this is worth doing), item 16 (one
manifest schema change, not two), item 9 (review of the format before it grows).

**Acceptance criteria.**
- A version-1 repository opens read-only under the new code and upgrades with a command that is
  restartable after a crash at any storage call (crash-tested) and never deletes data until the new
  layout is verified by `check --read-data`.
- The object count for a 10^6-file test repository falls by at least two orders of magnitude, and
  `check`, `prune`, and a backup of unchanged data are measured faster (numbers in
  `docs/RELIABILITY.md`).
- The storage contract, the crash tests at every call, and the randomized round trips pass on the new
  layout on every backend.

**Effort.** 60 hours.

## 20. Windows support

**Scope.** Make the code build and pass its tests on Windows: path and reserved-name handling,
permissions and attributes, symbolic links and junctions, file locking and rename semantics, long
paths, case-insensitive collision handling in restore, and a Windows job in CI and the release
workflow.

**Rationale.** The code has fallbacks for platforms without Unix permissions and links, but they have
never been built or run, and `docs/INSTALL.md` declares Windows unsupported. A large share of backup
users run Windows. It is last because it is the largest platform gap that is not a defect: the
product is honest about it, and the work is mostly in the restore and scanner layers.

**Dependencies.** Item 1 (CI that runs), item 8 (handle-based traversal has a Windows counterpart),
item 16 (alternate data streams and ACLs are the Windows form of extended metadata).

**Acceptance criteria.**
- `cargo test --workspace` passes on `windows-latest`, with the tests that need Unix features skipped
  explicitly and reported, not silently.
- Restore refuses or renames reserved names (`CON`, trailing dots and spaces) with a clear message,
  and a name that collides case-insensitively is detected before the first write.
- The release workflow publishes a Windows archive whose binary runs `vaultsnap version`.
- `docs/INSTALL.md` and `docs/LIMITATIONS.md` state exactly what works.

**Effort.** 40 hours (high uncertainty: roughly plus or minus half).

---

## Assumptions

- **One engineer** who knows this codebase; hours are working time, not calendar time, and include
  writing the tests and documentation that [`AGENTS.md`](AGENTS.md) requires for every change.
  Waiting for reviews, runners, accounts, or an external reviewer's schedule is not included.
- **Item 9's 52 hours** include about 40 hours of an outside reviewer's time, which is a cost and a
  calendar dependency, not engineering effort; without it the total is 360 hours.
- **Estimates are central values.** Items 1, 8, 15, 16, 19, and 20 are the most uncertain (they
  depend on what the first run on a new platform or at a new scale turns up); treat them as plus
  or minus 50 %. The others are plus or minus 30 %.
- **No released users.** Nothing has been tagged or published, so format changes need no migration
  path until the first tag. After it, items 4, 7, 16, and 19 each carry the cost of one (their
  estimates do not include that).
- **The rule for tests stays:** nothing is weakened to pass, a failure is analyzed before anything is
  changed (`docs/RELIABILITY.md` section 2 shows what that looks like), and what could not be run is
  reported as unverified.
- **Platforms:** macOS on Apple silicon is the one machine that has run the code. Hours for Linux
  items assume a Linux runner or machine is available. Costs of cloud accounts for item 12 are
  small but real and are not in the hours.
- **Not in the roadmap, deliberately:** a daemon or built-in scheduler (the scheduler files in
  `contrib/` are the supported approach), a graphical interface, mounting a snapshot as a file
  system, deduplication across repositories, and server-side or client-side compression levels
  beyond the current "zstd when it helps". Each could be proposed on its own once the items above
  are done.
