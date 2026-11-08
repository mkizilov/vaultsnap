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
  `retention` (`RetentionPolicy`), `repository` (`Repository`, `RepositoryLayout`), `ops/*` (one
  file per operation, each an `async` method on `Repository`). No CLI or terminal concerns here.
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
