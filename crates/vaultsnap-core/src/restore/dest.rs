//! The file system operations of a restore.
//!
//! Everything here assumes the restore root is a real directory and that nothing below it may be
//! trusted: before a directory is used it is looked at without following symbolic links, a file is
//! created exclusively (`O_EXCL`, so an existing name, even a dangling link, makes it fail) and
//! without following a link at the last component, and a name is only ever replaced by a rename,
//! which replaces a link itself and never what it points to.
//!
//! The checks happen just before the operations, not atomically with them: this code uses no
//! `unsafe` and therefore no `openat`, so a local attacker who can change the target while the
//! restore runs could still win a race. `docs/RESTORE.md` says so.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, FileTimes, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::scan::Timestamp;
use crate::snapshot::Attributes;
use crate::util::random_hex;

/// Names of temporary files start with this, so leftovers of a killed restore are recognizable.
pub(super) const TEMP_PREFIX: &str = ".vaultsnap-tmp-";

/// How many times a random temporary name is retried before giving up.
const NAME_ATTEMPTS: usize = 8;

/// What exists at a path, looked at without following a symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Present {
    Directory,
    File,
    Symlink,
    Other,
}

impl Present {
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Directory => "a directory",
            Self::File => "a file",
            Self::Symlink => "a symbolic link",
            Self::Other => "a special file",
        }
    }
}

/// What is at `path`, if anything. A symbolic link is reported as a link, never followed.
pub(super) fn probe(path: &Path) -> Result<Option<Present>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            let kind = meta.file_type();
            Ok(Some(if kind.is_dir() {
                Present::Directory
            } else if kind.is_symlink() {
                Present::Symlink
            } else if kind.is_file() {
                Present::File
            } else {
                Present::Other
            }))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::io("inspect", path, err)),
    }
}

#[cfg(unix)]
fn private_file_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn private_file_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn directory_open_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
}

#[cfg(not(unix))]
fn directory_open_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn directory_builder(private: bool) -> DirBuilder {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = DirBuilder::new();
    builder.mode(if private { 0o700 } else { 0o755 });
    builder
}

#[cfg(not(unix))]
fn directory_builder(_private: bool) -> DirBuilder {
    DirBuilder::new()
}

#[cfg(unix)]
fn make_symlink(target: &[u8], link: &Path) -> io::Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    std::os::unix::fs::symlink(OsStr::from_bytes(target), link)
}

#[cfg(not(unix))]
fn make_symlink(_target: &[u8], _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links are not restored on this platform",
    ))
}

#[cfg(unix)]
fn set_mode(handle: &File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    handle.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_handle: &File, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// The outcome of putting something in place without replacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Published {
    Done,
    /// The name was taken (possibly a moment ago); nothing was changed.
    Exists,
}

/// A private temporary file in the directory its content is destined for. It is removed when
/// dropped, so an error, a cancellation, or a panic cannot leave it behind; a process that is
/// killed outright can, which the name makes recognizable.
pub(super) struct TempFile {
    pub(super) file: File,
    path: PathBuf,
    armed: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            if let Err(err) = fs::remove_file(&self.path) {
                let path = self.path.display();
                tracing::debug!(%path, error = %err, "could not remove a temporary file");
            }
        }
    }
}

/// Creates a temporary file in `dir`: new (a name that exists, even as a dangling link, is never
/// reused), not following links, readable and writable by the owner only.
pub(super) fn create_temp(dir: &Path) -> Result<TempFile> {
    for _ in 0..NAME_ATTEMPTS {
        let path = dir.join(format!("{TEMP_PREFIX}{}", random_hex(8)?));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        private_file_options(&mut options);
        match options.open(&path) {
            Ok(file) => {
                return Ok(TempFile {
                    file,
                    path,
                    armed: true,
                })
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(Error::io("create", &path, err)),
        }
    }
    Err(unused_name_error(dir))
}

fn unused_name_error(dir: &Path) -> Error {
    Error::io(
        "create",
        dir,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no unused temporary name was found",
        ),
    )
}

