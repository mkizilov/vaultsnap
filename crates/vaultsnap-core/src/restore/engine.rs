//! The restore pipeline: plan, check, create, then fix up directories.
//!
//! Order of events:
//!
//! 1. The manifest is validated and planned. Every destination is computed and checked; any
//!    unsafe path ends the restore here, before the target is touched.
//! 2. The target is resolved: symbolic links in it are followed once, here, and it must not be
//!    inside the repository.
//! 3. With [`ConflictPolicy::Fail`], everything that would clash is found before the first write.
//! 4. Entries are created in stored order (a directory before its contents). A failure of one
//!    entry (a damaged chunk, a refused write) is recorded and the rest carry on; a cancellation or
//!    a conflict in the fail mode ends the restore.
//! 5. Directories get their permissions and times last, deepest first, because writing into a
//!    directory changes its modification time and a read-only directory cannot be written to.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::dest::{
    apply_attributes, create_hard_link, create_symlink, create_temp, finalize_directory, probe,
    Destination, Dir, Present, Published,
};
use super::plan::{plan, symlink_is_contained, Plan, PlannedEntry};
use super::{
    ConflictPolicy, RestoreEvent, RestoreOptions, RestoreProblem, RestoreReport, RestoreSymlinks,
};
use crate::chunk_store::ChunkStore;
use crate::error::{Error, Result};
use crate::snapshot::{Attributes, ChunkRef, Manifest, ManifestEntry, NodeKind};

/// Restores `manifest` as `options` say. The manifest must come from an authenticated snapshot;
/// it is validated again, and its paths are checked again, regardless. `repository_root` is the
/// directory the repository occupies on this machine, if it is a directory here; the target may
/// not be inside it.
pub(crate) fn run(
    manifest: &Manifest,
    chunks: &ChunkStore,
    repository_root: Option<&Path>,
    options: &RestoreOptions,
) -> Result<RestoreReport> {
    let started = Instant::now();
    if options.cancel.is_cancelled() {
        return Err(Error::RestoreCancelled);
    }
    manifest.validate()?;
    let plan = plan(manifest, &options.includes)?;
    let target = resolve_target(&options.target, repository_root)?;

    if options.on_conflict == ConflictPolicy::Fail {
        preflight(&plan, &target.path, target.exists)?;
    }
    if !target.exists {
        fs::create_dir_all(&target.path)
            .map_err(|err| Error::io("create directory", &target.path, err))?;
    }
    let root = fs::canonicalize(&target.path)
        .map_err(|err| Error::io("resolve", &target.path, err))?;

    let mut restorer = Restorer {
        manifest,
        chunks,
        options,
        dest: Destination::new(root.clone()),
        report: RestoreReport::new(manifest.id.clone(), root),
        restored: HashMap::new(),
        directories: Vec::new(),
    };
    restorer.execute(&plan)?;
    let mut report = restorer.report;
    report.duration = started.elapsed();
    Ok(report)
}

struct Target {
    path: PathBuf,
    exists: bool,
}

/// Finds the directory to restore into. Symbolic links in the part that exists are resolved (the
/// user named this path, so they are trusted); the part that does not exist yet must be plain
/// names. The result may not be inside the repository.
fn resolve_target(target: &Path, repository_root: Option<&Path>) -> Result<Target> {
    let invalid = |reason: &str| Error::InvalidTarget(reason.to_owned());
    if target.as_os_str().is_empty() {
        return Err(invalid("the path is empty"));
    }
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| Error::io("inspect", Path::new("."), err))?
            .join(target)
    };

    let mut existing = PathBuf::new();
    let mut missing: Vec<OsString> = Vec::new();
    for component in absolute.components() {
        if missing.is_empty() {
            let candidate = existing.join(component.as_os_str());
            match fs::metadata(&candidate) {
                Ok(_) => existing = candidate,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    missing.push(component.as_os_str().to_os_string());
                }
                Err(err) => return Err(Error::io("inspect", &candidate, err)),
            }
        } else {
            missing.push(component.as_os_str().to_os_string());
        }
    }
    if missing.iter().any(|name| name == OsStr::new("..") || name == OsStr::new(".")) {
        return Err(invalid("a part of the path that does not exist yet contains `.` or `..`"));
    }

    let base = fs::canonicalize(&existing).map_err(|err| Error::io("resolve", &existing, err))?;
    let is_directory = fs::metadata(&base)
        .map_err(|err| Error::io("inspect", &base, err))?
        .is_dir();
    if !is_directory {
        return Err(invalid("it is not a directory (or lies below something that is not)"));
    }
    let mut path = base;
    path.extend(&missing);

    if let Some(repository_root) = repository_root {
        let repository = fs::canonicalize(repository_root)
            .map_err(|err| Error::io("resolve", repository_root, err))?;
        if path.starts_with(&repository) {
            return Err(invalid("it is inside the repository"));
        }
    }
    Ok(Target {
        path,
        exists: missing.is_empty(),
    })
}

