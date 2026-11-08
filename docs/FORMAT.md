# VaultSnap repository format (version 1, planned)

Status: **partly implemented**. `init`, the `config` file, the local store, and locking exist and
are described as built. Everything about keys, encryption, chunking, packs, indexes, and snapshot
objects is still *planned*; `backup`, `snapshots`, `restore`, `check`, and `prune` return "not
implemented". Code that lands later must follow this document, and changes to the format must
update it first and bump `FORMAT_VERSION` (`crates/vaultsnap-core/src/repository.rs`).

Cryptographic primitives named here are the *planned* choices; the final selection is recorded in
this file before the first byte of ciphertext is written.

## Goals

- Incremental: unchanged data is never stored twice, across files and across snapshots.
- Encrypted and authenticated: a party who can read and modify the repository storage learns
  little and cannot undetectably alter content.
- Crash-safe: an interrupted operation never makes existing snapshots unreadable.
- Append-mostly: objects are immutable once written; only `prune` deletes.
- Plain files on a local filesystem first; the layout maps onto object stores later.

## Directory layout

```
<repo>/
  config            plaintext header: format version, repository id (built)
  data/             object storage; encrypted pack files <xx>/<id>, <xx> = first two hex chars
  snapshots/        snapshot metadata; one encrypted snapshot object per snapshot
  locks/<id>.lock   advisory lock files (built)
  keys/<key-id>     key slots: the master key wrapped by a passphrase-derived key (planned)
  index/<id>        encrypted index objects mapping chunk ids to pack locations (planned)
```

These names are the constants on `RepositoryLayout`. `init` creates `config`, `data/`,
`snapshots/`, and `locks/`. `keys/` and `index/` are created by the stages that introduce
encryption and the index. Object names are lowercase hexadecimal (`[0-9a-f]`), 64 characters long,
and are validated before being turned into paths. The store (below) accepts only `/`-separated
segments of `[A-Za-z0-9._-]` that do not start with `.`.

## The `config` file (built)

Plain text, written once by `init`, never rewritten in place:

```
vaultsnap-repository
format_version=1
repository_id=<32 lowercase hex characters, 128 random bits from the OS>
created_at_unix=<seconds since the Unix epoch>
```

The parser is strict: the first line must be the header, every other line is `key=value`, no
duplicate or unknown keys, at most 4096 bytes. `format_version` is checked *before* any other
field, so a repository written by a newer release is reported as an unsupported version rather
than as malformed. The repository id is unique per repository and is the only identity the
config carries; it contains no secret.

## Initialization (built)

`init` on a path:

1. Rejects an empty path, and a path that exists but is not a directory.
2. If the path is a directory containing a `config`: parses it, then fails with *already
   initialized* (or *unsupported version* / *invalid config*). Nothing is written.
3. If it is a non-empty directory with anything other than `data/`, `snapshots/`, `locks/`, or
   `.tmp-*` files, fails without writing: a VaultSnap repository is never mixed into other data.
4. Otherwise creates the root (and missing parents) if needed, then `data/`, `snapshots/`,
   `locks/` with mode `0700`, and finally writes `config` atomically, never overwriting.

`config` is written last and is what makes a directory a repository, so an interrupted `init`
leaves only empty directories and can be re-run (step 3 accepts them). Concurrent `init` calls on
the same path produce exactly one repository; the others fail with *already initialized*.

## Local store (built)

All repository file access goes through `LocalStore`:

- **Names** are validated (see above); absolute paths, `..`, hidden names, and empty segments are
  rejected before touching the filesystem.
- **Symlinks** found at any component below the repository root are refused, never followed. (The
  root itself may be a symlink chosen by the user.)
- **Atomic commit**: a write is staged in `.tmp-<random>` in the destination directory, flushed
  with `fsync`, and published in one step: `write_new` hard-links it to the final name (fails if the
  name exists, so nothing is ever overwritten); `write_replace` renames it over the old file. The
  directory is then `fsync`ed. Readers see the old state or the complete new file.
