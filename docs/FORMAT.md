# VaultSnap repository format (version 1, planned)

Status: **partly implemented**. `init`, the `config` file, the local store, locking, and key
management (master key, key files, derived keys, authenticated encryption) exist and are marked
*built*, as are chunking, the encrypted, deduplicated chunk store, the filesystem scanner, and
encrypted snapshot manifests. Packs and indexes are still *planned*, and nothing yet performs a
backup end to end; `backup`, `snapshots`, `restore`, `check`, and `prune` return "not
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
  data/<xx>/<id>    encrypted chunk objects (built); <xx> = first two hex chars of the chunk id
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

- **Snapshot id** (built): 256 random bits from the OS, as 64 lowercase hex characters
  (`SnapshotId`), generated when the manifest is built. It is the snapshot's file name, it is
  stored inside the manifest, and it is authenticated with the file, so a snapshot cannot be
  renamed or swapped. Pack and index ids (planned) will be digests of their stored bytes.
- **Chunk id** (built): `BLAKE3-keyed-hash(key = ContentId key, plaintext chunk)`, 256 bits, as 64
  lowercase hex characters (`ChunkId`). The `ContentId` key is derived from the master key (see
  *Keys and encryption*), so ids are specific to one repository and cannot be computed without
  it: a party who can read the repository cannot test whether a file they already have is stored
  in it. Identical chunks have identical ids within a repository, which is what deduplication
  uses.

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

## Chunk objects (built)

Each chunk is stored as one file, `data/<xx>/<id>`, where `<id>` is the chunk id in hex and `<xx>`
its first two hex digits (256 shard directories, created `0700` on demand). Layout, all integers
little endian:

```
offset  size  field
0       4     magic "VSCO"
4       1     format version = 1
5       1     object kind = 1 (chunk)
6       1     compression: 0 = none, 1 = zstd
7       1     flags, reserved, must be 0
8       4     plaintext length of the chunk in bytes (u32)
12      24    XChaCha20-Poly1305 nonce, random per object
36      n     ciphertext of the payload (n = payload length)
36+n    16    Poly1305 tag
```

- **Key**: the `DataEncryption` key. **Associated data**: the 12 header bytes followed by the 32
  raw bytes of the chunk id. The header is therefore readable but cannot be changed, and an
  object cannot be filed under a different id, without authentication failing.
- **Payload**: the chunk itself, or `zstd(chunk)` (level 3, one frame) if that is *strictly
  shorter*; otherwise stored uncompressed. Both the compressed and the plain forms add the same
  52 bytes of overhead.
