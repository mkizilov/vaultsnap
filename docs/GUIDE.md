# User guide

How to set VaultSnap up, run it on a schedule, get files back, and keep the repository healthy. The
exact options are in [`CLI.md`](CLI.md); what to do when something is wrong is in
[`DISASTER_RECOVERY.md`](DISASTER_RECOVERY.md); what it cannot do is in
[`LIMITATIONS.md`](LIMITATIONS.md).

> **Status.** The flows below (init, backup, restore drill, the `jq` recipes, retention, the scheduled
> wrapper, stale locks) were run against the real binary on macOS; the scheduler *units* (systemd,
> launchd, cron) were not installed or run, and nothing was run on Linux
> ([`RELIABILITY.md`](RELIABILITY.md)). Do [the restore drill](#5-restoring) on data you can afford to
> lose before you rely on any of it.

Contents: [1. Concepts](#1-concepts) · [2. Initializing](#2-initializing-a-repository) ·
[3. Backing up](#3-backing-up) · [4. Schedules](#4-backup-schedules) · [5. Restoring](#5-restoring) ·
[6. Retention](#6-retention-and-pruning) · [7. Checking](#7-checking-the-repository) ·
[8. Remote repositories](#8-remote-repositories) · [9. Cheat sheet](#9-cheat-sheet)

## 1. Concepts

- A **repository** is where backups live: a directory (on another disk, a NAS, a USB drive) or an
  S3-compatible bucket. Everything in it is encrypted except a small `config` file and lock files.
  It is a plain set of files; you can copy it with any tool.
- A **snapshot** is one backup run: the state of the directories you gave it at that moment. Every
  snapshot restores completely on its own, however many later ones exist. Only changed data is
  stored again; identical data is stored once across all snapshots (deduplication).
- The **passphrase** unlocks the repository. There is no recovery if it is lost
  ([why](DISASTER_RECOVERY.md#no-passphrase-no-data)).
- `vaultsnap` is told the repository with `--repo PATH` (or `VAULTSNAP_REPO`) and the passphrase
  with `--password-file FILE` (or `VAULTSNAP_PASSWORD_FILE`), or asks for it on the terminal.
  Neither a passphrase nor a credential is ever an argument.

```sh
export VAULTSNAP_REPO=/mnt/backup/vaultsnap          # set once; the examples below rely on it
vaultsnap init                                        # once
vaultsnap backup ~/Documents ~/Pictures               # as often as you like
vaultsnap snapshots                                   # what exists
vaultsnap restore 3f9a --target ~/restored            # get files back
```

## 2. Initializing a repository

**Where.** On a different physical device from the data (a second disk, a NAS, a bucket), ideally in
a different place. A repository on the same disk protects against mistakes, not against the disk
failing. It must not be inside a directory you back up (`backup` refuses that).

**The passphrase.** Use a long random one; it protects every byte. Put it in a file only you can
read, so that scheduled runs need no terminal:

```sh
mkdir -p ~/.config/vaultsnap && chmod 700 ~/.config/vaultsnap
head -c 32 /dev/urandom | base64 > ~/.config/vaultsnap/passphrase    # 44 characters
chmod 600 ~/.config/vaultsnap/passphrase
```

One trailing newline in the file is ignored; the file may be at most 4096 bytes; VaultSnap warns if
other users can read it. **Also store the passphrase somewhere that survives losing this machine**
(a password manager, a printed copy in a safe). A backup you cannot unlock is not a backup.

```sh
vaultsnap --password-file ~/.config/vaultsnap/passphrase init
# created VaultSnap repository 5f0c1e2a9b7d4c3e8a6b0d1f2c3e4a5b at /mnt/backup/vaultsnap
```

`init` without `--password-file` asks for the passphrase twice on the terminal (nothing is shown as
you type). **Write down the repository id it prints** next to the passphrase: it is not secret, and
together with the passphrase it is enough to bring a repository back even if its `config` file is
lost ([how](DISASTER_RECOVERY.md#a-lost-config-file-tested)). `init` never overwrites anything: it refuses a
directory that already holds a repository or other data.

Key derivation is deliberately slow (Argon2id, 64 MiB of memory, 3 passes): every command that
unlocks the repository pays it once.


## 3. Backing up

```sh
vaultsnap backup ~/Documents ~/Pictures
vaultsnap backup /home --exclude '*.tmp' --exclude 'cache/' --exclude '.Trash/' --tag laptop
```

- Give it directories or files. Each is recorded under its absolute path, so a snapshot of
  `~/Documents` and `~/Pictures` restores to those paths.
- `--exclude PATTERN` skips what matches (repeatable); a trailing `/` matches directories only; an
  excluded directory is not entered. Pattern rules are in [`SCANNING.md`](SCANNING.md). `--tag`
  labels the snapshot, `--host` records a host name.
- The first backup reads everything. Later ones skip files whose size, modification and
  inode-change times, inode, and device are unchanged *and* more than two seconds older than the
  previous backup, so a daily run touches only what changed. `--rescan` reads everything anyway,
  which is a good idea after a restore from a disk image or if file times may have been reset.
- It is safe to use the machine meanwhile. A file that changes while it is being read is read
  again, up to three times, and recorded as unreadable if it will not hold still.
- **Nothing is visible until the backup is complete.** If it is interrupted (Ctrl-C, a crash, power
  loss, a network failure) no snapshot appears; what was uploaded is reused next time.
- Symbolic links are stored as links, never followed; special files (sockets, FIFOs, devices) are
  recorded and not opened. Owners, extended attributes, and ACLs are not stored
  ([`LIMITATIONS.md`](LIMITATIONS.md)).

**Exit status** (`backup`): 0 complete; 1 failed, nothing was created; 3 the snapshot exists but
some files could not be read and are not in it (they are listed); 130 cancelled.
`vaultsnap show SNAPSHOT` lists what was skipped and why.

## 4. Backup schedules

A schedule is a scheduler running one command that needs no terminal. `contrib/` has everything
for the three usual ones; they all run the same script, `contrib/vaultsnap-scheduled.sh`, which does
what a sensible nightly job does:

1. `backup` the configured directories,
2. `prune` with your retention rules (after a successful or incomplete backup, never after a failed one),
3. `check` the repository (quick on most days, full on a weekday you choose),

and exits 0 when all went well, 3 when the snapshot exists but a person should look (unreadable
files, or the check found errors), and otherwise with the status of whatever failed. Its settings
are environment variables, all described at the top of the script and in
`contrib/vaultsnap.env.example`; the passphrase is only ever named as a file.

Set up once (adapt the paths):

```sh
install -m 0755 contrib/vaultsnap-scheduled.sh /usr/local/bin/vaultsnap-scheduled
```

**systemd (Linux, system-wide)**

```sh
install -d -m 0700 /etc/vaultsnap
install -m 0600 contrib/vaultsnap.env.example /etc/vaultsnap/vaultsnap.env      # edit it
install -m 0600 ~/.config/vaultsnap/passphrase /etc/vaultsnap/passphrase
install -m 0644 contrib/systemd/vaultsnap-backup.service contrib/systemd/vaultsnap-backup.timer \
    /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now vaultsnap-backup.timer
systemctl list-timers vaultsnap-backup.timer      # when it runs next
journalctl -u vaultsnap-backup                    # what it did
```

The timer runs at 02:30 with a random delay up to 15 minutes, and `Persistent=true` makes a run that
was missed (the machine was off) happen when it is next on. A run that ends with status 1 or 3 leaves
the service in the *failed* state, which monitoring of failed units picks up.

**cron (Linux, macOS)**

```sh
install -d -m 0700 ~/.config/vaultsnap && mkdir -p ~/.local/state
install -m 0600 contrib/vaultsnap.env.example ~/.config/vaultsnap/env            # edit it
crontab -e        # paste the line from contrib/cron/vaultsnap.crontab
```

cron runs only while the machine is on, and does not catch up; on a laptop prefer systemd or
launchd.

**launchd (macOS)**

```sh
cp contrib/launchd/org.vaultsnap.backup.plist ~/Library/LaunchAgents/     # edit the paths in it
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/org.vaultsnap.backup.plist
launchctl print gui/$(id -u)/org.vaultsnap.backup | head
```

A run missed while the Mac slept happens when it wakes. Full Disk Access may be needed for
`/usr/local/bin/vaultsnap-scheduled` and `vaultsnap` to read protected folders.

**How often.** Nightly is a good default; as often as hourly is fine, because an unchanged file
costs one `stat`. Several machines or schedules can share a repository: backups, checks, restores,
and listings take *shared* locks and do not block each other. `prune` needs the repository to itself
and refuses (changing nothing) while any other command is running; the next scheduled run tries
again.

**Knowing it works.** A backup you do not look at is a hope. Three habits:

- Alert on a non-zero exit status of the scheduled job (systemd failed state, cron output to a
  log you read or a monitoring check on the log's age).
- Alert when the newest snapshot is older than you expect:

  ```sh
  vaultsnap snapshots --json | jq -r '.snapshots[-1].created'      # UTC, e.g. 2026-11-08T02:31:07.123456789Z
  ```
- Do the restore drill below now and then.

**A job that was killed** (the machine lost power during a backup) leaves a *lock file* behind. The
one a killed backup leaves does not stop later backups, but it stops `prune` until it is removed, and
the nightly check reports it as a warning. A killed `prune` leaves an exclusive lock that makes every
command refuse until it is removed ([how](DISASTER_RECOVERY.md#a-stale-lock-tested)).

## 5. Restoring

```sh
vaultsnap snapshots                                       # find the snapshot; the id is its first characters
vaultsnap ls 3f9a ~/Documents/taxes                       # look inside (optional)
vaultsnap restore 3f9a --target ~/restored                # everything in snapshot 3f9a...
vaultsnap restore 3f9a --target ~/restored --include ~/Documents/taxes     # ...or one path
```

- A snapshot is named by the start of its id (at least 4 characters, unique).
- Files are restored below `--target` under their original absolute path:
  `/home/me/Documents/a.txt` comes back as `~/restored/home/me/Documents/a.txt`. The target is created
  if missing and must not be inside the repository.
- Restoring **never overwrites** unless you say so (`--on-conflict fail` is the default and decides
  before writing anything; `skip` leaves existing files; `overwrite` replaces them atomically).
- Symbolic links are restored only if they cannot lead out of the restored tree
  (`--symlinks contained`, the default); `skip` restores none, `as-recorded` restores all.
- Permissions and modification times are restored; owners are not. Every chunk is authenticated
  before it is used, so damage is reported, never silently restored.
- Each file appears complete or not at all (written to a temporary name and renamed). If a restore is
  interrupted, run it again with `--on-conflict skip` or `overwrite`.
- Exit status 3 means some paths could not be restored; they are listed on standard error.

**Restoring on another machine** needs only the repository, the passphrase, and a VaultSnap that
understands its format (`vaultsnap version`). Nothing else is stored anywhere.

**The restore drill** (do it after the first backup and every few months):

```sh
vaultsnap backup ~/Documents
id=$(vaultsnap snapshots --json | jq -r '.snapshots[-1].id')       # or copy it from `snapshots`
vaultsnap restore "$id" --target /tmp/drill
diff -r ~/Documents "/tmp/drill$HOME/Documents" && echo "identical"   # the layout is the absolute path
rm -rf /tmp/drill
```

If you cannot do this, you do not have a backup yet.

## 6. Retention and pruning

Snapshots accumulate; `prune` decides which to keep and deletes the rest *and the data nothing needs
any more*. It is the only command that deletes anything.

A snapshot is **kept if any rule selects it**:

| Rule | Keeps |
|------|-------|
| `--keep-last N` | the N newest snapshots |
| `--keep-daily N` | the newest snapshot of each of the N most recent **days that have a snapshot** |
| `--keep-weekly N` | the same for ISO weeks (Monday to Sunday) |
| `--keep-monthly N` | the same for calendar months |
| `--keep-yearly N` | the same for years |

Days, months, and years are UTC, and times are the creation times recorded inside the snapshots, not
the clock of the machine that prunes: running prune twice, or on another day, keeps the same snapshots,
and a month without backups does not make the history vanish. The newest snapshot is always kept.
At least one rule is required; zero is refused.

| Goal | Rules |
|------|-------|
| A laptop backed up nightly | `--keep-daily 7 --keep-weekly 4 --keep-monthly 12 --keep-yearly 3` |
| A server backed up hourly | `--keep-last 24 --keep-daily 14 --keep-weekly 8 --keep-monthly 12` |
| Never grow past the latest few | `--keep-last 3` |

```sh
vaultsnap prune --keep-daily 7 --keep-weekly 4 --keep-monthly 12 --dry-run    # look first
vaultsnap prune --keep-daily 7 --keep-weekly 4 --keep-monthly 12              # shows the plan, asks you to type "yes"
vaultsnap prune --keep-daily 7 --keep-weekly 4 --keep-monthly 12 --yes        # unattended
```

`--dry-run` changes nothing and wins over `--yes`. Without a terminal `prune` refuses unless `--yes`
is given. It checks, before deleting anything, that every snapshot can be read and that the ones it
keeps are complete; if not it stops and changes nothing. Space is returned only for data that no
kept snapshot uses, so removing one of several similar snapshots frees little. See
[`PRUNE.md`](PRUNE.md) for the guarantees and for what an interrupted prune leaves.

## 7. Checking the repository

```sh
vaultsnap check                # quick: structure of every object, all snapshots authenticated
vaultsnap check --read-data    # full: reads and decrypts everything
```

A check **only reports; it never repairs or deletes.** Quick finds missing and truncated objects and
damaged snapshots without reading payloads, so it is cheap enough to run after every backup. It cannot
see flipped bits inside a chunk; only the full check can. Run the full check on a schedule that
matches how much you trust the storage (weekly to monthly), and always after moving or copying a
repository. Exit status 0: no errors (warnings such as orphaned data are normal); 3: errors found;
1: the check could not run. What each finding means and what to do is in [`CHECK.md`](CHECK.md).

## 8. Remote repositories

`--repo s3://BUCKET[/FOLDER]` keeps the repository in a bucket of S3 (or MinIO, or any compatible
service) with exactly the same commands:

```sh
export AWS_ACCESS_KEY_ID=...  AWS_SECRET_ACCESS_KEY=...      # or a profile, SSO, a role
vaultsnap --repo s3://my-bucket/laptop init
vaultsnap --repo s3://my-bucket/laptop backup ~/Documents
vaultsnap --repo s3://my-bucket --s3-endpoint https://s3.example.org --s3-region us-east-1 snapshots
```

Everything is encrypted before it leaves the machine. The bucket must exist; credentials come only from
the AWS credential chain (there is no option to pass them); certificates are always verified (a
private CA is trusted with `--s3-ca-bundle FILE`). For a service that is not AWS, give a region
(`--s3-region`, `VAULTSNAP_S3_REGION`, or `AWS_REGION`; any value the service accepts) or every command
prints a warning about the instance-metadata service. For a schedule, put `VAULTSNAP_REPO=s3://...`
(and `VAULTSNAP_S3_ENDPOINT` and `VAULTSNAP_S3_REGION` if needed) in the environment file and give the
job its credentials the way you would any AWS tool. This has been tried against a MinIO server and
not against AWS itself ([`RELIABILITY.md`](RELIABILITY.md)). Details, costs, and limits are in
[`S3.md`](S3.md).

## 9. Cheat sheet

| I want to... | Run |
|--------------|-----|
| start | `vaultsnap init` (once) |
| back up | `vaultsnap backup PATH...` |
| see snapshots | `vaultsnap snapshots` / `show ID` / `ls ID [PATH]` / `diff OLD NEW` |
| get files back | `vaultsnap restore ID --target DIR [--include PATH]` |
| free space | `vaultsnap prune --keep-... [--dry-run]` |
| verify | `vaultsnap check [--read-data]` |
| know my version | `vaultsnap version` |

| Exit status | Meaning |
|-------------|---------|
| 0 | Success |
| 1 | The command failed; the repository was not changed (see each command for the one exception) |
| 2 | Usage error; nothing ran |
| 3 | Finished, but the result has a problem: files not backed up, damaged snapshots, files not restored, errors found by a check, objects a prune could not delete |
| 130 | Cancelled with Ctrl-C or SIGTERM |
