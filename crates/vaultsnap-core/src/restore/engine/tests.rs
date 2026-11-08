//! Engine tests: hand-built manifests over a real chunk store, so that attacks a real backup can
//! never produce (hostile names, links that are already in the target, damaged chunks, a restore
//! that is cancelled halfway) can be expressed. Restoring real backups is tested end to end in
//! `tests/restore.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tempfile::TempDir;

use super::*;
use crate::chunking::test_util::pseudo_random;
use crate::keys::RepositoryKeys;
use crate::snapshot::testing::{attributes, dir as fixture_dir, file as fixture_file, minimal};
use crate::snapshot::Root;
use crate::store::Store;

struct Fx {
    dir: TempDir,
    chunks: ChunkStore,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).expect("mkdir");
        Store::local(&repo).create_dir("data").expect("mkdir");
        let keys = RepositoryKeys::generate().expect("generate");
        let chunks = ChunkStore::new(Store::local(&repo), &keys).expect("chunk store");
        Self { dir, chunks }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn target(&self) -> PathBuf {
        self.dir.path().join("out")
    }

    /// Where `/data/<rel>` of the sample snapshots is restored.
    fn restored(&self, rel: &str) -> PathBuf {
        self.target().join("data").join(rel)
    }

    fn put(&self, data: &[u8]) -> ChunkRef {
        let put = self.chunks.put(data).expect("put");
        ChunkRef {
            id: put.id,
            len: u32::try_from(data.len()).expect("small"),
        }
    }

    fn options(&self) -> RestoreOptions {
        RestoreOptions::new("abcd".parse().expect("valid reference"), self.target())
    }

    fn run(&self, manifest: &Manifest, options: &RestoreOptions) -> Result<RestoreReport> {
        run(manifest, &self.chunks, Some(self.repo().as_path()), options)
    }

    fn object_path(&self, chunk: &ChunkRef) -> PathBuf {
        self.repo().join(chunk.id.object_name())
    }
}

fn directory(parent: Option<u32>, name: &str) -> ManifestEntry {
    let mut entry = fixture_dir(parent, name.as_bytes());
    entry.attributes.mode = Some(0o755);
    entry
}

struct Tree<'a> {
    fx: &'a Fx,
    entries: Vec<ManifestEntry>,
}

impl<'a> Tree<'a> {
    fn new(fx: &'a Fx) -> Self {
        Self {
            fx,
            entries: vec![directory(None, "")],
        }
    }

    fn push(&mut self, entry: ManifestEntry) -> u32 {
        self.entries.push(entry);
        u32::try_from(self.entries.len() - 1).expect("small")
    }

    fn dir(&mut self, parent: u32, name: &str) -> u32 {
        self.push(directory(Some(parent), name))
    }

    /// A file stored in chunks of at most 1000 bytes.
    fn file(&mut self, parent: u32, name: &str, data: &[u8]) -> u32 {
        let chunks: Vec<ChunkRef> = data.chunks(1000).map(|part| self.fx.put(part)).collect();
        self.push(fixture_file(parent, name.as_bytes(), chunks))
    }

    fn symlink(&mut self, parent: u32, name: &str, target: &str) -> u32 {
        self.push(ManifestEntry {
            parent: Some(parent),
            name: name.as_bytes().to_vec(),
            kind: NodeKind::Symlink {
                target: target.as_bytes().to_vec(),
            },
            attributes: attributes(target.len() as u64),
        })
    }

    fn hard_link(&mut self, parent: u32, name: &str, target: u32) -> u32 {
        let size = self.entries[target as usize].attributes.size;
        self.push(ManifestEntry {
            parent: Some(parent),
            name: name.as_bytes().to_vec(),
            kind: NodeKind::HardLink { target },
            attributes: attributes(size),
        })
    }

    fn manifest(self, source: &str) -> Manifest {
        let mut manifest = minimal();
        manifest.roots = vec![Root {
            source: source.as_bytes().to_vec(),
            entries: self.entries,
        }];
        manifest.stats = manifest.compute_stats();
        manifest
    }
}

const BIG: u64 = 2500;

/// `/data` with `docs/big.bin` (3 chunks), `docs/empty`, `readme.txt`, and a link to it.
fn sample(fx: &Fx) -> Manifest {
    let mut tree = Tree::new(fx);
    let docs = tree.dir(0, "docs");
    tree.file(docs, "big.bin", &pseudo_random(BIG as usize, 1));
    tree.file(docs, "empty", b"");
    tree.file(0, "readme.txt", b"hello");
    tree.symlink(0, "link", "readme.txt");
    tree.manifest("/data")
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every file name below `dir`, to find leftovers.
fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).expect("read_dir") {
            let path = entry.expect("entry").path();
            let is_dir = fs::symlink_metadata(&path).expect("metadata").is_dir();
            if is_dir {
                pending.push(path.clone());
            }
            found.push(path);
        }
    }
    found.sort();
    found
}