impl TempFile {
    /// Moves the content to `dest`. With `overwrite` the file is renamed over whatever is there
    /// (atomically; a link is replaced, not followed; a directory makes it fail). Without, the name
    /// is taken only if it is free: a hard link, which fails if the name exists, and where hard
    /// links are not supported a check followed by a rename.
    pub(super) fn publish(mut self, dest: &Path, overwrite: bool) -> Result<Published> {
        if overwrite {
            fs::rename(&self.path, dest).map_err(|err| Error::io("replace", dest, err))?;
            self.armed = false;
            return Ok(Published::Done);
        }
        match fs::hard_link(&self.path, dest) {
            // The temporary name is removed when `self` is dropped.
            Ok(()) => Ok(Published::Done),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(Published::Exists),
            Err(_) => {
                if probe(dest)?.is_some() {
                    return Ok(Published::Exists);
                }
                fs::rename(&self.path, dest).map_err(|err| Error::io("create", dest, err))?;
                self.armed = false;
                Ok(Published::Done)
            }
        }
    }
}

/// Creates something at an unused temporary name in `dir` with `create`, retrying other names.
fn create_at_temp_name(dir: &Path, create: impl Fn(&Path) -> io::Result<()>) -> Result<PathBuf> {
    for _ in 0..NAME_ATTEMPTS {
        let path = dir.join(format!("{TEMP_PREFIX}{}", random_hex(8)?));
        match create(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(Error::io("create", &path, err)),
        }
    }
    Err(unused_name_error(dir))
}

/// Renames `temp` over `dest`; if that fails, removes `temp`.
fn replace_with(temp: &Path, dest: &Path) -> Result<()> {
    if let Err(err) = fs::rename(temp, dest) {
        if let Err(cleanup) = fs::remove_file(temp) {
            let path = temp.display();
            tracing::debug!(%path, error = %cleanup, "could not remove a temporary link");
        }
        return Err(Error::io("replace", dest, err));
    }
    Ok(())
}

/// Creates a symbolic link `dest` (in `dir`) with the given target text. Without `overwrite`
/// an existing name is left alone and reported. With it the link is made under a temporary name
/// and renamed over the old entry, so what `dest` used to point to is never touched.
pub(super) fn create_symlink(
    target: &[u8],
    dir: &Path,
    dest: &Path,
    overwrite: bool,
) -> Result<Published> {
    if !overwrite {
        return match make_symlink(target, dest) {
            Ok(()) => Ok(Published::Done),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(Published::Exists),
            Err(err) => Err(Error::io("create symbolic link", dest, err)),
        };
    }
    let temp = create_at_temp_name(dir, |path| make_symlink(target, path))?;
    replace_with(&temp, dest)?;
    Ok(Published::Done)
}

/// Makes `dest` (in `dir`) another name for the file `existing`, with the same rules as
/// [`create_symlink`]. Fails if hard links are not possible, and the caller then writes a copy.
pub(super) fn create_hard_link(
    existing: &Path,
    dir: &Path,
    dest: &Path,
    overwrite: bool,
) -> Result<Published> {
    if !overwrite {
        return match fs::hard_link(existing, dest) {
            Ok(()) => Ok(Published::Done),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(Published::Exists),
            Err(err) => Err(Error::io("link", dest, err)),
        };
    }
    let temp = create_at_temp_name(dir, |path| fs::hard_link(existing, path))?;
    replace_with(&temp, dest)?;
    Ok(Published::Done)
}

/// A point in time as a `SystemTime`, or `None` if it cannot be represented.
pub(super) fn system_time(time: Timestamp) -> Option<SystemTime> {
    let nanos = Duration::new(0, time.nanos);
    if time.secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(time.secs.unsigned_abs(), time.nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(time.secs.unsigned_abs()))?
            .checked_add(nanos)
    }
}

