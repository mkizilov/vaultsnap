//! Deciding what a restore will create, and where, before anything is written.
//!
//! The plan is pure: it looks only at the manifest and the selection. Every destination is a list
//! of components below the restore root, and every component is checked here, again, even though
//! manifest validation already refuses the same names. A restore must stay safe if the manifest
//! rules ever change, and the plan is the one place that decides where files may go.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::history::entries;
use crate::snapshot::{is_valid_name, Manifest, ManifestEntry, NodeKind};

/// One thing to create.
pub(super) struct PlannedEntry<'m> {
    /// Index of the root in the manifest.
    pub root: usize,
    /// Index of the entry in the root.
    pub index: usize,
    /// The manifest entry.
    pub entry: &'m ManifestEntry,
    /// The destination: the components below the restore root, the source path of the root
    /// first, then the path below it. Never empty for anything but a directory.
    pub rel: Vec<OsString>,
}

pub(super) struct Plan<'m> {
    /// Everything to create, roots in order and each root's entries in stored order, which puts
    /// a directory before its contents.
    pub entries: Vec<PlannedEntry<'m>>,
    /// Selected regular files (and hard links, which may have to be written as files).
    pub files: u64,
    /// Their total size.
    pub bytes: u64,
}

fn unsafe_path(location: String, reason: &'static str) -> Error {
    Error::UnsafePath { location, reason }
}

/// Characters that are special on the platforms where more than `/` separates paths.
#[cfg(unix)]
fn platform_safe(_name: &[u8]) -> bool {
    true
}

#[cfg(not(unix))]
fn platform_safe(name: &[u8]) -> bool {
    !name.iter().any(|&byte| byte == b'\\' || byte == b':')
}

#[cfg(unix)]
fn to_os_string(bytes: &[u8]) -> Option<OsString> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    Some(OsStr::from_bytes(bytes).to_os_string())
}

#[cfg(not(unix))]
fn to_os_string(bytes: &[u8]) -> Option<OsString> {
    std::str::from_utf8(bytes).ok().map(OsString::from)
}

/// A single path component that is safe to create: not empty, not `.` or `..`, no `/` or NUL, of
/// bounded length, and nothing the platform would treat as a separator.
pub(super) fn safe_component(name: &[u8]) -> Option<OsString> {
    if is_valid_name(name) && platform_safe(name) {
        to_os_string(name)
    } else {
        None
    }
}

/// The components of a root's source path: the absolute path it had when it was backed up, which
/// becomes a relative path below the restore root.
fn source_components(source: &[u8], root: usize) -> Result<Vec<OsString>> {
    source
        .split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty())
        .map(|part| {
            safe_component(part).ok_or_else(|| {
                unsafe_path(
                    format!("root {root}"),
                    "the source path has a component that is not a safe name",
                )
            })
        })
        .collect()
}

/// Selects the entries to restore (everything, or what `includes` names with all that lies below),
/// adds the directories above them, and computes every destination.
pub(super) fn plan<'m>(manifest: &'m Manifest, includes: &[PathBuf]) -> Result<Plan<'m>> {
    let mut selected: Vec<Vec<bool>> = manifest
        .roots
        .iter()
        .map(|root| vec![includes.is_empty(); root.entries.len()])
        .collect();
    for include in includes {
        let mut matched = false;
        // An empty path would otherwise be a prefix of everything.
        let candidates = entries(manifest, Some(include.as_path()))
            .filter(|_| !include.as_os_str().is_empty());
        for listed in candidates {
            selected[listed.root][listed.index] = true;
            matched = true;
        }
        if !matched {
            return Err(Error::IncludeNotFound {
                path: include.clone(),
            });
        }
    }

    let mut planned = Vec::new();
    let (mut files, mut bytes) = (0_u64, 0_u64);
    for (root_index, root) in manifest.roots.iter().enumerate() {
        let sources = source_components(&root.source, root_index)?;
        let total = root.entries.len();

        // Parents come first (checked below), so one pass from the end marks every ancestor of
        // an entry that is wanted.
        let mut needed = selected[root_index].clone();
        for index in (1..total).rev() {
            if needed[index] {
                if let Some(parent) = root.entries[index].parent {
                    let parent = usize::try_from(parent).unwrap_or(usize::MAX);
                    if parent < index {
                        needed[parent] = true;
                    }
                }
            }
        }

        let mut rels: Vec<Vec<OsString>> = vec![Vec::new(); total];
        for (index, entry) in root.entries.iter().enumerate() {
            let location = || format!("root {root_index}, entry {index}");
            if index == 0 {
                if entry.parent.is_some() || !entry.name.is_empty() {
                    return Err(unsafe_path(location(), "entry 0 must be the unnamed root entry"));
                }
                rels[0] = sources.clone();
            } else {
                let parent = entry
                    .parent
                    .and_then(|parent| usize::try_from(parent).ok())
                    .filter(|&parent| parent < index)
                    .ok_or_else(|| {
                        unsafe_path(location(), "the parent of an entry must be an earlier entry")
                    })?;
                let name = safe_component(&entry.name).ok_or_else(|| {
                    unsafe_path(location(), "the name is not a safe path component")
                })?;
                let mut rel = rels[parent].clone();
                rel.push(name);
                rels[index] = rel;
            }
            if !needed[index] {
                continue;
            }
            if index == 0 && !matches!(entry.kind, NodeKind::Directory) && sources.is_empty() {
                return Err(unsafe_path(location(), "the entry would replace the restore root"));
            }
            if selected[root_index][index]
                && matches!(entry.kind, NodeKind::File { .. } | NodeKind::HardLink { .. })
            {
                files += 1;
                bytes = bytes.saturating_add(entry.attributes.size);
            }
            planned.push(PlannedEntry {
                root: root_index,
                index,
                entry,
                // Cloned, not taken: the children of this entry build their paths from it.
                rel: rels[index].clone(),
            });
        }
    }
    Ok(Plan {
        entries: planned,
        files,
        bytes,
    })
}

