# Limitations

What VaultSnap does not do, does only partly, or does with a cost you should know about. Each item
is a decision or a gap; none is hidden anywhere else. The security-relevant ones are also in
[`SECURITY.md`](SECURITY.md), the operational ones in the guides.

## Verification status

- **Built and tested on one platform:** macOS on Apple silicon, Rust 1.94.1. The whole test suite
  passes there (1027 tests), as do the S3 tests against a real MinIO server, formatting, lints, and
  documentation checks. **Nothing has been built or run on Linux or Windows**, and the GitHub
  workflows (CI, release) have not run on GitHub. [`RELIABILITY.md`](RELIABILITY.md) lists exactly what
  was run and what was not.
- **Not tested against AWS S3 itself**, nor TLS with a private CA, named profiles, SSO, or role
  credentials; only against a local MinIO ([`S3.md`](S3.md)).
- **Signals through the binary are untested:** Ctrl-C and SIGTERM handling is exercised only through
  the library's cancel token. A power cut, and torn writes, are not simulated; the crash tests model a
  storage that stops answering.
- **Measured on one machine, up to 64 MiB per dataset** (and 256 MiB / 10,000 files for memory). Nothing
  larger has been tried.
- The `Cargo.lock` is committed; releases build from it (`--locked`).

## Platforms and file systems

- **Intended for Linux and macOS**; only macOS has been tested. Windows is not supported (there are
  fallbacks for systems without Unix permissions and links, but they have never been built or run).
  Path names that are not valid UTF-8 are preserved on Unix and shown escaped.
- **The repository's file system must support hard links** (ext4, XFS, btrfs, APFS, NTFS, ZFS do; FAT
  and exFAT do not), because the local backend publishes new objects with a no-overwrite hard link.
  Network shares vary; run `init` and a `check` on one before trusting it. A bucket needs no such
  thing.
- Case-insensitive file systems: restore's up-front conflict check cannot see case-only clashes, so
  `--on-conflict fail` may stop midway there.
- **Not captured:** extended attributes, ACLs, file flags (including macOS flags and resource forks
  kept as xattrs), and sparse-file hole maps (sparse files are stored and restored with their holes
  filled). Owners are recorded but **not restored**. Set-user-id and set-group-id bits are never
  restored. Sockets, FIFOs, and device nodes are recorded and never restored.
- Hard links are restored as hard links only within one backup root; time resolution is what the
  file system keeps.

## What the commands do not do

- **No repair.** `check` reports; it never fixes. Recovery is by putting copies back and re-running
  `backup` ([`DISASTER_RECOVERY.md`](DISASTER_RECOVERY.md)).
- **No change of passphrase, no second passphrase.** The key file format allows several, the commands
  do not offer them. To change the passphrase, create a new repository and back up to it.
- **Retention is by age rules only** and treats all snapshots as one history: no grouping by host,
  path, or tag; no "forget this one snapshot"; no append-only mode (whoever can back up can prune).
- **No built-in scheduler or daemon.** Use cron, systemd, or launchd ([`GUIDE.md`](GUIDE.md)).
- **No resumable restore** beyond re-running it (each file is complete or absent; re-running with
  `--on-conflict skip` continues). No partial restore of a single file's byte range, and no mount
  of a snapshot as a file system.
- **No progress or bandwidth limit** beyond `--jobs` (and `--s3-max-requests` for buckets).
- `backup` exposes only some of the scanner's policies: no `--include`, no following of symbolic
  links, no `--one-file-system`, although the library supports them ([`SCANNING.md`](SCANNING.md)).
- A listing (`snapshots`, `show`) reads and decrypts every snapshot; it is slow for hundreds of large
  snapshots.

## Scale

- Each chunk is **one object** (a file, or an S3 object), committed durably before it is published.
  Measured: a tree of thousands of small files backs up at about 1.3 MiB/s (7.7 ms per file on macOS,
  where a durable sync is a full flush to the disk, whatever the number of workers), while large files
  reach 100 MiB/s and more; restoring and checking small files is much faster than backing them up.
  A multi-terabyte repository holds millions of small objects, which is slow to list, copy, and check,
  and costs many requests on a bucket. Packing chunks into larger files, or syncing once per backup,
  would fix that; both are changes not made yet ([`RELIABILITY.md`](RELIABILITY.md)).
- Memory grows with the number of distinct chunks and objects (a few dozen bytes each, for `check` and
  `prune`) and with the largest snapshot manifest (at most 1 GiB; one manifest in memory at a time,
  plus the previous snapshot's for change detection). Backup memory is otherwise bounded by about
  `jobs` times twice the maximum chunk size (4 MiB by default).
- `backup` checks each chunk with its own request, one after the other when committing the snapshot.
  On a bucket far away, a backup of millions of chunks spends much of its time waiting.
- Deduplication is within one repository and one chunk size: data stored with other chunk sizes does
  not deduplicate against it.

## Locking and concurrency

- Locks are advisory files, not a security control. A process that is killed leaves its lock file; it
  blocks `prune` (and everything after a dead `prune`) until a person removes it. Nothing removes it
  automatically, deliberately.
- On a storage that does not keep its promises (a bucket service that ignores conditional writes, a
  network file system with weak consistency), concurrent writers could corrupt a repository. The S3
  backend tests the service when it connects and refuses one that fails; a file system is not tested.
- Backups reuse an object that exists without reading it. A damaged object stays damaged, and is
  reused, until it is removed ([`CHECK.md`](CHECK.md)).

## Security

- **Metadata is visible to whoever can read the storage:** the number and sizes of objects (chunk
  sizes reveal rough compressibility and, with the public chunking table, can fingerprint known
  files), and when each was written. File names, paths, contents, host names, and tags are
  encrypted. [`SECURITY.md`](SECURITY.md) lists everything.
- **Rollback and deletion are not detected.** Someone with write access can delete a snapshot, or
  restore the whole repository to an older consistent state, undetected. Keep a copy they cannot
  write to.
- A local attacker who can change the *restore target* during a restore could swap a directory for a
  link between a check and its use (the code forbids `unsafe`, so it cannot use `openat`). Restore
  into a directory only you can write to.
- Restored files are not `fsync`ed; a power cut right after a restore can leave a short file.
- Cryptographic key material is zeroized where the code controls it; copies inside the crypto crates
  and the operating system's swap are not.
- The AWS credentials for a bucket are a secret VaultSnap's encryption does not cover.

## Compatibility

- There is one repository format (version 1). Older binaries refuse newer repositories; no migration
  tool exists because none has been needed ([`FORMAT.md`](FORMAT.md)).
- The JSON fields of `snapshots`, `show`, `ls`, `diff`, and `check` are a contract (fields are only
  added, at the end); the text output is for people and may change.