fn no_temporary_files(dir: &Path) {
    let leftovers: Vec<_> = all_files(dir)
        .into_iter()
        .filter(|path| path.to_string_lossy().contains(".vaultsnap-tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// ---------------------------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------------------------

#[test]
fn a_snapshot_is_rebuilt_byte_for_byte_from_its_chunks() {
    let fx = Fx::new();
    let manifest = sample(&fx);

    let report = fx.run(&manifest, &fx.options()).expect("restore");
    assert!(report.is_complete(), "{report:?}");
    assert!(report.skipped.is_empty() && report.warnings.is_empty(), "{report:?}");
    assert_eq!((report.files, report.directories, report.symlinks, report.hard_links), (3, 2, 1, 0));
    assert_eq!(report.bytes, BIG + 5);
    assert_eq!(report.snapshot, manifest.id);
    assert_eq!(report.target, fs::canonicalize(fx.target()).expect("canonical"));

    assert_eq!(fs::read(fx.restored("docs/big.bin")).expect("read"), pseudo_random(BIG as usize, 1));
    assert_eq!(fs::read(fx.restored("docs/empty")).expect("read"), b"");
    assert_eq!(fs::read(fx.restored("readme.txt")).expect("read"), b"hello");
    #[cfg(unix)]
    assert_eq!(fs::read_link(fx.restored("link")).expect("link"), Path::new("readme.txt"));
    no_temporary_files(&fx.target());
}

#[cfg(unix)]
#[test]
fn permissions_and_times_are_restored_and_privilege_bits_are_not() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fx::new();
    let mut tree = Tree::new(&fx);
    let secret = tree.file(0, "secret", b"x");
    let setuid = tree.file(0, "setuid", b"y");
    let docs = tree.dir(0, "docs");
    let inner = tree.file(docs, "inner", b"z");
    tree.entries[secret as usize].attributes.mode = Some(0o600);
    tree.entries[setuid as usize].attributes.mode = Some(0o6755);
    tree.entries[docs as usize].attributes.mode = Some(0o750);
    tree.entries[inner as usize].attributes.mode = Some(0o444);
    let manifest = tree.manifest("/data");

    let report = fx.run(&manifest, &fx.options()).expect("restore");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let mode = |rel: &str| fs::metadata(fx.restored(rel)).expect("metadata").permissions().mode() & 0o7777;
    assert_eq!(mode("secret"), 0o600);
    assert_eq!(mode("setuid"), 0o755, "set-user-id and set-group-id are never restored");
    assert_eq!(mode("docs"), 0o750);
    assert_eq!(mode("docs/inner"), 0o444);
    assert_eq!(mode(""), 0o755, "the directory of the source itself");

    let seconds = |rel: &str| {
        let modified = fs::metadata(fx.restored(rel)).expect("metadata").modified().expect("mtime");
        modified.duration_since(std::time::UNIX_EPOCH).expect("after the epoch").as_secs()
    };
    for rel in ["secret", "setuid", "docs/inner", "docs", ""] {
        assert_eq!(seconds(rel), 1_700_000_000, "{rel}");
    }
}

/// Makes a directory writable again when dropped, so the temporary directory can be removed
/// even if a test fails.
#[cfg(unix)]
struct Unlock(PathBuf);

#[cfg(unix)]
impl Drop for Unlock {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        if let Err(err) = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755)) {
            eprintln!("could not unlock {}: {err}", self.0.display());
        }
    }
}

#[cfg(unix)]
#[test]
fn a_directory_without_write_permission_is_filled_before_it_is_locked() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fx::new();
    let _unlock = Unlock(fx.restored("locked"));
    let mut tree = Tree::new(&fx);
    let locked = tree.dir(0, "locked");
    tree.file(locked, "inside", b"content");
    tree.entries[locked as usize].attributes.mode = Some(0o555);
    let manifest = tree.manifest("/data");

    let report = fx.run(&manifest, &fx.options()).expect("restore");
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(fs::read(fx.restored("locked/inside")).expect("read"), b"content");
    let meta = fs::metadata(fx.restored("locked")).expect("metadata");
    assert_eq!(meta.permissions().mode() & 0o777, 0o555);
    // The directory keeps the time of the snapshot although a file was written into it.
    let secs = meta.modified().expect("mtime").duration_since(std::time::UNIX_EPOCH).expect("epoch");
    assert_eq!(secs.as_secs(), 1_700_000_000);
}

// ---------------------------------------------------------------------------------------------
// Malicious manifests
// ---------------------------------------------------------------------------------------------

#[test]
fn hostile_entry_names_are_refused_before_anything_is_written() {
    let fx = Fx::new();
    let hostile: [&[u8]; 7] = [b"..", b".", b"", b"a/b", b"x\0y", b"../../escape", b"/etc/passwd"];
    for name in hostile {
        let mut tree = Tree::new(&fx);
        tree.file(0, "fine.txt", b"fine");
        let chunk = fx.put(b"evil");
        tree.push(fixture_file(0, name, vec![chunk]));
        let manifest = tree.manifest("/data");

        let err = fx.run(&manifest, &fx.options()).expect_err("must be refused");
        assert!(
            matches!(err, Error::InvalidManifest(_) | Error::UnsafePath { .. }),
            "{name:?}: {err}"
        );
        assert!(!fx.target().exists(), "{name:?}: the target was created");
    }
}

#[test]
fn hostile_source_paths_are_refused_even_though_the_manifest_rules_allow_them() {
    let fx = Fx::new();
    for source in ["/data/../../escape", "/..", "..", "/a/./b", "/with\0nul"] {
        let mut tree = Tree::new(&fx);
        tree.file(0, "f", b"data");
        let manifest = tree.manifest(source);
        let result = fx.run(&manifest, &fx.options());
        assert!(
            matches!(result, Err(Error::UnsafePath { .. } | Error::InvalidManifest(_))),
            "{source:?}: {result:?}"
        );
        assert!(!fx.target().exists(), "{source:?}: the target was created");
        assert!(names(fx.dir.path()).iter().all(|name| name == "repo"), "nothing appeared next to it");
    }
}

