# Releasing VaultSnap

How a release is made, what it contains, and what has to be true before a tag is pushed. The
mechanics are `scripts/package.sh` (one package for one platform) and
`.github/workflows/release.yml` (all platforms, then a GitHub release). Installation from a package
is in [`INSTALL.md`](INSTALL.md).

## What a release is

For each tag `vX.Y.Z` the workflow publishes, per platform, `vaultsnap-X.Y.Z-<target>.tar.gz`
holding the optimized binary, `README.md`, `LICENSE`, `CHANGELOG.md`, `docs/`, and `contrib/`
(scheduler files), a `.sha256` file for each, and one combined `SHA256SUMS`.

| Target | Built on |
|--------|----------|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| `aarch64-apple-darwin` | `macos-latest` |
| `x86_64-apple-darwin` | `macos-latest` (cross-built; not run on the runner) |

There is no Windows package ([`LIMITATIONS.md`](LIMITATIONS.md)) and no static (musl) build: the
S3 client's TLS library and `zstd` compile C code, which is simplest against the system C library.

Release binaries are built with `--release --locked`: thin LTO, one codegen unit, symbols stripped,
and **unwinding left on**, so that a panic in a worker still runs destructors, which release the
repository lock and wipe key material.

## Version information

The version has one source, `[workspace.package] version` in `Cargo.toml`. `vaultsnap --version`
prints it with the git commit (`git describe --always --dirty --abbrev=12`, recorded by
`crates/vaultsnap-cli/build.rs`); `vaultsnap version` adds the target, profile, and the repository
format numbers read from the constants the code itself compares against, so they cannot drift.
A build from a source archive has no git: set `VAULTSNAP_BUILD_COMMIT` to the commit when building.
A `-dirty` suffix means tracked files differed from the commit: never ship such a binary.

Versioning rules:

- **Patch** (`0.1.0` to `0.1.1`): fixes; reads and writes the same formats.
- **Minor**: new commands, options, and output fields; may start *writing* a newer format
  version only if the release notes say so, and always keeps *reading* every older one.
- **Major**: may remove or change commands, options, and the documented JSON.
- Any change to the repository format, key file, snapshot schema, or chunk object format bumps the
  relevant number in [`FORMAT.md`](FORMAT.md), keeps the old decoder, adds tests, and is the first
  item of that release's changelog entry.

## Before tagging

All of this on the commit that will be tagged, on a machine with the pinned toolchain:

1. `Cargo.lock` is committed (`git ls-files Cargo.lock`). The release workflow refuses to run
   without it.
2. The version in `Cargo.toml` is the one to release, and `CHANGELOG.md` has a `## [X.Y.Z]` section
   with the date (the workflow checks for the heading). Move the entries from `[Unreleased]`.
3. The checks CI runs, locally (`AGENTS.md` has the exact commands):

   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --locked -- -D warnings
   cargo test --workspace --locked
   RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
   sh contrib/test-scheduled.sh && sh scripts/test-package.sh
   cargo audit                      # cargo install cargo-audit --locked; must report no vulnerabilities
   ```
4. The MinIO tests have passed once ([`S3.md`](S3.md)): `cargo test -p vaultsnap-core --test
   s3_minio` against a running server (`scripts/minio-local.sh`), and not as skips. They are the only
   tests that meet a real S3 service.
5. `scripts/package.sh` works on at least one real machine and the packed binary runs
   (`vaultsnap version`).
6. A restore drill from the package, not from a development build ([`GUIDE.md`](GUIDE.md)):
   init, back up a directory, restore it, `diff -r`, `check --read-data`.
7. If the repository format changed: an old repository (made with the previous release) is read by
   the new binary, `check --read-data` is clean, and the changelog says whether anything has to be
   migrated.
8. The working tree is clean and the commit is on the main branch.

## Tagging and publishing

```sh
git tag -s vX.Y.Z -m "VaultSnap X.Y.Z"      # signed if you have a key; -a otherwise
git push origin vX.Y.Z
```

The workflow then: checks that the tag equals `v` + the version in `Cargo.toml`, that `Cargo.lock`
exists, and that the changelog has the section; runs `cargo test --workspace --locked`; builds the
four packages; and, for a tag, publishes the GitHub release with all archives and `SHA256SUMS`.
Started by hand (`workflow_dispatch`) it builds the packages and keeps them as workflow artifacts
without publishing anything: use that to rehearse.

After it finishes, download one archive from the release page, verify it against `SHA256SUMS`, and
run `vaultsnap version` once.

## Signing and supply chain

- `SHA256SUMS` protects against a damaged download, not against a compromised release page. Sign the
  tag (`git tag -s`), and publish the checksum file's hash somewhere separate if that matters to
  you. Signed or notarized macOS binaries are not produced: Gatekeeper will quarantine a downloaded
  binary (`xattr -d com.apple.quarantine vaultsnap`), or build from source.
- Dependencies are pinned by `Cargo.lock`. Before a release, look at what changed in it
  (`git diff Cargo.lock`) and run `cargo audit`, which is not wired into CI yet (roadmap item 2).
  The last run found 7 advisories through a default feature of the AWS SDK and the fix was to turn
  that feature off; check the new feature set of any SDK upgrade with `cargo tree -i rustls@0.21`
  (it must fail with "did not match any packages").
- Nothing in a release depends on the machine that built it except the system C library it links.

## After a bad release

Do not move or delete a published tag. Publish a fixed patch release, and say in its changelog what
was wrong. If the problem can damage repositories (a format bug), say so first, say which versions are
affected, and say what to run (`vaultsnap check --read-data`) to find out whether a repository is.