/// Applies the recorded permissions and times to an open file or directory, through the open
/// handle so that nothing can have been swapped in the meantime. Set-user-id and set-group-id
/// bits are never restored (nor the sticky bit on files); the sticky bit is kept on directories.
/// Returns what could not be applied. Owners are not restored: that needs privileges, and doing it
/// by default would be surprising.
pub(super) fn apply_attributes(
    handle: &File,
    attributes: &Attributes,
    directory: bool,
) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(mode) = attributes.mode {
        let mask = if directory { 0o1777 } else { 0o777 };
        if let Err(err) = set_mode(handle, mode & mask) {
            problems.push(format!("the permissions could not be set: {err}"));
        }
    }
    let mut times = FileTimes::new();
    let mut any = false;
    if let Some(modified) = attributes.modified {
        match system_time(modified) {
            Some(time) => {
                times = times.set_modified(time);
                any = true;
            }
            None => problems.push("the modification time cannot be represented here".to_owned()),
        }
    }
    if let Some(time) = attributes.accessed.and_then(system_time) {
        times = times.set_accessed(time);
        any = true;
    }
    if any {
        if let Err(err) = handle.set_times(times) {
            problems.push(format!("the times could not be set: {err}"));
        }
    }
    problems
}

/// Applies the recorded attributes to the directory at `path`, which is opened without following a
/// link and must still be a directory.
pub(super) fn finalize_directory(path: &Path, attributes: &Attributes) -> Result<Vec<String>> {
    let mut options = OpenOptions::new();
    options.read(true);
    directory_open_options(&mut options);
    let handle = options
        .open(path)
        .map_err(|err| Error::io("open", path, err))?;
    let meta = handle
        .metadata()
        .map_err(|err| Error::io("inspect", path, err))?;
    if !meta.is_dir() {
        let changed = io::Error::other("it is no longer a directory");
        return Err(Error::io("open", path, changed));
    }
    Ok(apply_attributes(&handle, attributes, true))
}

/// The result of making sure a directory exists.
pub(super) enum Dir {
    Ready {
        path: PathBuf,
        /// This restore made the directory (as opposed to finding it).
        created: bool,
    },
    /// Something that is not a directory is in the way.
    Blocked { at: PathBuf, found: Present },
}

/// The restore root and the directories below it that are known to be real directories.
pub(super) struct Destination {
    root: PathBuf,
    verified: HashSet<PathBuf>,
    made: HashSet<PathBuf>,
}

impl Destination {
    /// `root` must be an existing directory with symbolic links resolved.
    pub(super) fn new(root: PathBuf) -> Self {
        let mut verified = HashSet::new();
        verified.insert(root.clone());
        Self {
            root,
            verified,
            made: HashSet::new(),
        }
    }

    pub(super) fn path_of(&self, rel: &[OsString]) -> PathBuf {
        let mut path = self.root.clone();
        path.extend(rel);
        path
    }

    /// Makes sure the directory `rel` exists below the root, creating what is missing, one
    /// component at a time and never through a link. Directories made for an entry of the
    /// snapshot (`entry_dir`, the last component) start private (`0700`); the attributes the
    /// snapshot records are applied when the restore is done. Others get the usual `0755`.
    ///
    /// Something that is not a directory in the way blocks the path. With `replace` a file, link,
    /// or special file is removed (a link itself, not what it points to) and a directory put in
    /// its place; a directory is never removed.
    pub(super) fn ensure_dir(
        &mut self,
        rel: &[OsString],
        entry_dir: bool,
        replace: bool,
    ) -> Result<Dir> {
        let mut current = self.root.clone();
        for (position, name) in rel.iter().enumerate() {
            current.push(name);
            let private = entry_dir && position + 1 == rel.len();
            if let Some(found) = self.ensure_one(&current, private, replace)? {
                return Ok(Dir::Blocked { at: current, found });
            }
        }
        let created = self.made.contains(&current);
        Ok(Dir::Ready {
            path: current,
            created,
        })
    }

