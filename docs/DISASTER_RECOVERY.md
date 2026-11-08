# Disaster recovery

What to keep, how to get everything back after losing the machine, and what to do for each way a
repository can go wrong. The detailed meaning of each `check` finding is in [`CHECK.md`](CHECK.md);
how restores behave is in [`RESTORE.md`](RESTORE.md).

> **Status.** The recovery paths marked *(tested)* are exercised by automated tests that pass
> (`crates/vaultsnap-core/tests/crash.rs`, `check.rs`, `restore.rs`); the stale-lock and lost-`config`
> procedures were also run by hand against the real binary. Nothing was run on Linux, and a power
> cut is not simulated ([`RELIABILITY.md`](RELIABILITY.md)). The rest relies on the documented format
> and on commands working as described. **Rehearse recovery before you need it.**

## What you must keep

Keep this list somewhere that survives losing the machine, the repository, and your house:

| Item | Why | Secret? |
|------|-----|---------|
| **The passphrase** | The only way to unlock the repository | **Yes** |
| **The repository id** (printed by `init`; the `repository_id=` line of `config`) | With the passphrase it is enough to rebuild a lost `config` file | No |
| **Where the repository is** (path, bucket, endpoint, region) | You cannot restore from a repository you cannot find | No |
| **Credentials for it**, if remote (AWS access key, profile, role) | Reading a bucket | **Yes** |
| **The VaultSnap version that wrote it** (`vaultsnap version`), ideally the release package itself | Only a release that understands the format can read it | No |

The data is only as safe as the passphrase: it protects the master key that encrypts everything.

## Recovering after losing the machine

You need the repository (or a copy of it), the passphrase, and VaultSnap. Not the old machine, not
any configuration file of it.

```sh
# 1. Install a VaultSnap that reads the repository (docs/INSTALL.md) and check it
vaultsnap version

# 2. Point at the repository and the passphrase
export VAULTSNAP_REPO=/mnt/backup/vaultsnap          # or s3://bucket/folder, with credentials
export VAULTSNAP_PASSWORD_FILE=/safe/passphrase      # or leave it out to type it

# 3. See what is there
vaultsnap snapshots

# 4. Restore into an empty directory first, then move things where they belong
vaultsnap restore LATEST_ID --target /mnt/recovered
```

A restore lays files out under their original absolute path, so `/home/me/Documents/a.txt` comes
back as `/mnt/recovered/home/me/Documents/a.txt`. Restoring straight into `/` is possible but is the
least cautious choice; restore to a scratch directory, look, then copy. Permissions and times
come back; owners do not (`chown -R` as needed). If the newest snapshot is incomplete or damaged,
use an earlier one: every snapshot restores on its own.

If `check` or `restore` reports problems, read on.

## No passphrase, no data

There is no recovery of a lost passphrase: the repository holds a random master key encrypted
(Argon2id + XChaCha20-Poly1305) under a key derived from the passphrase, and the passphrase is
stored nowhere. This is the point of encrypted backups. There is no backdoor, escrow, or reset, and
adding a second passphrase (so two people could unlock it) is not implemented yet
([`LIMITATIONS.md`](LIMITATIONS.md)). Keep the passphrase safe in two places.

A wrong passphrase and a modified key file give the same message (*incorrect passphrase, or the key
file was modified*). If you are sure of the passphrase, suspect the key file (`keys/`) and the
`config` file, and restore them from a copy of the repository.

## A lost `config` file *(tested)*

The `config` file is four short lines, not secret, and the one object the passphrase cannot make up
for: key files are bound to the repository id, which is stored in the clear only there. Without it
the repository cannot be unlocked (`check` and `restore` stop at the start).

**If you recorded the repository id** you can write the file by hand. Create `config` in the
repository (in a bucket, upload it as the object `config`) with exactly these four lines, putting
your id on the `repository_id=` line; the creation time may be any number:

```
vaultsnap-repository
format_version=1
repository_id=5f0c1e2a9b7d4c3e8a6b0d1f2c3e4a5b
created_at_unix=0
```

Then `vaultsnap snapshots` works again. A different id does not unlock anything (the test checks
this), so you cannot damage anything by trying. **If you did not record the id**, look for any copy of
the repository's `config` (another copy of the repository, a backup of the backup drive); the id is
128 random bits and cannot be guessed. This is the strongest reason to keep a copy of `config` and
the id with the passphrase.

## A stale lock *(tested)*

Every command that touches the repository holds a lock file in `locks/` and removes it when it
finishes. A command that is killed (power loss, `kill -9`, a crashed machine) cannot, and VaultSnap
never removes a lock for you, because nobody can tell a dead process from a slow one.

What it looks like: `error: repository is locked by locks/<name>.lock; if no other VaultSnap process
is running, remove that lock file`. Backups, checks, restores, and listings take *shared* locks, so
the lock a killed one leaves behind does not stop the others: later backups run, and `check` reports
it as a `lock_present` warning. It blocks only `prune`, which needs the repository to itself. A
killed `prune` leaves an *exclusive* lock, and then every command, `check` included, refuses until it
is removed.

What to do:

