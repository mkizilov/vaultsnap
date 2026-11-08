//! What the scanner reports about each path.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// A point in time as seconds and nanoseconds since the Unix epoch. Seconds are negative before
/// 1970; `nanos` is always below one billion and counts forward from `secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    /// Whole seconds since the epoch.
    pub secs: i64,
    /// Nanoseconds after `secs`, `0..1_000_000_000`.
    pub nanos: u32,
}

impl Timestamp {
    /// Builds a timestamp, carrying out-of-range nanoseconds into the seconds.
    pub fn new(secs: i64, nanos: i64) -> Self {
        let carry = nanos.div_euclid(1_000_000_000);
        Self {
            secs: secs.saturating_add(carry),
            nanos: nanos.rem_euclid(1_000_000_000) as u32,
        }
    }

    /// The current time.
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// Converts a system time, including times before the epoch.
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self {
                secs: i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
                nanos: after.subsec_nanos(),
            },
            Err(err) => {
                let before = err.duration();
                Self::new(
                    -i64::try_from(before.as_secs()).unwrap_or(i64::MAX),
                    -i64::from(before.subsec_nanos()),
                )
            }
        }
    }
}

/// File attributes captured at scan time. Fields a platform cannot provide are `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// Size in bytes (for a symbolic link, the length of its target text).
    pub size: u64,
    /// Last modification of the content.
    pub modified: Option<Timestamp>,
    /// Last access. Changes whenever the file is read; do not use it to detect changes.
    pub accessed: Option<Timestamp>,
    /// Last change of the inode (permissions, ownership, links, content): Unix `ctime`.
    pub changed: Option<Timestamp>,
    /// Creation ("birth") time, where the platform and filesystem record it.
    pub created: Option<Timestamp>,
    /// Unix permission bits including setuid, setgid, and sticky (`0o7777`).
    pub mode: Option<u32>,
    /// Owning user id (Unix).
    pub uid: Option<u32>,
    /// Owning group id (Unix).
    pub gid: Option<u32>,
    /// Whether the platform considers the entry read-only.
    pub readonly: bool,
    /// Id of the filesystem the entry is on (Unix `st_dev`).
    pub device: Option<u64>,
    /// Inode number (Unix).
    pub inode: Option<u64>,
    /// Number of hard links (Unix).
    pub links: Option<u64>,
}

impl Metadata {
    #[cfg(unix)]
    pub(crate) fn from_fs(meta: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            size: meta.len(),
            modified: Some(Timestamp::new(meta.mtime(), meta.mtime_nsec())),
            accessed: Some(Timestamp::new(meta.atime(), meta.atime_nsec())),
            changed: Some(Timestamp::new(meta.ctime(), meta.ctime_nsec())),
            created: meta.created().ok().map(Timestamp::from_system_time),
            mode: Some(meta.mode() & 0o7777),
            uid: Some(meta.uid()),
            gid: Some(meta.gid()),
            readonly: meta.permissions().readonly(),
            device: Some(meta.dev()),
            inode: Some(meta.ino()),
            links: Some(meta.nlink()),
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn from_fs(meta: &fs::Metadata) -> Self {
        Self {
            size: meta.len(),
            modified: meta.modified().ok().map(Timestamp::from_system_time),
            accessed: meta.accessed().ok().map(Timestamp::from_system_time),
            changed: None,
            created: meta.created().ok().map(Timestamp::from_system_time),
            mode: None,
            uid: None,
            gid: None,
            readonly: meta.permissions().readonly(),
            device: None,
            inode: None,
            links: None,
        }
    }
}

/// The type of a scanned path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// A directory. Its children follow it in the scan.
    Directory,
    /// A regular file, whose content can be read with `Scanner::open`.
    File,
    /// A symbolic link, with the target text exactly as stored (not resolved or checked).
    Symlink {
        /// What the link points to.
        target: PathBuf,
    },
    /// Another path of a file already reported (hard-link policy `Preserve`). `target` is the
    /// scan-root-relative path of the entry that carries the data.
    HardLink {
        /// Path of the entry that holds the file's data.
        target: PathBuf,
    },
    /// A named pipe.
    Fifo,
    /// A Unix domain socket.
    Socket,
    /// A character device node.
    CharDevice,
    /// A block device node.
    BlockDevice,
    /// Anything else the platform exposes.
    Other,
}