#[test]
fn a_manifest_with_a_broken_tree_shape_is_refused() {
    let fx = Fx::new();
    let mut tree = Tree::new(&fx);
    tree.file(0, "a", b"a");
    let mut manifest = tree.manifest("/data");
    manifest.roots[0].entries[1].parent = Some(1);
    let err = fx.run(&manifest, &fx.options()).expect_err("must be refused");
    assert!(matches!(err, Error::InvalidManifest(_) | Error::UnsafePath { .. }), "{err}");
    assert!(!fx.target().exists());
}

#[test]
fn an_unknown_include_restores_nothing_and_creates_nothing() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    let mut options = fx.options();
    options.includes = vec![PathBuf::from("/data/docs/big.bin"), PathBuf::from("/data/missing")];
    let err = fx.run(&manifest, &options).expect_err("must fail");
    assert!(matches!(err, Error::IncludeNotFound { .. }), "{err}");
    assert!(!fx.target().exists());
}

// ---------------------------------------------------------------------------------------------
// Selected paths
// ---------------------------------------------------------------------------------------------

#[test]
fn selected_paths_are_restored_with_the_directories_above_them() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    let mut options = fx.options();
    options.includes = vec![PathBuf::from("/data/docs/big.bin")];

    let report = fx.run(&manifest, &options).expect("restore");
    assert_eq!((report.files, report.directories, report.symlinks), (1, 2, 0));
    assert_eq!(fs::read(fx.restored("docs/big.bin")).expect("read"), pseudo_random(BIG as usize, 1));
    assert_eq!(names(&fx.restored("docs")), ["big.bin"]);
    assert_eq!(names(&fx.restored("")), ["docs"], "the rest of the snapshot is not restored");
}

#[test]
fn a_selected_directory_brings_all_that_is_below_it() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    let mut options = fx.options();
    options.includes = vec![PathBuf::from("/data/docs/")];
    let report = fx.run(&manifest, &options).expect("restore");
    assert_eq!((report.files, report.directories), (2, 2));
    assert_eq!(names(&fx.restored("docs")), ["big.bin", "empty"]);
}

// ---------------------------------------------------------------------------------------------
// Conflicts
// ---------------------------------------------------------------------------------------------

fn pre_existing(fx: &Fx) {
    fs::create_dir_all(fx.restored("docs")).expect("mkdir");
    fs::write(fx.restored("readme.txt"), b"mine").expect("write");
    fs::write(fx.restored("docs/keep.txt"), b"keep").expect("write");
}

#[test]
fn fail_mode_restores_nothing_if_anything_would_be_overwritten() {
    let fx = Fx::new();
    pre_existing(&fx);
    let manifest = sample(&fx);

    let err = fx.run(&manifest, &fx.options()).expect_err("conflict");
    match err {
        Error::RestoreConflict { count, first } => {
            // The directories merge; only readme.txt is in the way.
            assert_eq!(count, 1);
            let root = fs::canonicalize(fx.restored("")).expect("canonical");
            assert_eq!(first, root.join("readme.txt"));
        }
        other => panic!("expected a conflict, got {other}"),
    }
    assert_eq!(fs::read(fx.restored("readme.txt")).expect("read"), b"mine");
    assert!(!fx.restored("docs/big.bin").exists(), "nothing at all was written");
    assert!(!fx.restored("link").exists());
    assert_eq!(names(&fx.restored("docs")), ["keep.txt"]);
}

#[test]
fn skip_mode_leaves_what_exists_and_restores_the_rest() {
    let fx = Fx::new();
    pre_existing(&fx);
    let manifest = sample(&fx);
    let mut options = fx.options();
    options.on_conflict = ConflictPolicy::Skip;

    let report = fx.run(&manifest, &options).expect("restore");
    assert!(report.is_complete(), "skipping is not failing: {report:?}");
    assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
    assert!(report.skipped[0].path.ends_with("readme.txt"));
    assert!(report.skipped[0].reason.contains("already exists"));
    assert_eq!(fs::read(fx.restored("readme.txt")).expect("read"), b"mine");
    assert_eq!(fs::read(fx.restored("docs/keep.txt")).expect("read"), b"keep");
    assert_eq!(fs::read(fx.restored("docs/big.bin")).expect("read"), pseudo_random(BIG as usize, 1));
    assert_eq!((report.files, report.symlinks), (2, 1), "big.bin and empty, and the link");
}

#[test]
fn overwrite_mode_replaces_files_and_merges_directories() {
    let fx = Fx::new();
    pre_existing(&fx);
    let manifest = sample(&fx);
    let mut options = fx.options();
    options.on_conflict = ConflictPolicy::Overwrite;

    let report = fx.run(&manifest, &options).expect("restore");
    assert!(report.is_complete() && report.skipped.is_empty(), "{report:?}");
    assert_eq!(fs::read(fx.restored("readme.txt")).expect("read"), b"hello");
    assert_eq!(fs::read(fx.restored("docs/keep.txt")).expect("read"), b"keep", "merged, not wiped");
    no_temporary_files(&fx.target());
}

