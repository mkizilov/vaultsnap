# VaultSnap

A reliable encrypted incremental backup utility written in Rust.

## Status

**Early scaffold.** The workspace, command-line interface, error model, tests, and CI exist.
Repository storage, encryption, and backup logic are **not implemented**: `init`, `backup`,
`snapshots`, `restore`, `check`, and `prune` parse their arguments, validate them, and then fail
with `... is not implemented yet`. Nothing is written to disk by any command.

The planned design is specified first, before any storage code:

- [`docs/FORMAT.md`](docs/FORMAT.md): repository layout, identifiers, chunking, encryption,
  crash safety.
- [`docs/SECURITY.md`](docs/SECURITY.md): threat model, secrets handling, restore safety.

## Workspace

| Crate             | Purpose                                                        |
|-------------------|----------------------------------------------------------------|
| `vaultsnap-core`  | Library: repository handle, layout, snapshot ids, retention, operations |
| `vaultsnap-cli`   | Binary `vaultsnap`: `clap` argument parsing and output          |

Libraries: `clap` (CLI), `tracing` + `tracing-subscriber` (diagnostics), `thiserror` (errors),
`tokio` (async runtime; repository operations are `async`).

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
- Formatting, linting, and tests of this scaffold were written without a Rust toolchain being
  available in the authoring environment; the first CI run is the first verification.

## License

MIT.
