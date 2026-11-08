# VaultSnap security boundaries (planned)

Status: **partly implemented**. Repository creation, the local store (permissions, symlink
refusal, atomic writes), locking, and key management exist: the master key is random, protected by
an Argon2id-derived key, and per-purpose keys are derived with HKDF. The encrypted chunk store
exists: chunks are named by a keyed hash, compressed when beneficial, and sealed with
XChaCha20-Poly1305, and snapshot manifests (every file name, path, host, tag, attribute, and the
chunk lists) are encrypted and authenticated together with their snapshot id, and committed
atomically. Nothing yet connects the scanner to these stores (backup does not exist). The
`config` file is plaintext by design and holds no secret. This document defines what the
implementation must guarantee, and what it deliberately does not. See `FORMAT.md` for the data
layout.

## Protected assets

- Confidentiality of file contents, file names, directory structure, and metadata stored in a
  repository.
- Integrity and authenticity of everything restored from a repository.
- The passphrase and the master key.

## Adversaries

| Adversary                                              | Defended |
|--------------------------------------------------------|----------|
| Reads repository storage (stolen disk, curious host)    | Yes      |
| Modifies, deletes, reorders, or replays repository files | Detected |
| Guesses the passphrase offline                         | Slowed by Argon2id; a weak passphrase is still weak |
| Compromised machine running VaultSnap                  | **No**   |
| Attacker who can read process memory or the passphrase | **No**   |
| Traffic analysis of sizes, counts, and timing           | **No**   |

"Detected" means VaultSnap fails the operation and says so. It cannot restore data an attacker
has deleted, and it cannot tell a rollback to an older complete repository state from the
current one.

## Information that is not hidden

Total repository size, the number of chunk objects, the exact size of every chunk object and
therefore (to within the 52-byte overhead) of every chunk, object timestamps as set by the
storage, and the repository format version. The chunk length is also stored in clear in the
object header (authenticated, not secret). Objects are not padded.

**Compression leaks compressibility.** Chunks are compressed before encryption, so an object's
size shows roughly how well its chunk compressed: highly repetitive data yields tiny objects,
random or already-compressed data yields objects as large as the chunk. This is inherent to
compress-then-encrypt. It is limited to per-chunk granularity because every chunk is compressed
on its own, never together with other data, which is what makes the CRIME/BREACH style of attack
(an attacker injecting guesses next to a secret in one compression context) inapplicable.

**Snapshots reveal their size and timing.** The snapshot file's size shows roughly how many
entries the backup has and how compressible their names are, its modification time shows when the
backup ran, and the number of files in `snapshots/` is the number of snapshots. Their contents,
including the number of roots, host, tags, and every name, are encrypted. A snapshot's list of
chunk ids is encrypted too, so which chunks a snapshot uses is not visible; only the chunk store
itself shows what exists.

**Deduplication is visible.** Two chunks with the same content have the same id and share one
file. Someone who can see the storage can tell which snapshots reuse which chunks and how much
data is shared, but cannot link chunks across repositories (ids are keyed per repository) or
confirm the content of a chunk.