#[test]
fn a_directory_where_a_file_belongs_is_never_replaced() {
    for policy in [ConflictPolicy::Fail, ConflictPolicy::Skip, ConflictPolicy::Overwrite] {
        let fx = Fx::new();
        fs::create_dir_all(fx.restored("readme.txt")).expect("mkdir");
        fs::write(fx.restored("readme.txt/precious"), b"keep").expect("write");
        let manifest = sample(&fx);
        let mut options = fx.options();
        options.on_conflict = policy;

        let result = fx.run(&manifest, &options);
        assert_eq!(fs::read(fx.restored("readme.txt/precious")).expect("read"), b"keep", "{policy:?}");
        match policy {
            ConflictPolicy::Fail => assert!(matches!(result, Err(Error::RestoreConflict { .. }))),
            ConflictPolicy::Skip => {
                let report = result.expect("restore");
                assert_eq!(report.skipped.len(), 1);
                assert!(report.is_complete());
            }
            ConflictPolicy::Overwrite => {
                let report = result.expect("restore");
                assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
                assert!(report.failed[0].reason.contains("never replaces"), "{:?}", report.failed);
                assert!(!report.is_complete());
                assert_eq!(fs::read(fx.restored("docs/big.bin")).expect("read").len(), BIG as usize);
            }
        }
    }
}

#[test]
fn a_file_where_a_directory_belongs_blocks_it_and_everything_below() {
    for policy in [ConflictPolicy::Fail, ConflictPolicy::Skip, ConflictPolicy::Overwrite] {
        let fx = Fx::new();
        fs::create_dir_all(fx.restored("")).expect("mkdir");
        fs::write(fx.restored("docs"), b"a file named like a directory").expect("write");
        let manifest = sample(&fx);
        let mut options = fx.options();
        options.on_conflict = policy;

        let result = fx.run(&manifest, &options);
        match policy {
            ConflictPolicy::Fail => {
                assert!(matches!(result, Err(Error::RestoreConflict { .. })));
                assert!(fx.restored("docs").is_file(), "untouched");
                assert!(!fx.restored("readme.txt").exists(), "nothing was written");
            }
            ConflictPolicy::Skip => {
                let report = result.expect("restore");
                assert!(fx.restored("docs").is_file(), "untouched");
                assert_eq!(report.skipped.len(), 3, "docs, and what is below it: {:?}", report.skipped);
                assert_eq!(fs::read(fx.restored("readme.txt")).expect("read"), b"hello");
            }
            ConflictPolicy::Overwrite => {
                let report = result.expect("restore");
                assert!(report.is_complete(), "{report:?}");
                assert!(fx.restored("docs").is_dir());
                assert_eq!(fs::read(fx.restored("docs/big.bin")).expect("read").len(), BIG as usize);
            }
        }
    }
}

#[test]
fn two_entries_for_the_same_place_clash_in_fail_mode_and_the_last_wins_when_overwriting() {
    let fx = Fx::new();
    let mut first = Tree::new(&fx);
    first.file(0, "same", b"first version");
    let mut second = Tree::new(&fx);
    second.file(0, "same", b"second version");
    let mut manifest = first.manifest("/data");
    manifest.roots.push(second.manifest("/data").roots.remove(0));
    manifest.stats = manifest.compute_stats();

    let err = fx.run(&manifest, &fx.options()).expect_err("clash");
    assert!(matches!(err, Error::RestoreConflict { count: 1, .. }), "{err}");
    assert!(!fx.target().exists(), "nothing was written");

    let mut options = fx.options();
    options.on_conflict = ConflictPolicy::Overwrite;
    let report = fx.run(&manifest, &options).expect("restore");
    assert!(report.is_complete());
    assert_eq!(fs::read(fx.restored("same")).expect("read"), b"second version");

    let skip_target = fx.dir.path().join("skip-out");
    let mut options = RestoreOptions::new("abcd".parse().expect("valid"), &skip_target);
    options.on_conflict = ConflictPolicy::Skip;
    let report = fx.run(&manifest, &options).expect("restore");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(fs::read(skip_target.join("data/same")).expect("read"), b"first version");
}

// ---------------------------------------------------------------------------------------------
// Things that are already in the target: links must never be followed
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
mod hostile_target {
    use super::*;

    fn outside(fx: &Fx) -> PathBuf {
        let outside = fx.dir.path().join("outside");
        fs::create_dir(&outside).expect("mkdir");
        fs::write(outside.join("sentinel"), b"untouched").expect("write");
        outside
    }

    #[test]
    fn a_link_that_is_already_where_the_source_directory_goes_is_not_followed() {
        for policy in [ConflictPolicy::Fail, ConflictPolicy::Skip, ConflictPolicy::Overwrite] {
            let fx = Fx::new();
            let outside = outside(&fx);
            fs::create_dir_all(fx.target()).expect("mkdir");
            std::os::unix::fs::symlink(&outside, fx.target().join("data")).expect("symlink");
            let manifest = sample(&fx);
            let mut options = fx.options();
            options.on_conflict = policy;

            let result = fx.run(&manifest, &options);
            assert_eq!(names(&outside), ["sentinel"], "{policy:?}: something was written through the link");
            assert_eq!(fs::read(outside.join("sentinel")).expect("read"), b"untouched");
            match policy {
                ConflictPolicy::Fail => assert!(matches!(result, Err(Error::RestoreConflict { .. }))),
                ConflictPolicy::Skip => {
                    let report = result.expect("restore");
                    assert_eq!(report.files, 0);
                    assert_eq!(report.skipped.len(), 6, "{:?}", report.skipped);
                    assert!(fs::symlink_metadata(fx.target().join("data")).expect("metadata").file_type().is_symlink());
                }
                ConflictPolicy::Overwrite => {
                    let report = result.expect("restore");
                    assert!(report.is_complete(), "{report:?}");
                    let meta = fs::symlink_metadata(fx.target().join("data")).expect("metadata");
                    assert!(meta.is_dir() && !meta.file_type().is_symlink(), "the link was replaced");
                    assert_eq!(fs::read(fx.restored("readme.txt")).expect("read"), b"hello");
                }
            }
        }
    }

