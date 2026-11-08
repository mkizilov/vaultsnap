# VaultSnap repository format (version 1, planned)

Status: **partly implemented**. `init`, the `config` file, the local store, locking, and key
management (master key, key files, derived keys, authenticated encryption) exist and are marked
*built*. Chunking, packs, indexes, and snapshot objects, and therefore encryption of file data and
metadata, are still *planned*; `backup`, `snapshots`, `restore`, `check`, and `prune` return "not
implemented". Code that lands later must follow this document, and changes to the format must
update it first and bump `FORMAT_VERSION` (`crates/vaultsnap-core/src/repository.rs`).

Repositories created by pre-release builds before key management existed have no `keys/`
directory or key file; they are not usable and must be re-created (nothing of value could have
been stored in them).

Cryptographic primitives are the ones named under *Keys and encryption*; no primitive is
implemented by VaultSnap itself.

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
  keys/<id>         key files: the master key wrapped by a passphrase-derived key (built)
  index/<id>        encrypted index objects mapping chunk ids to pack locations (planned)
```

These names are the constants on `RepositoryLayout`. `init` creates `config`, `data/`,
`snapshots/`, `locks/`, `keys/`, and one key file. `index/` is created by the stage that
introduces the index. Object names are lowercase hexadecimal (`[0-9a-f]`), 64 characters long,
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
3. If it is a non-empty directory with anything other than `data/`, `snapshots/`, `locks/`,
   `keys/`, or `.tmp-*` files, fails without writing: a VaultSnap repository is never mixed into
   other data.
4. Otherwise creates the root (and missing parents) if needed, then `data/`, `snapshots/`,
   `locks/`, `keys/` with mode `0700`; generates the repository id and master key; writes the key
   file; and finally writes `config` atomically, never overwriting.

The passphrase is obtained by the caller before `init` starts, so a missing or unreadable
passphrase creates nothing.

`config` is written last and is what makes a directory a repository, so an interrupted `init`
leaves only directories and possibly a key file and can be re-run (step 3 accepts them). A key
file left by an interrupted run belongs to a repository id that never received a `config`; it
fails to authenticate under the new id and is ignored when unlocking. Concurrent `init` calls on
the same path produce exactly one repository; the others fail with *already initialized* and
remove the key file they wrote.

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

## Chunking and deduplication (chunker built; storing chunks planned)

Files are split with content-defined chunking (FastCDC with normalized chunking) so inserting or
deleting bytes only changes the chunks near the edit. A chunk whose id is already in the index is
not stored again. The chunker (`vaultsnap_core::chunking`) is independent of storage; the rules
below are the format and must never change without a format version bump, because they decide
every chunk boundary and therefore whether new data deduplicates against old.

**Parameters.** `min`, `avg`, `max` chunk size in bytes with `64 <= min <= avg <= max <= 64 MiB`
and `avg >= 256`. Defaults: 256 KiB, 1 MiB, 4 MiB. Chunks produced with different parameters do
not deduplicate against each other (the parameters will be recorded in `config` when backup
stores chunks).

**Gear table.** `GEAR[i]`, `i = 0..255`, is the `(i+1)`-th output of SplitMix64 started from
state `0x5661756c74536e61` (ASCII "VaultSna"): for each output, `state += 0x9E3779B97F4A7C15`
(wrapping), then `z = state; z = (z ^ z >> 30) * 0xBF58476D1CE4E5B9; z = (z ^ z >> 27) *
0x94D049BB133111EB; output = z ^ z >> 31` (wrapping, 64-bit).

**Cut rule.** Scanning a chunk from its first byte, with `h` a 64-bit register:

1. Bytes with index `< min - 64` are skipped. Every later byte `b` updates
   `h = (h << 1) + GEAR[b]` (wrapping). Because the register is 64 bits wide, by the time the
   chunk has `min` bytes `h` depends only on the last 64 bytes.
2. For each length `n` from `min` upwards: update `h` with byte `n - 1`; if `n == max`, cut at
   `max`; otherwise cut at `n` if `h & mask == 0`, where `mask` has the top `bits + 2` bits set
   while `n < avg` and the top `bits - 2` bits set once `n >= avg`. `bits` is `log2(avg)` rounded
   to the nearest integer (round up when `avg >= 2^floor * sqrt(2)`, computed in integers).
3. If the data ends first, the rest is the last chunk (it may be shorter than `min`). Empty data
   has no chunks.

Consequences: a cut depends only on the preceding 64 bytes and the chunk length so far, never on
read sizes or file offsets; an edit disturbs only the chunks it touches and the cut points
re-align shortly after it; chunks before an insertion or deletion are always unchanged, and
appending data changes only the previous last chunk. Runs of one repeated byte produce chunks of
one constant length, so they deduplicate perfectly.

**Streaming.** The reader is consumed incrementally into a buffer of at most `2 * max` bytes
(allocated lazily from 64 KiB), so memory does not grow with file size.

**Known limitation.** The gear table is fixed and public, so chunk sizes in a repository reveal
the cut pattern of the data to someone who can see the storage (see `SECURITY.md`).

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

## Keys and encryption (built, except where marked planned)

**Primitives** (all from maintained RustCrypto crates; none implemented here): Argon2id
(`argon2`), HKDF-SHA256 (`hkdf`, `sha2`), XChaCha20-Poly1305 (`chacha20poly1305`), OS randomness
(`getrandom`). Zeroizing buffers come from `zeroize`.

**Master key.** `init` draws 256 random bits from the OS. It exists unencrypted only in memory,
in a zeroizing buffer, and is never written to disk, logged, or printed.

**Key files.** Each file in `keys/` is a text file wrapping the master key under a passphrase:

```
vaultsnap-key
key_version=1
kdf=argon2id
kdf_argon2_version=19
kdf_m_cost_kib=65536
kdf_t_cost=3
kdf_p_cost=4
kdf_salt=<32 lowercase hex characters: 16 random bytes, unique per file>
cipher=xchacha20poly1305
sealed=<144 lowercase hex characters: nonce(24) || ciphertext(32) || tag(16)>
```

- The key-encryption key is `Argon2id(passphrase, kdf_salt, m, t, p)` with a 32-byte output. The
  defaults (64 MiB, 3 passes, 4 lanes) follow RFC 9106; the parameters used are stored in the file,
  so they can be raised for new repositories without affecting existing ones. When reading a
  file, parameters above 1 GiB, 16 passes, or 16 lanes are refused so a modified file cannot
  exhaust memory or time.
- `sealed` is XChaCha20-Poly1305 of the master key with a fresh random 192-bit nonce. The
  associated data is the text of every header line above except `sealed`, in file order, with the
  line `repository_id=<id>` inserted after `key_version` (the exact bytes are built by
  `KeySlot::aad` in `crates/vaultsnap-core/src/keys.rs`). Changing any parameter, the salt, or the
  algorithms, or moving the file to another repository, makes authentication fail.
- The parser is as strict as for `config`: fixed header, `key=value` lines, no duplicate or
  unknown keys, at most 4096 bytes, and `key_version` is checked before everything else, so a
  newer file is reported as an unsupported version. Algorithm names and the Argon2 version are
  fixed strings; anything else is refused rather than guessed at.
- A wrong passphrase and a modified file are indistinguishable (both fail authentication) and
  reported as "incorrect passphrase, or the key file was modified".
- Unlocking tries every file in `keys/`; files that do not authenticate are skipped, files that
  cannot be parsed are errors. This is what makes several passphrases possible later (planned:
  adding and removing key files rewrites `keys/` only, never data).

**Derived keys.** One key per purpose, `HKDF-SHA256(salt = none, ikm = master key, info = label)`
with 32-byte output. Labels are part of the format and never change or get reused:

| Purpose              | Label                                  | Used for (planned)              |
|----------------------|----------------------------------------|---------------------------------|
| `ContentId`          | `vaultsnap/v1/content-id`              | keyed hash naming chunks        |
| `DataEncryption`     | `vaultsnap/v1/data-encryption`         | pack files                      |
| `MetadataEncryption` | `vaultsnap/v1/metadata-encryption`     | snapshot and index objects      |

**Sealed messages.** `nonce(24 random bytes) || ciphertext || tag(16)` with XChaCha20-Poly1305
and caller-supplied associated data. The object kind and format version will be bound as
associated data so one kind of object cannot be substituted for another (planned, with the
objects). Random 192-bit nonces make reuse negligible without any counter state.

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
