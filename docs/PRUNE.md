# Retention and garbage collection (`prune`)

`vaultsnap prune` applies a retention policy: it removes the snapshots the policy does not keep,
and then the chunk objects that no kept snapshot needs. It is **the only operation that deletes
repository data**, and deletion cannot be undone, so it is built around one rule: *a snapshot that
is kept must stay restorable, whatever happens, including a crash at any instant.*

Usage and examples are in [`CLI.md`](CLI.md#prune); the repository layout is in
[`FORMAT.md`](FORMAT.md); what to do about integrity problems is in [`CHECK.md`](CHECK.md).

## What is kept

A policy is a set of rules, and a snapshot is kept if **any** rule selects it. At least one rule is
required and every count must be at least 1, so a policy can never keep nothing. All snapshots of a
repository are one history; there are no groups by host, path, or tag.

| Rule | Keeps |
|------|-------|
| `--keep-last N` | The N newest snapshots. |
| `--keep-daily N` | The newest snapshot of each of the N most recent **days that have a snapshot**. |
| `--keep-weekly N` | The same for weeks. |
| `--keep-monthly N` | The same for months. |
| `--keep-yearly N` | The same for years. |

* Days, months, and years are those of the **UTC** calendar and weeks are ISO weeks (Monday to
  Sunday), so the result does not depend on the time zone of the machine that prunes.
* "The last N periods *that have a snapshot*" rather than "the last N calendar periods before
  today" means the result does not depend on the clock either. Pruning twice, or on another day,
  selects the same snapshots, and a long pause in backups does not make the history vanish.
* Times are the creation times recorded **inside the authenticated manifests**, never file times.
  Snapshots with the same time are ordered by id, so the choice is deterministic.
* The **newest snapshot is always kept**, by every rule.
* A recorded time is the clock of the machine that made the backup. A backup made with the clock
  set far in the past counts as old, however recently it was made. The plan lists every time, so
  read it before confirming.

The rules are written in the order they are listed here in the plan (`keep last 3, daily 7`), and a
kept snapshot lists which rules kept it.

## What the prune does

1. **Read.** Every snapshot is read through authenticated decryption. **If any cannot be read, the
   prune refuses** (*N snapshot(s) cannot be read*), because the chunks in use are then unknown, and
   deleting on a guess could destroy data that only the unreadable snapshot needs. Nothing is
   changed. Run [`check`](CHECK.md), put an intact copy of the file back, or, if the snapshot is lost
   for good, move its file out of `snapshots/`, and prune again.
2. **Decide.** The policy is applied to the snapshots, oldest first. A decision that would remove
   the newest snapshot is treated as a bug and not acted on.
3. **Mark.** The chunks that the *kept* snapshots refer to are collected.
4. **Check.** Every marked chunk must exist. If a snapshot that would be kept is already missing
   chunks, **the prune refuses** (*a snapshot that would be kept needs N chunk(s) that do not
   exist*): removing the other snapshots could destroy the last intact copy of that data. A missing
   chunk of a snapshot that is being removed anyway does not matter.
5. **Sweep, in this order:**
   1. delete the snapshot files that are not kept, oldest first;
   2. make that durable (the `snapshots/` directory is synced);
   3. delete the chunk objects that no kept snapshot needs.
6. **Verify.** Every chunk a kept snapshot needs is checked to still exist.

The order in step 5 is the whole point. A snapshot file that exists always has all of its chunks:
chunks are only deleted *after* the snapshots that needed them are gone and that is on disk. So a
crash, a power cut, a cancellation, or a failed deletion at any point leaves a repository in which
every snapshot is complete. The objects that are deleted are exactly the ones **not** marked, and
the marking is done from the kept snapshots before anything is deleted.

An object is "a chunk object" only if its name says so: a regular file named by 64 lowercase
hexadecimal digits in the `data/xx/` directory its first two digits name. Everything else in the
repository (stray files, temporary files, key files, `config`, locks) is never listed and never
deleted. [`check`](CHECK.md) reports strays.

## Asking before deleting

The library has three modes:

| Mode | Effect |
|------|--------|
| `DryRun` (the default) | Works out the plan and reports it. Deletes nothing. |
| `Confirmed(digest)` | Deletes, but only if the plan made under the exclusive lock is the plan whose digest was confirmed. Otherwise it fails with *the repository changed since the plan was made* and deletes nothing. |
| `Unattended` | Deletes what the policy selects now, without anyone having looked. The caller takes the responsibility. |

The digest names which snapshots the plan keeps and which it removes. It does not cover the chunks:
those are whatever the kept snapshots do not need at the moment of the prune.

The command maps onto these modes:

* `--dry-run` runs `DryRun` and prints the plan. It wins over `--yes`.
* `--yes` runs `Unattended`: delete without asking. A script has to say so on purpose.
* Neither: the command prints the plan and asks for the word `yes` (not `y`, not `YES`, not an empty
  line, not the end of the input), then runs `Confirmed` with the digest of the plan it showed.
  **No lock is held while the question is open**, so a backup can finish in the meantime, in which
  case the confirmation is refused and the new plan has to be looked at and confirmed again.
* Neither, and standard input is not a terminal: nobody can be asked, so the command refuses and
  changes nothing. Use `--dry-run` to look and `--yes` to proceed.

The policy and the missing permission are checked before the passphrase is asked for or the
repository is touched.

## Locking

| Run | Lock | Effect |
|-----|------|--------|
| Dry run | shared | Runs next to a backup, a restore, a check, or another dry run. Next to a running backup the numbers for unreferenced objects include what the backup has stored but not yet committed. |
| Anything that deletes | **exclusive**, for the whole run | Fails with *locked* while any backup, restore, check, or other prune holds the repository, and makes them fail while it runs. |

Locks are never waited for and never removed automatically. A process killed while it held a lock
leaves a **stale lock file** in `locks/` that blocks every prune that deletes until someone removes
it by hand; the error names the file. Make sure nothing is running first (see
[`CHECK.md`](CHECK.md)).

## Interruptions and failures

| What happens | State afterwards | What to do |
|--------------|------------------|------------|
| Refused (policy, unreadable snapshot, kept snapshot missing chunks, wrong passphrase, locked, plan changed) | Unchanged. | Fix the cause. |
| Ctrl-C or SIGTERM at the question | Unchanged (exit 130). | Run again. |
| Cancelled while snapshot files are being removed | Some of the doomed snapshots are gone; every remaining snapshot is complete; no chunk has been touched. | Run it again. |
| Cancelled or killed while chunk objects are being removed | The doomed snapshots are gone; some unreferenced objects remain. `check` reports them as orphans (warnings). Every snapshot is complete. | Run it again; it removes them. |
| Power loss | The snapshot removals are made durable before any chunk is deleted, so a removed snapshot cannot come back with its chunks gone. A deleted chunk object may reappear; it is an orphan again. | Run it again. |
| An object cannot be deleted (permission, I/O error) | Left in place, listed (at most 100, the count is exact), and the prune goes on with the rest. Exit status 3. Every kept snapshot is intact. | Fix the cause (permissions, disk), run it again. |
| Killed while holding the exclusive lock | A stale lock file. | Remove it by hand once sure nothing runs. |

Cancelling takes effect before the next deletion, never in the middle of one.

## What the prune does not do

* **It does not read chunk contents.** It works from the manifests and the names and sizes of the
  object files. A chunk that is damaged but present is kept if a kept snapshot needs it, and
  removed if none does. Use [`check --read-data`](CHECK.md) to find damage.
* **It does not repair anything**, and it refuses to run on top of damage it cannot reason about
  (an unreadable snapshot, a kept snapshot with missing chunks).
* **It does not rewrite or compact objects.** Every chunk is its own file, so deleting a chunk frees
  its space at once and nothing needs rewriting.
* **It removes only what a policy selects.** There is no command that forgets one named snapshot.
* **It does not detect a rollback.** Someone who can write to the repository can restore an older
  copy of the whole directory, or delete files, and VaultSnap cannot tell (see
  [`SECURITY.md`](SECURITY.md)).
* **It does not remove strays or temporary files**, only chunk objects and snapshot files.

## Verification status

The prune and its tests pass on macOS with Rust 1.94.1 ([`RELIABILITY.md`](RELIABILITY.md)); the
crash tests also kill a prune at every storage call it makes and show that the repository stays
consistent. The behavior above is what the code and tests state:

* Unit tests for the retention rules (calendar boundaries, ISO weeks, leap years, ties, the newest
  snapshot) in `crates/vaultsnap-core/src/retention.rs`, and for the pipeline (shared chunks,
  refusals, digest checks, cancellation after every possible number of deletions, a deletion that
  fails) in `crates/vaultsnap-core/src/prune/engine.rs`.
* Integration tests on repositories made by real backups, in
  `crates/vaultsnap-core/tests/prune.rs`: shared chunks stay, exactly the unreferenced objects go,
  every kept snapshot restores byte for byte and passes a full check, dry runs and refusals change
  nothing, interrupted prunes are finished by a second run, and a prune racing a backup or another
  prune never damages a snapshot.
* CLI tests against the binary in `crates/vaultsnap-cli/tests/prune.rs`, and renderer and
  confirmation tests in `crates/vaultsnap-cli/src/prune/tests.rs`.

Not covered: the interactive question on a real terminal (it needs one), Ctrl-C and SIGTERM
delivered to the binary, behavior on network or case-insensitive file systems, and Windows.
