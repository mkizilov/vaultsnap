# Verifying a repository: `vaultsnap check`

`vaultsnap check` examines a repository and **reports** what is wrong with it. This document says
what it examines, what it can and cannot find, how to read its report, and **what to do about each
kind of failure**.

> The tests that deliberately damage repositories and check the reports are in
> `crates/vaultsnap-core/tests/check.rs` and `crates/vaultsnap-cli/tests/check.rs`; they pass on
> macOS with Rust 1.94.1. [`RELIABILITY.md`](RELIABILITY.md) says exactly what has been run and what
> has not (Linux, Windows, and non-local storage have not).

## The promise: a check never changes anything

`check` **never repairs, moves, rewrites, or deletes** anything. It opens everything read-only. The
only file it writes is its own shared lock file in `locks/`, which it removes when it ends. Not a
byte of the repository, and not the modification time of any file in it, is different afterwards,
however damaged the repository is (the tests prove this for damaged repositories in both modes).

Repairs are decisions that belong to you, because the right one depends on whether you have a
second copy. The report tells you what is wrong, what is affected, and what the options are.

## Running it

```sh
vaultsnap --repo REPO check                    # quick
vaultsnap --repo REPO check --read-data        # full
vaultsnap --repo REPO check --read-data --json # machine-readable
```

