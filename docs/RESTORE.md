# Restoring snapshots

`vaultsnap restore` and `Repository::restore` rebuild the files of a snapshot, or selected paths of
it, under a directory chosen by the caller. This document is the specification of what a restore
does and does not guarantee. The command line is in [`CLI.md`](CLI.md#restore); the threat model
is in [`SECURITY.md`](SECURITY.md#restore-safety).

Everything described here is implemented and its tests pass on macOS with Rust 1.94.1;
[`RELIABILITY.md`](RELIABILITY.md) says exactly what has been run and what has not (Linux, Windows,
and the Ctrl-C and SIGTERM handlers of the binary have not).

## What a restore does

```
snapshot id ──► load (authenticated decryption, validation) ──► plan ──► check ──► create ──► finish
```

1. **Load.** The snapshot is found by id (or the start of one), read through authenticated
   decryption, and validated. A damaged, modified, renamed, foreign, or wrong-key snapshot ends the
   restore here, before the target is touched.
2. **Plan.** The entries to restore are selected (everything, or the `--include` paths with all that
   is below them, plus the directories above them), and the destination of every entry is
   computed. Every path component is checked, again, as safe. An unsafe path ends the restore
   before anything exists on disk. An `--include` that matches nothing is an error too.
3. **Check.** The target is resolved and must not be inside the repository. With the default
   conflict mode, everything that would clash is found now, so a restore that is going to fail
   because of a conflict fails before it has written anything.
4. **Create.** Entries are created in stored order, which puts every directory before its contents:
   directories, then for each file its chunks are fetched, authenticated, and written to a
   temporary file that is moved into place; symbolic links; hard links.
5. **Finish.** Directories receive their recorded permissions and times, deepest first, because
   writing into a directory changes its modification time and a read-only directory cannot be
   written to. This also happens when the restore is cancelled.

## Where files go

The files of a source go to the **absolute path they had when they were backed up, below the
target**. A source `/home/me/Documents` restored with `--target /mnt/restore` appears as
`/mnt/restore/home/me/Documents/…`. This keeps several sources apart whatever their names, and it
is the same path `vaultsnap ls` prints, so `--include` takes the paths you see there.

* The target directory is created, with missing parents, if it does not exist.
* Symbolic links in the part of the target path that exists are resolved once, when the restore
  starts, because the caller chose that path. Nothing below the resulting root is ever followed.
* A target inside the repository is refused (also through links), as is a target that is a file or
  lies below one, and a missing part of the path that contains `.` or `..`.
* The directories above a source (`home`, `me`, …) are created with the usual `0755`; they are not
  part of the snapshot and receive no recorded attributes.

## Path safety

Snapshot content is untrusted even after authentication: it was written by a backup run on a
machine that may have been compromised. The rules, checked in the plan and again in the file
operations:

* **Every name must be one safe path component**: not empty, not `.` or `..`, no `/`, no NUL, at
  most 4096 bytes. The same rule applies to every component of a root's source path. (On platforms
  where `\` or `:` separate path parts, those are refused too, and names that are not valid text.)
* **Parents come first.** An entry's parent must be an earlier directory of the same root. Together
  with the rule above, an entry's destination is always strictly below the restore root.
* **Nothing is followed.** Before a directory is used it is looked at without following symbolic
  links; a link anywhere along a destination path stops the entry (conflict, skip, or replace the
  link itself, depending on the mode). Files are created with `O_CREAT|O_EXCL|O_NOFOLLOW`; a name
  that exists, even as a dangling link, is never opened. A name is only ever replaced with a
  rename, which replaces a link itself and never what it points to.
* **Directories are finished through the open directory**, without following a link, and the
  attributes of files are applied through the open file, before it is renamed into place.
* **No special files.** FIFOs, sockets, and device nodes in a snapshot are never created. They are
  listed as skipped.
* **No privileges.** Set-user-id and set-group-id bits are never restored, nor the sticky bit on a
  file (it is kept on directories). Owners are not restored: that needs privileges, and doing it
  by default would be surprising.

The manifest itself already refuses unsafe names when it is written and read; the restore does not
rely on that. The plan is the single place that decides where files may go, and it is tested with
hand-built manifests that no correct backup could produce.

## Files

Each regular file is reconstructed from its ordered list of chunks:

1. A private temporary file (`.vaultsnap-tmp-<random>`, mode `0600`, created exclusively) is made
   in the destination directory, so the final rename never crosses a file system.
2. For each chunk, in order: the chunk is read with `ChunkStore::get`, which authenticates it
   (XChaCha20-Poly1305 over header, chunk id, and ciphertext), decrypts and decompresses it within
   the authenticated length, and checks that its content hashes back to its id. Only then is it
   written. Its length must also equal the length the snapshot records.
3. After the last chunk, the total must equal the file's recorded size.
4. Permissions (without privilege bits) and times are applied to the open file.
5. The file is moved into place: renamed over the old name in overwrite mode, otherwise published
   with a hard link that fails if the name exists (and, where hard links are not available, a
   check followed by a rename).

If anything goes wrong — a damaged or missing chunk, a full disk, a cancellation — the temporary
file is removed when the code unwinds, and the destination was never touched. **A file is either
complete and exact, or absent (or, in overwrite mode, still the old one).** Nothing is flushed to
disk before the rename (`fsync`), so after a power failure a restored file can be empty or short;
restores are repeatable, and the data in the repository is the authority.

Hard links are restored as hard links to the file restored for their target in the same run. If
the target was not selected, was skipped, or the file system cannot link, the content is written
as a file of its own. Content is never lost; only the sharing may be.

## Conflicts

What happens when something already exists where an entry would go (`--on-conflict`):

| Mode | Behavior |
|------|----------|
| `fail` (default) | Everything is checked first. If anything would be replaced, or two entries of the snapshot would be put in the same place, **nothing is restored** and the number of conflicts and the first one are reported. |
| `skip` | What exists is left untouched; everything else is restored. Skipped entries are reported. A file where a directory should be takes everything below that directory along. |
| `overwrite` | Files and symbolic links in the way are replaced atomically (a link is replaced, not followed). A file or link where a directory should be is removed and the directory made. **A directory is never removed or replaced**: a file that meets a directory is reported as failed. |

Directories are different: a directory in the snapshot that meets a directory in the target is not
a conflict; their contents are merged, in every mode. The merge never deletes: files in the target
that are not in the snapshot stay.

Directory attributes: a directory the restore creates gets the recorded permissions and times. A
directory that already existed is left alone in `fail` and `skip` mode (its permissions and time
are the target's, not the snapshot's) and is updated in `overwrite` mode. The restore root itself
(the target directory) is never touched.

Leftover temporary files of a killed restore are not conflicts, are never trusted, and are never
deleted by a later restore; they can be removed by hand (`.vaultsnap-tmp-*`).

## Symbolic links

A symbolic link is data: its target is arbitrary text. The restore never follows links, so
creating one is safe in itself, but a link that points out of the restored tree can lead other
programs (or you) somewhere unintended: `/etc/passwd`, or `../../../.ssh`. `--symlinks` decides:

| Policy | Behavior |
|--------|----------|
| `contained` (default) | A link is restored only if it can never lead out of the restore root: the target is relative and non-empty; it goes up (`..`) only at the start, and at most as many levels as the link is below the restore root; after the first name it only goes down. Anything else is skipped and reported. |
| `skip` | No symbolic links are restored. |
| `as-recorded` | Every link is restored exactly as recorded, absolute and escaping ones included. Use it only for snapshots you trust, or when you need the links. |

Why `contained` is safe: a link is created in a real directory below the root (a restore never
creates a directory through a link). Resolving its target first climbs through real parents, at most
up to the root, and then only descends. Because a `..` never comes after a name, a chain of links
cannot add up to a climb either: each link resolves from a real directory, never from the middle of
another link's target.

The limit of that argument is links that were **already in the target** before the restore: a
contained link that descends through a link you planted (`target/data/evil -> /`) can lead out.
Restore into a directory you control.

`contained` also keeps links that point to things that do not exist (dangling), and links that
point at directories; they are only restored, never used.

Link targets are written byte for byte as recorded, whether or not they are valid text. The
permissions and times of a link itself are not restored.

## Cancellation and interruption

`CancelToken::cancel` (Ctrl-C and SIGTERM in the command line) stops the restore at the next
chunk or entry. What is there is complete and exact; the file being written is removed; the
directories made so far get their recorded attributes. The report of a cancelled restore is not
returned (the call fails with *cancelled*), but the files are valid.

**Resuming** is running the same restore again:

* with `--on-conflict skip`, files that are already there are left alone, so the run fills in
  what is missing. The times of directories that were written into by the resumed run are not the
  recorded ones;
* with `--on-conflict overwrite`, everything is rewritten and every directory attribute is
  set again, ending in exactly the state of an uninterrupted restore;
* with the default `fail`, the run refuses, because the finished files are in the way.

A restore that is **killed** (SIGKILL, power loss) cannot clean up. It can leave a
`.vaultsnap-tmp-*` file, and directories it made keep their initial permissions, `0700` (owner
only), because they were not finished. Run the restore again with `--on-conflict overwrite`
to complete and normalize.

## Locking

A restore holds a **shared** repository lock, so it can run next to a `backup`, and a `prune` that
deletes (which takes the exclusive lock, see [`PRUNE.md`](PRUNE.md)) cannot delete chunks while a
restore reads them. It never writes to
the repository. A lock left behind by a killed process must be removed by hand; the error names
the file.

## The report

`RestoreReport` (and the summary the command prints) counts files, directories, links, and bytes,
and lists three kinds of problems separately:

* `skipped`: left out on purpose (already there in `skip` mode, refused by the link policy, a
  special file). Not a failure.
* `failed`: should have been restored but was not: a missing or damaged chunk, a refused write, a
  directory in the way. The command exits with status 3 and the restore of everything else went on.
* `warnings`: permissions or times that could not be applied to something that was restored (for
  example a time the file system cannot represent).

Errors that end the whole restore instead (status 1): the snapshot is missing or damaged, an unsafe
path, an unusable target, an unknown `--include`, a conflict in `fail` mode, a cancellation (130).

## Limits and known gaps

* **Races.** The checks of a destination happen just before the operation, not atomically with it.
  The code uses no `unsafe` and therefore no `openat`. A local attacker who can change the target
  tree *while the restore runs* can swap a directory for a link between the check and the use.
  Restore into a directory only you can write to. Everything that could be done without `unsafe`
  is done: exclusive creation, no-follow opens, rename over names, attributes through open handles.
* **Durability.** No `fsync` of files or directories; see above.
* **Not restored**: owners (`uid`/`gid`), the creation time, the permissions and times of symbolic
  links, extended attributes, ACLs, file flags, sparse-file holes (holes are written as zeros),
  special files. None of these are in snapshots yet, except owners, which are recorded and shown by
  `ls`.
* **Platforms.** Written for Unix. On other platforms symbolic links are not restored (reported as
  failed), permissions are not applied, and names that are not valid text or contain `\` or `:`
  are refused. Never built or run there.
* **Time ranges.** A recorded time that the file system cannot represent is reported as a warning
  and left unset.
* **Case-insensitive file systems.** Two names that differ only in case are two entries of the
  snapshot but one file on such a system. The check before writing compares names exactly, so it
  cannot see this clash: in `fail` mode it is noticed when the second entry is reached, after the
  first was written, and the restore stops there with a conflict (so "nothing is restored" does not
  hold on such file systems); in `skip` mode the second is skipped; in `overwrite` mode it replaces
  the first.
* **Memory** is one chunk at a time plus the manifest; a restore is sequential. There is no
  parallelism and no resumption inside a file.
* **Not verified.** Nothing in this document was exercised by a running compiler or test; the tests
  exist in `crates/vaultsnap-core/src/restore/`, `src/ops/restore.rs`,
  `crates/vaultsnap-core/tests/restore.rs`, and `crates/vaultsnap-cli/tests/restore.rs`.