**Chunk boundaries are a fingerprint.** The chunker's gear table is fixed and public, so the
sequence of chunk sizes of a file is a deterministic function of its content. Someone who can see
the stored (encrypted) chunk lengths and already has a candidate file can chunk it and compare
the size sequence to confirm the file is probably in the repository. Chunk *ids* do not have this
problem (they are keyed with the repository's `ContentId` key), but lengths do. Mitigation under
consideration: derive the gear table from a per-repository secret, and/or pad chunks. Until then
this is a known limitation for users whose threat model includes confirming known files against
storage-side metadata.

The chunker itself processes untrusted file content only as opaque bytes, allocates at most
`2 * max_size` bytes however large the input, and is designed not to panic on any input
(exercised by the property tests).

## Secrets handling

- The passphrase is never accepted as a command-line argument or as an environment variable
  value (both are visible to other processes and often end up in shell history or logs). It is
  read from an explicit file (`--password-file`, or `VAULTSNAP_PASSWORD_FILE` naming a file) or,
  when none is given and a terminal is attached, from an interactive prompt with echo disabled
  (asked twice when creating a repository). Without a file and without a terminal the command
  fails rather than guessing. The file is read once; one trailing newline is removed; it must be
  non-empty and at most 4096 bytes, and VaultSnap warns if other users can read it.
- The passphrase is obtained *before* anything is created on disk.
- Secrets (passphrase, master key, derived keys, decrypted plaintext) are held in buffers that
  are zeroed on drop (`zeroize`), and `Passphrase` and `RepositoryKeys` print as `<redacted>`, so
  they cannot leak through `Debug`, `tracing` fields, or error messages. Tests assert that the
  passphrase appears in neither output nor stored files, even at `-vvv`.
- Limits of zeroization: copies made inside dependencies (the cipher key schedule, Argon2's
  working memory, HKDF's pseudorandom key), by the terminal or operating system, in swap, or by
  `Clone` of a `Passphrase` are only cleared if and when those components do so. Memory locking
  (`mlock`) is not used. An attacker who can read VaultSnap's memory is out of scope (above).
- Randomness (master key, salts, nonces, ids) comes only from the operating system via
  `getrandom`; there is no fallback generator.
- Passphrase strength is the user's responsibility. Argon2id slows offline guessing but cannot
  rescue a weak passphrase, and VaultSnap does not enforce a policy beyond non-empty.
- Diagnostics (`tracing`) may log counts, ids, and repository paths. They must not log file
  contents or key material. File names are logged only at `debug` level or lower.
- Key files and the repository are created with owner-only permissions (`0600` files, `0700`
  directories) on Unix.

## Restore safety

Restore treats snapshot content as untrusted until authenticated, and even authenticated content
as untrusted for path purposes:

- Entry names must be a single path component: no `/`, no `..`, no empty or NUL-containing names.
- Output is created only beneath the target directory. Existing symlinks along the destination
  path are not followed out of it.
- Files are created without following symlinks and are never overwritten silently unless
  requested.
- Special files (devices, sockets) are skipped unless explicitly enabled; setuid/setgid bits are
  not restored by default.

## Input validation

- Object names and snapshot ids are 64 lowercase hex characters, checked before building paths
  (`SnapshotId`). This prevents path traversal through user input.
- Lengths and counts read from the repository are bounded before allocating. Chunk object files
  larger than the largest valid object are never read; a declared chunk length above 64 MiB is
  rejected; decompression is capped at the authenticated declared length.
- Chunk objects are verified completely before any byte is returned: magic and version, lengths,
  the AEAD tag over header, chunk id, and ciphertext, the decompressed length, and finally that
  the keyed hash of the content equals the id. Truncated, extended, modified, misplaced, and
  wrong-key objects all fail with a corruption error and release no data. Chunk object names are
  built only from a parsed 32-byte id, never from free text.
- The repository format version is checked before any other parsing of repository contents.

## Filesystem handling (built)

- Repository directories are created `0700` and files `0600`.
- Files are never overwritten by creation: `init` and `write_new` fail rather than replace.
- `init` refuses to place a repository inside a non-empty directory it does not recognize.
- Names passed to the store are validated; symlinks below the repository root are refused.
  The check is made before the operation, so a local attacker who can race symlink swaps inside
  the repository directory is outside the threat model (they already have write access to it).
- Temporary files are created exclusively (`create_new`) with random names.

## Reading the user's files (scanner; built)

The tree being backed up is **not trusted**: it may be changed by other users and processes
while a backup runs, including by an attacker who can write to it while VaultSnap runs with
more privilege than that attacker (for example a root backup of a shared directory). The scanner
is designed so such a party cannot make a backup read files outside the requested root or hang
it:

- Symbolic links are never followed by default, so a link cannot pull in `/etc/shadow` or any
  other target. Following inside the root, or anywhere, is a separate explicit option, and
  loops and links to ancestors are always cut.
- When a file is opened, the final component is opened with `O_NOFOLLOW`, the opened file must
  be a regular file with the device and inode recorded at scan time, and `O_NONBLOCK` keeps a
  swapped-in FIFO from blocking. A file or directory swapped for a link or another file between
  scan and read is therefore refused (`FileChanged`), not read. This relies on the kernel's
  inode identity; it is not available on Windows, where the protection is weaker.
- Special files are never opened; FIFOs, sockets, and devices are recorded as metadata only.
- Files are read as streams; a file of any size cannot exhaust memory, and a file that changes
  while it is read fails the read instead of silently producing inconsistent data.
- Filesystem boundaries are not crossed by default, so pseudo-filesystems and network mounts are
  not read by accident.

This does not stop an attacker from *reading* what the backup user can read, nor from making a
file change in a way the checks cannot see (see `SCANNING.md`).

## Snapshots (manifests; built)

- **Confidentiality and authenticity.** The manifest, including every path, is sealed with
  XChaCha20-Poly1305 under the `MetadataEncryption` key, and the header and snapshot id are
  authenticated. A snapshot cannot be edited, truncated, extended, renamed, or swapped with
  another without failing authentication; it cannot be read without the key.
- **Untrusted even when authentic.** A manifest that decrypts correctly is still validated before
  use: names are single safe path components (no `/`, NUL, `.`, `..`, or empty), unique among
  siblings, parents precede children, chunk lengths add up to sizes, hard links name earlier
  files. This makes path traversal and loops impossible by construction for a restore, even for
  a manifest produced by a buggy or future writer. Symlink targets are data and must never be
  followed during restore.
- **Bounded work.** Counts and lengths are checked against the bytes left before allocating; the
  manifest is capped at 1 GiB; decompression is capped at the authenticated length. Malformed
  and hostile inputs are meant to fail with an error and never panic; tests truncate the
  encoding at every length and mutate every byte (they have not been run yet).
- **Atomic, complete-or-absent.** A snapshot is published in one no-overwrite step, only after it
  validates and all its chunks are confirmed present. Incomplete snapshots are never visible;
  damaged ones are reported as damaged. Snapshots are never overwritten, so a later bug cannot
  silently rewrite history.
- **Not hidden / not defended.** An attacker who can write to the repository can delete a
  snapshot or roll the whole repository back to an older complete state, and VaultSnap cannot
  tell (it has no external record of which snapshots should exist). Plaintext manifests and
  their paths exist in memory while in use and are only partly zeroized (the encoded bytes are;
  the decoded structure is not).
- **No names in diagnostics.** Validation errors identify entries by index, never by name.

## Concurrency

Advisory locks stop two VaultSnap processes from corrupting each other. They are not a security
control: someone with write access to the repository can ignore them.

## Code and supply chain

- `unsafe` code is forbidden workspace-wide (`unsafe_code = "forbid"`).
- Cryptography comes from maintained, audited crates; there are no home-grown primitives.
- Dependencies are added deliberately, are pinned by `Cargo.lock`, and are reviewed for
  maintenance status before use.
- Tests use temporary directories inside the project and never touch real user data.

## Reporting

Report suspected vulnerabilities privately to the maintainers rather than in a public issue.