    #[test]
    fn a_link_in_the_middle_of_a_destination_path_is_not_followed() {
        for policy in [ConflictPolicy::Fail, ConflictPolicy::Skip, ConflictPolicy::Overwrite] {
            let fx = Fx::new();
            let outside = outside(&fx);
            fs::create_dir_all(fx.restored("")).expect("mkdir");
            std::os::unix::fs::symlink(&outside, fx.restored("docs")).expect("symlink");
            let manifest = sample(&fx);
            let mut options = fx.options();
            options.on_conflict = policy;

            let result = fx.run(&manifest, &options);
            assert_eq!(names(&outside), ["sentinel"], "{policy:?}");
            match policy {
                ConflictPolicy::Fail => assert!(matches!(result, Err(Error::RestoreConflict { .. }))),
                ConflictPolicy::Skip => {
                    let report = result.expect("restore");
                    assert_eq!(report.skipped.len(), 3, "docs, big.bin, empty: {:?}", report.skipped);
                    assert_eq!(fs::read(fx.restored("readme.txt")).expect("read"), b"hello");
                }
                ConflictPolicy::Overwrite => {
                    result.expect("restore");
                    assert!(fx.restored("docs").is_dir());
                }
            }
        }
    }

    #[test]
    fn a_link_at_the_name_of_a_file_is_replaced_never_written_through() {
        for policy in [ConflictPolicy::Fail, ConflictPolicy::Skip, ConflictPolicy::Overwrite] {
            let fx = Fx::new();
            let outside = outside(&fx);
            fs::create_dir_all(fx.restored("")).expect("mkdir");
            // One link to a file that exists, one dangling, as an attacker would prepare them.
            std::os::unix::fs::symlink(outside.join("sentinel"), fx.restored("readme.txt")).expect("symlink");
            std::os::unix::fs::symlink(outside.join("created-by-attack"), fx.restored("link")).expect("symlink");
            let manifest = sample(&fx);
            let mut options = fx.options();
            options.on_conflict = policy;

            let result = fx.run(&manifest, &options);
            assert_eq!(fs::read(outside.join("sentinel")).expect("read"), b"untouched", "{policy:?}");
            assert!(!outside.join("created-by-attack").exists(), "{policy:?}");
            match policy {
                ConflictPolicy::Fail => assert!(matches!(result, Err(Error::RestoreConflict { count: 2, .. }))),
                ConflictPolicy::Skip => {
                    let report = result.expect("restore");
                    assert_eq!(report.skipped.len(), 2, "{:?}", report.skipped);
                }
                ConflictPolicy::Overwrite => {
                    let report = result.expect("restore");
                    assert!(report.is_complete(), "{report:?}");
                    let meta = fs::symlink_metadata(fx.restored("readme.txt")).expect("metadata");
                    assert!(meta.is_file() && !meta.file_type().is_symlink());
                    assert_eq!(fs::read(fx.restored("readme.txt")).expect("read"), b"hello");
                    assert_eq!(fs::read_link(fx.restored("link")).expect("link"), Path::new("readme.txt"));
                }
            }
        }
    }

    #[test]
    fn a_target_that_is_a_link_to_a_directory_is_resolved_once() {
        let fx = Fx::new();
        let real = fx.dir.path().join("real-target");
        fs::create_dir(&real).expect("mkdir");
        let link = fx.dir.path().join("link-target");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let manifest = sample(&fx);
        let mut options = fx.options();
        options.target = link;

        let report = fx.run(&manifest, &options).expect("restore");
        assert_eq!(report.target, fs::canonicalize(&real).expect("canonical"));
        assert_eq!(fs::read(real.join("data/readme.txt")).expect("read"), b"hello");
    }
}

// ---------------------------------------------------------------------------------------------
// Symbolic links and hard links
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
mod links {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn tree_with_links(fx: &Fx) -> Manifest {
        let mut tree = Tree::new(fx);
        let docs = tree.dir(0, "docs");
        tree.file(0, "readme.txt", b"hello");
        tree.symlink(0, "ok", "readme.txt");
        tree.symlink(docs, "up", "../readme.txt");
        tree.symlink(0, "absolute", "/etc/passwd");
        tree.symlink(docs, "far", "../../../escape");
        tree.symlink(0, "tricky", "docs/../../escape");
        tree.manifest("/data")
    }

    fn exists(fx: &Fx, rel: &str) -> bool {
        fs::symlink_metadata(fx.restored(rel)).is_ok()
    }