| Option | Meaning |
|--------|---------|
| `--read-data` | Full check: read, authenticate, and decrypt every chunk object. |
| `--json` | Print one JSON document instead of text; see [JSON](#json-output). |
| `--jobs N` | Threads for reading chunk objects (default: the number of CPUs, at most 4). |
| `--max-findings N` | List at most N findings, errors first; counts stay exact (default 1000; 0 lists all). |

It asks for the passphrase like `backup` does (`--password-file` for automation), because
authenticating snapshots and chunks needs the keys.

### Exit status

| Status | Meaning |
|--------|---------|
| 0 | No **errors** were found. Warnings may have been (orphans, leftovers); they do not change the status. |
| 3 | The check ran and found **errors**: damage or loss. The report says which. |
| 1 | The check could not run: see [Failures that stop the check](#failures-that-stop-the-check). |
| 2 | Usage error. |
| 130 | Cancelled with Ctrl-C or SIGTERM. There is no report, and nothing was changed. |

A script that must stay quiet about warnings can test for 0; one that wants to know about every
irregularity should read the JSON.

## Quick and full

| | Quick (default) | Full (`--read-data`) |
|---|---|---|
| Configuration, directories, key files, stray files, locks | ✔ | ✔ |
| Every snapshot **authenticated**, decrypted, and validated | ✔ | ✔ |
| Every chunk a snapshot needs **exists** | ✔ | ✔ |
| Every object's header and size are possible, and the chunk length it declares equals the length the snapshots record | ✔ | ✔ |
| Orphaned objects found | ✔ | ✔ |
| Missing, truncated, or extended **uncompressed** objects found | ✔ | ✔ |
| Damaged header fields (magic, flags, kind, compression, declared length) found | ✔ | ✔ |
| Object stored under another chunk's name found | ✔ only if the chunk lengths differ | ✔ always |
| **Flipped or changed bits inside an object found** | ✘ | ✔ |
| **Truncated compressed object found** | ✘ (a shorter compressed object is still a possible size) | ✔ |
| Content of every chunk hashes to its id | ✘ | ✔ |
| Cost | one directory walk, all snapshots read, 12 bytes per object | the whole repository read and decrypted |

So: **a clean quick check does not prove the data is intact.** It proves the structure is, that
nothing is missing, and that the snapshots are authentic. Bit rot inside the encrypted payload of
a chunk, which is the most likely kind of silent damage on aging storage, is found only by the full
check. Both modes say which one they were, and a quick check ends with a reminder.

A sensible routine: a quick check after every backup run (it takes seconds to minutes), a full
check periodically (monthly, or after any hardware event, move, or copy of the repository), and a
full check before relying on a repository to recover data.

## What is examined, in order

1. **Metadata.** `config` is read and must be valid; otherwise there is no repository to check
   (see below). Then the directories `data`, `snapshots`, `locks`, `keys` must exist and be
   directories. Every entry is classified by its name: a snapshot is a 64-digit hexadecimal file in
   `snapshots/`, a chunk object a 64-digit file in the `data/xx/` directory named by its first two
   digits, a key file or lock file a 32-digit one. Key files are parsed (their format and cost
   parameters; opening them would need each key's own passphrase). Anything else is reported as
   unexpected, or as a leftover of an interrupted write when it is named like the store's
   temporary files (`.tmp-…`).
2. **Snapshots.** Each snapshot file is read through authenticated decryption against the key and
   its own id, then its manifest is validated. A snapshot that fails either is reported as damaged
   and is not used for anything else. The snapshots are read one at a time; memory stays small.
3. **References.** From the readable snapshots, the set of chunks they need. A needed chunk with no
   object is **missing**; an object nobody needs is an **orphan**.
4. **Objects.** Every object in `data/` (needed or not) is examined as the mode says, with a few
   threads. The result does not depend on the number of threads.
5. **Affected snapshots.** If anything needed is missing or damaged, the readable snapshots are read
   once more to count, for each, the files that need a damaged or missing chunk.

### Next to other operations

A check holds a **shared** lock: it runs next to a backup, a restore, or another check, and refuses
to run while something holds an exclusive lock (a `prune` that deletes). Snapshot files are
listed before objects are, so a backup that commits while the check runs can show up as orphans,
never as missing objects. A backup in progress also shows up as a `lock_present` warning and as
`.tmp-…` files; those are expected.

## Reading the report

```
Repository 0123456789abcdef0123456789abcdef (format version 1)
Mode         quick: snapshots authenticated, chunk objects examined by structure only

Snapshots    2 readable, 1 damaged
Chunks       4 referenced by snapshots, 1 missing
Objects      5 in data/ (12.1 KiB), 1 damaged
Unreferenced 2 objects (2.0 KiB); may include objects of snapshots that could not be read
Key files    1
Took         1.5s

ERRORS (3)
  snapshot_damaged  snapshots/<id>
      snapshot <id> is corrupt: authentication failed …
  chunk_missing  data/ab/<id>
      no object exists for a chunk that snapshots refer to
  chunk_corrupt  data/cd/<id>
      length does not match the header (truncated or extended)

WARNINGS (3)
  orphan_object  data/ef/<id>
      no readable snapshot refers to this object
  …
  and 1 more not listed (--max-findings sets the limit, 0 lists all)

AFFECTED SNAPSHOTS (cannot be restored completely)
  aaaaaaaa  3 files affected (2 damaged or missing chunks)

WHAT TO DO (the check changed nothing; see docs/CHECK.md)
  …

Result: 3 errors found, 3 warnings. The repository was not changed.
Quick mode does not read chunk contents: damage inside the encrypted data is found only with --read-data.
```

(The numbers are illustrative; the layout is exact and is asserted by `crates/vaultsnap-cli/src/check/tests.rs`.)

* **Findings** are listed errors first, each group in the order of the kinds below, then in the
  order of the repository's own names. A finding has a *kind*, a *subject* (a path inside the
  repository: `snapshots/<id>`, `data/ab/<id>`, `keys/<name>`, …, never a name from your backed-up
  files), and a *detail*. Subjects can be arbitrary for a stray file, which anyone with write
  access to the repository named; the text output escapes control characters.
* **Affected snapshots** is the answer to "what can I not restore?". It counts regular file entries;
  hard links to such a file are not counted again. To see *which* files, restore that snapshot into
  a scratch directory: `restore` lists every path it could not restore, with the reason.
* **Unreferenced** objects are orphans. The count is *inexact* (and says so) whenever a snapshot
  could not be read, because the chunks only that snapshot needs look unused.
* The **limit** on listed findings never hides an error behind a warning, and never changes a
  count.

### JSON output

`check --json` prints exactly one JSON document on one line. Field names, their order, and the
spellings of `mode`, `severity`, and `kind` never change; new fields are only added at the end of an
object. There is no time in the document, so the same repository state gives the same bytes.

```json
{"repository_id":"…","format_version":1,"mode":"quick","ok":false,"errors":1,"warnings":0,"suppressed":0,
 "counts":{"missing_directory":0,"snapshot_damaged":0,"unsupported_version":0,"chunk_missing":1,"chunk_corrupt":0,
           "chunk_length_mismatch":0,"io_error":0,"orphan_object":0,"unexpected_file":0,"leftover_temp_file":0,
           "lock_present":0,"key_file_invalid":0},
 "stats":{"key_files":1,"snapshots":2,"snapshots_damaged":0,"objects":9,"object_bytes":50321,"chunks_referenced":10,
          "chunks_missing":1,"objects_checked":9,"objects_damaged":0,"bytes_authenticated":0,"orphans":0,
          "orphan_bytes":0,"orphans_exact":true},
 "affected_snapshots":[{"id":"<64 hex>","short_id":"3f2a91c4","files":1,"damaged_chunks":1}],
 "findings":[{"severity":"error","kind":"chunk_missing","subject":"data/ab/<id>",
              "detail":"no object exists for a chunk that snapshots refer to"}]}
```

| Field | Meaning |
|-------|---------|
| `ok` | `true` if there are no errors (warnings allowed). The exit status is 0 exactly then. |
| `errors`, `warnings` | How many findings of each severity occurred, listed or not. |
| `suppressed` | How many occurred but are not in `findings` because of `--max-findings`. |
| `counts` | Exact number of findings of each kind. Every kind is present, zero or not. |
| `stats.snapshots`, `stats.snapshots_damaged` | Snapshots that could and could not be read. |
| `stats.objects`, `stats.object_bytes` | Chunk objects found in `data/` and their total size. |
| `stats.chunks_referenced`, `stats.chunks_missing` | Distinct chunks readable snapshots need; how many have no object. |
| `stats.objects_checked`, `stats.objects_damaged` | Objects examined (all of them) and how many failed. |
| `stats.bytes_authenticated` | Bytes of objects read completely and authenticated; always 0 in quick mode. |
| `stats.orphans`, `stats.orphan_bytes`, `stats.orphans_exact` | Unreferenced objects, their size, and whether the count is exact. |
| `affected_snapshots` | Snapshots that need a damaged or missing chunk: `files` need at least one of `damaged_chunks` chunks. |
| `findings[]` | `severity` (`error`, `warning`), `kind`, `subject`, `detail`. |

## Responding to findings

Two rules apply to all of them:

1. **Do not run anything that writes to the repository until you understand the report**,
   including `backup`, unless the section below tells you to: a backup into a damaged repository
   can build on damaged objects (it reuses an object that exists without reading it).
2. **Keep every damaged file.** Copy it somewhere before you move or replace it. A damaged object
   is the only evidence of what happened, and sometimes the only trace of the data.

The one thing that cures most data loss is **a second copy of the repository**. If you have one,
most of the answers below are "put the file back from the copy" (then run `check --read-data`
again). If you have none, the answers say what is still possible.

### Errors

#### `missing_directory`: a required directory is missing or is not a directory

*Meaning.* `data`, `snapshots`, `locks`, or `keys` is gone, or something else is where it should
be. Missing `data` means every chunk is missing; missing `snapshots` means the history is gone
(everything in `data` then looks like orphans).

*Cause.* Deleted by hand, a failed disk or sync tool, a restore of only part of a backup of the
repository.

*Not on an object store.* A repository kept on a storage backend without directories (see
[`STORAGE.md`](STORAGE.md)) has no empty directories: one that holds no objects is simply not
there, so this finding is never reported for such a backend, and an empty `snapshots` or `data`
is seen through the counts (no snapshots, no objects) instead.

*Do.* Put the directory back, with its contents, from a copy of the repository. If you have none,
look in the trash, in the file system's snapshots or the cloud provider's history. Creating an empty
directory only hides the loss. A missing `locks` directory alone is harmless to recreate (empty).
If `keys` is missing, nothing can be unlocked and the check cannot run at all (see below).

*Don't.* Don't run `backup` into a repository whose `data` or `snapshots` is missing before you have
decided whether to recover it: a new backup would start a fresh history next to the lost one.

#### `snapshot_damaged`: a snapshot cannot be authenticated, decrypted, or validated

*Meaning.* The snapshot file under `snapshots/` is not what was written: bits changed, it was cut
short or extended, replaced by another file, copied from another repository, or its contents are
inconsistent. A snapshot is all or nothing; this one cannot be trusted or restored. **Other
snapshots are not affected**, and the chunks it used are still there (they look like orphans in
this report).

*Cause.* Storage corruption, an interrupted copy, a sync tool conflict, manual editing, a file from
another repository. A *wrong key* looks the same; if every snapshot is reported damaged, suspect
the key or passphrase first.

*Do.* Put an intact copy of that file back (`snapshots/<id>` is immutable, so any copy of it from any
time is as good as the original). Without a copy that snapshot is lost; its data is not, if later
snapshots contain it. If the data is still on the source, run `backup` to make a new snapshot.

*Don't.* Don't edit the file or rename another file over it; both make it damaged or wrong.

#### `unsupported_version`: written by a newer VaultSnap

*Meaning.* A snapshot, a chunk object, or a key file has a format version this build does not
understand. Nothing is damaged.

*Do.* Use a VaultSnap version that is at least as new as the one that wrote the repository. Do not
modify the file. (If a `config` has an unsupported version, `check` stops at the beginning with the
same advice.)

#### `chunk_missing`: a snapshot needs a chunk that has no object

*Meaning.* Every snapshot that needs this chunk **cannot restore the files** that use it (the
report's *affected snapshots* count them). Other files are fine.

*Cause.* The object file was deleted or lost, or lives in the wrong place (a misplaced object also
appears as `unexpected_file`), or its whole `data/xx/` directory was lost.

*Do, in this order.*
1. Look for the object in any copy of the repository and put it back at `data/<first two
   digits>/<id>`. Run `check --read-data` again.
2. If the files that need it still exist **unchanged** on the source, run `backup` of that source
   again. A backup stores a chunk whenever no object exists for it, and a chunk's id is a keyed
   hash of its content, so the re-created object is exactly the one the old snapshots need: they
   become whole again, not only the new snapshot. (A file that changed since has different
   chunks; the old version's chunk is only recoverable from a copy.)
3. Otherwise the affected files in the affected snapshots are lost; restore everything else
   (`restore` reports exactly which paths failed).

*Don't.* Don't create a dummy or empty file in its place: the check would report it as corrupt, and
a later backup would find an object where the chunk belongs and reuse it as if it were intact, so
the chunk would never be stored again.

#### `chunk_corrupt`: a chunk object is damaged

*Meaning.* The object exists but is not what was written: bad structure, wrong size
(*truncated or extended*), changed bits (*authentication failed*), or an authentic object stored
under another chunk's name. A restore refuses it, so the files that need it fail, as for a missing
chunk. The detail says what was seen. A damaged object that no snapshot needs (an orphan) is
reported too, because **the next backup would reuse it as it is**: it only checks that an object
exists, not that it is intact.

*Cause.* Bit rot or a failing disk, an interrupted copy, a file system bug, manual editing.

*Do, in this order.*
1. If you have a copy of the repository, copy the intact object back over the damaged one (after
   saving the damaged one). Run `check --read-data` again.
2. Otherwise **move the damaged object out of the repository** (keep it), and handle the chunk as
   *missing*: if the original data is unchanged, run `backup` again. Because the object is no longer
   there, the backup stores it afresh; if you leave the damaged object in place, the backup
   reuses it and nothing is repaired.
3. If the original data is gone, the affected files are lost from the affected snapshots.

Also treat the storage as suspect: if more than a few objects are damaged, or damage keeps
appearing, check the disk (SMART, `fsck`, a different cable or machine) before trusting new backups
to it.

*Don't.* Don't "fix" an object by editing it; any change makes authentication fail.

#### `chunk_length_mismatch`: snapshots and an object disagree about a chunk's size

*Meaning.* The object is structurally valid (in a full check, it also authenticates), but the chunk
it holds has a different length from the one the snapshots record for that id. That should be
impossible; it points to damage that kept the file well formed, a snapshot with wrong content, or a
bug.

*Do.* Treat the object as `chunk_corrupt`. If a full check says `chunk_corrupt` for the same
object, that is the better description. Report it as a bug if it persists on a repository whose
storage is healthy.

#### `io_error`: the operating system refused a read

*Meaning.* A file or directory could not be read (permissions, hardware error, a network drive that
went away). The check cannot say whether the data is intact.

*Do.* Fix the cause (ownership and permissions of the repository, the mount, the disk), then run
the check again. This kind says nothing about the repository itself.

### Warnings

#### `orphan_object`: no snapshot refers to this object

*Meaning.* Harmless data that costs space. Normal after an interrupted or cancelled backup (the
chunks were stored, the snapshot never was) and after a snapshot file was removed.

*Do.* Nothing, or run [`prune`](PRUNE.md) to reclaim the space: it removes every chunk object
that no kept snapshot refers to, orphans included (even with a policy that keeps every snapshot,
e.g. `--keep-last 1000000`). The check never does. If you expected the
object to belong to a snapshot, and `snapshots_damaged` is above zero, it probably does: the
report says so (`orphans_exact: false`). Fix the damaged snapshot first.

*Note.* A **very large** number of orphans after a damaged snapshot or a deleted snapshot is the
sign to look for the missing snapshot file.

#### `unexpected_file`: an entry VaultSnap did not write

*Meaning.* A file or directory that does not belong: `.DS_Store`, a note, a backup tool's file, a
chunk in the wrong directory. VaultSnap ignores it. A chunk-named file in the wrong `data/xx/`
directory is never found by VaultSnap, so the chunk it holds counts as missing.

*Do.* Nothing is required. Remove it by hand if you know it is not yours. A misplaced chunk: move
it to `data/<first two digits>/` (it is named by its id).

#### `leftover_temp_file`: a temporary file from an interrupted write

*Meaning.* A write that was killed halfway (`.tmp-…`). It is never read and never counted as data.

*Do.* Remove it by hand when no VaultSnap process is running (a running backup creates and removes
such files all the time).

#### `lock_present`: a lock file exists

*Meaning.* Some process holds the repository, or a process that was killed left its lock behind
(*stale*). The detail shows the kind, the process id, and the creation time.

*Do.* If a VaultSnap process is running, it is expected. If none is, the lock is stale: make sure
nothing is running (and that the process id in the detail is gone), then remove the file by hand. A
stale **shared** lock does nothing but this warning; a stale **exclusive** lock stops every
operation, the check included.

#### `key_file_invalid`: a key file cannot be parsed

*Meaning.* A file in `keys/` is damaged. Another key file opened the repository (otherwise the
check could not have started), so nothing is lost now.

*Do.* Keep a copy of the key file that works (**without it the repository is unreadable, whatever
else you have**). The damaged one can be removed by hand. Stale key files left by an interrupted
`init` are harmless leftovers of the same kind.

## Failures that stop the check

These end the command with status 1 and no report, because there is nothing to report about.
Nothing is changed.

| Message | Meaning | Do |
|---------|---------|----|
| `… is not a VaultSnap repository` | The path is not a directory, or has no `config`. | Check the path (`--repo`/`VAULTSNAP_REPO`). If `config` was deleted: it is a small text file holding the repository id (printed by `init`); restore it from a copy. **Without it nothing can be opened**, so keep a copy of it (and the id) separately. |
| `invalid repository config: …` | `config` is damaged or hand-edited. | Restore it from a copy. Do not guess its contents: the repository id in it is bound into every key file and snapshot. |
| `repository format version N is not supported` | Written by a newer VaultSnap. | Use a newer version. |
| `incorrect passphrase, or the key file was modified` | The passphrase is wrong, or the only usable key file is damaged; the two cannot be told apart. | Check the passphrase (and the password file for stray characters). If sure, restore `keys/` from a copy. |
| `repository is damaged: the repository has no key files` / a failure listing `keys` | `keys/` is missing or empty. | Restore `keys/` from a copy. Without a key file the data cannot be decrypted by anyone. |
| `repository is locked by locks/…` | An exclusive lock exists: a running prune, or a stale lock. | Wait, or make sure nothing runs and remove the stale file by hand. |
| `cancelled: the check was stopped …` | You interrupted it. | Run it again. |

## What a check cannot find

* **A snapshot that was deleted**, or the whole repository **replaced by an older, internally
  consistent copy**. A file that is gone leaves nothing to compare with; the objects of a deleted
  snapshot just look like orphans. Only an external record can detect this (the list of snapshots
  from an earlier `snapshots --json`, kept elsewhere). See `SECURITY.md`.
* **Damage inside a chunk's payload, in quick mode.** Use `--read-data`.
* **Key files that still parse but would not open** with some other passphrase: each key file is
  opened only with the passphrase you gave.
* **Mistakes made before backup:** the check proves the repository holds what was backed up, not
  that it was the right thing to back up.
* **Sizes are metadata.** Object and repository sizes come from the file system; they are not
  authenticated.
* **Memory** is proportional to the number of distinct chunks (one small record each), not to the
  size of the data.

## What a check costs

A quick check reads every snapshot (decrypting them) and 12 bytes of every object, plus one
directory walk. A full check additionally reads, decrypts, decompresses, and hashes every object:
expect it to take as long as reading the repository from disk once, with a few CPU cores busy. It
shows a progress line on a terminal. It can be interrupted at any time without consequence.