/// One path found by the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the scan root, in the platform's native encoding (on Unix arbitrary
    /// bytes, not necessarily UTF-8). Empty for the root itself.
    pub path: PathBuf,
    /// What it is.
    pub kind: EntryKind,
    /// Its attributes.
    pub metadata: Metadata,
    /// True if this entry was reached by following a symbolic link, in which case `kind` and
    /// `metadata` describe the link's target.
    pub followed_symlink: bool,
}

impl Entry {
    /// True for directories.
    pub fn is_directory(&self) -> bool {
        self.kind == EntryKind::Directory
    }

    /// True for regular files whose data can be read.
    pub fn is_file(&self) -> bool {
        self.kind == EntryKind::File
    }
}

/// Why a path is not part of the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The path disappeared between listing its directory and inspecting it.
    Vanished,
    /// An I/O error, typically "permission denied", prevented reading it.
    Unreadable {
        /// What was being attempted.
        operation: &'static str,
        /// The kind of error.
        kind: io::ErrorKind,
        /// The operating system's message.
        message: String,
    },
    /// A directory on a different filesystem, not entered because crossing is off.
    OtherFilesystem,
    /// A special file, skipped by policy.
    SpecialFile(EntryKind),
}

/// A path that was found but is not in the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Path relative to the scan root.
    pub path: PathBuf,
    /// Why.
    pub reason: SkipReason,
}

/// One item of the scan stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanItem {
    /// A recorded path.
    Entry(Entry),
    /// A path that was deliberately or unavoidably left out.
    Skipped(Skipped),
}

/// Running totals of a scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Entries reported (all kinds).
    pub entries: u64,
    /// Of which directories.
    pub directories: u64,
    /// Of which regular files carrying data.
    pub files: u64,
    /// Of which symbolic links.
    pub symlinks: u64,
    /// Of which hard links to an already reported file.
    pub hard_links: u64,
    /// Of which FIFOs, sockets, devices, and other special files.
    pub special_files: u64,
    /// Total size of the regular files carrying data, in bytes.
    pub bytes: u64,
    /// Paths reported as skipped.
    pub skipped: u64,
    /// Paths omitted by an exclude pattern (a directory counts once, not its contents).
    pub excluded: u64,
    /// Non-directory paths omitted because they matched no include pattern.
    pub not_included: u64,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn timestamps_normalize_nanoseconds() {
        assert_eq!(
            Timestamp::new(1, 1_500_000_000),
            Timestamp {
                secs: 2,
                nanos: 500_000_000
            }
        );
        assert_eq!(
            Timestamp::new(0, -1),
            Timestamp {
                secs: -1,
                nanos: 999_999_999
            }
        );
        assert_eq!(Timestamp::new(5, 0), Timestamp { secs: 5, nanos: 0 });
    }

    #[test]
    fn timestamps_convert_from_system_time() {
        let at = Timestamp::from_system_time;
        assert_eq!(at(UNIX_EPOCH), Timestamp { secs: 0, nanos: 0 });
        let later = UNIX_EPOCH + Duration::new(1_000, 250);
        assert_eq!(
            at(later),
            Timestamp {
                secs: 1_000,
                nanos: 250
            }
        );
        let earlier = UNIX_EPOCH - Duration::from_millis(500);
        assert_eq!(
            at(earlier),
            Timestamp {
                secs: -1,
                nanos: 500_000_000
            }
        );
        let whole = UNIX_EPOCH - Duration::from_secs(10);
        assert_eq!(
            at(whole),
            Timestamp {
                secs: -10,
                nanos: 0
            }
        );
    }

    #[test]
    fn timestamps_order_chronologically() {
        assert!(Timestamp::new(-1, 999_999_999) < Timestamp::new(0, 0));
        assert!(Timestamp::new(3, 1) > Timestamp::new(3, 0));
    }
}