/// Whether a symbolic link with this target, in a directory `depth` levels below the restore
/// root, can only ever lead to somewhere inside the restore root.
///
/// The target must be relative and non-empty. It may go up (`..`) only at the start, and at most
/// `depth` times; after the first name it may only go down. Because a `..` never follows a name,
/// links that point at other links cannot add up to an escape: every link resolves, step by step,
/// from a real directory that is itself below the root, and a link's own directory is always a
/// real directory (a restore never creates a directory through a link).
pub(super) fn symlink_is_contained(target: &[u8], depth: usize) -> bool {
    if target.is_empty() || target.contains(&0) || target.starts_with(b"/") {
        return false;
    }
    let mut ups = 0_usize;
    let mut descending = false;
    for part in target.split(|&byte| byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                if descending {
                    return false;
                }
                ups += 1;
            }
            _ => descending = true,
        }
    }
    ups <= depth
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::testing::{attributes, chunk, dir, file, minimal, rich};
    use crate::snapshot::{ManifestEntry, Root};

    fn rels(plan: &Plan<'_>) -> Vec<String> {
        plan.entries
            .iter()
            .map(|planned| {
                let parts: Vec<String> =
                    planned.rel.iter().map(|part| part.to_string_lossy().into_owned()).collect();
                parts.join("/")
            })
            .collect()
    }

    /// A manifest with one root `source` and the given entries after the root directory.
    fn manifest_with(source: &[u8], rest: Vec<ManifestEntry>) -> Manifest {
        let mut manifest = minimal();
        let mut entries = vec![dir(None, b"")];
        entries.extend(rest);
        manifest.roots = vec![Root {
            source: source.to_vec(),
            entries,
        }];
        manifest
    }

    fn unsafe_reason(result: Result<Plan<'_>>) -> &'static str {
        match result {
            Err(Error::UnsafePath { reason, .. }) => reason,
            Err(other) => panic!("expected UnsafePath, got {other}"),
            Ok(_) => panic!("expected the plan to be refused"),
        }
    }

    #[test]
    fn the_whole_snapshot_is_planned_below_the_source_paths_in_stored_order() {
        let manifest = rich();
        let plan = plan(&manifest, &[]).expect("plan");
        let planned = rels(&plan);
        assert_eq!(planned.len(), 13);
        assert_eq!(planned[0], "home/me/Documents");
        assert_eq!(planned[1], "home/me/Documents/docs");
        assert_eq!(planned[2], "home/me/Documents/docs/report.txt");
        assert_eq!(planned[12], "home/me/notes.txt");
        assert_eq!(plan.files, 5, "three files and the hard link of root 0, one file of root 1");
        assert_eq!(plan.bytes, 1500 + 1500 + 1000 + 77);
    }

    #[test]
    fn selecting_a_path_adds_the_directories_above_it() {
        let manifest = rich();
        let include = [PathBuf::from("/home/me/Documents/docs/report.txt")];
        let plan = plan(&manifest, &include).expect("plan");
        assert_eq!(
            rels(&plan),
            [
                "home/me/Documents",
                "home/me/Documents/docs",
                "home/me/Documents/docs/report.txt"
            ]
        );
        assert_eq!(
            (plan.files, plan.bytes),
            (1, 1500),
            "only the file itself was asked for, not the directories above it"
        );
    }

    #[test]
    fn a_selected_directory_brings_everything_below_it_and_several_paths_combine() {
        let manifest = rich();
        let include = [
            PathBuf::from("/home/me/Documents/docs"),
            PathBuf::from("/home/me/notes.txt"),
        ];
        let plan = plan(&manifest, &include).expect("plan");
        let planned = rels(&plan);
        assert_eq!(planned.len(), 6, "{planned:?}");
        assert!(planned.contains(&"home/me/notes.txt".to_owned()));
        assert!(planned.contains(&"home/me/Documents/docs/alias".to_owned()));
        assert!(!planned.contains(&"home/me/Documents/link".to_owned()));
    }

    #[test]
    fn an_include_that_matches_nothing_is_an_error() {
        let manifest = rich();
        for include in ["/nowhere", "/home/me/Doc", "relative/path", ""] {
            let result = plan(&manifest, &[PathBuf::from(include)]);
            assert!(matches!(result, Err(Error::IncludeNotFound { .. })), "{include:?}");
        }
    }

    #[test]
    fn names_that_could_climb_or_split_a_path_are_refused() {
        let bad: [&[u8]; 6] = [b"..", b".", b"", b"a/b", b"nul\0byte", b"/"];
        for name in bad {
            let manifest = manifest_with(b"/data", vec![file(0, name, vec![chunk(1, 4)])]);
            let reason = unsafe_reason(plan(&manifest, &[]));
            assert_eq!(reason, "the name is not a safe path component", "{name:?}");
        }
        let long = vec![b'x'; 5000];
        let manifest = manifest_with(b"/data", vec![file(0, &long, vec![chunk(1, 4)])]);
        assert!(matches!(plan(&manifest, &[]), Err(Error::UnsafePath { .. })));
    }

    #[test]
    fn source_paths_that_could_climb_are_refused() {
        let bad: [&[u8]; 5] = [b"/a/../../etc", b"/..", b"/a/./b", b"..", b"/a\0b"];
        for source in bad {
            let manifest = manifest_with(source, vec![]);
            let reason = unsafe_reason(plan(&manifest, &[]));
            assert_eq!(reason, "the source path has a component that is not a safe name");
        }
    }

    #[cfg(unix)]
    #[test]
    fn odd_but_harmless_source_paths_are_accepted() {
        let cases: [(&[u8], &str); 4] = [
            (b"/", ""),
            (b"//a//b/", "a/b"),
            (b"/with space/\xff\xfe", "with space/\u{fffd}\u{fffd}"),
            (b"relative/path", "relative/path"),
        ];
        for (source, expected) in cases {
            let manifest = manifest_with(source, vec![]);
            let plan = plan(&manifest, &[]).expect("plan");
            assert_eq!(rels(&plan), [expected]);
        }
    }

    #[test]
    fn a_broken_tree_shape_is_refused() {
        // A parent that comes later, a parent that is itself, and a root entry that has a name.
        let mut forward = manifest_with(b"/data", vec![file(2, b"a", vec![]), dir(Some(0), b"b")]);
        forward.roots[0].entries[1].parent = Some(2);
        assert!(matches!(plan(&forward, &[]), Err(Error::UnsafePath { .. })));

        let mut itself = manifest_with(b"/data", vec![dir(Some(0), b"a")]);
        itself.roots[0].entries[1].parent = Some(1);
        assert!(matches!(plan(&itself, &[]), Err(Error::UnsafePath { .. })));

        let mut orphan = manifest_with(b"/data", vec![dir(Some(0), b"a")]);
        orphan.roots[0].entries[1].parent = None;
        assert!(matches!(plan(&orphan, &[]), Err(Error::UnsafePath { .. })));

        let mut named = manifest_with(b"/data", vec![]);
        named.roots[0].entries[0].name = b"x".to_vec();
        assert!(matches!(plan(&named, &[]), Err(Error::UnsafePath { .. })));
    }

    #[test]
    fn a_file_cannot_take_the_place_of_the_restore_root() {
        let mut manifest = manifest_with(b"/", vec![]);
        manifest.roots[0].entries[0].kind = NodeKind::File { chunks: Vec::new() };
        manifest.roots[0].entries[0].attributes = attributes(0);
        let reason = unsafe_reason(plan(&manifest, &[]));
        assert_eq!(reason, "the entry would replace the restore root");
    }

    #[test]
    fn errors_name_entries_by_index_not_by_name() {
        let manifest = manifest_with(b"/data", vec![file(0, b"..", vec![])]);
        let message = plan(&manifest, &[]).err().expect("refused").to_string();
        assert!(message.contains("root 0, entry 1"), "{message}");
        assert!(!message.contains("data"), "{message}");
    }

    #[test]
    fn contained_links_stay_inside_whatever_they_point_at() {
        // (target, depth of the directory the link lives in below the root, contained?)
        let cases: [(&[u8], usize, bool); 22] = [
            (b"file.txt", 0, true),
            (b"sub/file.txt", 0, true),
            (b"./sub/./file", 0, true),
            (b".", 0, true),
            (b"..", 1, true),
            (b"../..", 2, true),
            (b"../sibling/file", 1, true),
            (b"..", 0, false),
            (b"../..", 1, false),
            (b"/etc/passwd", 3, false),
            (b"/", 5, false),
            (b"", 2, false),
            (b"a/../..", 5, false),
            (b"a/..", 5, false),
            (b"sub/../../x", 9, false),
            (b"../a/..", 5, false),
            (b"a\0b", 1, false),
            (b"//absolute", 1, false),
            (b"dir/", 0, true),
            (b"a//b", 0, true),
            (b"...", 0, true),
            (b"../.hidden", 1, true),
        ];
        for (target, depth, expected) in cases {
            assert_eq!(
                symlink_is_contained(target, depth),
                expected,
                "{:?} at depth {depth}",
                String::from_utf8_lossy(target)
            );
        }
    }
}
