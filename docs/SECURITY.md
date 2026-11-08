# VaultSnap security boundaries (planned)

Status: **partly implemented**. Repository creation, the local store (permissions, symlink
refusal, atomic writes), locking, and key management exist: the master key is random, protected by
an Argon2id-derived key, and per-purpose keys are derived with HKDF. File data and metadata are
not encrypted yet because backup does not exist yet, so nothing user-supplied is stored at all.
The `config` file is plaintext by design and holds no secret. This document defines what the
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

Total repository size, number and approximate size of packs, object timestamps as set by the
storage, and the repository format version. Chunk sizes are bounded and packs are padded only to
the extent documented in `FORMAT.md` once defined.

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
- Lengths and counts read from the repository are bounded before allocating.
- The repository format version is checked before any other parsing of repository contents.

## Filesystem handling (built)

- Repository directories are created `0700` and files `0600`.
- Files are never overwritten by creation: `init` and `write_new` fail rather than replace.
- `init` refuses to place a repository inside a non-empty directory it does not recognize.
- Names passed to the store are validated; symlinks below the repository root are refused.
  The check is made before the operation, so a local attacker who can race symlink swaps inside
  the repository directory is outside the threat model (they already have write access to it).
- Temporary files are created exclusively (`create_new`) with random names.

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
