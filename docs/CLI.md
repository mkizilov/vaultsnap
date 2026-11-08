# Command-line reference

```
vaultsnap [--repo PATH] [--password-file FILE] [-v...] <COMMAND>
```

| Command | What it does | State |
|---------|--------------|-------|
| [`init`](#init) | Create an empty repository | implemented |
| [`backup`](#backup) | Back up files and directories into a new snapshot | implemented |
| [`snapshots`](#snapshots) | List snapshots, oldest first, with sizes | implemented |
| [`show`](#show) | Everything about one snapshot | implemented |
| [`ls`](#ls) | List the files in a snapshot | implemented |
| [`diff`](#diff) | Compare two snapshots | implemented |
| [`restore`](#restore) | Restore a snapshot, or selected paths, into a directory | implemented |
| [`check`](#check) | Verify repository integrity, quick or full; report only, never repairs | implemented |
| [`prune`](#prune) | Remove snapshots outside a retention policy and the chunks nothing needs; the only command that deletes | implemented |

## Global options

| Option | Meaning |
|--------|---------|
| `-r, --repo PATH` | The repository. Also read from `VAULTSNAP_REPO`. Required by every command. |
| `--password-file FILE` | File holding the repository passphrase (one trailing newline is ignored). Also read from `VAULTSNAP_PASSWORD_FILE`. Without it, the passphrase is asked for on the terminal with echo off. There is no way to give it as an argument. |
| `-v`, `-vv`, `-vvv` | More diagnostics on standard error (info, debug, trace). `RUST_LOG` overrides. Secrets are never logged. |
| `-h, --help`, `-V, --version` | Usage and version. Every command also has `--help`. |

Options may come before or after the command name.

`backup`, `snapshots`, `show`, `ls`, `diff`, `restore`, and `prune` unlock the repository first, so a
wrong passphrase fails before anything is read, printed, or written. Without `--password-file` and without a
terminal, they fail with *no terminal available to ask for a passphrase*.

## Exit status

| Status | Meaning |
|--------|---------|
| 0 | Success. For `diff`, success does **not** mean "no differences": differences are normal output. |
| 1 | The command failed. The repository was not changed. `backup` created no snapshot; `restore` normally restored nothing (see [`restore`](#restore) for the one exception); `prune` normally deleted nothing (see [`prune`](#prune)). |
| 2 | Usage error: unknown option, missing argument, or a malformed snapshot reference. Nothing ran. |
| 3 | The command finished, but its result has a problem. `backup`: the snapshot was created but some paths could not be read. `snapshots`: some snapshots are damaged and are not shown. `restore`: everything that could be restored was, but some paths could not be (a damaged or missing chunk, a refused write). `check`: the check ran and found **errors** (damage or loss); warnings alone give status 0. `prune`: it finished, but some unreferenced chunk objects could not be deleted. |
| 130 | `backup`, `restore`, `check`, or `prune` was cancelled with Ctrl-C or SIGTERM. |

Standard output carries the result; warnings and errors go to standard error. If the reader of
standard output goes away (`vaultsnap ls … | head`), the output simply ends.

## Snapshot references

`show`, `ls`, and `diff` name snapshots by id. An id is 64 lowercase hexadecimal characters, and
any **start** of an id of at least 4 characters is accepted as long as it matches exactly one
snapshot. `snapshots` shows the first 8 characters of each id, which is enough in practice.

```sh
vaultsnap --repo ~/backups show 3f2a91c4         # the unique snapshot starting 3f2a91c4
```

A malformed reference (fewer than 4 characters, upper case, anything but `0-9a-f`) is a usage
error (status 2). A reference that matches nothing fails with *snapshot … does not exist*; one
that matches several fails with *matches N snapshots; use more characters of the id* (status 1).
There is no `latest`: list the snapshots and pick one. `restore` takes the same references.

## Snapshot data is verified before it is shown

`snapshots`, `show`, `ls`, and `diff` read every snapshot through authenticated decryption: the
file is checked against the repository key and its own id before anything is decrypted, and the
decrypted manifest is validated before it is used. A snapshot that fails (damaged, truncated,
tampered with, renamed, or from another repository) is never shown:

* `show`, `ls`, and `diff` fail with *snapshot … is corrupt* (status 1);
* `snapshots` lists the others, reports each failure on standard error, and exits with status 3.

These commands take a *shared* lock, so they can run next to a `backup` but not during a `prune`.
They never modify the repository.

---

## init

```sh
vaultsnap --repo ~/backups init
vaultsnap --repo ~/backups --password-file ~/.vaultsnap-pw init
```

Creates the repository and prints `created VaultSnap repository <id> at <path>`. It asks for the
passphrase twice on the terminal unless `--password-file` is given. It never overwrites anything.

## backup

```sh
vaultsnap --repo ~/backups backup ~/Documents ~/notes.txt
vaultsnap --repo ~/backups backup ~/Documents --exclude '*.tmp' --exclude 'cache/' --tag laptop
vaultsnap --repo ~/backups backup ~/Documents --host workstation --jobs 2
vaultsnap --repo ~/backups backup ~/Documents --rescan
```

| Option | Meaning |
|--------|---------|
| `PATH...` | One or more files or directories (required). |
| `--exclude PATTERN` | Glob to leave out; repeatable. |
| `--tag TAG` | Label stored in the snapshot; repeatable. |
| `--host NAME` | Host name stored in the snapshot (empty if not given). |
| `--jobs N` | Worker threads; default one per CPU, at most 4, never more than 64. |
| `--rescan` | Read every file instead of trusting unchanged metadata. |

Prints `created snapshot <id>` and a summary; see [`BACKUP.md`](BACKUP.md) for the output, the
change detection, and what happens on failure. Status 3 means the snapshot exists but is
incomplete; 130 means cancelled.

---

## snapshots

```sh
vaultsnap --repo ~/backups snapshots
vaultsnap --repo ~/backups snapshots --json
```

Lists every snapshot, **oldest first**. The order is the creation time recorded inside each
authenticated manifest, ties broken by id; file times play no part. Columns:

| Column | Meaning |
|--------|---------|
| `ID` | The first 8 characters of the id. A `!` after it marks an incomplete snapshot (some paths could not be read when it was taken). |
| `CREATED` | When the backup started, UTC. |
| `HOST`, `TAGS` | As given to `backup` (`-` if none). |
| `FILES` | Regular files in the snapshot. |
| `LOGICAL` | Total size of the files. |
| `STORED` | Space its distinct chunks take in the repository. |
| `UNIQUE` | The part of `STORED` that no other snapshot uses. |
| `SOURCES` | The directories and files that were backed up. |

See [Sizes](#sizes) for exactly what the three size columns mean.

Output for the fixture used by the unit tests (the tests assert exactly this text):

```
ID         CREATED               HOST         TAGS    FILES  LOGICAL   STORED     UNIQUE  SOURCES
aaaaaaaa   2023-11-14T22:13:20Z  workstation  laptop      3      8 B    160 B       80 B  /data
bbbbbbbb!  2023-11-15T22:13:20Z  -            -           4  1.5 MiB  1.0 MiB  512.0 KiB  /data, /etc/hosts

2 snapshots; they refer to 1.0 MiB of stored data in 7 chunks
! incomplete: some paths could not be backed up (see `vaultsnap show`)
```

The footer also says *UNIQUE may be too large: N snapshots could not be read* when any snapshot
is damaged, because a damaged snapshot may have used chunks that look unique to another one. An
empty repository prints `No snapshots.`

With `--json` the same information is one JSON document, see [`snapshots --json`](#snapshots---json).

## show

```sh
vaultsnap --repo ~/backups show 3f2a91c4
vaultsnap --repo ~/backups show 3f2a91c4 --json
```

Everything about one snapshot: identity, status, the sources with their totals, entry counts, the
sizes, the backup configuration, and every path that was left out and why. Output for the test
fixture:

```
Snapshot        abababababababababababababababababababababababababababababababab
Created         2025-06-15T15:06:40Z
Host            workstation
Tags            laptop, weekly
Program         vaultsnap 0.1.0
Status          INCOMPLETE: 1 path could not be backed up
Manifest        version 1

Sources
  /data  (3 files, 1 directory, 8 B, 1 left out)

Contents
  files           3
  directories     1
  symbolic links  1
  hard links      1
  special files   0

Sizes
  logical         8 B    the files as a restore would write them
  deduplicated    8 B    the distinct chunks of this snapshot, uncompressed
  stored          160 B  the same chunks as stored (compressed, encrypted)
  unique stored   80 B   stored data no other snapshot refers to
  chunks          2 (1 unique to this snapshot)

Configuration
  chunk sizes       256.0 KiB / 1.0 MiB / 4.0 MiB (minimum / average / maximum)
  symbolic links    recorded as links, never followed
  hard links        stored once, other names linked to it
  unreadable paths  skipped and reported
  special files     recorded, never opened
  changing files    the read fails (retried, then reported)
  other filesystems not entered
  include patterns  -
  exclude patterns  *.tmp

Left out (1)
  /data/locked  unreadable: open: Permission denied
```

`Status` is `complete` unless some paths could not be read. Paths left out by policy or because
they disappeared during the backup are listed under *Left out* but do not make a snapshot
incomplete. A `WARNING` line appears under *Sizes* if the snapshot refers to chunks that are
missing from the repository, and a `note` if another snapshot could not be read (so *unique
stored* may be too large).

## ls

```sh
vaultsnap --repo ~/backups ls 3f2a91c4
vaultsnap --repo ~/backups ls 3f2a91c4 /home/me/Documents/taxes
vaultsnap --repo ~/backups ls 3f2a91c4 --json
```

Lists every entry of a snapshot in the order the backup walked them (each directory before its
contents, names sorted by their bytes), one per line like `ls -l`:

```
drwxr-xr-x  1000:1000  -  2023-11-14T22:13:20Z  /data/
-rw-r--r--  1000:1000  5  2023-11-14T22:13:20Z  /data/a b.txt
-rw-------  1000:1000  3  2023-11-14T22:13:20Z  /data/new\nline
lrwxrwxrwx  1000:1000  -  2023-11-14T22:13:20Z  /data/link -> a b.txt
-rw-r--r--  1000:1000  5  2023-11-14T22:13:20Z  /data/alias => /data/a b.txt
-rw-r--r--  1000:1000  0  2023-11-14T22:13:20Z  /data/bad-\xff
```

Columns: permissions, `uid:gid`, size in bytes (`-` for entries without content), modification
time (UTC), path. Directories end with `/`, a symbolic link shows `-> target`, and a hard link
shows `=> the path that carries its data`. Unknown values are `-` or `?`.

Paths are **absolute paths on the machine that was backed up**: the source path given to `backup`
(resolved) followed by the path below it. With `PATH`, only that path and everything below it are
listed; it matches whole components (`/home/me/Doc` does not match `/home/me/Documents`) and a
trailing `/` does not matter. A `PATH` that matches nothing fails with *snapshot … has no entry at
or below …* (status 1).

### File names in text output

Names are data from the backed-up machine and can contain anything. In text output they are
escaped so they cannot inject terminal escape sequences or add lines:

| In the name | Shown as |
|-------------|----------|
| backslash | `\\` |
| newline, carriage return, tab | `\n`, `\r`, `\t` |
| any other ASCII control character (including ESC) | `\xNN`, for example `\x1b` |
| a byte that is not valid UTF-8 | `\xNN`, for example `\xff` |
| other control characters and invisible or direction-changing characters | `\u{N}`, for example `\u{202e}` |

Everything else, including spaces and non-ASCII text, is printed as is. The escaping is
reversible. In JSON output names are exact: see below.

## diff

```sh
vaultsnap --repo ~/backups diff 3f2a91c4 b7e01d52
vaultsnap --repo ~/backups diff 3f2a91c4 b7e01d52 --json
```

Compares an older snapshot (first) with a newer one (second) and lists the paths that were added
(`+`), deleted (`-`), or modified (`M`), in path order. Output for the test fixture:

```
Comparing aaaaaaaa (2023-11-14T22:13:20Z) -> bbbbbbbb (2023-11-15T22:13:20Z)

+ /data/added.txt
M /data/edit.txt  (content, mtime)
- /data/gone dir/
M /data/link  (type: file -> symlink)
+ /data/new\nline

2 added, 1 deleted, 2 modified, 10 unchanged
logical size:  1.0 MiB -> 1.5 MiB (+512.0 KiB)
files:         +9 B added, -0 B deleted (regular files only)
stored data:   2.0 KiB only in the older snapshot, 4.0 KiB only in the newer
```

Identical snapshots print `No differences.` The command works with the snapshots in either
order; swapping them swaps added and deleted.

### What counts as a difference

Paths are matched by their **absolute path**: the same file is the same file only if it had the
same path in both snapshots. A moved or renamed file is a deletion plus an addition. For a path
in both snapshots, the parenthesised words say what differs:

| Word | Meaning |
|------|---------|
| `content` | A regular file's size or chunk list differs. Chunk ids are keyed hashes of the data, so equal lists mean equal bytes; no file data is read. A hard link counts with the file it links to, so changing a file also modifies its hard links, and which of several names carries the data is not a difference. |
| `type: A -> B` | The kind of entry changed (a file became a symbolic link, say). Nothing else is compared then. |
| `link_target` | A symbolic link points somewhere else. |
| `mode` | The permission bits differ. |
| `owner` | The owning user or group differs. |
| `mtime` | The modification time differs. Not compared for directories: a directory's time changes whenever something inside it is added or removed, which would drown the real changes. |

Access, inode-change, and creation times are never compared. If the two backups used different
chunk size limits, identical data can produce different chunk lists, so a file of unchanged size
may be reported as `content`; the output then carries a `note`.

The summary's size lines: *logical size* is the total size of the files in each snapshot;
*files* is the total size of regular files that were added and deleted; *stored data* is the
repository space of the chunks that only one of the two snapshots refers to, which is how much
data the newer snapshot added and how much only the older one holds.

---

## restore

```sh
# Everything in a snapshot, into a new directory
vaultsnap --repo ~/backups restore 3f2a91c4 --target /mnt/restore

# Only some files or directories (paths as `ls` prints them)
vaultsnap --repo ~/backups restore 3f2a91c4 --target /mnt/restore \
    --include /home/me/Documents/taxes --include /home/me/notes.txt

# Into a directory that already holds files: keep them, or replace them
vaultsnap --repo ~/backups restore 3f2a91c4 --target ~/Documents --on-conflict skip
vaultsnap --repo ~/backups restore 3f2a91c4 --target ~/Documents --on-conflict overwrite

# Restore symbolic links exactly as recorded (snapshots you trust)
vaultsnap --repo ~/backups restore 3f2a91c4 --target /mnt/restore --symlinks as-recorded
```

Restores a snapshot, or selected paths of it, below the directory given with `--target`. The
command is built to be safe to point at a snapshot you do not fully trust: it authenticates every
chunk before using it, never writes outside the target, never follows a link, and writes every
file through a temporary name and a rename. The full specification is in
[`RESTORE.md`](RESTORE.md).

| Option | Meaning |
|--------|---------|
| `SNAPSHOT` | Snapshot id, or the start of one (4 or more characters). |
| `--target DIR` | Directory to restore into (required). Created, with its parents, if missing. Never inside the repository. |
| `--include PATH` | Restore this path and everything below it (repeatable). Default: everything. The directories above it are created too. A path that is not in the snapshot is an error. |
| `--on-conflict MODE` | `fail` (default), `skip`, or `overwrite`: see below. |
| `--symlinks POLICY` | `contained` (default), `skip`, or `as-recorded`: see below. |

**Where files go.** Below the target, under the absolute path they had when they were backed up:
`/home/me/Documents/a.txt`, restored with `--target /mnt/restore`, becomes
`/mnt/restore/home/me/Documents/a.txt`. Several sources never collide, and `--include` takes the
paths that `vaultsnap ls` prints. There is no option yet to drop the leading path: to get the files
directly into a directory, restore into a scratch directory and move them.

**What is restored.** File content, byte for byte; permissions (without set-user-id and
set-group-id); modification and access times; directories with their permissions and times
(applied last, so a read-only directory can still be filled); symbolic links under the policy
below; hard links as hard links. Not restored: owners, special files (FIFOs, sockets, devices),
extended attributes, ACLs.

### Conflicts: `--on-conflict`

| Mode | When something already exists where an entry would go |
|------|------------------------------------------------------|
| `fail` | Checks everything first and, if anything would be replaced, restores **nothing** and exits with status 1, naming the number of conflicts and the first one. This is the default. |
| `skip` | Leaves what exists, restores the rest, and lists what was skipped. |
| `overwrite` | Replaces files and links, atomically. A directory is **never** replaced: a file that meets a directory is reported as failed. |

Directories that meet directories are merged in every mode; nothing in the target is ever
deleted. After an interrupted restore, run it again with `skip` (fills the gaps) or `overwrite`
(also resets directory permissions and times).

### Symbolic links: `--symlinks`

| Policy | Behavior |
|--------|----------|
| `contained` | Restores a link only if its target is relative and can never lead out of the restore root (`..` only at the start, and not above the root). Others, such as `/etc/passwd` or `../../../x`, are skipped and listed. This is the default. |
| `skip` | Restores no links. |
| `as-recorded` | Restores every link exactly as recorded, including absolute and escaping ones. |

The restore itself never follows a link, whatever the policy; the policy decides which links are
left lying around for other programs to follow.

### Output

The summary goes to standard output. (The numbers below are illustrative; the layout is what the
command prints.)

```
restored snapshot <64-character id> into /mnt/restore
  files         6 (200.0 KiB written)
  directories   9
  links         1 symbolic, 0 hard
  skipped       1 (left out on purpose)
  duration      0.1s
```

The lines `skipped`, `failed`, and `attributes` appear only when they are not zero. Problems are
listed on standard error, with the path first (escaped, since it comes from the snapshot) and the
reason after it:

```
1 could not be restored:
  /mnt/restore/home/me/big.bin: object 9c1e…7a is corrupt: …
1 skipped:
  /mnt/restore/home/me/escape.link: its target leads, or could lead, out of the restore root; restoring it needs --symlinks as-recorded
```

A terminal also shows a progress line (`64.0 KiB of 200.0 KiB written, 4 entries`) that is erased
when the restore ends; it is never printed when standard error is not a terminal.

### Exit status

| Status | Meaning |
|--------|---------|
| 0 | Everything that should be restored was. Skipped paths do not count against this. |
| 1 | The restore stopped: the snapshot is missing, damaged, or unsafe; the target is unusable; an `--include` matches nothing; a conflict in `fail` mode. All of these are found before the first write, so nothing was restored, with one exception: a conflict that only appears while writing (something created in the target by someone else in the meantime, or a case-insensitive file system) stops the restore there, with the files before it restored. |
| 2 | Usage error (unknown mode or policy, missing `--target`, malformed snapshot reference). |
| 3 | The restore finished, but some paths could not be restored (a missing or damaged chunk, a refused write, a directory in the way in `overwrite` mode). Everything else was restored, and no partial files remain. They are listed on standard error. |
| 130 | Cancelled with Ctrl-C or SIGTERM. Files already restored are complete; nothing half-written remains. |

Because chunks are authenticated before use, a damaged repository shows up here as status 3 with
the affected paths listed, never as silently wrong files.

---

## check

```sh
# Quick: metadata, snapshots authenticated, objects examined by structure (seconds to minutes)
vaultsnap --repo ~/backups check

# Full: also read, authenticate, and decrypt every chunk (reads the whole repository)
vaultsnap --repo ~/backups check --read-data

# For scripts and monitoring
vaultsnap --repo ~/backups check --read-data --json
vaultsnap --repo ~/backups check --json | jq '.counts, .affected_snapshots'

# Fewer threads, or every finding listed
vaultsnap --repo ~/backups check --read-data --jobs 2 --max-findings 0
```

Verifies the repository and **reports**; it never repairs, moves, or deletes anything (the one
file it writes is its own lock, which it removes). It asks for the passphrase because it
authenticates snapshots and chunks.

| Option | Meaning |
|--------|---------|
| `--read-data` | Full check: every chunk object is read, authenticated, and decrypted. Without it only the structure of the objects is examined. |
| `--json` | One JSON document on one line instead of text. |
| `--jobs N` | Threads for reading objects (default: the number of CPUs, at most 4). The report does not depend on it. |
| `--max-findings N` | List at most N findings, errors first (default 1000; 0 lists all). Counts stay exact. |

What each mode finds, every kind of finding, **what to do about each failure**, and the JSON
fields are in [`CHECK.md`](CHECK.md). In short:

* A **clean quick check does not prove the data is intact**: it never reads the payload of a chunk.
  Damage inside a chunk is found only with `--read-data`.
* Errors (exit status 3): a missing or damaged directory, snapshot, or chunk object; an
  unsupported version; a read the system refused. The report lists which snapshots can no longer
  be restored completely.
* Warnings (status 0): orphaned objects, stray files, leftovers of interrupted writes, lock files,
  key files that do not parse.
* The check cannot run, and says why, when the location is not a repository, `config` is damaged,
  the passphrase is wrong, `keys/` is gone, or another process holds an exclusive lock (status 1).

Output for a repository with a missing chunk (quick mode; the numbers are illustrative, the layout
is what the command prints and `crates/vaultsnap-cli/src/check/tests.rs` asserts):

```
Repository 0123456789abcdef0123456789abcdef (format version 1)
Mode         quick: snapshots authenticated, chunk objects examined by structure only

Snapshots    2 readable, 0 damaged
Chunks       10 referenced by snapshots, 1 missing
Objects      9 in data/ (49.1 KiB), 0 damaged
Unreferenced 0 objects (0 B)
Key files    1
Took         0.1s

ERRORS (1)
  chunk_missing  data/ab/ab…(64 hex digits)
      no object exists for a chunk that snapshots refer to

AFFECTED SNAPSHOTS (cannot be restored completely)
  3f2a91c4  1 file affected (1 damaged or missing chunk)

WHAT TO DO (the check changed nothing; see docs/CHECK.md)
  chunk_missing: Snapshots that refer to it cannot be restored completely (see the affected …

Result: 1 error found, 0 warnings. The repository was not changed.
Quick mode does not read chunk contents: damage inside the encrypted data is found only with --read-data.
```

`check --json` has the fields `repository_id`, `format_version`, `mode`, `ok`, `errors`,
`warnings`, `suppressed`, `counts`, `stats`, `affected_snapshots`, `findings`, in that order,
described in [`CHECK.md`](CHECK.md#json-output). Like the other `--json` outputs it is one line, and
there is no time in it, so the same repository gives the same bytes.

## prune

```sh
# Look first: shows which snapshots go and what space is freed, changes nothing
vaultsnap --repo ~/backups prune --keep-last 7 --keep-monthly 12 --dry-run

# Apply it: shows the plan, then waits for you to type "yes"
vaultsnap --repo ~/backups prune --keep-last 7 --keep-monthly 12

# In a script (standard input is not a terminal): say so explicitly
vaultsnap --repo ~/backups --password-file /etc/vaultsnap.pass prune --keep-daily 7 --keep-weekly 4 --yes
```

Removes the snapshots a retention policy does not keep, and then the chunk objects that no kept
snapshot needs. **This is the only command that deletes data, and it cannot be undone.** How the
policy is applied, why a kept snapshot cannot lose a chunk, and what happens when a prune is
interrupted are in [`PRUNE.md`](PRUNE.md).

| Option | Meaning |
|--------|---------|
| `--keep-last N` | Keep the N newest snapshots. |
| `--keep-daily N` | Keep the newest snapshot of each of the N most recent days that have a snapshot. |
| `--keep-weekly N` | The same for ISO weeks (Monday to Sunday). |
| `--keep-monthly N` | The same for months. |
| `--keep-yearly N` | The same for years. |
| `--dry-run` | Print the plan and change nothing. Wins over `--yes`. |
| `--yes` | Delete without asking. Required when standard input is not a terminal. |

At least one `--keep-*` option is required, every count must be at least 1, and a snapshot is kept
if **any** rule selects it. Days, months, and years are UTC, and times come from the authenticated
manifests. The newest snapshot is always kept. The policy and the missing confirmation are checked
before the passphrase is asked for.

Without `--dry-run` and `--yes`, `prune` prints the plan to standard output, asks on standard error
and reads one line from standard input. Only the word `yes` proceeds; anything else, an empty line,
or the end of the input stops with *not confirmed; nothing was changed*. What you confirm is what is
executed: if a snapshot was added or removed while you were deciding, the prune fails with *the
repository changed since the plan was made* and deletes nothing. When standard input is not a
terminal and `--yes` is not given, the command refuses and changes nothing.

Output of a dry run (the numbers are illustrative, the layout is what the command prints and
`crates/vaultsnap-cli/src/prune/tests.rs` asserts):

```
Policy       keep last 2
Snapshots    3 (2 kept, 1 to remove)

  REMOVE  3f2a91c4  2025-06-01T09:00:02Z
  KEEP    8c0d17e5  2025-06-02T09:00:01Z  last
  KEEP    b41e66a9  2025-06-03T09:00:03Z  last

Chunks       40 in use by the kept snapshots
Unreferenced 3 objects (3.5 KiB) that no kept snapshot needs

Dry run: nothing was changed.
```

The question, on standard error:

```
This permanently deletes 1 snapshot and 3 unreferenced chunk objects (3.5 KiB) from the repository.
This cannot be undone. Type "yes" to continue:
```

After the plan, an executed prune prints:

```
Removed      1 snapshot and 3 chunk objects (3.5 KiB)
Verified     all 40 chunks used by the kept snapshots are present
Took         0.1s
```

`Verified` is the check made at the end: every chunk a kept snapshot needs still exists. If some
objects could not be deleted, a `Not removed` line appears and each object is listed on standard
error with the reason (at most 100 are listed; the count is exact):

```
2 could not be deleted:
  data/9c/9c1e…7a: failed to remove …: Permission denied (os error 13)
```

A terminal also shows a progress line (`removing chunk objects: 12 of 40`) that is erased when the
prune ends; it is never printed when standard error is not a terminal. The `yes` question cannot be
answered through a pipe, by design: scripts use `--yes`.

### Exit status

| Status | Meaning |
|--------|---------|
| 0 | A dry run, or a prune that deleted everything it planned (possibly nothing). |
| 1 | Refused or failed. Nearly always nothing was changed: an invalid policy, no way to ask and no `--yes`, *not confirmed*, a snapshot that cannot be read, a kept snapshot that is already missing chunks, a plan that changed, a repository that is locked, a wrong passphrase. The one exception is an error while removing a snapshot file, which stops the run there; every remaining snapshot is still complete and running again finishes the job. |
| 2 | Usage error (a non-numeric count, an unknown option). |
| 3 | The prune finished and every kept snapshot is intact, but some unreferenced chunk objects could not be deleted. They are harmless leftovers; the next prune tries again. |
| 130 | Cancelled with Ctrl-C or SIGTERM. At the question nothing was changed. During a run, every snapshot that still exists is complete; run it again to finish. |

A prune that deletes holds the **exclusive** lock for its whole run and fails with *locked* while
a backup, restore, check, or another prune is running. A dry run holds only the shared lock. A
process killed while holding a lock leaves a stale lock file that blocks pruning until it is
removed by hand; see [`PRUNE.md`](PRUNE.md#locking).

---

## Sizes

Four numbers describe how much a snapshot holds. They are computed from the verified manifests
and from the length of each chunk's object file; no file data is read or decrypted.

| Name | Definition |
|------|------------|
| **logical** | The total size of the regular files, which is what a restore writes. A hard link is not a second copy. A chunk that repeats inside the snapshot still counts every time. |
| **deduplicated** | The plaintext size of the snapshot's *distinct* chunks: the logical size after removing data that repeats inside the snapshot. |
| **stored** | The size in the repository of those distinct chunks (compressed and encrypted, with their headers). Chunks shared with other snapshots are counted in full. |
| **unique stored** | The part of *stored* that **no other snapshot** refers to: what removing this snapshot alone would free. Only snapshots that could be read are taken into account, so with any damaged snapshot it may be too large (the output says so). |

Chunks whose object is missing from the repository count as zero stored bytes and are reported
as *missing*; that always means the repository is damaged. Stored sizes come from file metadata
and are not verified against the content; verifying content is the job of `check`.

`snapshots` and `show` measure each snapshot against all the others. `diff` measures the two
snapshots against each other only.

---

## JSON output

`snapshots`, `show`, `ls`, and `diff` accept `--json`. Each prints **exactly one JSON document on
one line**, followed by a newline. Pipe it through `jq` to read it. Conventions:

* Field names, their order, and the spellings of fixed values never change; new fields can be
  added **at the end** of an object. Consumers should ignore fields they do not know.
* Times are UTC, RFC 3339, with nine fractional digits: `2025-06-15T15:06:40.000000005Z`.
  They sort as text.
* Sizes are byte counts (JSON numbers).
* Paths are JSON strings holding the exact text. A path that is **not valid UTF-8** is written
  with U+FFFD replacement characters, and a sibling field named like the path with `_hex` appended
  (`path_hex`, `target_hex`, `link_to_hex`) holds its exact bytes in hex. That field is absent
  when the path is exact. Newlines and control characters are ordinary JSON string escapes.
* Unknown values are `null`; fields are not omitted (except the `…_hex` fields).
* Warnings never appear in the JSON; they go to standard error.

### snapshots --json

```json
{"snapshots":[{"id":"<64 hex>","short_id":"aaaaaaaa","created":"2023-11-14T22:13:20.000000000Z","host":"workstation","tags":["laptop"],"sources":[{"path":"/data"}],"complete":true,"unreadable_paths":0,"skipped_paths":0,"files":3,"directories":1,"symlinks":0,"hard_links":0,"special_files":0,"sizes":{"logical_bytes":8,"chunks":2,"deduplicated_bytes":8,"stored_bytes":160,"unique_chunks":1,"unique_stored_bytes":80,"missing_chunks":0}}],"damaged":[{"id":"<64 hex>","error":"snapshot … is corrupt: …"}],"referenced_chunks":7,"referenced_stored_bytes":1100000,"unique_sizes_exact":false}
```

| Field | Meaning |
|-------|---------|
| `snapshots` | The snapshots that verified, oldest first. |
| `snapshots[].complete` | `false` if some paths could not be read. `unreadable_paths` counts those; `skipped_paths` counts everything left out. |
| `snapshots[].sources[]` | `{path, path_hex?}` for each backed-up source. |
| `snapshots[].files … special_files` | Entry counts of the whole snapshot. |
| `snapshots[].sizes` | `logical_bytes`, `chunks` (distinct), `deduplicated_bytes`, `stored_bytes`, `unique_chunks`, `unique_stored_bytes`, `missing_chunks`: see [Sizes](#sizes). |
| `damaged[]` | Snapshots that failed verification: `{id, error}`. The command also exits with status 3. |
| `referenced_chunks`, `referenced_stored_bytes` | Distinct chunks the listed snapshots refer to, and their stored size. |
| `unique_sizes_exact` | `false` if any snapshot is damaged (unique sizes may be too large). |

### show --json

Fields, in order: `id`, `short_id`, `created`, `host`, `tags`, `program`, `manifest_version`,
`complete`, `unreadable_paths`, `sources[]` (`path`, `path_hex?`, `files`, `directories`,
`symlinks`, `hard_links`, `special_files`, `bytes`, `skipped`), `totals` (`files`, `directories`,
`symlinks`, `hard_links`, `special_files`, `logical_bytes`), `sizes` (as above),
`unique_sizes_exact`, `config`, `skipped[]`.

`config` has `chunk_min_bytes`, `chunk_avg_bytes`, `chunk_max_bytes`; `symlinks`
(`preserve`, `follow_inside_root`, `follow_all`); `hard_links` (`preserve`, `duplicate`);
`on_error` (`skip`, `fail`); `special_files` (`record`, `skip`); `changed_files` (`fail`,
`accept`); `cross_filesystems` (boolean); `includes` and `excludes` (arrays of patterns).

`skipped[]` has `path`, `path_hex?`, `source` (index into `sources`), `reason` (`vanished`,
`unreadable`, `other_filesystem`, `special_file`), and `detail` (a message for `unreadable`, the
kind of file for `special_file`, otherwise `null`).

### ls --json

```json
{"snapshot":"<64 hex>","entries":[{"path":"/data","type":"directory","size":null,"mode":"0755","uid":1000,"gid":1000,"modified":"2023-11-14T22:13:20.123456789Z","target":null,"link_to":null,"chunks":null},{"path":"/data/link","type":"symlink","size":null,"mode":"0777","uid":1000,"gid":1000,"modified":"2023-11-14T22:13:20.123456789Z","target":"a b.txt","link_to":null,"chunks":null}]}
```

`entries[]` are in the same order as the text output. Fields, in order: `path`, `path_hex?`,
`type` (`directory`, `file`, `symlink`, `hardlink`, `fifo`, `socket`, `chardev`, `blockdev`,
`other`), `size` (bytes of content for `file` and `hardlink`, else `null`), `mode` (octal string
such as `"0644"`, or `null`), `uid`, `gid`, `modified`, `target` and `target_hex?` (symbolic link
target), `link_to` and `link_to_hex?` (for a hard link, the path of the file carrying the data),
`chunks` (number of chunks of a `file`, else `null`).

### diff --json

```json
{"from":{"id":"<64 hex>","short_id":"aaaaaaaa","created":"2023-11-14T22:13:20.000000000Z"},"to":{"id":"<64 hex>","short_id":"bbbbbbbb","created":"2023-11-15T22:13:20.000000000Z"},"summary":{"added":2,"deleted":1,"modified":2,"unchanged":10,"added_bytes":9,"deleted_bytes":0,"logical_bytes_from":1048576,"logical_bytes_to":1572864,"chunks_only_in_from":1,"chunks_only_in_to":2,"stored_bytes_only_in_from":2048,"stored_bytes_only_in_to":4096,"missing_chunks":0,"chunker_differs":false},"changes":[{"status":"modified","path":"/data/edit.txt","type":"file","previous_type":"file","size":29,"previous_size":13,"differences":["content","mtime"]}]}
```

`summary` holds the counts, `added_bytes` and `deleted_bytes` (regular files), the logical size of
each snapshot, the distinct chunks and stored bytes found in only one of the two snapshots,
`missing_chunks`, and `chunker_differs`. `changes[]` is sorted by path. Fields of a change, in
order: `status` (`added`, `deleted`, `modified`), `path`, `path_hex?`, `type` (the type in the
newer snapshot, or in the older one for `deleted`), `previous_type` (for `modified` only), `size`
and `previous_size` (content sizes as in `ls`), and `differences`: for `modified`, a list of
`content`, `type`, `link_target`, `mode`, `owner`, `mtime`, otherwise empty.

### Recipes

These are examples of how the output can be used; they were not run in the environment this
documentation was written in.

```sh
# The id of the newest snapshot
vaultsnap --repo ~/backups snapshots --json | jq -r '.snapshots[-1].id'

# Compare the two newest snapshots and list only the deleted paths
ids=$(vaultsnap --repo ~/backups snapshots --json | jq -r '.snapshots[-2:][].id')
vaultsnap --repo ~/backups diff $ids --json | jq -r '.changes[] | select(.status=="deleted") | .path'

# Which snapshots would free the most space if removed?
vaultsnap --repo ~/backups snapshots --json |
  jq -r '.snapshots | sort_by(-.sizes.unique_stored_bytes)[] | "\(.sizes.unique_stored_bytes)\t\(.short_id)"'
```

---

## Notes and limits

* The examples marked as test fixture output are the exact text and JSON that the unit tests in
  `crates/vaultsnap-cli/src/inspect/tests.rs` assert. At the time of writing those tests had not
  been run (see the README's known gaps).
* A listing reads and decrypts every snapshot, and `show` too (to find what is unique to the
  snapshot). The cost is proportional to the total size of the manifests plus one `stat` per
  distinct chunk. Memory is bounded by the largest manifest plus one small record per distinct
  chunk.
* Times are always UTC. Sizes use binary units (KiB = 1024 bytes) in text and plain bytes in JSON.
* There are no extended attributes, ACLs, or file flags in snapshots yet, so they cannot be shown
  or compared.