    /// `None` when `path` is a directory afterwards, otherwise what blocks it.
    fn ensure_one(&mut self, path: &Path, private: bool, replace: bool) -> Result<Option<Present>> {
        if self.verified.contains(path) {
            return Ok(None);
        }
        let mut removed = false;
        for _ in 0..NAME_ATTEMPTS {
            match probe(path)? {
                Some(Present::Directory) => {
                    self.verified.insert(path.to_path_buf());
                    return Ok(None);
                }
                Some(found) => {
                    if !replace || removed {
                        return Ok(Some(found));
                    }
                    fs::remove_file(path).map_err(|err| Error::io("remove", path, err))?;
                    removed = true;
                }
                None => match directory_builder(private).create(path) {
                    Ok(()) => {
                        self.verified.insert(path.to_path_buf());
                        self.made.insert(path.to_path_buf());
                        return Ok(None);
                    }
                    // Someone else made it in the meantime: look again.
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(err) => return Err(Error::io("create directory", path, err)),
                },
            }
        }
        Err(Error::io(
            "create directory",
            path,
            io::Error::other("the path keeps changing"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use tempfile::TempDir;

    use super::*;

    fn rel(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    fn new_destination() -> (TempDir, Destination) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let root = fs::canonicalize(dir.path()).expect("canonical");
        (dir, Destination::new(root))
    }

    #[test]
    fn missing_directories_are_created_one_by_one_and_remembered() {
        let (_dir, mut dest) = new_destination();
        let Dir::Ready { path, created } = dest
            .ensure_dir(&rel(&["a", "b", "c"]), true, false)
            .expect("ok")
        else {
            panic!("expected a ready directory");
        };
        assert!(created && path.is_dir());
        assert_eq!(path, dest.path_of(&rel(&["a", "b", "c"])));

        // Asking again finds it (and still knows this restore made it).
        let Dir::Ready { created, .. } = dest
            .ensure_dir(&rel(&["a", "b", "c"]), true, false)
            .expect("ok")
        else {
            panic!("expected a ready directory");
        };
        assert!(created);
        let Dir::Ready { created, .. } =
            dest.ensure_dir(&rel(&["a", "b"]), true, false).expect("ok")
        else {
            panic!("expected a ready directory");
        };
        assert!(
            created,
            "intermediate directories were made by this restore too"
        );
    }

    #[test]
    fn the_root_itself_is_ready_and_not_created() {
        let (_dir, mut dest) = new_destination();
        let Dir::Ready { path, created } = dest.ensure_dir(&[], true, false).expect("ok") else {
            panic!("expected a ready directory");
        };
        assert!(!created);
        assert_eq!(path, dest.path_of(&[]));
    }

    #[cfg(unix)]
    #[test]
    fn entry_directories_start_private_and_others_do_not() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, mut dest) = new_destination();
        dest.ensure_dir(&rel(&["above", "entry"]), true, false)
            .expect("ok");
        let mode = |parts: &[&str]| {
            fs::metadata(dest.path_of(&rel(parts)))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&["above", "entry"]), 0o700);
        assert_ne!(mode(&["above"]) & 0o700, 0o000, "usable by the owner");
    }

    #[test]
    fn existing_directories_are_merged_and_not_reported_as_created() {
        let (_dir, mut dest) = new_destination();
        fs::create_dir_all(dest.path_of(&rel(&["x", "y"]))).expect("mkdir");
        let Dir::Ready { created, .. } =
            dest.ensure_dir(&rel(&["x", "y"]), true, false).expect("ok")
        else {
            panic!("expected a ready directory");
        };
        assert!(!created);
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_never_walked_through_even_if_it_points_inside() {
        let (_dir, mut dest) = new_destination();
        let real = dest.path_of(&rel(&["real"]));
        fs::create_dir(&real).expect("mkdir");
        std::os::unix::fs::symlink(&real, dest.path_of(&rel(&["link"]))).expect("symlink");

        let outcome = dest
            .ensure_dir(&rel(&["link", "child"]), true, false)
            .expect("ok");
        match outcome {
            Dir::Blocked { at, found } => {
                assert_eq!(at, dest.path_of(&rel(&["link"])));
                assert_eq!(found, Present::Symlink);
            }
            Dir::Ready { .. } => panic!("walked through a symbolic link"),
        }
        assert!(!real.join("child").exists());
    }

    #[test]
    fn a_file_in_the_way_blocks_unless_replacing_is_allowed() {
        let (_dir, mut dest) = new_destination();
        let file = dest.path_of(&rel(&["name"]));
        fs::write(&file, b"data").expect("write");

        let blocked = dest
            .ensure_dir(&rel(&["name", "sub"]), false, false)
            .expect("ok");
        assert!(matches!(
            blocked,
            Dir::Blocked {
                found: Present::File,
                ..
            }
        ));
        assert!(file.is_file(), "nothing was touched");

        let replaced = dest
            .ensure_dir(&rel(&["name", "sub"]), false, true)
            .expect("ok");
        assert!(matches!(replaced, Dir::Ready { .. }));
        assert!(file.is_dir() && file.join("sub").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_link_removes_the_link_and_leaves_its_target_alone() {
        let (dir, mut dest) = new_destination();
        let outside = dir.path().join("outside-target");
        fs::create_dir(&outside).expect("mkdir");
        fs::write(outside.join("precious"), b"keep me").expect("write");
        let link = dest.path_of(&rel(&["link"]));
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");

        let outcome = dest.ensure_dir(&rel(&["link"]), true, true).expect("ok");
        assert!(matches!(outcome, Dir::Ready { created: true, .. }));
        let meta = fs::symlink_metadata(&link).expect("metadata");
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert_eq!(
            fs::read(outside.join("precious")).expect("read"),
            b"keep me"
        );
        assert!(
            fs::read_dir(&outside).expect("read_dir").count() == 1,
            "nothing was added outside"
        );
    }

    #[test]
    fn a_directory_is_never_removed_to_make_way() {
        let (_dir, mut dest) = new_destination();
        // A directory where a directory is wanted is simply used.
        fs::create_dir_all(dest.path_of(&rel(&["d", "keep"]))).expect("mkdir");
        let outcome = dest.ensure_dir(&rel(&["d"]), true, true).expect("ok");
        assert!(matches!(outcome, Dir::Ready { created: false, .. }));
        assert!(dest.path_of(&rel(&["d", "keep"])).is_dir());
    }

    #[test]
    fn probing_tells_the_kinds_apart_without_following_links() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        assert_eq!(probe(dir.path()).expect("probe"), Some(Present::Directory));
        let file = dir.path().join("f");
        fs::write(&file, b"x").expect("write");
        assert_eq!(probe(&file).expect("probe"), Some(Present::File));
        assert_eq!(probe(&dir.path().join("missing")).expect("probe"), None);
        #[cfg(unix)]
        {
            let link = dir.path().join("l");
            std::os::unix::fs::symlink(dir.path(), &link).expect("symlink");
            assert_eq!(probe(&link).expect("probe"), Some(Present::Symlink));
            let dangling = dir.path().join("d");
            std::os::unix::fs::symlink("nowhere", &dangling).expect("symlink");
            assert_eq!(probe(&dangling).expect("probe"), Some(Present::Symlink));
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    #[test]
    fn temporary_files_are_private_and_unique() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("create temporary directory");
        let first = create_temp(dir.path()).expect("temp");
        let second = create_temp(dir.path()).expect("temp");
        assert_ne!(first.path, second.path);
        assert!(first
            .path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|n| n.starts_with(TEMP_PREFIX)));
        let mode = first
            .file
            .metadata()
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_temporary_file_that_is_dropped_leaves_nothing() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        {
            let _temp = create_temp(dir.path()).expect("temp");
            assert_eq!(names(dir.path()).len(), 1);
        }
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn publishing_without_overwrite_takes_a_free_name_and_never_replaces() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("create temporary directory");
        let dest = dir.path().join("final");

        let mut first = create_temp(dir.path()).expect("temp");
        first.file.write_all(b"first").expect("write");
        assert_eq!(
            first.publish(&dest, false).expect("publish"),
            Published::Done
        );
        assert_eq!(fs::read(&dest).expect("read"), b"first");
        assert_eq!(names(dir.path()), ["final"], "the temporary name is gone");

        let mut second = create_temp(dir.path()).expect("temp");
        second.file.write_all(b"second").expect("write");
        assert_eq!(
            second.publish(&dest, false).expect("publish"),
            Published::Exists
        );
        assert_eq!(
            fs::read(&dest).expect("read"),
            b"first",
            "the existing file is untouched"
        );
        assert_eq!(
            names(dir.path()),
            ["final"],
            "and the temporary file is removed"
        );
    }

    #[test]
    fn publishing_with_overwrite_replaces_in_one_step() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("create temporary directory");
        let dest = dir.path().join("final");
        fs::write(&dest, b"old").expect("write");
        let mut temp = create_temp(dir.path()).expect("temp");
        temp.file.write_all(b"new").expect("write");
        assert_eq!(temp.publish(&dest, true).expect("publish"), Published::Done);
        assert_eq!(fs::read(&dest).expect("read"), b"new");
        assert_eq!(names(dir.path()), ["final"]);
    }

    #[test]
    fn overwriting_a_directory_fails_and_cleans_up() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let dest = dir.path().join("final");
        fs::create_dir(&dest).expect("mkdir");
        let temp = create_temp(dir.path()).expect("temp");
        assert!(temp.publish(&dest, true).is_err());
        assert_eq!(names(dir.path()), ["final"], "no temporary file is left");
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_is_not_followed_when_a_temporary_name_is_chosen_or_published() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let outside = tempfile::tempdir().expect("create temporary directory");
        let victim = outside.path().join("victim");
        let dest = dir.path().join("final");
        std::os::unix::fs::symlink(&victim, &dest).expect("symlink");

        let temp = create_temp(dir.path()).expect("temp");
        assert_eq!(
            temp.publish(&dest, false).expect("publish"),
            Published::Exists
        );
        assert!(!victim.exists(), "nothing was created through the link");
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_replaces_a_symbolic_link_and_not_what_it_points_to() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("create temporary directory");
        let outside = tempfile::tempdir().expect("create temporary directory");
        let victim = outside.path().join("victim");
        fs::write(&victim, b"precious").expect("write");
        let dest = dir.path().join("final");
        std::os::unix::fs::symlink(&victim, &dest).expect("symlink");

        let mut temp = create_temp(dir.path()).expect("temp");
        temp.file.write_all(b"restored").expect("write");
        assert_eq!(temp.publish(&dest, true).expect("publish"), Published::Done);
        assert_eq!(fs::read(&victim).expect("read"), b"precious");
        let meta = fs::symlink_metadata(&dest).expect("metadata");
        assert!(meta.is_file() && !meta.file_type().is_symlink());
        assert_eq!(fs::read(&dest).expect("read"), b"restored");
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_created_exactly_as_recorded_and_never_replace_silently() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let dest = dir.path().join("link");
        assert_eq!(
            create_symlink(b"../x/y", dir.path(), &dest, false).expect("create"),
            Published::Done
        );
        assert_eq!(
            fs::read_link(&dest).expect("read_link"),
            Path::new("../x/y")
        );
        assert_eq!(
            create_symlink(b"other", dir.path(), &dest, false).expect("create"),
            Published::Exists
        );
        assert_eq!(
            fs::read_link(&dest).expect("read_link"),
            Path::new("../x/y")
        );
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_a_link_with_a_link_replaces_it_and_leaves_no_temporary_name() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let dest = dir.path().join("link");
        std::os::unix::fs::symlink("old", &dest).expect("symlink");
        assert_eq!(
            create_symlink(b"new", dir.path(), &dest, true).expect("create"),
            Published::Done
        );
        assert_eq!(fs::read_link(&dest).expect("read_link"), Path::new("new"));
        assert_eq!(names(dir.path()), ["link"]);

        // A directory is not replaced, and the failed attempt cleans up after itself.
        let directory = dir.path().join("dir");
        fs::create_dir(&directory).expect("mkdir");
        assert!(create_symlink(b"x", dir.path(), &directory, true).is_err());
        assert_eq!(names(dir.path()), ["dir", "link"]);
    }

    #[test]
    fn hard_links_share_content_and_obey_the_same_rules() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let original = dir.path().join("original");
        fs::write(&original, b"shared").expect("write");
        let alias = dir.path().join("alias");
        assert_eq!(
            create_hard_link(&original, dir.path(), &alias, false).expect("link"),
            Published::Done
        );
        assert_eq!(fs::read(&alias).expect("read"), b"shared");
        assert_eq!(
            create_hard_link(&original, dir.path(), &alias, false).expect("link"),
            Published::Exists
        );

        fs::write(&alias, b"changed through the link").expect("write");
        assert_eq!(
            fs::read(&original).expect("read"),
            b"changed through the link"
        );

        let other = dir.path().join("other");
        fs::write(&other, b"to be replaced").expect("write");
        assert_eq!(
            create_hard_link(&original, dir.path(), &other, true).expect("link"),
            Published::Done
        );
        assert_eq!(fs::read(&other).expect("read"), b"changed through the link");
        assert_eq!(names(dir.path()), ["alias", "original", "other"]);
    }