    #[test]
    fn contained_is_the_default_and_restores_only_links_that_cannot_lead_out() {
        let fx = Fx::new();
        let manifest = tree_with_links(&fx);

        let report = fx.run(&manifest, &fx.options()).expect("restore");
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(report.symlinks, 2);
        assert_eq!(fs::read_link(fx.restored("ok")).expect("link"), Path::new("readme.txt"));
        assert_eq!(fs::read_link(fx.restored("docs/up")).expect("link"), Path::new("../readme.txt"));
        assert_eq!(fs::read(fx.restored("docs/up")).expect("read"), b"hello", "and it works");
        for refused in ["absolute", "docs/far", "tricky"] {
            assert!(!exists(&fx, refused), "{refused} should not have been restored");
        }
        assert_eq!(report.skipped.len(), 3);
        assert!(report.skipped[0].reason.contains("out of the restore root"), "{:?}", report.skipped);
    }

    #[test]
    fn skip_restores_no_links_at_all() {
        let fx = Fx::new();
        let manifest = tree_with_links(&fx);
        let mut options = fx.options();
        options.symlinks = RestoreSymlinks::Skip;

        let report = fx.run(&manifest, &options).expect("restore");
        assert_eq!(report.symlinks, 0);
        assert_eq!(report.skipped.len(), 5);
        for link in ["ok", "docs/up", "absolute", "docs/far", "tricky"] {
            assert!(!exists(&fx, link), "{link}");
        }
        assert!(exists(&fx, "readme.txt"));
    }

    #[test]
    fn as_recorded_restores_every_link_exactly_and_still_writes_nothing_outside() {
        let fx = Fx::new();
        let manifest = tree_with_links(&fx);
        let mut options = fx.options();
        options.symlinks = RestoreSymlinks::AsRecorded;

        let report = fx.run(&manifest, &options).expect("restore");
        assert_eq!(report.symlinks, 5);
        assert_eq!(fs::read_link(fx.restored("absolute")).expect("link"), Path::new("/etc/passwd"));
        assert_eq!(fs::read_link(fx.restored("docs/far")).expect("link"), Path::new("../../../escape"));
        assert_eq!(fs::read_link(fx.restored("tricky")).expect("link"), Path::new("docs/../../escape"));
        assert_eq!(names(fx.dir.path()), ["out", "repo"], "nothing appeared next to the target");
    }

    #[test]
    fn hard_links_are_restored_as_hard_links_of_the_restored_file() {
        let fx = Fx::new();
        let mut tree = Tree::new(&fx);
        let original = tree.file(0, "original", b"shared content");
        tree.hard_link(0, "alias", original);
        let manifest = tree.manifest("/data");

        let report = fx.run(&manifest, &fx.options()).expect("restore");
        assert_eq!((report.files, report.hard_links), (1, 1));
        let (a, b) = (
            fs::metadata(fx.restored("original")).expect("metadata"),
            fs::metadata(fx.restored("alias")).expect("metadata"),
        );
        assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()), "one file with two names");
        assert_eq!(a.nlink(), 2);
        assert_eq!(fs::read(fx.restored("alias")).expect("read"), b"shared content");
    }

    #[test]
    fn a_hard_link_whose_target_is_not_restored_becomes_a_file_of_its_own() {
        let fx = Fx::new();
        let mut tree = Tree::new(&fx);
        let original = tree.file(0, "original", b"shared content");
        tree.hard_link(0, "alias", original);
        tree.hard_link(0, "alias2", original);
        let manifest = tree.manifest("/data");
        let mut options = fx.options();
        options.includes = vec![PathBuf::from("/data/alias"), PathBuf::from("/data/alias2")];

        let report = fx.run(&manifest, &options).expect("restore");
        assert_eq!((report.files, report.hard_links), (1, 1), "{report:?}");
        assert!(!exists(&fx, "original"));
        assert_eq!(fs::read(fx.restored("alias")).expect("read"), b"shared content");
        assert_eq!(fs::metadata(fx.restored("alias")).expect("metadata").nlink(), 2, "the two aliases share it");
    }

    #[test]
    fn overwriting_an_existing_name_with_a_hard_link_replaces_it_atomically() {
        let fx = Fx::new();
        let mut tree = Tree::new(&fx);
        let original = tree.file(0, "original", b"new content");
        tree.hard_link(0, "alias", original);
        let manifest = tree.manifest("/data");
        fs::create_dir_all(fx.restored("")).expect("mkdir");
        fs::write(fx.restored("alias"), b"old").expect("write");
        let mut options = fx.options();
        options.on_conflict = ConflictPolicy::Overwrite;

        let report = fx.run(&manifest, &options).expect("restore");
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(fs::read(fx.restored("alias")).expect("read"), b"new content");
        assert_eq!(fs::metadata(fx.restored("alias")).expect("metadata").nlink(), 2);
        no_temporary_files(&fx.target());
    }
}

#[test]
fn special_files_are_skipped_and_reported() {
    let fx = Fx::new();
    let mut tree = Tree::new(&fx);
    tree.file(0, "plain", b"x");
    let mut fifo = directory(Some(0), "pipe");
    fifo.kind = NodeKind::Fifo;
    tree.push(fifo);
    let manifest = tree.manifest("/data");

    let report = fx.run(&manifest, &fx.options()).expect("restore");
    assert!(report.is_complete());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("special files"));
    assert!(!fx.restored("pipe").exists());
}

// ---------------------------------------------------------------------------------------------
// Corrupted chunks
// ---------------------------------------------------------------------------------------------

