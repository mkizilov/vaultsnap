# Backup

How `vaultsnap backup` works, what it guarantees, and what it does not. The on-disk formats it
writes are specified in [`FORMAT.md`](FORMAT.md); how it reads the tree is in
[`SCANNING.md`](SCANNING.md); the threat model is in [`SECURITY.md`](SECURITY.md).

## Command

```sh
vaultsnap --repo REPO backup PATH [PATH...] [--exclude PATTERN]... [--tag TAG]... [--host NAME]
                                            [--jobs N] [--rescan]
```

| Option | Meaning |
|--------|---------|
| `PATH...` | One or more files or directories. Each is a *source*: one root of the snapshot, recorded under its resolved absolute path. The same source given twice, and any source inside the repository, are refused before anything is stored. |
| `--exclude PATTERN` | Glob to leave out (repeatable); syntax and anchoring in `SCANNING.md`. An invalid pattern is refused before anything is stored. |
| `--tag TAG` | Label stored (encrypted) in the snapshot (repeatable). |
| `--host NAME` | Host name stored in the snapshot. Empty if not given; it is never guessed. |
| `--jobs N` | Worker threads. Default: one per CPU, at most 4; at most 64. |
| `--rescan` | Read every file instead of trusting metadata from the previous snapshot. |

The passphrase comes from `--password-file`/`VAULTSNAP_PASSWORD_FILE` or one no-echo prompt on the
terminal, never from an argument. It is read and the repository unlocked before any source is
touched; a wrong passphrase stores nothing.

Everything else is conservative and fixed by the command line: symbolic links are recorded and not
followed, hard links are stored once, special files are recorded and never opened, other
filesystems are not entered, and the repository itself is always left out of a backup of a
directory that contains it.

### Output and exit status

On success `stdout` has `created snapshot <id>` followed by the sources (and the earlier snapshot
each was compared with), entry counts, file and byte totals split into *read* and *unchanged*, new
and already-stored chunks, bytes written, and the duration. On a terminal `stderr` also shows a
single progress line (files done, files unchanged, bytes read), redrawn at most five times a second;
when `stderr` is not a terminal there is no progress output and no terminal control codes.

| Exit status | Meaning |
|-------------|---------|
| 0 | A complete snapshot was created. |
| 1 | The backup failed; no snapshot was created. |
| 2 | Usage error (nothing was run). |
| 3 | A snapshot was created but is **incomplete**: some paths that should have been backed up could not be read. They are listed on `stderr`, recorded in the snapshot's skipped list, and counted in its totals, so the snapshot can never be mistaken for a complete one. |
| 130 | Cancelled by SIGINT or SIGTERM; no snapshot was created. |

## Pipeline

1. **Prepare.** Resolve and validate the sources, compile the patterns, and choose for each source
   the *parent*: the newest of the eight most recently committed snapshots that has a root with the
   same resolved path. Snapshots are ordered by commit time of the file, which is only a hint; a
   damaged candidate is logged and ignored, not fatal.
2. **Walk.** The scanner walks each source deterministically on the calling thread.
3. **Read and store.** Each regular file that is not reusable goes to one of `--jobs` worker
   threads, which streams it through the content-defined chunker and stores each chunk (keyed id,
   compression, encryption, no-overwrite publish). A chunk that already exists is reused, not
   rewritten.
4. **Assemble.** Results are added to the manifest strictly in scan order, so the manifest does
   not depend on thread timing: the same tree backed up with 1 or 8 workers produces the same
   manifest apart from the snapshot id and times.
5. **Commit.** The manifest is validated, every chunk it names is confirmed present, and it is
   published in one no-overwrite step (`FORMAT.md`, "Atomic commit").

The whole run holds a **shared** repository lock, so backups can run side by side but not during
`prune`, which takes the exclusive lock.

## Skipping unchanged files

A file is not read when the earlier snapshot has the same path with **all** of: equal size,
modification time, inode-change time, inode, and device, *and* the file's modification and
inode-change times are older than the start of the earlier backup by at least two seconds, *and*
every chunk of its recorded chunk list is still in the repository. Then the recorded chunk list is
copied into the new manifest.

- The inode-change time moves on any content, permission, owner, or link-count change and cannot
  be set by ordinary programs, so a program that rewrites a file and restores its modification
  time is still noticed.
- The two-second margin covers coarse filesystem timestamps (down to two seconds on FAT): a file
  modified again within the same tick after it was read would otherwise look unchanged. The price
  is that a file written in the last two seconds before a backup started is read again next time.
- Platforms or filesystems that do not supply these timestamps, inodes, or devices never match, so
  every file is read. That is slower, never wrong.