/// The first path that already exists in a way that clashes with restoring `rel`: any
/// non-directory along the way, or anything at all where a non-directory is to go.
fn find_conflict(
    root: &Path,
    rel: &[OsString],
    is_directory: bool,
    known: &mut HashSet<PathBuf>,
) -> Result<Option<PathBuf>> {
    let mut current = root.to_path_buf();
    for (position, name) in rel.iter().enumerate() {
        current.push(name);
        let last = position + 1 == rel.len();
        if !known.contains(&current) {
            match probe(&current)? {
                None => return Ok(None),
                Some(Present::Directory) => {
                    known.insert(current.clone());
                }
                Some(_) => return Ok(Some(current)),
            }
        }
        if last && !is_directory {
            return Ok(Some(current));
        }
    }
    Ok(None)
}

/// With [`ConflictPolicy::Fail`]: finds every clash, with the target and inside the snapshot
/// itself, before anything is written.
fn preflight(plan: &Plan<'_>, root: &Path, inspect_disk: bool) -> Result<()> {
    let mut known: HashSet<PathBuf> = HashSet::new();
    let mut planned: HashMap<&[OsString], bool> = HashMap::new();
    let mut count = 0_usize;
    let mut first: Option<PathBuf> = None;
    for item in &plan.entries {
        let is_directory = matches!(item.entry.kind, NodeKind::Directory);
        let clash = match planned.insert(item.rel.as_slice(), is_directory) {
            Some(previous) => !(previous && is_directory),
            None => false,
        };
        let conflict = if clash {
            Some(item.rel.iter().fold(root.to_path_buf(), |path, part| path.join(part)))
        } else if inspect_disk {
            find_conflict(root, &item.rel, is_directory, &mut known)?
        } else {
            None
        };
        if let Some(path) = conflict {
            count += 1;
            first.get_or_insert(path);
        }
    }
    match first {
        Some(first) => Err(Error::RestoreConflict { count, first }),
        None => Ok(()),
    }
}

struct Restorer<'a> {
    manifest: &'a Manifest,
    chunks: &'a ChunkStore,
    options: &'a RestoreOptions,
    dest: Destination,
    report: RestoreReport,
    /// Where regular files of this run are, by (root, entry), for hard links to link to.
    restored: HashMap<(usize, usize), PathBuf>,
    /// Directories to finish at the end, in the order they were made.
    directories: Vec<(PathBuf, &'a Attributes)>,
}

impl<'a> Restorer<'a> {
    fn overwrite(&self) -> bool {
        self.options.on_conflict == ConflictPolicy::Overwrite
    }

    fn observe(&self, event: &RestoreEvent<'_>) {
        if let Some(observer) = &self.options.observer {
            observer.event(event);
        }
    }

    fn cancelled(&self) -> bool {
        self.options.cancel.is_cancelled()
    }

    fn execute(&mut self, plan: &Plan<'a>) -> Result<()> {
        self.observe(&RestoreEvent::Planned {
            files: plan.files,
            bytes: plan.bytes,
        });
        let outcome = self.create_entries(plan);
        // Also after a cancellation or a conflict: directories made so far must not stay private
        // and without their times, because a later run that skips what exists would not fix them.
        self.finish_directories();
        outcome
    }

    fn create_entries(&mut self, plan: &Plan<'a>) -> Result<()> {
        for planned in &plan.entries {
            if self.cancelled() {
                return Err(Error::RestoreCancelled);
            }
            match self.entry(planned) {
                Ok(()) => {}
                Err(err @ (Error::RestoreCancelled | Error::RestoreConflict { .. })) => {
                    return Err(err)
                }
                Err(err) => self.fail(planned, &err),
            }
        }
        Ok(())
    }

    fn skip(&mut self, planned: &PlannedEntry<'a>, reason: String) {
        let path = self.dest.path_of(&planned.rel);
        self.report.skipped.push(RestoreProblem { path, reason });
    }

    fn fail(&mut self, planned: &PlannedEntry<'a>, err: &Error) {
        let path = self.dest.path_of(&planned.rel);
        tracing::debug!(path = %path.display(), error = %err, "could not restore an entry");
        self.report.failed.push(RestoreProblem {
            path,
            reason: err.to_string(),
        });
    }