fn three_files(fx: &Fx) -> (Manifest, Vec<ChunkRef>) {
    let mut tree = Tree::new(fx);
    tree.file(0, "a.txt", b"first file");
    let middle = pseudo_random(2500, 2);
    let b = tree.file(0, "b.bin", &middle);
    tree.file(0, "c.txt", b"third file");
    let chunks = match &tree.entries[b as usize].kind {
        NodeKind::File { chunks } => chunks.clone(),
        other => panic!("{other:?}"),
    };
    (tree.manifest("/data"), chunks)
}

fn flip_a_byte(path: &Path) {
    let mut bytes = fs::read(path).expect("read object");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    fs::write(path, bytes).expect("write object");
}

#[test]
fn a_damaged_chunk_fails_its_file_only_and_leaves_no_partial_file() {
    let fx = Fx::new();
    let (manifest, chunks) = three_files(&fx);
    flip_a_byte(&fx.object_path(&chunks[1]));

    let report = fx.run(&manifest, &fx.options()).expect("the restore itself goes on");
    assert!(!report.is_complete());
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert!(report.failed[0].path.ends_with("b.bin"));
    assert!(report.failed[0].reason.contains("corrupt"), "{:?}", report.failed);
    assert_eq!(fs::read(fx.restored("a.txt")).expect("read"), b"first file");
    assert_eq!(fs::read(fx.restored("c.txt")).expect("read"), b"third file");
    assert!(!fx.restored("b.bin").exists(), "a file whose data cannot be trusted is not created");
    assert_eq!(names(&fx.restored("")), ["a.txt", "c.txt"], "and no temporary file is left");
    assert_eq!((report.files, report.bytes), (2, 20));
}

#[test]
fn a_missing_chunk_and_a_chunk_from_another_repository_are_failures_too() {
    let fx = Fx::new();
    let (manifest, chunks) = three_files(&fx);
    fs::remove_file(fx.object_path(&chunks[0])).expect("remove");
    let report = fx.run(&manifest, &fx.options()).expect("restore");
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert!(report.failed[0].reason.contains("not in the repository"), "{:?}", report.failed);
    assert!(!fx.restored("b.bin").exists());

    // An object swapped for another valid object is caught because the id is authenticated.
    let fx = Fx::new();
    let (manifest, chunks) = three_files(&fx);
    let other = fx.put(&pseudo_random(1000, 99));
    fs::copy(fx.object_path(&other), fx.object_path(&chunks[0])).expect("copy");
    let report = fx.run(&manifest, &fx.options()).expect("restore");
    assert_eq!(report.failed.len(), 1);
    assert!(!fx.restored("b.bin").exists());
}

#[test]
fn a_chunk_that_is_not_the_length_the_snapshot_records_is_refused() {
    let fx = Fx::new();
    let (mut manifest, _) = three_files(&fx);
    if let NodeKind::File { chunks } = &mut manifest.roots[0].entries[2].kind {
        chunks[0].len -= 1;
    }
    manifest.roots[0].entries[2].attributes.size -= 1;
    manifest.stats = manifest.compute_stats();

    let report = fx.run(&manifest, &fx.options()).expect("restore");
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert!(report.failed[0].reason.contains("differs from the length"), "{:?}", report.failed);
    assert!(!fx.restored("b.bin").exists());
}

// ---------------------------------------------------------------------------------------------
// Interruptions
// ---------------------------------------------------------------------------------------------

fn cancel_after_chunks(options: &mut RestoreOptions, chunks: usize) {
    let cancel = options.cancel.clone();
    let seen = Arc::new(AtomicUsize::new(0));
    options.observer = Some(Arc::new(move |event: &RestoreEvent<'_>| {
        if matches!(event, RestoreEvent::BytesWritten { .. }) && seen.fetch_add(1, Ordering::SeqCst) + 1 == chunks {
            cancel.cancel();
        }
    }));
}

fn four_files(fx: &Fx) -> (Manifest, Vec<Vec<u8>>) {
    let contents: Vec<Vec<u8>> = (0..4).map(|seed| pseudo_random(2000, 10 + seed)).collect();
    let mut tree = Tree::new(fx);
    for (index, data) in contents.iter().enumerate() {
        tree.file(0, &format!("f{index}"), data);
    }
    (tree.manifest("/data"), contents)
}

#[test]
fn a_cancelled_restore_leaves_whole_files_only_and_no_temporary_file() {
    let fx = Fx::new();
    let (manifest, contents) = four_files(&fx);
    let mut options = fx.options();
    // Each file has two chunks: stop while the second file is half written.
    cancel_after_chunks(&mut options, 3);

    let err = fx.run(&manifest, &options).expect_err("cancelled");
    assert!(matches!(err, Error::RestoreCancelled), "{err}");
    assert_eq!(names(&fx.restored("")), ["f0"], "the finished file stays, the half-written one is gone");
    assert_eq!(fs::read(fx.restored("f0")).expect("read"), contents[0]);
    no_temporary_files(&fx.target());

    // The directory the restore made has its recorded permissions and time, not the private
    // permissions it started with.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let meta = fs::metadata(fx.restored("")).expect("metadata");
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        let modified = meta.modified().expect("mtime").duration_since(std::time::UNIX_EPOCH);
        assert_eq!(modified.expect("after the epoch").as_secs(), 1_700_000_000);
    }
}

