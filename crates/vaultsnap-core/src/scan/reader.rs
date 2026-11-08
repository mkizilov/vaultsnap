//! Streaming, race-aware access to the content of a scanned file.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::entry::{Entry, EntryKind, Metadata};
use super::options::ChangePolicy;
use super::walker::Scanner;
use crate::error::{Error, Result};

/// A reader over one scanned file. It never loads the file into memory: it is an ordinary
/// [`Read`], so it plugs straight into [`crate::chunking::Chunker`].
///
/// Opening is hardened against the file changing after the scan: the open refuses to follow a
/// symbolic link that replaced the file, and the opened file must be the very file that was
/// scanned (same device and inode on Unix) and a regular file. Reading is checked at the end:
/// when the end of the file is reached, its size and modification and inode-change times must
/// match what they were at open, and the number of bytes read must equal the size. Otherwise the
/// read fails (default) or the reader is marked [changed](FileReader::changed), depending on the
/// scanner's [`ChangePolicy`]. Data read before such a failure must be discarded.
pub struct FileReader {
    file: File,
    path: PathBuf,
    opened: Metadata,
    policy: ChangePolicy,
    bytes_read: u64,
    finished: bool,
    changed: bool,
}

impl FileReader {
    /// The attributes of the file as of the moment it was opened. If they differ from the scan's
    /// (the file changed in between), these are the ones to store.
    pub fn metadata(&self) -> &Metadata {
        &self.opened
    }

    /// How many bytes have been read so far.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Whether a change was detected at the end of the file (only observable with
    /// [`ChangePolicy::Accept`]; with `Fail` the read returned an error instead).
    pub fn changed(&self) -> bool {
        self.changed
    }

    fn verify(&mut self) -> io::Result<()> {
        let after = Metadata::from_fs(&self.file.metadata()?);
        let unchanged = self.bytes_read == self.opened.size
            && after.size == self.opened.size
            && after.modified == self.opened.modified
            && after.changed == self.opened.changed;
        if unchanged {
            return Ok(());
        }
        self.changed = true;
        match self.policy {
            ChangePolicy::Accept => Ok(()),
            ChangePolicy::Fail => Err(io::Error::other(Error::FileChanged {
                path: self.path.clone(),
                reason: "its size or modification time changed while it was being read"
                    .to_owned(),
            })),
        }
    }
}

impl Read for FileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read(buf)?;
        self.bytes_read += count as u64;
        if count == 0 && !buf.is_empty() && !self.finished {
            self.finished = true;
            self.verify()?;
        }
        Ok(count)
    }
}

impl fmt::Debug for FileReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileReader")
            .field("path", &self.path)
            .field("bytes_read", &self.bytes_read)
            .field("changed", &self.changed)
            .finish_non_exhaustive()
    }
}

pub(super) fn open(scanner: &Scanner, entry: &Entry) -> Result<FileReader> {
    if entry.kind != EntryKind::File {
        return Err(Error::InvalidArgument("only regular file entries can be opened"));
    }
    let path = if entry.path.as_os_str().is_empty() {
        scanner.root().to_path_buf()
    } else {
        scanner.root().join(&entry.path)
    };
    let changed = |reason: &str| Error::FileChanged {
        path: path.clone(),
        reason: reason.to_owned(),
    };

    let mut options = OpenOptions::new();
    options.read(true);
    configure(&mut options, entry.followed_symlink);
    let file = options
        .open(&path)
        .map_err(|err| open_error(&path, err, &changed))?;

    let fresh = file
        .metadata()
        .map_err(|err| Error::io("inspect", &path, err))?;
    if !fresh.is_file() {
        return Err(changed("it is no longer a regular file"));
    }
    let opened = Metadata::from_fs(&fresh);
    if let (Some(scanned), Some(now)) = (
        entry.metadata.device.zip(entry.metadata.inode),
        opened.device.zip(opened.inode),
    ) {
        if scanned != now {
            return Err(changed("it was replaced by a different file"));
        }
    }

    Ok(FileReader {
        file,
        path,
        opened,
        policy: scanner.options().changed_files,
        bytes_read: 0,
        finished: false,
        changed: false,
    })
}

fn open_error(path: &Path, err: io::Error, changed: &dyn Fn(&str) -> Error) -> Error {
    if is_symlink_refusal(&err) {
        changed("it was replaced by a symbolic link")
    } else if err.kind() == io::ErrorKind::NotFound {
        changed("it was removed")
    } else {
        Error::io("open", path, err)
    }
}

/// Never follow a symbolic link at the final component (unless the entry itself was reached by
/// following one), and never block opening something that is not a regular file any more.
#[cfg(unix)]
fn configure(options: &mut OpenOptions, followed_symlink: bool) {
    use std::os::unix::fs::OpenOptionsExt;

    let flags = if followed_symlink {
        libc::O_NONBLOCK
    } else {
        libc::O_NOFOLLOW | libc::O_NONBLOCK
    };
    options.custom_flags(flags);
}

#[cfg(not(unix))]
fn configure(_options: &mut OpenOptions, _followed_symlink: bool) {}

#[cfg(unix)]
fn is_symlink_refusal(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
fn is_symlink_refusal(_err: &io::Error) -> bool {
    false
}