    fn entry(&mut self, planned: &PlannedEntry<'a>) -> Result<()> {
        let entry: &'a ManifestEntry = planned.entry;
        match &entry.kind {
            NodeKind::Directory => self.directory(planned),
            NodeKind::File { chunks } => self.file(planned, chunks),
            NodeKind::Symlink { target } => self.symlink(planned, target),
            NodeKind::HardLink { target } => self.hard_link(planned, *target),
            _ => {
                let reason = "special files (FIFOs, sockets, devices) are not restored";
                self.skip(planned, reason.to_owned());
                Ok(())
            }
        }
    }

    /// Something that is not a directory is in the way of `planned`.
    fn blocked(&mut self, planned: &PlannedEntry<'a>, at: &Path, found: Present) -> Result<()> {
        if self.options.on_conflict == ConflictPolicy::Skip {
            let reason = format!("{} is in the way at {}", found.describe(), at.display());
            self.skip(planned, reason);
            Ok(())
        } else {
            Err(Error::RestoreConflict {
                count: 1,
                first: at.to_path_buf(),
            })
        }
    }

    /// The name was taken after it had been looked at.
    fn taken(&mut self, planned: &PlannedEntry<'a>, dest: &Path) -> Result<()> {
        if self.options.on_conflict == ConflictPolicy::Skip {
            self.skip(planned, "it already exists".to_owned());
            Ok(())
        } else {
            Err(Error::RestoreConflict {
                count: 1,
                first: dest.to_path_buf(),
            })
        }
    }

    fn directory(&mut self, planned: &PlannedEntry<'a>) -> Result<()> {
        let entry: &'a ManifestEntry = planned.entry;
        let overwrite = self.overwrite();
        match self.dest.ensure_dir(&planned.rel, true, overwrite)? {
            Dir::Ready { path, created } => {
                // The attributes of a directory that was already there are only touched when the
                // user asked for things to be overwritten, and never those of the restore root.
                if (created || overwrite) && !planned.rel.is_empty() {
                    self.directories.push((path.clone(), &entry.attributes));
                }
                self.report.directories += 1;
                self.observe(&RestoreEvent::EntryRestored { path: &path });
                Ok(())
            }
            Dir::Blocked { at, found } => self.blocked(planned, &at, found),
        }
    }

    /// Makes the directory that will hold `planned` and checks what is at its name. `None` if the
    /// entry was skipped.
    fn prepare(&mut self, planned: &PlannedEntry<'a>) -> Result<Option<(PathBuf, PathBuf)>> {
        let overwrite = self.overwrite();
        let Some((name, parent)) = planned.rel.split_last() else {
            return Err(Error::InvalidManifest("an entry has no name".to_owned()));
        };
        let dir = match self.dest.ensure_dir(parent, false, overwrite)? {
            Dir::Ready { path, .. } => path,
            Dir::Blocked { at, found } => {
                return self.blocked(planned, &at, found).map(|()| None);
            }
        };
        let dest = dir.join(name);
        if let Some(found) = probe(&dest)? {
            match self.options.on_conflict {
                ConflictPolicy::Overwrite => {
                    if found == Present::Directory {
                        return Err(Error::DirectoryInTheWay { path: dest });
                    }
                }
                ConflictPolicy::Skip => {
                    self.skip(planned, format!("{} already exists", found.describe()));
                    return Ok(None);
                }
                ConflictPolicy::Fail => {
                    return Err(Error::RestoreConflict {
                        count: 1,
                        first: dest,
                    });
                }
            }
        }
        Ok(Some((dir, dest)))
    }

    fn file(&mut self, planned: &PlannedEntry<'a>, list: &[ChunkRef]) -> Result<()> {
        let Some((dir, dest)) = self.prepare(planned)? else {
            return Ok(());
        };
        let size = planned.entry.attributes.size;
        if self.write_file(planned, list, size, &dir, &dest)? {
            self.restored.insert((planned.root, planned.index), dest);
        }
        Ok(())
    }

