//! Walking the repository's directories: what is there, what should not be, and an inventory of
//! the snapshot files and chunk objects for the rest of the check.
//!
//! Nothing here opens an object's contents. Names are the evidence: a snapshot is a 64-digit hex
//! file name in `snapshots/`, a chunk object a 64-digit hex file name in the `data/` directory
//! named by its first two digits, a key file or lock file a 32-digit hex name. Everything else is
//! reported as unexpected, or as a leftover if its name is the store's temporary-file prefix.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::engine::Findings;
use super::{Finding, FindingKind};
use crate::chunk_store::ChunkId;
use crate::error::Result;
use crate::keys;
use crate::lock::{describe_lock, LockKind, MAX_LOCK_BYTES};
use crate::repository::RepositoryLayout;
use crate::snapshot::SnapshotId;
use crate::store::{DirEntryInfo, DirEntryKind, Store};

/// The directories every repository has.
const REQUIRED: [&str; 4] = [
    RepositoryLayout::DATA_DIR,
    RepositoryLayout::SNAPSHOTS_DIR,
    RepositoryLayout::LOCKS_DIR,
    RepositoryLayout::KEYS_DIR,
];

/// What the walk found that the rest of the check works from.
#[derive(Debug, Default)]
pub(super) struct Inventory {
    /// The snapshot files, in ascending order of id.
    pub snapshot_ids: Vec<SnapshotId>,
    /// The chunk objects, with the size of each file.
    pub objects: HashMap<ChunkId, u64>,
    /// Key files that parse.
    pub key_files: u64,
}

fn is_hex_name(name: &str, len: usize) -> bool {
    name.len() == len && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn describe(kind: DirEntryKind) -> &'static str {
    match kind {
        DirEntryKind::File => "a file",
        DirEntryKind::Directory => "a directory",
        DirEntryKind::Symlink => "a symbolic link",
        DirEntryKind::Other => "a special file",
    }
}

