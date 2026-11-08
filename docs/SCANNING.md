# Filesystem scanning

The scanner (`vaultsnap_core::scan`) walks the files a backup was asked to cover and reports
them. It reads **no file content during the walk**: it lists directories and inspects each path
(`lstat`). Content is read afterwards, one file at a time, as a stream (`Scanner::open`), which
plugs into the chunker. Status: written for Unix (Linux, macOS), tested on macOS only (see
*Verification status*); it compiles and lints clean for Linux. Platform-gated fallbacks for other
systems type-check for Windows but have never been linked or run (see *Platforms*).

## What is reported

For each path an **entry**: a path relative to the root (native encoding, empty for the root
itself), a **kind**, a **followed-symlink** flag, and **metadata**.

| Kind | Meaning |
|------|---------|
| `Directory` | Its entries follow it in the stream. |
| `File` | A regular file whose content can be opened and read. |
| `Symlink { target }` | A symbolic link, with the target text exactly as stored; never resolved or validated. |
| `HardLink { target }` | Another path to a file already reported; `target` is the relative path of the entry holding the data. |
| `Fifo`, `Socket`, `CharDevice`, `BlockDevice`, `Other` | Special files: metadata only, never opened. |

Metadata: size, modification time, access time, inode-change time (`ctime`), birth time,
permission bits (`0o7777`: including setuid, setgid, sticky), owner uid and gid, a read-only flag,
device id, inode, and link count. Timestamps are seconds and nanoseconds since the Unix epoch
(negative before 1970). Anything a platform cannot supply is `None`.

Not captured (yet): extended attributes, ACLs, file flags (`chattr`, `chflags`), macOS resource
forks and Finder metadata, SELinux labels, sparse-file hole maps (sparse files are read as their
full length, holes as zeros), device numbers of device nodes, and the time zone or clock accuracy
of the filesystem.

A path that is **left out** appears in the stream as `Skipped { path, reason }` with a reason
(vanished, unreadable with the OS error, other filesystem, special file by policy), so a caller
can tell a complete result from a partial one. Paths removed by include/exclude patterns are not
reported individually, only counted in the stats.

## Order

The stream is deterministic for a given tree: **depth first, a directory before its contents, and
names in each directory in ascending byte order** (the raw bytes of the name on Unix; UTF-16 code
units on Windows). It does not depend on the order the filesystem lists names in, on locale, or on
case folding. Two scans of an unchanged tree produce identical streams (access times can differ).
Memory is proportional to the names in the directories currently open (the sorted listing of each
directory on the current path) plus one record per hard-linked file, not to the size of the tree.
The walk uses an explicit stack, so depth is limited by the filesystem's path length limit, not by
the program's call stack.

## Policies

Every judgment call is an explicit, documented option with a conservative default.

### Symbolic links: `SymlinkPolicy`

| Value | Behavior |
|-------|----------|
| `Preserve` (default) | Every link is recorded as a link; **no link is ever followed**, so nothing outside the root, and nothing twice, is read through a link. |
| `FollowInsideRoot` | A link is followed only if it resolves (all links followed, `realpath`) to a path inside the resolved scan root. The target is recorded as an ordinary file or directory at the link's path, with the followed flag set. Links pointing outside the root are recorded as links. |
| `FollowAll` | Every link that resolves is followed, including to places outside the root. Opt-in only; this can read data the user did not ask to back up. |

In all modes a link that **dangles**, **loops** (a to b to a), is unreadable, or points to an
**ancestor directory** of itself is recorded as a link, never followed, so the walk always
terminates. Loops are detected by comparing the resolved path of a directory with the resolved
paths of the directories being walked. A link given as the **scan root** itself is resolved,
because it is the path the user explicitly named.

A hostile process that can rewrite the tree during the scan cannot redirect reading outside the
root through a link: see *Reading files*.

### Hard links: `HardLinkPolicy`

`Preserve` (default): the first path in scan order to reach a file with more than one link
(identified by device and inode) is recorded as a `File`; later paths to it as `HardLink` entries
naming the first. The data is stored once and the links can be recreated. Links to a file whose
other names are outside the root are still just that file. `Duplicate`: every path is an
independent `File` (the chunk store still deduplicates the bytes). Platforms without inode
numbers always behave as `Duplicate`.

### Unreadable paths: `ErrorPolicy`

`Skip` (default): an operation that fails, typically permission denied while listing a directory,
reading a link, or inspecting a path, yields `Skipped` with the OS error, and the scan carries on
with everything else. A directory that cannot be listed is not recorded at all. `Fail`: the first
such error ends the scan and is returned. A file whose *metadata* is readable but whose *content*
is not is recorded normally; the failure surfaces when it is opened.

### Files changing during the scan

* **Disappearing during the walk.** Each directory is listed once, then each name is inspected.
  A path that vanishes in between (`NotFound`) is reported as `Skipped { Vanished }` under every
  policy: it is a race with another process, not an error. Paths created after a directory was
  listed are not seen; the result reflects each directory at the time it was listed.
* **Changing while being read.** A file's content is only trustworthy if it did not change while
  it was read. `ChangePolicy::Fail` (default): when the reader reaches the end of the file it
  compares size, modification time, and inode-change time with their values at open, and the
  number of bytes read with the size; any difference makes the final read fail with a
  `FileChanged` error, so torn data cannot be mistaken for a consistent copy (data read before the
  error must be discarded, and a caller may retry). `Accept` lets the read succeed and marks the
  reader `changed()`.
  Limits: a writer that rewrites the content at the same size and then resets the modification
  time is still caught by `ctime`, which ordinary programs cannot set; a file that is modified and
  then restored exactly between open and end of read looks unchanged; files whose reported size
  is meaningless (`/proc`, some FUSE filesystems) are always flagged as changed; and `ctime` also
  moves for metadata-only changes such as `chmod` or a new hard link, which are reported as a
  change even though the bytes are intact (a false alarm, never a miss).