- The chunk check confirms *presence* only, not content. A damaged chunk is found when it is read
  (restore, or `check --read-data` when built), not by `backup`.
- `--rescan` disables the shortcut. Chunks are still deduplicated, so a rescan reads everything
  but writes nothing that is already stored.
- A moved or renamed directory, or a source given by a different path, has no parent: its files
  are read again, though their chunks deduplicate, so nothing new is stored.

## What a failure, an interruption, or a cancellation leaves behind

A snapshot becomes visible only in the last step, after everything else succeeded. So:

| Event | Result |
|-------|--------|
| Error reading the repository or writing a chunk, bad arguments, wrong passphrase | No snapshot. |
| Ctrl-C or SIGTERM | The workers stop between chunks, the lock is released, and no snapshot is created. Exit 130. |
| Process killed (SIGKILL, power loss) | No snapshot. At worst a `.tmp-` file (ignored by readers), unreferenced chunks (reused by the next run; `prune` will remove them once it is built), and a **stale shared lock file** in `locks/` that blocks `prune` until it is removed by hand; the error names the file. |
| Unreadable files | A snapshot, flagged incomplete (exit 3), listing them. |

Chunks already stored by an unsuccessful run are valid and are not wasted: the next run finds them
and stores nothing twice. Snapshots that were already complete are never touched; repository
files are only created, never modified.

A second Ctrl-C does not force an exit, because an abrupt exit can leave a stale lock; the backup
stops at the next chunk boundary. A file that blocks in the kernel (a hung network mount) can delay
that; use SIGKILL and remove the lock file in that case.

## Files that change while they are read

The scanner records a file's identity and attributes when it is opened and compares them again at
end of file. If they differ (the file was written, truncated, or replaced), that attempt is
discarded and the file is read again, up to three times. A file that keeps changing is given up on
and reported as an **unreadable** path (exit 3). The snapshot never contains data from a torn
read, and the attributes recorded for a file are those of the content that was read. A file that
disappears is noted as skipped but does not make the backup incomplete. Details and limits of this
check are in `SCANNING.md` and `SECURITY.md`.

## Partial results

| Left out | Counts as incomplete? |
|----------|-----------------------|
| Unreadable (permission denied, I/O error), kept changing while read, a hard link whose first file failed | Yes (exit 3) |
| Vanished during the backup, a mount point that was not entered, a special file excluded by policy | No; counted in the summary and logged with `-v` |

The library's `ErrorPolicy::Fail` (not exposed on the command line yet) stops at the first
unreadable file instead.

## Bounded resources

- Workers: `--jobs`, at most 64.
- Files in flight: at most four per worker, so the work queue and the finished-but-unassembled
  results are bounded.
- Memory per worker: the chunker buffer, at most twice the maximum chunk size (8 MiB by default),
  plus one chunk being compressed and encrypted.
- The manifest is held in memory until commit (about 100 bytes plus the name per entry and 36 per
  chunk), which is the dominant cost for very large trees (`FORMAT.md`, "Decisions").

## Reconstructability

Before a snapshot is published every chunk it names is confirmed present, and the manifest's
chunk lengths are required to add up to each file's size. The tests rebuild every file of every
completed snapshot from its chunk list, through the real encrypted store, and compare it with the
source bytes, after repeated runs and after modified, inserted, deleted, and added files; other
tests check that cancelled and failed runs leave no snapshot, release the lock, and (for
cancellation) leave earlier snapshots intact. Restoring files to disk
is not built yet, so this is the only end-to-end proof that a snapshot is complete; the tests have
not been run (see the README, "Known gaps").

## Limitations

- No `check` or `prune` yet. The snapshots a backup creates can be listed, shown, and compared
  with `snapshots`, `show`, `ls`, and `diff`, and restored with `restore` ([`CLI.md`](CLI.md),
  [`RESTORE.md`](RESTORE.md)).
- The command line exposes no scan policy: symbolic links are never followed, and there is no
  `--one-file-system` or `--include`. The library supports them (`BackupOptions::scan`).
- The two-second margin is fixed on the command line.
- Extended attributes, ACLs, file flags, and sparse-file hole maps are not captured.
- Only the content of a regular file is read; a file replaced by a symbolic link between the scan
  and the read is refused (`SCANNING.md`).
- The parent is chosen by commit time of the snapshot file, so restoring old files into the
  repository from a backup can change which snapshot is compared against. This only affects speed.
- Cancellation through signals was not exercised by the CLI tests (signalling a child process
  portably and without races is not possible in them); the cancellation logic is tested in the
  library with a cancel token.