fn subject(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// Reports an entry that is not what its place allows: a leftover temporary file, or something
/// that does not belong.
fn unexpected(findings: &mut Findings, dir: &str, entry: &DirEntryInfo, why: &str) {
    let leftover = Store::is_temp_name(&entry.name) && entry.kind == DirEntryKind::File;
    let (kind, detail) = if leftover {
        (
            FindingKind::LeftoverTempFile,
            "a temporary file left by an interrupted write".to_owned(),
        )
    } else {
        (
            FindingKind::UnexpectedFile,
            format!("{} {why}", describe(entry.kind)),
        )
    };
    findings.add(Finding {
        kind,
        subject: subject(dir, &entry.name),
        detail,
    });
}

/// Lists `dir`. A directory that cannot be listed is a finding, not a failure of the check.
fn entries_of(store: &Store, dir: &str, findings: &mut Findings) -> Vec<DirEntryInfo> {
    match store.list_entries(dir) {
        Ok(entries) => entries,
        Err(err) => {
            findings.add(Finding {
                kind: FindingKind::IoError,
                subject: dir.to_owned(),
                detail: err.to_string(),
            });
            Vec::new()
        }
    }
}

/// Walks the repository. `own_lock` is the name of the lock the check itself holds, which is not
/// reported. Only an unreadable root directory is an error; everything else is a finding.
///
/// The snapshot files are listed before the chunk objects. A backup commits its snapshot after it
/// has stored its chunks, so a backup running at the same time can add objects that no listed
/// snapshot refers to (orphans), but never a listed snapshot whose objects are missing.
pub(super) fn scan(
    store: &Store,
    own_lock: Option<&str>,
    findings: &mut Findings,
) -> Result<Inventory> {
    let mut inventory = Inventory::default();
    let root = store.list_entries("")?;

    // A backend without directories (an object store) has no empty ones: a directory with no
    // objects in it is simply not listed, which is not damage.
    let directories = store.capabilities().directories;
    let present_dir = |name: &str| {
        !directories
            || root
                .iter()
                .any(|entry| entry.name == name && entry.kind == DirEntryKind::Directory)
    };
    for name in REQUIRED.iter().filter(|_| directories) {
        match root.iter().find(|entry| entry.name == *name) {
            None => findings.add(Finding {
                kind: FindingKind::MissingDirectory,
                subject: (*name).to_owned(),
                detail: "the directory does not exist".to_owned(),
            }),
            Some(entry) if entry.kind != DirEntryKind::Directory => findings.add(Finding {
                kind: FindingKind::MissingDirectory,
                subject: (*name).to_owned(),
                detail: format!("{} where a directory is required", describe(entry.kind)),
            }),
            Some(_) => {}
        }
    }
    for entry in &root {
        let known = entry.name == RepositoryLayout::CONFIG_FILE
            || entry.name == RepositoryLayout::INDEX_DIR
            || REQUIRED.contains(&entry.name.as_str());
        if !known {
            unexpected(
                findings,
                "",
                entry,
                "that is not part of a VaultSnap repository",
            );
        }
    }

    if present_dir(RepositoryLayout::KEYS_DIR) {
        scan_keys(store, findings, &mut inventory);
    }
    if present_dir(RepositoryLayout::LOCKS_DIR) {
        scan_locks(store, own_lock, findings);
    }
    if present_dir(RepositoryLayout::SNAPSHOTS_DIR) {
        scan_snapshots(store, findings, &mut inventory);
    }
    if present_dir(RepositoryLayout::DATA_DIR) {
        scan_data(store, findings, &mut inventory);
    }
    Ok(inventory)
}

fn scan_keys(store: &Store, findings: &mut Findings, inventory: &mut Inventory) {
    let dir = RepositoryLayout::KEYS_DIR;
    for entry in entries_of(store, dir, findings) {
        if entry.kind == DirEntryKind::File && is_hex_name(&entry.name, 32) {
            match keys::key_file_problem(store, &entry.name) {
                None => inventory.key_files += 1,
                Some(problem) => findings.add(Finding {
                    kind: FindingKind::KeyFileInvalid,
                    subject: subject(dir, &entry.name),
                    detail: problem,
                }),
            }
        } else {
            unexpected(findings, dir, &entry, "that is not a key file");
        }
    }
}

fn scan_locks(store: &Store, own_lock: Option<&str>, findings: &mut Findings) {
    let dir = RepositoryLayout::LOCKS_DIR;
    let own = own_lock.and_then(|name| name.strip_prefix("locks/"));
    for entry in entries_of(store, dir, findings) {
        if Some(entry.name.as_str()) == own {
            continue;
        }
        let is_lock = entry.kind == DirEntryKind::File
            && entry
                .name
                .strip_suffix(".lock")
                .is_some_and(|stem| is_hex_name(stem, 32));
        if !is_lock {
            unexpected(findings, dir, &entry, "that is not a lock file");
            continue;
        }
        let name = subject(dir, &entry.name);
        let info = match store.read(&name, MAX_LOCK_BYTES) {
            Ok(data) => describe_lock(&data),
            // Released while the walk was running.
            Err(err) if err.is_not_found() => continue,
            Err(_) => describe_lock(&[]),
        };
        let mut detail = match info.kind {
            Some(LockKind::Shared) => "a shared lock".to_owned(),
            Some(LockKind::Exclusive) => "an exclusive lock".to_owned(),
            None => "a lock of unknown kind".to_owned(),
        };
        // Writing to a String cannot fail.
        if let Some(pid) = info.pid {
            let _ = write!(detail, " held by process {pid}");
        }
        if let Some(created) = info.created_at_unix {
            let _ = write!(detail, ", created at unix time {created}");
        }
        findings.add(Finding {
            kind: FindingKind::LockPresent,
            subject: name,
            detail,
        });
    }
}

fn scan_snapshots(store: &Store, findings: &mut Findings, inventory: &mut Inventory) {
    let dir = RepositoryLayout::SNAPSHOTS_DIR;
    for entry in entries_of(store, dir, findings) {
        let id = (entry.kind == DirEntryKind::File)
            .then(|| entry.name.parse::<SnapshotId>().ok())
            .flatten();
        match id {
            Some(id) => inventory.snapshot_ids.push(id),
            None => unexpected(findings, dir, &entry, "that is not a snapshot"),
        }
    }
}

fn scan_data(store: &Store, findings: &mut Findings, inventory: &mut Inventory) {
    let dir = RepositoryLayout::DATA_DIR;
    for shard in entries_of(store, dir, findings) {
        if shard.kind == DirEntryKind::Directory && is_hex_name(&shard.name, 2) {
            scan_shard(store, &shard.name, findings, inventory);
        } else {
            unexpected(findings, dir, &shard, "that is not a chunk directory");
        }
    }
}

fn scan_shard(store: &Store, shard: &str, findings: &mut Findings, inventory: &mut Inventory) {
    let dir = format!("{}/{shard}", RepositoryLayout::DATA_DIR);
    for entry in entries_of(store, &dir, findings) {
        let object = (entry.kind == DirEntryKind::File && is_hex_name(&entry.name, 64))
            .then(|| entry.name.parse::<ChunkId>().ok())
            .flatten();
        match object {
            Some(_) if !entry.name.starts_with(shard) => {
                let why = format!(
                    "named like a chunk but stored in the wrong directory; VaultSnap looks for \
                     it in data/{}/",
                    &entry.name[..2]
                );
                unexpected(findings, &dir, &entry, &why);
            }
            Some(id) => {
                inventory.objects.insert(id, entry.len);
            }
            None => unexpected(findings, &dir, &entry, "that is not a chunk object"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const SNAPSHOT: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    const CHUNK: &str = "ab0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd";

    /// A skeleton repository: the four directories and nothing in them.
    fn skeleton() -> (TempDir, Store) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let store = Store::local(dir.path());
        for name in REQUIRED {
            store.create_dir(name).expect("mkdir");
        }
        (dir, store)
    }

    fn scanned(store: &Store, own_lock: Option<&str>) -> (Inventory, Vec<Finding>) {
        let mut findings = Findings::new(0);
        let inventory = scan(store, own_lock, &mut findings).expect("scan");
        (inventory, findings.finish().findings)
    }

    fn summary(findings: &[Finding]) -> Vec<(FindingKind, String)> {
        findings
            .iter()
            .map(|f| (f.kind, f.subject.clone()))
            .collect()
    }

    #[test]
    fn an_empty_skeleton_has_nothing_to_report() {
        let (_dir, store) = skeleton();
        store.write_new("config", b"x").expect("write");
        let (inventory, findings) = scanned(&store, None);
        assert!(findings.is_empty(), "{findings:?}");
        assert!(inventory.snapshot_ids.is_empty() && inventory.objects.is_empty());
        assert_eq!(inventory.key_files, 0);
    }

    #[test]
    fn missing_or_wrongly_typed_required_directories_are_errors_and_the_walk_goes_on() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let store = Store::local(dir.path());
        store.create_dir("keys").expect("mkdir");
        fs::write(dir.path().join("snapshots"), b"a file").expect("write");
        fs::write(dir.path().join("locks"), b"a file").expect("write");

        let (_, findings) = scanned(&store, None);
        let described: Vec<(FindingKind, &str, &str)> = findings
            .iter()
            .map(|f| (f.kind, f.subject.as_str(), f.detail.as_str()))
            .collect();
        assert_eq!(
            described,
            [
                (
                    FindingKind::MissingDirectory,
                    "data",
                    "the directory does not exist"
                ),
                (
                    FindingKind::MissingDirectory,
                    "snapshots",
                    "a file where a directory is required"
                ),
                (
                    FindingKind::MissingDirectory,
                    "locks",
                    "a file where a directory is required"
                ),
            ]
        );
    }

    #[test]
    fn snapshot_files_and_chunk_objects_are_inventoried_by_name() {
        let (dir, store) = skeleton();
        store
            .write_new(&format!("snapshots/{SNAPSHOT}"), b"x")
            .expect("write");
        store.create_dir("data/ab").expect("mkdir");
        store
            .write_new(&format!("data/ab/{CHUNK}"), b"12345")
            .expect("write");

        let (inventory, findings) = scanned(&store, None);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(inventory.snapshot_ids.len(), 1);
        assert_eq!(inventory.snapshot_ids[0].as_str(), SNAPSHOT);
        let id: ChunkId = CHUNK.parse().expect("valid id");
        assert_eq!(inventory.objects.get(&id), Some(&5));
        drop(dir);
    }

    #[test]
    fn things_that_do_not_belong_are_unexpected_and_temporary_files_are_leftovers() {
        let (dir, store) = skeleton();
        let root = dir.path();
        fs::write(root.join("notes.txt"), b"x").expect("write");
        fs::write(root.join(".tmp-0011223344556677"), b"x").expect("write");
        fs::write(root.join("snapshots/not-a-snapshot"), b"x").expect("write");
        fs::write(root.join("snapshots/.tmp-0011223344556677"), b"x").expect("write");
        fs::create_dir(root.join("snapshots/a-directory")).expect("mkdir");
        fs::write(root.join("keys/readme"), b"x").expect("write");
        fs::write(root.join("locks/readme"), b"x").expect("write");
        fs::create_dir(root.join("data/zz")).expect("mkdir");
        fs::write(root.join("data/loose-file"), b"x").expect("write");
        fs::create_dir(root.join("data/ab")).expect("mkdir");
        fs::write(root.join("data/ab/short"), b"x").expect("write");
        fs::create_dir(root.join("data/ab/subdirectory")).expect("mkdir");
        fs::write(root.join("data/ab/.tmp-0011223344556677"), b"x").expect("write");
        // Named like a chunk, but in the wrong directory.
        fs::create_dir(root.join("data/cd")).expect("mkdir");
        fs::write(root.join(format!("data/cd/{CHUNK}")), b"x").expect("write");

        let (inventory, findings) = scanned(&store, None);
        assert!(
            inventory.objects.is_empty(),
            "a misplaced object is not an object"
        );
        let kinds: Vec<(FindingKind, String)> = summary(&findings);
        let expected = |kind, subject: &str| (kind, subject.to_owned());
        for item in [
            expected(FindingKind::UnexpectedFile, "notes.txt"),
            expected(FindingKind::LeftoverTempFile, ".tmp-0011223344556677"),
            expected(FindingKind::UnexpectedFile, "snapshots/not-a-snapshot"),
            expected(
                FindingKind::LeftoverTempFile,
                "snapshots/.tmp-0011223344556677",
            ),
            expected(FindingKind::UnexpectedFile, "snapshots/a-directory"),
            expected(FindingKind::UnexpectedFile, "keys/readme"),
            expected(FindingKind::UnexpectedFile, "locks/readme"),
            expected(FindingKind::UnexpectedFile, "data/zz"),
            expected(FindingKind::UnexpectedFile, "data/loose-file"),
            expected(FindingKind::UnexpectedFile, "data/ab/short"),
            expected(FindingKind::UnexpectedFile, "data/ab/subdirectory"),
            expected(
                FindingKind::LeftoverTempFile,
                "data/ab/.tmp-0011223344556677",
            ),
            expected(FindingKind::UnexpectedFile, &format!("data/cd/{CHUNK}")),
        ] {
            assert!(kinds.contains(&item), "{item:?} is missing from {kinds:?}");
        }
        assert_eq!(findings.len(), 13, "{kinds:?}");
        let misplaced = findings
            .iter()
            .find(|f| f.subject.starts_with("data/cd/"))
            .expect("found");
        assert!(
            misplaced.detail.contains("data/ab/"),
            "{}",
            misplaced.detail
        );
        let directory = findings
            .iter()
            .find(|f| f.subject == "data/zz")
            .expect("found");
        assert_eq!(
            directory.detail,
            "a directory that is not a chunk directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_reported_not_followed() {
        let (dir, store) = skeleton();
        let outside = tempfile::tempdir().expect("create temporary directory");
        fs::write(outside.path().join("target"), b"x").expect("write");
        std::os::unix::fs::symlink(
            outside.path().join("target"),
            dir.path().join(format!("snapshots/{SNAPSHOT}")),
        )
        .expect("symlink");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("data/ab")).expect("symlink");

        let (inventory, findings) = scanned(&store, None);
        assert!(inventory.snapshot_ids.is_empty() && inventory.objects.is_empty());
        let details: Vec<&str> = findings.iter().map(|f| f.detail.as_str()).collect();
        assert_eq!(
            details,
            [
                "a symbolic link that is not a snapshot",
                "a symbolic link that is not a chunk directory"
            ]
        );
    }

    #[test]
    fn key_files_are_parsed_and_a_bad_one_is_a_warning() {
        let (dir, store) = skeleton();
        let bad = "00112233445566778899aabbccddeeff";
        fs::write(dir.path().join(format!("keys/{bad}")), b"garbage").expect("write");

        let (inventory, findings) = scanned(&store, None);
        assert_eq!(inventory.key_files, 0);
        assert_eq!(
            summary(&findings),
            [(FindingKind::KeyFileInvalid, format!("keys/{bad}"))]
        );
        assert!(!findings[0].detail.is_empty());
    }

    #[test]
    fn other_locks_are_described_and_the_own_lock_is_not_reported() {
        let (dir, store) = skeleton();
        let stale = "0123456789abcdef0123456789abcdef.lock";
        let own = "ffeeddccbbaa99887766554433221100.lock";
        fs::write(
            dir.path().join(format!("locks/{stale}")),
            b"kind=exclusive\npid=4242\ncreated_at_unix=1700000000\n",
        )
        .expect("write");
        fs::write(dir.path().join(format!("locks/{own}")), b"kind=shared\n").expect("write");
        fs::write(
            dir.path()
                .join("locks/ffffffffffffffffffffffffffffffff.lock"),
            b"???",
        )
        .expect("write");

        let (_, findings) = scanned(&store, Some(&format!("locks/{own}")));
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings.iter().all(|f| f.kind == FindingKind::LockPresent));
        let first = &findings[0];
        assert_eq!(first.subject, format!("locks/{stale}"));
        assert_eq!(
            first.detail,
            "an exclusive lock held by process 4242, created at unix time 1700000000"
        );
        assert_eq!(findings[1].detail, "a lock of unknown kind");
    }

    #[test]
    fn a_misnamed_lock_file_is_unexpected() {
        let (dir, store) = skeleton();
        fs::write(dir.path().join("locks/0123.lock"), b"kind=shared\n").expect("write");
        let (_, findings) = scanned(&store, None);
        assert_eq!(
            summary(&findings),
            [(FindingKind::UnexpectedFile, "locks/0123.lock".to_owned())]
        );
    }

    #[test]
    fn an_unreadable_root_is_an_error_of_the_check_itself() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let store = Store::local(dir.path().join("missing"));
        let mut findings = Findings::new(0);
        assert!(scan(&store, None, &mut findings).is_err());
    }

    #[test]
    fn hex_names_are_exactly_lowercase_hex_of_the_right_length() {
        assert!(is_hex_name("ab", 2) && is_hex_name("0f9", 3));
        assert!(!is_hex_name("AB", 2) && !is_hex_name("abc", 2) && !is_hex_name("g1", 2));
        assert!(
            !is_hex_name("é", 2),
            "two bytes of a non-ASCII character are not two digits"
        );
    }
}
