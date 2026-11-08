# VaultSnap: project boundaries and working agreement

- Work only in `/Users/mk-rune/test_projects/02-vaultsnap`. This is one of fourteen independent repositories.
- Do not read, modify, clean up, or run commands in sibling repositories or the user's configuration.
- Work autonomously: choose sensible defaults and document assumptions; do not ask questions or delegate.
- Never install anything globally or use sudo, Homebrew, launchd, host credentials, or host Docker sockets.
- Dependencies, toolchains, caches, browser binaries, SDKs, temporary files and test data stay in this repository. Preserve the supplied project-local environment. Python packages go in `.venv`.
- `.agent-local/` is ignored, project-local build/runtime state. `.opencode/` is managed orchestration configuration; do not change or remove it or bypass the sandbox.
- Use existing system tools read-only, or install pinned tools under this repository. Do not change shell profiles, user settings, Keychain, or global Git settings.
- Bind test servers to loopback; use ports 21100–21199. Limit build/test workers to two unless a required benchmark explicitly needs more.
- Docker-dependent integration tests must remain reproducible but be reported as unverified here if they require access outside this repository. Do not bypass this boundary.
- Do not touch attached hardware, enroll services, flash firmware, install agents, or alter OS settings. Supply manual checklists for such steps.
- Implement only the current numbered prompt, preserving prior behavior. Run appropriate checks, fix failures, and report genuine limitations without fabricating test or benchmark results.
- Before finishing every numbered turn: stop its servers/watchers, remove scratch outputs, update relevant documentation, and make a local conventional-commit Git commit. Do not push.
- End each turn with a short summary of changes, checks actually run, remaining limitations, and commit hash. The orchestrator will provide the next prompt only after this turn ends.

Extend this file with the project's language-specific architecture and verification commands while retaining these boundaries.

## Engineering rules

- **Focused changes.** Each change does one thing and touches only what it needs. No drive-by
  refactors, renames, or formatting churn in unrelated code.
- **Tests.** Every behavior change comes with tests at the right level: unit tests beside the
  code, CLI integration tests in `crates/vaultsnap-cli/tests/`. Never weaken or delete a test to
  make it pass; fix the code or explain the genuine reason.
- **Specs first.** The repository format (`docs/FORMAT.md`) and security boundaries
  (`docs/SECURITY.md`) are updated before, or together with, any code that changes them.
- **Dependencies.** No unnecessary dependencies and no global installs. Add a crate only when it
  replaces substantial or risky code (parsing, crypto), and say so in the commit message. Rust
  toolchain, Cargo registry, and build output stay under `.agent-local/` (`CARGO_HOME`,
  `RUSTUP_HOME`) and `target/`, both git-ignored.
- **Cleanup.** Remove scratch files, temporary repositories, logs, and generated datasets, and
  stop servers or watchers, before finishing a turn. Tests create temporary data only through
  `tempfile` (which honors the project-local `TMPDIR`).
- **Transparent verification.** Reports list the exact commands run and their real results. Say
  plainly what could not be run (missing toolchain, no network, Docker unavailable) and mark it
  unverified. Never invent or paraphrase results.
- **No secrets.** Never commit keys, passphrases, or real user data. Passphrases are never read
  from command-line arguments.

## Architecture

Cargo workspace (`Cargo.toml`), Rust edition 2021, toolchain pinned in `rust-toolchain.toml`.

