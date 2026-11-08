# Installing VaultSnap

> **Status.** No release has been published. The code builds and its tests pass on macOS (Apple
> silicon) with Rust 1.94.1; it has not been built on Linux or Windows ([`RELIABILITY.md`](RELIABILITY.md)
> says exactly what has and has not been verified). Run `cargo test --workspace` on your machine
> before trusting it with data you care about, and keep other backups until you have restored from
> it yourself ([`DISASTER_RECOVERY.md`](DISASTER_RECOVERY.md)).

## Platforms

| Platform | Status |
|----------|--------|
| macOS, Apple silicon | Built, tested, benchmarked, and packaged. |
| macOS, Intel | Intended; the release workflow cross-builds it. Not built or run. |
| Linux, x86-64 and arm64 | Intended; CI and the release workflow are written for it. **Not built or run yet.** |
| Windows | **Not supported.** The code has fallbacks for systems without Unix permissions and links, but they have never been built or run; CI has a build check that may fail. |

VaultSnap does not need root. Run it as the user who owns the files to back up, or as root to back
up other users' files (`restore` does not restore owners; see [`LIMITATIONS.md`](LIMITATIONS.md)).

## From a release package

Release packages are built by `.github/workflows/release.yml` for each tag ([`RELEASING.md`](RELEASING.md))
and listed on the repository's releases page with a `SHA256SUMS` file.

```sh
# 1. Download the archive for your platform and SHA256SUMS, then verify:
shasum -a 256 -c --ignore-missing SHA256SUMS      # macOS
sha256sum -c --ignore-missing SHA256SUMS          # Linux

# 2. Unpack and install the one binary:
tar -xzf vaultsnap-0.1.0-x86_64-unknown-linux-gnu.tar.gz
install -m 0755 vaultsnap-0.1.0-x86_64-unknown-linux-gnu/vaultsnap /usr/local/bin/vaultsnap

# 3. Check it runs and what it can read:
vaultsnap version
```

The archive also holds the documentation (`docs/`), the license, the changelog, and the scheduler
files (`contrib/`, see [`GUIDE.md`](GUIDE.md)). A checksum protects against a damaged download, not
against a hostile site: get the archive and `SHA256SUMS` from the same trusted release page, over
HTTPS.

Keep a copy of the package you installed next to your backups. A repository is only readable by a
VaultSnap that understands its format (`vaultsnap version` lists the format numbers), and the one
that wrote it is the one that is certain to.

## From source

You need:

- the Rust toolchain pinned in `rust-toolchain.toml` (1.94.1; `rustup` installs it automatically
  when you build inside the repository), and
- a C compiler, because `zstd` and the TLS libraries of the S3 client (`ring`, `aws-lc-sys`) compile
  C code. On macOS that is the Command Line Tools (`xcode-select --install`); no CMake was needed
  there. `AGENTS.md` explains the two variables a sandbox that blocks `xcrun`'s cache needs.

```sh
git clone <repository-url> vaultsnap && cd vaultsnap
cargo install --path crates/vaultsnap-cli --locked     # installs `vaultsnap` into ~/.cargo/bin
vaultsnap version
```

`--locked` builds with the dependency versions in `Cargo.lock`. If the repository you cloned has no
`Cargo.lock` yet, drop `--locked`: the first build creates one. Whoever makes a release must commit
it ([`RELEASING.md`](RELEASING.md)).

To build the package that the release workflow builds (a tarball in `dist/`):

```sh
scripts/package.sh
```

To check the build before relying on it:

```sh
cargo test --workspace            # the property, randomized, and crash tests take the longest
```

## Making sure you have the right thing

```sh
vaultsnap --version      # vaultsnap 0.1.0 (3f9a1c20b7de): the version and the commit
vaultsnap version        # the formats this binary reads and writes
vaultsnap --help         # every command; vaultsnap COMMAND --help for its options
```

`unknown` instead of a commit means the binary was built from a source archive without git.

## Upgrading

There is one repository format so far (version 1), so every release reads every repository. The
rules for the future are in [`FORMAT.md`](FORMAT.md): a release that finds a repository, key file,
snapshot, or chunk object written in a newer format refuses it with *unsupported version* and
changes nothing, so an old binary can never damage a repository a newer one wrote. Install the
newer release instead. A format change is listed in [`CHANGELOG.md`](../CHANGELOG.md) before it
ships, together with whether older repositories are read as they are or have to be migrated.

Upgrading the binary never touches a repository by itself.

## Uninstalling

Delete the binary (and, if you installed them, the scheduler files). Repositories are ordinary
directories or buckets and are not touched.