1. Make sure no VaultSnap process is running against the repository, on any machine that uses it
   (`ps`, the schedulers' status).
2. Look at the file if you want to know whose it was: it holds `kind=shared` or `kind=exclusive`,
   the process id, and the time it was taken (`created_at_unix=`; `date -r SECONDS` on macOS, `date -d
   @SECONDS` on Linux).
3. Delete it: `rm /mnt/backup/vaultsnap/locks/<name>.lock`, or for a bucket
   `aws s3 rm s3://bucket/folder/locks/<name>.lock`. Only files in `locks/` that end in `.lock`.

The crash tests kill a backup and a prune at every storage call and show that, apart from this
lock, the repository is intact, `check` finds no errors, every visible snapshot restores exactly,
and the next run works.

## An interrupted backup, prune, or restore *(tested)*

- **Backup.** No snapshot appears; chunks it uploaded stay and are reused. Run it again. Orphaned
  chunks left behind are harmless and `prune` removes them.
- **Prune.** Snapshot files are removed first and chunks only afterwards, so an interruption can
  leave unreferenced data but never a snapshot that is missing data. Remove the stale lock if
  there is one, then run `prune` again; it finishes the job.
- **Restore.** Each file is complete or absent. Leftover `.vaultsnap-tmp-*` files and directories
  that are still private (`0700`) are normalized by running the restore again with
  `--on-conflict overwrite`.

## The repository is damaged

```sh
vaultsnap check --read-data           # exit 3 if there are errors
vaultsnap check --read-data --json > check.json
```

The report says which findings are errors and which snapshots can no longer be restored completely
(*affected snapshots*). Everything else is fine. The usual cases ([`CHECK.md`](CHECK.md) has
every finding):

| Finding | Meaning | What to do |
|---------|---------|------------|
| `chunk_missing`, `chunk_corrupt` | An object is gone or damaged; snapshots using that data cannot restore those files | Put an intact copy back. Otherwise **move the damaged object out** (do not leave it) and run `backup` of the unchanged source again: it re-creates the object, and the old snapshots become whole |
| `snapshot_damaged` | A snapshot file failed authentication | Put a copy of that file back (snapshot files never change); other snapshots are unaffected |
| `unsupported_version` | Written by a newer VaultSnap | Use a newer VaultSnap; do not modify anything |
| `io_error` | The OS refused a read | Fix the disk, mount, or permissions, then check again |
| `orphan_object`, `unexpected_file`, `leftover_temp_file`, `lock_present` | Harmless leftovers (a lock may need clearing, see above) | Nothing, or `prune` for the orphans |

If more than a few objects are damaged, or damage keeps appearing, suspect the **storage**: test the
disk (SMART, `fsck`), try another cable or machine, and do not entrust new backups to it. Restore
what you can first: `restore` reports exactly which paths failed.

**Do not** edit objects, create dummy files where an object belongs, or rename files over each
other. A dummy object would be found by the next backup and reused as if intact, so the chunk would
never be stored again.

## Moving or copying a repository

A repository is a set of files (or objects), identical for a directory and a bucket. Copy it with
any tool that copies bytes: `rsync -a`, `cp -R`, `aws s3 sync`. Rules:

- Copy while nothing is writing to it, and copy **everything**, `config` and `keys/` included.
- Afterwards run `vaultsnap check --read-data` on the copy. A copy that passes is as good as the
  original.
- The copy has the same repository id and the same passphrase. Two copies that are then written to
  separately diverge; do not merge them.
- Between a directory and a bucket the objects are the same (this is tested: a repository moved
  directory → memory → directory is byte for byte identical).

**A second copy is your defence against the storage failing, against a bad `prune`, and against
somebody with write access deleting or rolling back the repository.** VaultSnap detects damage and
tampering with the contents of what it reads, but it cannot know that a whole snapshot, or the whole
repository as of last month, has been silently removed ([`SECURITY.md`](SECURITY.md)). Keep at least
one copy that the machine being backed up cannot write to (a bucket with versioning or object lock, a
disk that is only attached for the copy).

## A bucket

Everything above applies; in addition:

- Losing the credentials does not lose the data: the bucket's owner can issue new ones. VaultSnap
  needs only read access to list, check, and restore, and write access to back up and prune.
- A bucket with **versioning** lets you return to an earlier state of the repository after a mistake
  or an attack; VaultSnap neither needs nor prevents it.
- If the service ignores conditional writes VaultSnap refuses to use it
  ([`S3.md`](S3.md)); a repository stored in one earlier must not be used for concurrent writers.

## A ransomware or malicious-deletion drill

Assume whoever controls the machine controls its credentials to the repository. They can delete the
repository, or write garbage into it. They cannot read it without the passphrase. Protect against
the first with a copy they cannot write to (above). After an attack:

1. Do not run `prune` or `backup` against the possibly damaged repository.
2. `check --read-data` it, and restore from the copy or from the snapshots that verify.
3. Rotate the passphrase and credentials: create a new repository with `init` and back up to it.
   (Changing the passphrase of an existing repository is not implemented.)

## Rehearsal

Four exercises worth doing once, on a scratch repository:

1. **Restore on a clean machine:** install from the package, set the repository and passphrase,
   restore a snapshot, compare it with the original (`diff -r`).
2. **Lose the `config`:** delete it from a copy, rewrite it from the id, confirm `snapshots` works.
3. **Kill a backup:** `kill -9` a running backup, confirm no new snapshot, `check` is clean except for a
   warning, and the next backup works; run `prune` and clear the lock as described above.
4. **Damage an object:** flip a byte in a copy, watch `check --read-data` find it, repair it from the
   original, and check again.