- `crates/vaultsnap-core`: library. `error` (thiserror `Error`), `snapshot` (`SnapshotId`),
  `retention` (`RetentionPolicy`), `repository` (`Repository`, `RepositoryLayout`, open/verify),
  `config` (strict `config` file parser/writer), `store` (`LocalStore`: validated names, symlink
  refusal, atomic `write_new`/`write_replace`, private permissions), `lock` (`RepositoryLock`),
  `ops/*` (one file per operation, each an `async` method on `Repository`), `blocking` (runs the
  synchronous store on Tokio's blocking pool), `util` (randomness, hex, time), `kv` (strict
  `key=value` parser shared by `config` and key files), `secret` (`Passphrase`), `crypto` (Argon2id
  `KdfParams`, HKDF `KeyPurpose`/`derive_key`, XChaCha20-Poly1305 `seal`/`open`), `keys`
  (`RepositoryKeys`, key files, unlocking), `chunking` (`ChunkerConfig`, `Chunker`: FastCDC over
  any `Read`, no storage coupling; its algorithm and gear table are part of the repository format,
  so never change them without updating `docs/FORMAT.md` and bumping the format version),
  `chunk_store` (`ChunkStore`, `ChunkId`; `object` is the byte format of one chunk object: header
  and chunk id are AEAD associated data, keyed BLAKE3 ids, optional zstd; `store` is the
  dedup/atomic-write/verified-read logic; the format is specified in `docs/FORMAT.md` and any
  change to it needs a version bump), `scan` (`Scanner`, `ScanOptions`, `FileReader`: the
  filesystem walk and streaming file access; every behavior choice is an explicit policy option
  documented in `docs/SCANNING.md`, and defaults must stay conservative: never follow symlinks,
  never open special files), `snapshot` (`Manifest` model and `validate`, `codec` = the byte
  layout of manifest schema v1, `object` = the encrypted container, `builder`, `store` =
  `SnapshotStore` commit/load/list; a change to the codec layout, the container, or the validity
  rules is a format change: new schema version, old decoder kept, `docs/FORMAT.md` updated, and
  tests added; manifests never leak file names into errors or logs). No CLI or terminal concerns
  here. All repository file
  access goes through `LocalStore`; do not use `std::fs` directly on repository paths elsewhere.
- Cryptography rules: only established crates, never a hand-written primitive; randomness only
  through `util::fill_random`/`random_bytes`; secrets live in `Zeroizing` buffers or types that
  print `<redacted>`; never format a secret into a log, error, or `Debug` output; any change to
  key file layout, AAD, or HKDF labels is a format change (update `docs/FORMAT.md`, bump versions,
  add tests). Tests use `testing::fast_kdf()` (cheap Argon2 parameters); production code never
  does.
- Implemented so far: `init`, config, store, locks, key management (`init`, `unlock`), the
  chunking engine, the encrypted chunk store, the filesystem scanner, encrypted snapshot
  manifests. `backup`, `snapshots`, `restore`, `check`, `prune` are stubs. Nothing connects the
  scanner, chunk store, and snapshot store into a backup yet, and nothing restores.
- Tests that need privileges or special files must detect their absence at run time and say so
  (for example when run as root, or when `mkfifo` or sockets are unavailable) rather than fail;
  any test that changes permissions must restore them with a guard so temporary directories can
  be cleaned up. Never use `unsafe` (it is forbidden), even in tests: spawn `mkfifo` instead.
- Tests and benchmarks: chunking property tests live in `crates/vaultsnap-core/tests/chunking.rs`
  (`proptest`; only properties that are provable, plus fixed-seed statistical tests with wide
  margins, to avoid flaky tests); the benchmark is `crates/vaultsnap-core/benches/chunking.rs`
  (`harness = false`, in-memory data only). Report benchmark numbers only if actually run.
- CLI passphrases (`passphrase` module) come from `--password-file`/`VAULTSNAP_PASSWORD_FILE` or a
  no-echo prompt, never from arguments or environment values, and are read before any repository
  file is created.
- `crates/vaultsnap-cli`: binary `vaultsnap`. `cli` (clap definitions), `commands` (argument to
  core mapping and printing), `logging` (tracing setup, stderr only).
- `docs/`: `FORMAT.md` and `SECURITY.md`.

Conventions: `unsafe` is forbidden; no `unwrap()` outside tests (`clippy::unwrap_used`); errors
are typed in core and converted at the CLI boundary; user-supplied identifiers are validated
before they become paths.

## Verification commands

Run from the repository root with the project-local environment:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

At most two build/test workers (`CARGO_BUILD_JOBS=2`). Test servers, if ever added, bind to
loopback ports 21100-21199. Tests that need Docker must stay reproducible and be reported as
unverified when they cannot run here.
