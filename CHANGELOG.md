# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/),
and versions follow [Semantic Versioning](https://semver.org/) for the command line and, separately,
the repository format ([`docs/FORMAT.md`](docs/FORMAT.md)): a release that cannot read an older
repository says so in this file.

## [Unreleased]

### Status

**0.1.0 has not been released.** The code was written without a compiler and was first built and
run when release packaging was added. On macOS (Apple silicon, Rust 1.94.1) formatting, lints,
documentation, and all 1027 tests pass, the S3 tests pass against a real MinIO server, the
benchmarks have run, `cargo audit` finds nothing, and SIGINT, SIGTERM, and TLS verification were
checked by hand against the real binary. It has not been built on Linux or Windows, and the GitHub
workflows have not run. [`docs/RELIABILITY.md`](docs/RELIABILITY.md) lists exactly what was and was
not verified, and [`ROADMAP.md`](ROADMAP.md) what to do next. `Cargo.lock` is committed.

### Added

- Repository format 1: `init`, versioned `config`, a random repository id, Argon2id-protected key
  files, XChaCha20-Poly1305 for every object, keyed BLAKE3 chunk ids.
- Content-defined chunking (FastCDC-style, fixed gear table), zstd compression when it helps,
  deduplication across all snapshots.
- `backup`: incremental, bounded concurrency, unchanged-file detection with a safety margin,
  cancellation without partial snapshots, a stable exit status for incomplete backups.
- `snapshots`, `show`, `ls`, `diff` with text and JSON output (a documented contract).
- `restore`: conflict modes, a safe symbolic-link policy, atomic file creation, nothing written
  outside the target.
- `check`: quick and full verification, machine-readable report, never repairs anything.
- `prune`: `--keep-last/daily/weekly/monthly/yearly`, mark-and-sweep that fails closed, dry run,
  explicit confirmation.
- Storage backends behind one interface: local directory, S3-compatible buckets
  (`--repo s3://BUCKET[/FOLDER]`), an in-memory store for tests; retries with backoff.
- `vaultsnap version`, and `vaultsnap --version` with the build's commit.
- Release packaging (`scripts/package.sh`, `.github/workflows/release.yml`), scheduler examples
  for cron, systemd, and launchd (`contrib/`), the guides in `docs/`, and `scripts/minio-local.sh`,
  which builds and runs a pinned MinIO natively (no Docker) for the S3 tests.
- Tests: unit, property-based, randomized filesystem round trips, crash recovery at every storage
  call of a backup and a prune, end-to-end command-line regressions, and the S3 backend against a
  real server; benchmarks for chunking and for end-to-end throughput and deduplication.

### Fixed

- The S3 backend no longer claims a conditional-replace guarantee for a service that creates an object
  on `If-Match` of a missing one (the MinIO release of April 2025 does). Repositories were never
  affected: they use only the exclusive create.
- Test-data generator: different seeds now give different data.
- Seven RustSec advisories in the shipped dependency tree (`h2`, `rustls`, `rustls-webpki`): a
  default feature of `aws-sdk-s3` linked the SDK's legacy HTTP and TLS client. The feature is off,
  11 crates left `Cargo.lock`, the binary is 1.9 MiB smaller, and the audit is clean. The current
  client, which the SDK selected by default anyway, was checked against a real TLS endpoint.
- `scripts/minio-local.sh stop` reported success and forgot the server when its shell was not
  allowed to signal it; it now checks that the server stopped and fails otherwise.
- Comments and documents that still said restore, check, prune, or the S3 backend were not
  implemented or not built.

### Changed

- `AGENTS.md` states the project's boundaries and finishing rules without reference to how the
  work was scheduled.
- Added [`ROADMAP.md`](ROADMAP.md): twenty prioritized steps with scope, rationale, dependencies,
  acceptance criteria, and hours.

### Known limitations

See [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md).