    #[test]
    fn times_before_and_after_the_epoch_are_converted() {
        let time = |secs, nanos| system_time(Timestamp { secs, nanos });
        assert_eq!(time(0, 0), Some(UNIX_EPOCH));
        assert_eq!(
            time(1_700_000_000, 5),
            UNIX_EPOCH.checked_add(Duration::new(1_700_000_000, 5))
        );
        assert_eq!(
            time(-1, 500_000_000),
            UNIX_EPOCH.checked_sub(Duration::from_millis(500))
        );
        assert_eq!(
            time(-10, 0),
            UNIX_EPOCH.checked_sub(Duration::from_secs(10))
        );
        // `SystemTime` holds a full `i64` of seconds on 64-bit systems, so the extreme times are
        // representable there; they must still be on the right side of the epoch, never wrapped.
        assert!(time(i64::MIN, 0).is_none_or(|t| t < UNIX_EPOCH));
        assert!(time(i64::MAX, 0).is_none_or(|t| t > UNIX_EPOCH));
        // A count of nanoseconds that carries past the largest second is out of range everywhere,
        // and is reported, not wrapped.
        assert_eq!(time(i64::MAX, 4_000_000_000), None);
    }

    fn attributes(mode: Option<u32>, modified: Option<Timestamp>) -> Attributes {
        Attributes {
            size: 0,
            modified,
            accessed: None,
            changed: None,
            created: None,
            mode,
            uid: None,
            gid: None,
            readonly: false,
            device: None,
            inode: None,
            links: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn permissions_and_times_are_applied_and_privilege_bits_are_not() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("create temporary directory");
        let path = dir.path().join("f");
        let file = File::create(&path).expect("create");
        let wanted = Timestamp {
            secs: 1_600_000_000,
            nanos: 0,
        };
        let problems = apply_attributes(&file, &attributes(Some(0o6755), Some(wanted)), false);
        assert!(problems.is_empty(), "{problems:?}");
        let meta = fs::metadata(&path).expect("metadata");
        assert_eq!(
            meta.permissions().mode() & 0o7777,
            0o755,
            "set-user-id and set-group-id are dropped"
        );
        let modified = meta
            .modified()
            .expect("modified")
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch");
        assert_eq!(modified.as_secs(), 1_600_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn directories_keep_the_sticky_bit_and_are_finalized_through_the_handle() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("create temporary directory");
        let path = dir.path().join("d");
        fs::create_dir(&path).expect("mkdir");
        let wanted = Timestamp {
            secs: 1_500_000_000,
            nanos: 0,
        };
        let problems =
            finalize_directory(&path, &attributes(Some(0o3770), Some(wanted))).expect("finalize");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o7777,
            0o1770
        );
        // Restore the permissions so the temporary directory can be removed.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod");
    }

    #[cfg(unix)]
    #[test]
    fn finalizing_refuses_a_link_that_replaced_the_directory() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let outside = tempfile::tempdir().expect("create temporary directory");
        let link = dir.path().join("d");
        std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
        let before = fs::metadata(outside.path())
            .expect("metadata")
            .modified()
            .expect("modified");

        let wanted = Timestamp {
            secs: 1_000_000_000,
            nanos: 0,
        };
        assert!(finalize_directory(&link, &attributes(Some(0o700), Some(wanted))).is_err());
        let after = fs::metadata(outside.path())
            .expect("metadata")
            .modified()
            .expect("modified");
        assert_eq!(
            before, after,
            "the directory the link points to was not touched"
        );
    }

    #[test]
    fn a_time_that_cannot_be_represented_is_reported_as_a_problem() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let file = File::create(dir.path().join("f")).expect("create");
        // No valid timestamp is unrepresentable on a 64-bit system, so this one carries its
        // nanoseconds past the largest second.
        let impossible = Timestamp {
            secs: i64::MAX,
            nanos: 4_000_000_000,
        };
        let problems = apply_attributes(&file, &attributes(None, Some(impossible)), false);
        assert_eq!(
            problems,
            ["the modification time cannot be represented here"]
        );
    }
}