- **Permissions**: directories `0700`, files `0600` (subject to umask, which can only tighten them).
- `read` takes a maximum size and refuses larger files.

The store needs a filesystem that supports hard links (any local Unix filesystem does).

## Locks (built)

`locks/<32 hex>.lock` contains `kind=shared|exclusive`, `pid=`, and `created_at_unix=`. A process
creates its own lock file atomically, then scans the other locks; if any conflicts (exclusive vs
anything, or a file it cannot recognize) it removes its own file and fails naming the holder. Two
racing exclusive requests may both fail but never both succeed. The lock is removed when the guard
is dropped. Stale locks left by a crashed process are never removed automatically; the error names
the file so an operator can delete it after confirming no VaultSnap process is running.

## Identifiers

- **Snapshot id**: 256-bit digest of the snapshot object's stored bytes, as 64 lowercase hex
  characters (`SnapshotId`). Pack and index ids are derived the same way.
- **Chunk id**: keyed BLAKE3 of the plaintext chunk, keyed with a subkey derived from the master
  key. Keying prevents a storage-side party from confirming that a known file is present.

## Chunking and deduplication

Files are split with content-defined chunking (FastCDC, target 1 MiB, min 256 KiB, max 4 MiB) so
inserting bytes into a file only changes nearby chunks. A chunk whose id is already in the index is
not stored again.

## Pack files

A pack holds many encrypted chunks plus an encrypted trailer listing them (chunk id, offset,
length). Packs are written to a temporary name inside the repository, flushed to disk, and then
renamed into `data/`. A pack becomes visible only when an index object that references it is
written.

## Snapshot object

An encrypted document containing: creation time, host name, tags, source paths, and a reference
to the root of a tree of directory and file entries. Directory and file entries are stored as
chunks, so unchanged subtrees are shared between snapshots. Entries record name, type, mode, size,
modification time, symlink target, and chunk ids.

## Encryption

- A random 256-bit **master key** is generated by `init`.
- Each `keys/` slot stores the master key encrypted under a key derived from a passphrase with
  Argon2id; the KDF salt and cost parameters are stored in the slot. Adding or changing a passphrase
  rewrites slots, never data.
- Objects are sealed with an AEAD (XChaCha20-Poly1305 planned) using a random 192-bit nonce
  stored in front of the ciphertext. The object type and format version are bound as associated
  data so one object kind cannot be substituted for another.
- Only audited, maintained crates (RustCrypto, `blake3`) are used. No custom primitives.

## Write ordering and crash safety

1. Write new packs (temporary name, sync, rename).
2. Write the index object that covers them.
3. Write the snapshot object.

Readers only trust snapshots, so a crash between steps leaves unreferenced packs that `prune`
later removes. Existing objects are never modified in place.

## Locking

`backup`, `restore`, `snapshots`, and `check` take shared locks; `prune` takes an exclusive lock.
Locks are advisory and carry a timestamp for diagnostics. Locking protects against concurrent
VaultSnap runs, not against a hostile writer (see `SECURITY.md`).

## Command semantics

| Command     | Reads              | Writes                                   |
|-------------|--------------------|------------------------------------------|
| `init`      | nothing            | `config`, one `keys/` slot, empty dirs   |
| `backup`    | index              | packs, index, snapshot                   |
| `snapshots` | snapshots          | nothing                                  |
| `restore`   | snapshot, packs    | files under the target directory only    |
| `check`     | everything*        | nothing                                  |
| `prune`     | snapshots, index   | removes snapshots, rewrites packs/index  |

\* `check` reads metadata only unless `--read-data` is given, which authenticates every pack.

`prune` keeps a snapshot if any retention rule selects it, refuses an empty policy, and supports
`--dry-run`. Pruning uses mark-and-sweep: mark every chunk reachable from kept snapshots, then
rewrite or delete packs with unreachable chunks.

## Versioning

`config` holds `format_version`. A reader refuses repositories whose version it does not know and
never writes to a repository with a newer version. Opening a repository (`load_config`, `lock`)
also verifies that `data/`, `snapshots/`, and `locks/` exist.