* **Replaced between scan and read.** Opening checks that the file is still a regular file with
  the same device and inode as when scanned, so a file swapped for a different one (or for a link
  or FIFO) is refused with `FileChanged`.

### Filesystem boundaries: `cross_filesystems`

Default `false`: a directory whose device id differs from the root's (a mount point) is reported
as `Skipped { OtherFilesystem }` and not entered, so `/proc`, `/sys`, network mounts, and other
volumes are not swept in by accident. The mount point directory itself is not recorded. Set
`true` to descend. Followed symbolic links are subject to the same rule. Platforms without device
ids cannot detect boundaries and behave as if crossing were allowed. Bind mounts of the same
filesystem share a device id and are not detected as boundaries.

### Special files: `SpecialFilePolicy`

`Record` (default): FIFOs, sockets, and device nodes are recorded as entries carrying metadata
only. They are **never opened**, so a FIFO with no writer cannot block the scan, and `open`
refuses anything but regular files. `Skip`: they are reported as skipped. A restore should not
recreate sockets (a socket file without its listener is meaningless) and needs privileges for
device nodes; that is a restore-time decision.

## Include and exclude patterns

Patterns are globs matched against the path relative to the root, always with `/` separators:

* `*` and `?` stay within one path component; `**` crosses components; `[abc]`, `[a-z]`, `[!x]`,
  and `{one,two}` work. Leading dots are not special. Matching is case sensitive (even on
  case-insensitive filesystems).
* No `/` in the pattern: matches the name at any depth (`*.log`).
* A leading `/` or a `/` inside the pattern anchors it at the root (`/build`, `src/*.o`).
* A trailing `/` on an *exclude* pattern matches directories only (`cache/`).
* On Unix a backslash escapes the next character (`\[x\]` matches the name `[x]`).
* **Exclude**: a matching entry is omitted, and a matching directory is not entered at all
  (nothing below it is listed or read). **Include**: if any include pattern is given, a
  non-directory entry is recorded only if it matches one; directories are always walked (unless
  excluded) so that included files below them are found, which means directories without any
  included file are still recorded. Exclusion wins over inclusion. Include patterns cannot end in
  `/`; use `dir/**`.

Patterns apply to the entry as found; with `FollowInsideRoot`/`FollowAll` a link to a directory
is matched as the directory it leads to.

## Reading files

`Scanner::open(entry)` returns a `FileReader`, a plain `Read`. Nothing is buffered beyond what
the caller's buffer holds, so file size does not affect memory; the chunker adds at most twice its
maximum chunk size. The open is hardened for hostile or racing trees (Unix):

* the final path component is opened with `O_NOFOLLOW` (unless the entry was reached through a
  followed link), so a link swapped in for the file is refused;
* `O_NONBLOCK` means a FIFO swapped in cannot hang the open;
* the opened file must be a regular file with the scanned device and inode, which also defeats a
  swapped *directory* component leading to some other file;
* its metadata at open (`FileReader::metadata`) is the fresh value to store if the file changed
  since the scan.

## Platforms

| | Linux | macOS | Windows (untested) |
|-|-------|-------|--------------------|
| Names, order | bytes, any value except `/` and NUL | bytes; APFS requires valid UTF-8 and preserves normalization form | UTF-16 units, order by unit; reserved names (`CON`, `NUL`) and trailing dots/spaces are not handled specially |
| Permissions, owner, mode bits | yes | yes | not captured (`None`); read-only flag only |
| Device / inode / link count | yes | yes | not captured; hard links behave as `Duplicate`, no boundary detection |
| Birth time | where the kernel and filesystem support it (`statx`) | yes | yes |
| Symlink races (`O_NOFOLLOW`, inode check) | yes | yes | not protected; the opened file is checked only to be a regular file |
| Special files | FIFO, socket, devices | FIFO, socket, devices | none; junctions and reparse points appear as links or `Other` |
| Case sensitivity of patterns | sensitive | sensitive (volume may be insensitive) | sensitive |

Other limitations:

* Names are reported as raw OS strings. Storing them in a snapshot must preserve arbitrary
  bytes, not only UTF-8.
* Directories with huge numbers of entries need memory for all their names at once.
* Hard-link detection keeps one small record per multiply linked file seen.
* Time stamps have the resolution of the filesystem (as low as 1 s on some); modification
  detection while reading inherits it, which is why size and ctime are compared as well.
* Network and FUSE filesystems may not provide stable inode numbers, or report sizes and times
  that do not reflect concurrent changes.
* A mount point is detected by device id only. Crossing into a bind mount is not detected.
* Running as root bypasses permission errors, so those paths are read rather than skipped.

## Verification status

The tests below pass on macOS with Rust 1.94.1 ([`RELIABILITY.md`](RELIABILITY.md)). Some of them
skip themselves where the platform cannot create the case (see below), and none has run on Linux.

The scanner is covered by integration tests on real directory trees: nested and scrambled
creation order, ordering checked against an independent walk, unusual names (spaces, newlines,
tabs, quotes, glob characters, 255-byte names, non-ASCII, non-UTF-8 where the filesystem allows),
empty files and directories, symbolic links in all three modes (including dangling, looping, and
to-ancestor links and links leaving the root), hard links, unreadable directories and files under
both error policies, paths vanishing mid-walk, files growing, shrinking, replaced, removed, and
swapped for links during reading, FIFOs and sockets, a device node, include/exclude patterns, and
streaming a 32 MiB file through the chunker. **Not verified**: real mount points (the boundary
decision is unit-tested with synthetic device ids only), Windows, and network or FUSE
filesystems.