    /// Writes the content of `list` to a temporary file next to `dest`, authenticating every
    /// chunk, gives it the recorded attributes, and moves it into place. `false` if the name was
    /// taken in the meantime and the entry was skipped.
    fn write_file(
        &mut self,
        planned: &PlannedEntry<'a>,
        list: &[ChunkRef],
        size: u64,
        dir: &Path,
        dest: &Path,
    ) -> Result<bool> {
        let mut temp = create_temp(dir)?;
        let mut written = 0_u64;
        for chunk in list {
            if self.cancelled() {
                return Err(Error::RestoreCancelled);
            }
            // Authenticated, decrypted, and checked against its id before a byte is used.
            let data = self.chunks.get(&chunk.id)?;
            if u64::try_from(data.len()).ok() != Some(u64::from(chunk.len)) {
                return Err(Error::CorruptObject {
                    id: chunk.id.to_hex(),
                    reason: "its length differs from the length the snapshot records".to_owned(),
                });
            }
            temp.file
                .write_all(&data)
                .map_err(|err| Error::io("write", dest, err))?;
            let length = u64::from(chunk.len);
            written += length;
            self.observe(&RestoreEvent::BytesWritten { bytes: length });
        }
        if written != size {
            return Err(Error::InvalidManifest(
                "the chunks of a file do not add up to its size".to_owned(),
            ));
        }
        let problems = apply_attributes(&temp.file, &planned.entry.attributes, false);
        match temp.publish(dest, self.overwrite())? {
            Published::Done => {}
            Published::Exists => {
                self.taken(planned, dest)?;
                return Ok(false);
            }
        }
        self.report.files += 1;
        self.report.bytes += written;
        for reason in problems {
            self.report.warnings.push(RestoreProblem {
                path: dest.to_path_buf(),
                reason,
            });
        }
        self.observe(&RestoreEvent::EntryRestored { path: dest });
        Ok(true)
    }

    fn symlink(&mut self, planned: &PlannedEntry<'a>, target: &[u8]) -> Result<()> {
        match self.options.symlinks {
            RestoreSymlinks::Skip => {
                self.skip(planned, "symbolic links are not restored (--symlinks skip)".to_owned());
                return Ok(());
            }
            RestoreSymlinks::Contained
                if !symlink_is_contained(target, planned.rel.len().saturating_sub(1)) =>
            {
                let reason = "its target leads, or could lead, out of the restore root; \
                              restoring it needs --symlinks as-recorded";
                self.skip(planned, reason.to_owned());
                return Ok(());
            }
            RestoreSymlinks::Contained | RestoreSymlinks::AsRecorded => {}
        }
        let Some((dir, dest)) = self.prepare(planned)? else {
            return Ok(());
        };
        match create_symlink(target, &dir, &dest, self.overwrite())? {
            Published::Done => {
                self.report.symlinks += 1;
                self.observe(&RestoreEvent::EntryRestored { path: &dest });
                Ok(())
            }
            Published::Exists => self.taken(planned, &dest),
        }
    }

    /// A hard link becomes a hard link to the file restored for its target in this run. If there is
    /// none (the target was not selected, or was skipped) or links cannot be made, the content
    /// is written as a file of its own.
    fn hard_link(&mut self, planned: &PlannedEntry<'a>, target: u32) -> Result<()> {
        let manifest: &'a Manifest = self.manifest;
        let target_index = usize::try_from(target).unwrap_or(usize::MAX);
        let target_entry = manifest.roots[planned.root].entries.get(target_index);
        let Some(ManifestEntry {
            kind: NodeKind::File { chunks: list },
            attributes: target_attributes,
            ..
        }) = target_entry
        else {
            return Err(Error::InvalidManifest(
                "a hard link does not name a regular file".to_owned(),
            ));
        };
        let Some((dir, dest)) = self.prepare(planned)? else {
            return Ok(());
        };
        let key = (planned.root, target_index);
        if let Some(existing) = self.restored.get(&key).cloned() {
            match create_hard_link(&existing, &dir, &dest, self.overwrite()) {
                Ok(Published::Done) => {
                    self.report.hard_links += 1;
                    self.observe(&RestoreEvent::EntryRestored { path: &dest });
                    return Ok(());
                }
                Ok(Published::Exists) => return self.taken(planned, &dest),
                Err(err) => {
                    tracing::debug!(error = %err, "could not make a hard link; writing a copy");
                }
            }
        }
        if self.write_file(planned, list, target_attributes.size, &dir, &dest)? {
            self.restored.entry(key).or_insert(dest);
        }
        Ok(())
    }

    /// Gives the directories their recorded attributes, the deepest first.
    fn finish_directories(&mut self) {
        let directories = std::mem::take(&mut self.directories);
        for (path, attributes) in directories.into_iter().rev() {
            match finalize_directory(&path, attributes) {
                Ok(problems) => {
                    for reason in problems {
                        self.report.warnings.push(RestoreProblem {
                            path: path.clone(),
                            reason,
                        });
                    }
                }
                Err(err) => self.report.warnings.push(RestoreProblem {
                    path,
                    reason: format!("its attributes could not be applied: {err}"),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests;