#[test]
fn an_interrupted_restore_is_completed_by_running_it_again() {
    for policy in [ConflictPolicy::Skip, ConflictPolicy::Overwrite] {
        let fx = Fx::new();
        let (manifest, contents) = four_files(&fx);
        let mut options = fx.options();
        cancel_after_chunks(&mut options, 5);
        fx.run(&manifest, &options).expect_err("cancelled");

        let mut again = fx.options();
        again.on_conflict = policy;
        let report = fx.run(&manifest, &again).expect("the second run completes it");
        assert!(report.is_complete(), "{policy:?}: {report:?}");
        for (index, data) in contents.iter().enumerate() {
            assert_eq!(fs::read(fx.restored(&format!("f{index}"))).expect("read"), *data, "{policy:?} f{index}");
        }
        no_temporary_files(&fx.target());
    }
}

#[test]
fn fail_mode_refuses_to_resume_over_the_files_of_an_interrupted_run() {
    let fx = Fx::new();
    let (manifest, _) = four_files(&fx);
    let mut options = fx.options();
    cancel_after_chunks(&mut options, 3);
    fx.run(&manifest, &options).expect_err("cancelled");

    let err = fx.run(&manifest, &fx.options()).expect_err("the finished file is a conflict");
    assert!(matches!(err, Error::RestoreConflict { count: 1, .. }), "{err}");
}

#[test]
fn a_restore_that_is_cancelled_before_it_starts_does_nothing() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    let options = fx.options();
    options.cancel.cancel();
    let err = fx.run(&manifest, &options).expect_err("cancelled");
    assert!(matches!(err, Error::RestoreCancelled));
    assert!(!fx.target().exists());
}

#[test]
fn leftovers_of_a_killed_restore_are_ignored_and_not_deleted() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    fs::create_dir_all(fx.restored("docs")).expect("mkdir");
    let stale = fx.restored("docs/.vaultsnap-tmp-0123456789abcdef");
    fs::write(&stale, b"half a file from a process that was killed").expect("write");

    let report = fx.run(&manifest, &fx.options()).expect("a stale temporary file is not a conflict");
    assert!(report.is_complete(), "{report:?}");
    assert!(stale.exists(), "someone else's file is never deleted");
    assert_eq!(fs::read(fx.restored("docs/big.bin")).expect("read").len(), BIG as usize);
}

// ---------------------------------------------------------------------------------------------
// The target
// ---------------------------------------------------------------------------------------------

#[test]
fn a_target_inside_the_repository_is_refused() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    for target in [fx.repo(), fx.repo().join("data"), fx.repo().join("new/place")] {
        let mut options = fx.options();
        options.target = target.clone();
        let err = fx.run(&manifest, &options).expect_err("refused");
        assert!(matches!(err, Error::InvalidTarget(_)), "{target:?}: {err}");
    }
    assert!(!fx.repo().join("new").exists());
    assert_eq!(names(&fx.repo()), ["data"]);
}

#[test]
fn a_target_that_is_a_file_or_below_one_is_refused() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    let file = fx.dir.path().join("a-file");
    fs::write(&file, b"x").expect("write");
    for target in [file.clone(), file.join("below")] {
        let mut options = fx.options();
        options.target = target;
        assert!(fx.run(&manifest, &options).is_err());
    }
    assert_eq!(fs::read(&file).expect("read"), b"x");
}

#[test]
fn dots_in_the_part_of_a_target_that_does_not_exist_yet_are_refused() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    let mut options = fx.options();
    options.target = fx.dir.path().join("new/../elsewhere");
    let err = fx.run(&manifest, &options).expect_err("refused");
    assert!(matches!(err, Error::InvalidTarget(_)), "{err}");
    assert!(names(fx.dir.path()).iter().all(|name| name == "repo"));

    options.target = PathBuf::new();
    assert!(matches!(fx.run(&manifest, &options), Err(Error::InvalidTarget(_))));
}

#[test]
fn a_missing_target_and_its_missing_parents_are_created() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    let mut options = fx.options();
    options.target = fx.dir.path().join("a/b/c");
    let report = fx.run(&manifest, &options).expect("restore");
    assert!(report.is_complete());
    assert_eq!(fs::read(fx.dir.path().join("a/b/c/data/readme.txt")).expect("read"), b"hello");
}

#[test]
fn progress_events_account_for_the_work() {
    let fx = Fx::new();
    let manifest = sample(&fx);
    let planned = Arc::new(std::sync::Mutex::new(None));
    let written = Arc::new(AtomicUsize::new(0));
    let restored = Arc::new(AtomicUsize::new(0));
    let (planned_in, written_in, restored_in) = (Arc::clone(&planned), Arc::clone(&written), Arc::clone(&restored));
    let mut options = fx.options();
    options.observer = Some(Arc::new(move |event: &RestoreEvent<'_>| match event {
        RestoreEvent::Planned { files, bytes } => {
            *planned_in.lock().expect("lock") = Some((*files, *bytes));
        }
        RestoreEvent::BytesWritten { bytes } => {
            written_in.fetch_add(*bytes as usize, Ordering::SeqCst);
        }
        RestoreEvent::EntryRestored { .. } => {
            restored_in.fetch_add(1, Ordering::SeqCst);
        }
    }));

    let report = fx.run(&manifest, &options).expect("restore");
    assert_eq!(*planned.lock().expect("lock"), Some((3, BIG + 5)));
    assert_eq!(written.load(Ordering::SeqCst) as u64, report.bytes);
    assert_eq!(restored.load(Ordering::SeqCst) as u64, report.files + report.directories + report.symlinks);
}