- **Limits**: a chunk is at most 64 MiB (the chunker's limit). A reader refuses a larger declared
  length, refuses files larger than the largest valid object before reading them, and
  decompresses with an output cap equal to the authenticated declared length, so a malicious
  object cannot expand beyond it.
- **Verification on read**, in this order, failing at the first problem: (1) at least 5 bytes
  and the magic; (2) the format version (an unknown version is reported as such, nothing else is
  guessed); (3) minimum length, object kind, compression code, flags, declared length within
  limits, and a body length consistent with the declared length (exactly `24 + length + 16` for
  uncompressed objects, at most that for compressed ones); (4) the AEAD tag over header, id, and
  ciphertext; (5) decompression within the declared length and an exact length match; (6) the
  keyed BLAKE3 hash of the result must equal the id the object was requested by. Only then is
  the chunk returned. Steps 1-3 reject truncated and extended files cheaply; step 4 catches
  every modification of the stored bytes; step 6 additionally catches an authentic object that
  holds the wrong content.
- **Atomic, deduplicating write**: the chunk id is computed, and if `data/<xx>/<id>` exists
  nothing else happens (*duplicate*). Otherwise the object is staged in a `.tmp-` file in the
  shard directory, flushed with `fsync`, and published with a no-overwrite hard link (see *Local
  store*), then the directory is synced. If that publish finds the name already taken, another
  writer got there first and the result is again *duplicate*. Exactly one of any number of
  concurrent writers of the same chunk creates the object; the others leave nothing behind.
- **Trust in existing objects**: deduplication decides by the presence of the object and does
  not re-verify it. Damage is detected when the chunk is read and by the repository check
  (planned), not hidden. Repairing a damaged chunk means deleting its object and storing the
  chunk again.
- Stale `.tmp-` files left by a crash are ignored and can be removed by a future repository
  check.

### Cryptographic format decisions

| Decision | Reason |
|----------|--------|
| Chunk ids are a *keyed* hash (BLAKE3 keyed mode with a derived subkey) | An unkeyed content hash lets anyone with read access confirm that a known file is in the repository and link repositories. A MAC-style PRF keeps deduplication and removes both. BLAKE3 keyed mode is a standard PRF/MAC, needs no custom construction, and is fast on large chunks. |
| The id key and the encryption key are separate HKDF outputs | Independent keys per purpose: a flaw or leak in one use cannot be turned against the other. |
| XChaCha20-Poly1305 with a fresh random 192-bit nonce per object | 192 bits make random nonce collisions negligible for any repository size, so no counter or nonce state needs coordinating between concurrent writers or machines. Encrypting the same chunk twice gives different bytes. |
| No convergent (deterministic) encryption | It would make equal chunks produce equal ciphertexts and defeat the keyed-id design. Deduplication happens through ids, before encryption. |
| Header in clear but authenticated; chunk id in the associated data | Readers can dispatch on version, compression, and length before decrypting, and nothing in the header or the file's name can be altered or swapped undetected. |
| Compress, then encrypt, per chunk | Encrypted data does not compress, so compression must come first. Compressing each chunk alone, with no secret or attacker-influenced data in the same context, avoids the CRIME/BREACH class of attacks. It does reveal approximate compressibility through the object size (see `SECURITY.md`). |
| Compression only when strictly smaller | Incompressible data (encrypted, already compressed) pays no decompression cost and no expansion. |
| Decompression capped by the authenticated length | A modified object fails authentication before anything is decompressed, and even an object written by a holder of the key cannot expand beyond its declared length, which is itself bounded by the 64 MiB chunk limit. This bounds memory use for any damaged or hostile object. |
| The id is re-derived from the plaintext after decryption | Authentication proves who wrote an object, not that it holds the right data. Recomputing the id ties content to name regardless of how a wrong pairing arose (software bug, repair tooling, restoring from backup). |
| Explicit magic and version byte; unknown versions refused | Future formats (packs, new ciphers, new compression) can coexist; a reader never interprets bytes it does not understand. A format change is a new version number, never a silent reinterpretation. |
| One file per chunk | Simplest correct atomic unit, and the unit of concurrency control. Small chunks mean many files; **packs** that group chunks are planned and will be a new object kind/version, with this layout still readable. |

## Pack files (planned)

A pack will hold many encrypted chunks plus an encrypted trailer listing them (chunk id, offset,
length), to avoid one file per chunk. Packs will be written to a temporary name, flushed, and
renamed into `data/`, and will become visible only when an index object that references them is
written. Until then every chunk is a loose object as described above.

## Snapshots (built)

A snapshot is one immutable file, `snapshots/<id>`, holding an encrypted *manifest*: everything
a restore needs to know about one backup. Two version numbers apply and are both checked: the
**container version** (the encryption envelope, below) and the **manifest schema version** (the
layout of the plaintext). They are independent so either can evolve alone.

### Container (version 1)

The same shape as a chunk object, with its own magic and kind. Integers are little endian.

```
offset  size  field
0       4     magic "VSSN"
4       1     container format version = 1
5       1     object kind = 2 (snapshot)
6       1     compression: 0 = none, 1 = zstd
7       1     flags, reserved, must be 0
8       4     length of the manifest bytes in bytes (u32), at most 1 GiB
12      24    XChaCha20-Poly1305 nonce, random per file
36      n     ciphertext of the manifest bytes (zstd-compressed first only if strictly smaller)
36+n    16    Poly1305 tag
```

- **Key**: the `MetadataEncryption` key. **Associated data**: the 12 header bytes followed by the
  32 raw bytes of the snapshot id. The header cannot be altered, and a file cannot be renamed to
  another id, without authentication failing; two snapshots cannot be swapped.
- **Verification on read**, failing at the first problem: at least 5 bytes and the magic; the
  container version (an unknown version is reported as such, nothing else is guessed); minimum
  length, kind, compression code, flags, declared length within the 1 GiB limit, and a body
  length consistent with it; the AEAD tag; decompression capped at the authenticated declared
  length and an exact length match. A file larger than the largest valid file is never read.
- Everything sensitive, every file name and path, host name, and tag, is inside the ciphertext.

### Manifest schema (version 1)

All integers little endian and fixed width. `str` and `bytes` are a `u32` length followed by that
many bytes (`str` must be valid UTF-8; `bytes` are arbitrary, used for paths and names because
Unix names are not necessarily UTF-8). `list<T>` is a `u32` count followed by the items.
`opt<T>` is a flag byte (0 absent, 1 present, anything else invalid) followed by the value if
present. `ts` is `i64` seconds since the Unix epoch and `u32` nanoseconds (below 10^9).

```
u32   schema version = 1                    (read first; unknown versions are rejected as such)
[32]  snapshot id                           (must equal the id in the container's file name)
str   repository id                         (32 lowercase hex characters; must equal the repository's)
ts    created
str   host                                  (as supplied by the caller; may be empty)
list<str> tags
str   program                               (e.g. "vaultsnap 0.1.0")
-- backup configuration --
u32 u32 u32   chunker minimum, average, maximum size
u8    symlink policy      0 preserve, 1 follow inside root, 2 follow all
u8    hard link policy    0 preserve, 1 duplicate
u8    error policy        0 skip, 1 fail
u8    special file policy 0 record, 1 skip
u8    changed-file policy 0 fail, 1 accept
u8    cross filesystems   0 or 1
list<str> include patterns, list<str> exclude patterns
-- content --
list<root>:
  bytes source                              (the resolved path that was backed up)
  list<entry>:                              (pre-order; entry 0 is the root itself)
    u32   parent index                      (0xFFFFFFFF only for entry 0)
    bytes name                              (one path component; empty only for entry 0)
    u8    kind: 0 directory, 1 file, 2 symlink, 3 hard link, 4 fifo, 5 socket,
                6 char device, 7 block device, 8 other
    file:      list<{ [32] chunk id, u32 chunk length }>     (in file order)
    symlink:   bytes target
    hard link: u32 index of the entry that carries the data
    attributes: u64 size, opt<ts> modified, accessed, changed, created,
                opt<u32> mode, uid, gid, u8 readonly, opt<u64> device, inode, links
list<skipped>:
  u32 root index, bytes path, u8 reason (0 vanished, 1 unreadable, 2 other filesystem,
  3 special file), str detail            (empty for reasons 0 and 2)
7 x u64  totals: directories, files, symlinks, hard links, special files, bytes, skipped
```

The whole input must be consumed. A conforming reader checks every count and length against the
bytes that remain *before* allocating, so a lying count cannot exhaust memory.

### Validity rules

A manifest is accepted (when written and when read) only if all of these hold. They are the
security boundary against a manifest that is authentic but wrong, or written by a buggy or
future writer:

- the repository id is well formed, at least one root exists, every root has a non-empty source
  path without NUL, and a root that is not a directory has exactly one entry;
- entry 0 of each root has no parent and an empty name; every other entry has a parent with a
  **smaller index** that is a **directory**, and a name that is a single safe path component:
  non-empty, at most 4096 bytes, no `/`, no NUL, not `.` or `..`. Names are **unique among
  siblings**. Together these make cycles and path traversal impossible by construction, whatever
  a restore later does with the names;
- for a file, every chunk length is between 1 and 64 MiB and the lengths **add up to the size**;
- a hard link names an **earlier regular file** of its root; a symlink target is non-empty and
  has no NUL (it is data: it may be absolute or contain `..`, and a restore must never follow it
  while restoring);
- timestamps have nanoseconds below 10^9, skipped records name an existing root, the chunker
  settings are valid, every enumerated code is known, and the stored totals **equal the totals
  computed from the entries**.

Error messages identify entries by index, never by name, because names are sensitive.

### Compatibility and versioning

- Reading is the dispatch point: the schema version is read first and each supported version has
  its own decoder, so a build keeps reading every version it has ever written (**historical
  snapshots stay readable**). Only version 1 exists so far.
- A snapshot with a schema or container version newer than the build understands is reported
  as unsupported, with both numbers, and is never partly interpreted; nothing is modified.
- Changing the layout, a code, or the validity rules in a way an old reader would misread requires
  a new schema version, an entry here, and a decoder that still accepts the old one. Adding what
  to capture (extended attributes, ACLs, flags, sparse maps) is such a change.
- The container version is bumped only for changes to the envelope (cipher, header, compression).

### Atomic commit

A snapshot is committed in this order, stopping at the first failure:

1. the manifest is **validated**, and its repository id compared with the repository's;
2. **every chunk it refers to is confirmed present** in the chunk store; if any is missing the
   commit fails and reports how many (an interrupted or racing backup, or a concurrent prune,
   must not produce a snapshot that cannot be restored);
3. the manifest is encoded, compressed if smaller, and encrypted;
4. the file is **published in one step without overwriting** (staged in a `.tmp-` file, flushed,
   hard-linked to `snapshots/<id>`, directory synced).

Until step 4 completes nothing named like a snapshot exists, so an incomplete snapshot can never
be seen, listed, or loaded. A crash leaves at most a `.tmp-` file (ignored by all readers) and
unreferenced chunks (removed by `prune`). Committing an id that exists fails with *already
exists* and changes nothing; concurrent commits of the same id yield exactly one success.
The presence check is a point-in-time check: a chunk deleted *after* it is not detected until the
snapshot is read or checked, which is why `prune` must hold the exclusive lock and `backup` the
shared one.

### Loading

Loading authenticates the file against its id, decrypts, decodes, validates, and then requires
the manifest's own snapshot id and repository id to match the file's name and the repository.
A damaged, truncated, extended, wrong-key, or misplaced file is a *corrupt snapshot*, and
listings report such files with their error instead of omitting them or presenting them as good.
Files in `snapshots/` that are not named like a snapshot id, and `.tmp-` files, are ignored.

### Decisions

| Decision | Reason |
|----------|--------|
| One manifest file per snapshot, immutable | Atomic by construction (one publish step), trivially consistent for concurrent readers, nothing to repair. |
| Random snapshot id stored inside and bound as associated data | A content-address of the ciphertext cannot be stored inside the manifest; a random id can, and binding it makes renaming and swapping detectable. |
| Tree stored as parent index + single-component name | Makes the validity rules (no traversal, no cycles, unique siblings) checkable in one pass, and avoids repeating path prefixes. |
| Hand-specified binary layout, no serialization framework | The format is a long-term contract; it must not change because a library changed, and every field is validated explicitly. |
| Paths as bytes, not text | Unix names are arbitrary bytes; converting would corrupt names on restore. |
| Totals stored and cross-checked | A listing can show sizes without walking entries, and an inconsistent manifest is detected. |
| Commit refuses a snapshot with missing chunks | Never present as complete something that cannot be restored. |
| Whole manifest in one object (for now) | Simple and correct. Cost: memory proportional to the manifest (about 100 bytes plus the name per entry, 36 per chunk), a 1 GiB cap, and listing decrypts every snapshot. Splitting trees into chunk-stored subtrees, which also shares unchanged directories between snapshots, would be a new schema version. |

The entries come from the filesystem scanner (`SCANNING.md`), which defines what is captured:
type, size, modification, access, inode-change, and birth times at nanosecond resolution,
permission bits (`0o7777`), owner uid and gid, device and inode (for change detection), and the
hard-link and symlink structure. Extended attributes, ACLs, file flags, and sparse-file hole maps
are not captured yet.

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
| `ContentId`          | `vaultsnap/v1/content-id`              | keyed hash naming chunks (built)|
| `DataEncryption`     | `vaultsnap/v1/data-encryption`         | chunk objects (built), packs    |
| `MetadataEncryption` | `vaultsnap/v1/metadata-encryption`     | snapshots (built), index objects|

**Sealed messages.** `nonce(24 random bytes) || ciphertext || tag(16)` with XChaCha20-Poly1305
and caller-supplied associated data. Chunk objects bind their header (magic, version, kind,
compression, length) and chunk id; snapshot and index objects will bind their own kind and
version in the same way. Random 192-bit nonces make reuse negligible without any counter state.

## Write ordering and crash safety

1. Write new chunk objects (and, later, packs): temporary name, sync, no-overwrite publish.
2. Write the index object that covers them (planned).
3. Commit the snapshot (built; it checks that the chunks it names exist, then publishes).

Readers only trust snapshots, so a crash between steps leaves unreferenced chunks that `prune`
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
