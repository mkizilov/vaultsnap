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

There is **no encryption yet**. `init` writes only plaintext, non-secret metadata. Do not store
data you care about; backups cannot be made at all at this stage.

### `vaultsnap init`

Creates, at `--repo`: a `config` file (format version, random repository id, creation time) and
the `data/`, `snapshots/`, and `locks/` directories, with owner-only permissions (`0700`/`0600`).
The `config` file is written atomically and last. `init` never overwrites anything: it fails if a
repository already exists there (and reports an unsupported format version separately), if the
path is a file, or if the directory holds unrelated files. Re-running it after an interrupted
`init` is safe.

The planned design is specified first, before any storage code:

- [`docs/FORMAT.md`](docs/FORMAT.md): repository layout, identifiers, chunking, encryption,
  crash safety.
- [`docs/SECURITY.md`](docs/SECURITY.md): threat model, secrets handling, restore safety.

## Workspace

| Crate             | Purpose                                                        |
|-------------------|----------------------------------------------------------------|
| `vaultsnap-core`  | Library: repository handle and layout, `config`, local store, locks, snapshot ids, retention, operations |
| `vaultsnap-cli`   | Binary `vaultsnap`: `clap` argument parsing and output          |

Libraries: `clap` (CLI), `tracing` + `tracing-subscriber` (diagnostics), `thiserror` (errors),
`tokio` (async runtime; repository operations are `async` and run blocking file I/O on Tokio's
blocking pool), `getrandom` (repository ids and temporary file names from OS randomness).

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
stderr, or `RUST_LOG` for fine control. Passphrases will never be accepted on the command line
(see `docs/SECURITY.md`).

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
- CLI integration tests are in `crates/vaultsnap-cli/tests/`. They run the compiled binary and use
  helpers in `tests/common/` that create scratch directories via `TMPDIR`.

CI (`.github/workflows/ci.yml`) runs the four commands above on Linux and macOS.

## Known gaps

- `Cargo.lock` has not been generated or committed yet; it must be produced by the first build
  with network access and committed. Until then CI does not use `--locked`.
- Formatting, linting, and tests were written without a Rust toolchain being available in the
  authoring environment (no network, no preinstalled `cargo`); nothing has been compiled or run
  yet, and the first CI run is the first verification.
- The local store publishes new files with hard links, so the repository must live on a
  filesystem that supports them.

## License

MIT.
