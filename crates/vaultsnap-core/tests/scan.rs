//! Tests of the filesystem scanner through its public API, on real directory trees.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use tempfile::TempDir;
use vaultsnap_core::scan::{
    ChangePolicy, Entry, EntryKind, ErrorPolicy, HardLinkPolicy, Scan, ScanItem, ScanOptions,
    Scanner, SkipReason, Skipped, SpecialFilePolicy, SymlinkPolicy,
};
use vaultsnap_core::{Chunker, ChunkerConfig, Error};

fn temp() -> TempDir {
    tempfile::tempdir().expect("create temporary directory")
}

fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent directories");
    }
    fs::write(&path, contents).expect("write file");
}

fn scanner(root: &Path, options: ScanOptions) -> Scanner {
    Scanner::new(root, options).expect("create scanner")
}

fn default_scanner(root: &Path) -> Scanner {
    scanner(root, ScanOptions::default())
}

fn scan(scanner: &Scanner) -> Vec<ScanItem> {
    scanner
        .scan()
        .collect::<Result<Vec<_>, _>>()
        .expect("scan succeeds")
}

fn drain(walk: &mut Scan<'_>) -> Vec<ScanItem> {
    walk.by_ref()
        .collect::<Result<Vec<_>, _>>()
        .expect("scan succeeds")
}

fn entries(items: &[ScanItem]) -> Vec<&Entry> {
    items
        .iter()
        .filter_map(|item| match item {
            ScanItem::Entry(entry) => Some(entry),
            ScanItem::Skipped(_) => None,
        })
        .collect()
}

fn skipped(items: &[ScanItem]) -> Vec<&Skipped> {
    items
        .iter()
        .filter_map(|item| match item {
            ScanItem::Skipped(skipped) => Some(skipped),
            ScanItem::Entry(_) => None,
        })
        .collect()
}

fn find<'a>(items: &'a [ScanItem], rel: &str) -> &'a Entry {
    entries(items)
        .into_iter()
        .find(|entry| entry.path == Path::new(rel))
        .unwrap_or_else(|| panic!("no entry for {rel:?}"))
}

fn show(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        path.display().to_string()
    }
}

/// One line per entry: a type letter and the path (`l`/`h` add ` -> target`).
fn render(items: &[ScanItem]) -> Vec<String> {
    entries(items)
        .into_iter()
        .map(|entry| {
            let path = show(&entry.path);
            match &entry.kind {
                EntryKind::Directory => format!("d {path}"),
                EntryKind::File => format!("f {path}"),
                EntryKind::Symlink { target } => format!("l {path} -> {}", target.display()),
                EntryKind::HardLink { target } => format!("h {path} -> {}", target.display()),
                EntryKind::Fifo => format!("p {path}"),
                EntryKind::Socket => format!("s {path}"),
                EntryKind::CharDevice => format!("c {path}"),
                EntryKind::BlockDevice => format!("b {path}"),
                EntryKind::Other => format!("? {path}"),
            }
        })
        .collect()
}

fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_owned()).collect()
}

fn read_all(scanner: &Scanner, entry: &Entry) -> Vec<u8> {
    let mut reader = scanner.open(entry).expect("open entry");
    let mut data = Vec::new();
    reader.read_to_end(&mut data).expect("read entry");
    data
}

// ---------------------------------------------------------------------------------------------
// Traversal, ordering, metadata
// ---------------------------------------------------------------------------------------------

#[test]
fn nested_directories_are_walked_depth_first_in_byte_order() {
    let dir = temp();
    let root = dir.path();
    // Created in a scrambled order on purpose.
    write(root, "é/z", b"1");
    write(root, "b.txt", b"2");
    write(root, "a/z.txt", b"3");
    write(root, "B/x", b"4");
    write(root, "a b/y", b"5");
    write(root, "a/b/c.txt", b"6");
    fs::create_dir(root.join("empty")).expect("mkdir");

    let items = scan(&default_scanner(root));
    assert_eq!(
        render(&items),
        strings(&[
            "d .",
            "d B",
            "f B/x",
            "d a",
            "d a/b",
            "f a/b/c.txt",
            "f a/z.txt",
            "d a b",
            "f a b/y",
            "f b.txt",
            "d empty",
            "d é",
            "f é/z",
        ])
    );
    assert!(skipped(&items).is_empty());
}

#[test]
fn the_order_matches_an_independent_walk_of_a_larger_tree() {
    fn expected(dir: &Path, rel: &Path, out: &mut Vec<String>) {
        let mut names: Vec<_> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let rel = rel.join(&name);
            if fs::symlink_metadata(&path).expect("stat").is_dir() {
                out.push(format!("d {}", rel.display()));
                expected(&path, &rel, out);
            } else {
                out.push(format!("f {}", rel.display()));
            }
        }
    }

    let dir = temp();
    let root = dir.path();
    for a in 0..4 {
        write(root, &format!("d{a}/top.txt"), b"t");
        for b in 0..4 {
            for c in 0..6 {
                write(root, &format!("d{a}/e{b}/f{c}"), format!("{a}{b}{c}").as_bytes());
            }
        }
    }
    let mut want = vec!["d .".to_owned()];
    expected(root, Path::new(""), &mut want);

    let scanner = default_scanner(root);
    let items = scan(&scanner);
    assert_eq!(render(&items), want);
    assert_eq!(scanner.scan().stats().entries, 0, "a new walk starts from zero");

    let mut walk = scanner.scan();
    let count = walk.by_ref().count() as u64;
    assert_eq!(walk.stats().entries, count);
    assert_eq!(walk.stats().files, 4 + 4 * 4 * 6);
    assert_eq!(walk.stats().directories, 1 + 4 + 16);
}

#[test]
fn scanning_twice_gives_identical_results() {
    let dir = temp();
    for index in 0..30 {
        write(dir.path(), &format!("dir{}/file{index}", index % 5), b"content");
    }
    let scanner = default_scanner(dir.path());
    let project = |items: &[ScanItem]| -> Vec<(String, u64, Option<_>)> {
        entries(items)
            .into_iter()
            .map(|e| (show(&e.path), e.metadata.size, e.metadata.modified))
            .collect()
    };
    assert_eq!(project(&scan(&scanner)), project(&scan(&scanner)));
}

#[test]
fn records_size_kind_and_modification_time() {
    let dir = temp();
    write(dir.path(), "data.bin", &[7_u8; 1234]);
    let file = OpenOptions::new()
        .write(true)
        .open(dir.path().join("data.bin"))
        .expect("open for writing");
    file.set_modified(UNIX_EPOCH + Duration::new(1_000_000_000, 123_456_789))
        .expect("set modification time");
    drop(file);

    let items = scan(&default_scanner(dir.path()));
    let entry = find(&items, "data.bin");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.metadata.size, 1234);
    let modified = entry.metadata.modified.expect("modification time");
    assert_eq!(modified.secs, 1_000_000_000);
    assert!(modified.nanos < 1_000_000_000);
    assert!(!entry.followed_symlink);
}

#[cfg(unix)]
#[test]
fn records_permissions_ownership_and_identity() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = temp();
    let root = dir.path();
    write(root, "script.sh", b"#!/bin/sh\n");
    fs::set_permissions(root.join("script.sh"), fs::Permissions::from_mode(0o750))
        .expect("chmod file");
    fs::create_dir(root.join("private")).expect("mkdir");
    fs::set_permissions(root.join("private"), fs::Permissions::from_mode(0o700))
        .expect("chmod dir");
    fs::create_dir(root.join("shared")).expect("mkdir");
    fs::set_permissions(root.join("shared"), fs::Permissions::from_mode(0o1777))
        .expect("chmod sticky dir");

    let items = scan(&default_scanner(root));
    let std_meta = fs::metadata(root.join("script.sh")).expect("stat");
    let script = &find(&items, "script.sh").metadata;
    assert_eq!(script.mode, Some(0o750));
    assert_eq!(script.uid, Some(std_meta.uid()));
    assert_eq!(script.gid, Some(std_meta.gid()));
    assert_eq!(script.inode, Some(std_meta.ino()));
    assert_eq!(script.device, Some(std_meta.dev()));
    assert_eq!(script.links, Some(1));
    assert_eq!(find(&items, "private").metadata.mode, Some(0o700));
    assert_eq!(find(&items, "shared").metadata.mode, Some(0o1777));
    assert!(find(&items, "private").is_directory());
}

#[test]
fn empty_files_and_empty_directories_are_recorded() {
    let dir = temp();
    write(dir.path(), "empty.txt", b"");
    fs::create_dir(dir.path().join("empty_dir")).expect("mkdir");

    let scanner = default_scanner(dir.path());
    let items = scan(&scanner);
    assert_eq!(render(&items), strings(&["d .", "d empty_dir", "f empty.txt"]));
    let file = find(&items, "empty.txt");
    assert_eq!(file.metadata.size, 0);
    assert!(read_all(&scanner, file).is_empty());
}

#[cfg(unix)]
#[test]
fn unusual_file_names_survive_exactly() {
    let dir = temp();
    let root = dir.path();
    let long = "x".repeat(255);
    let names = [
        "with space",
        "new\nline",
        "tab\there",
        "-leading-dash",
        "unicode-日本語-é",
        "emoji-😀",
        "back\\slash",
        "quote'\"s",
        "*star?",
        "[brackets]",
        ".hidden",
        "trailing.",
        "  padded  ",
        "colon:name",
        long.as_str(),
    ];
    for name in names {
        write(root, name, name.as_bytes());
    }
    write(root, "dir with space/inner file", b"inner");

    let scanner = default_scanner(root);
    let items = scan(&scanner);
    for name in names {
        let entry = find(&items, name);
        assert_eq!(entry.kind, EntryKind::File, "{name:?}");
        assert_eq!(read_all(&scanner, entry), name.as_bytes(), "{name:?}");
    }
    assert_eq!(find(&items, "dir with space/inner file").metadata.size, 5);
    assert_eq!(entries(&items).len(), 1 + names.len() + 2);
    assert_eq!(render(&items), render(&scan(&scanner)));
}

#[cfg(unix)]
#[test]
fn names_that_are_not_utf8_are_preserved_byte_for_byte() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let dir = temp();
    let name = OsStr::from_bytes(b"bad-\xff\xfe-name");
    if let Err(err) = fs::write(dir.path().join(name), b"payload") {
        // Some filesystems (APFS) only accept valid UTF-8 names.
        eprintln!("note: this filesystem rejects non-UTF-8 names ({err}); case skipped");
        return;
    }
    let scanner = default_scanner(dir.path());
    let items = scan(&scanner);
    let entry = entries(&items)
        .into_iter()
        .find(|entry| entry.path.as_os_str().as_bytes() == b"bad-\xff\xfe-name")
        .expect("the entry keeps its exact bytes");
    assert_eq!(entry.path.clone().into_os_string().into_vec(), b"bad-\xff\xfe-name");
    assert_eq!(read_all(&scanner, entry), b"payload");
}

#[test]
fn the_filesystem_boundary_option_changes_nothing_within_one_filesystem() {
    let dir = temp();
    write(dir.path(), "a/b/c.txt", b"x");
    write(dir.path(), "d.txt", b"y");
    let stay = scan(&scanner(dir.path(), ScanOptions::default()));
    let cross = scan(&scanner(
        dir.path(),
        ScanOptions {
            cross_filesystems: true,
            ..ScanOptions::default()
        },
    ));
    assert_eq!(render(&stay), render(&cross));
    assert!(skipped(&stay).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Symbolic links
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
struct LinkFixture {
    root: TempDir,
    outside: TempDir,
}

#[cfg(unix)]
fn link_fixture() -> LinkFixture {
    use std::os::unix::fs::symlink;

    let root = temp();
    let outside = temp();
    write(outside.path(), "secret.txt", b"top secret");
    write(outside.path(), "deep/x", b"x");
    write(root.path(), "real/f.txt", b"inside");
    write(root.path(), "real/sub/g.txt", b"g");
    let r = root.path();
    symlink("real/f.txt", r.join("link_file")).expect("symlink");
    symlink("real/sub", r.join("link_dir")).expect("symlink");
    symlink(outside.path(), r.join("link_abs_dir")).expect("symlink");
    symlink(outside.path().join("secret.txt"), r.join("link_abs_file")).expect("symlink");
    symlink("missing/nowhere", r.join("dangling")).expect("symlink");
    symlink("loop_b", r.join("loop_a")).expect("symlink");
    symlink("loop_a", r.join("loop_b")).expect("symlink");
    symlink(".", r.join("up")).expect("symlink");
    LinkFixture { root, outside }
}

#[cfg(unix)]
fn with_symlinks(policy: SymlinkPolicy) -> ScanOptions {
    ScanOptions {
        symlinks: policy,
        ..ScanOptions::default()
    }
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_recorded_not_followed_by_default() {
    let fx = link_fixture();
    let out = fx.outside.path().display().to_string();
    let scanner = default_scanner(fx.root.path());
    let items = scan(&scanner);

    let mut expected = strings(&["d .", "l dangling -> missing/nowhere"]);
    expected.push(format!("l link_abs_dir -> {out}"));
    expected.push(format!("l link_abs_file -> {out}/secret.txt"));
    expected.extend(strings(&[
        "l link_dir -> real/sub",
        "l link_file -> real/f.txt",
        "l loop_a -> loop_b",
        "l loop_b -> loop_a",
        "d real",
        "f real/f.txt",
        "d real/sub",
        "f real/sub/g.txt",
        "l up -> .",
    ]));
    assert_eq!(render(&items), expected);
    assert!(entries(&items).iter().all(|entry| !entry.followed_symlink));

    // Nothing from outside the root is ever reached.
    let below_link = |e: &&Entry| e.path.starts_with("link_abs_dir") && e.path != Path::new("link_abs_dir");
    assert!(!entries(&items).iter().any(below_link));
    let link = find(&items, "link_file");
    assert!(matches!(scanner.open(link), Err(Error::InvalidArgument(_))));
}

#[cfg(unix)]
#[test]
fn following_inside_the_root_never_leaves_it() {
    let fx = link_fixture();
    let out = fx.outside.path().display().to_string();
    let scanner = scanner(fx.root.path(), with_symlinks(SymlinkPolicy::FollowInsideRoot));
    let items = scan(&scanner);

    let mut expected = strings(&["d .", "l dangling -> missing/nowhere"]);
    expected.push(format!("l link_abs_dir -> {out}"));
    expected.push(format!("l link_abs_file -> {out}/secret.txt"));
    expected.extend(strings(&[
        "d link_dir",
        "f link_dir/g.txt",
        "f link_file",
        "l loop_a -> loop_b",
        "l loop_b -> loop_a",
        "d real",
        "f real/f.txt",
        "d real/sub",
        "f real/sub/g.txt",
        "l up -> .",
    ]));
    assert_eq!(render(&items), expected);

    assert!(find(&items, "link_file").followed_symlink);
    assert!(find(&items, "link_dir").followed_symlink);
    assert!(!find(&items, "link_dir/g.txt").followed_symlink);
    assert_eq!(read_all(&scanner, find(&items, "link_file")), b"inside");
    assert_eq!(read_all(&scanner, find(&items, "link_dir/g.txt")), b"g");
}

#[cfg(unix)]
#[test]
fn following_everything_is_an_explicit_choice_and_still_stops_at_loops() {
    let fx = link_fixture();
    let scanner = scanner(fx.root.path(), with_symlinks(SymlinkPolicy::FollowAll));
    let items = scan(&scanner);
    let lines = render(&items);

    for wanted in [
        "d link_abs_dir",
        "d link_abs_dir/deep",
        "f link_abs_dir/deep/x",
        "f link_abs_dir/secret.txt",
        "f link_abs_file",
        "l loop_a -> loop_b",
        "l up -> .",
        "l dangling -> missing/nowhere",
    ] {
        assert!(lines.iter().any(|line| line == wanted), "missing {wanted:?} in {lines:#?}");
    }
    assert_eq!(read_all(&scanner, find(&items, "link_abs_file")), b"top secret");
}

// ---------------------------------------------------------------------------------------------
// Hard links
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
fn hard_link_tree() -> TempDir {
    let dir = temp();
    let root = dir.path();
    write(root, "a", b"shared");
    fs::hard_link(root.join("a"), root.join("b")).expect("link");
    fs::hard_link(root.join("a"), root.join("c")).expect("link");
    write(root, "d", b"solo");
    fs::create_dir(root.join("sub")).expect("mkdir");
    fs::hard_link(root.join("a"), root.join("sub/x")).expect("link");
    dir
}

#[cfg(unix)]
#[test]
fn hard_links_are_recorded_once_and_linked_by_default() {
    let dir = hard_link_tree();
    let scanner = default_scanner(dir.path());
    let mut walk = scanner.scan();
    let items = drain(&mut walk);
    assert_eq!(
        render(&items),
        strings(&["d .", "f a", "h b -> a", "h c -> a", "f d", "d sub", "h sub/x -> a"])
    );
    assert_eq!(find(&items, "a").metadata.links, Some(4));
    assert_eq!(walk.stats().hard_links, 3);
    assert_eq!(walk.stats().files, 2);
    assert_eq!(walk.stats().bytes, 6 + 4, "linked data is counted once");
}

#[cfg(unix)]
#[test]
fn hard_links_can_be_treated_as_independent_files() {
    let dir = hard_link_tree();
    let options = ScanOptions {
        hard_links: HardLinkPolicy::Duplicate,
        ..ScanOptions::default()
    };
    let scanner = scanner(dir.path(), options);
    let items = scan(&scanner);
    assert_eq!(
        render(&items),
        strings(&["d .", "f a", "f b", "f c", "f d", "d sub", "f sub/x"])
    );
    assert_eq!(read_all(&scanner, find(&items, "sub/x")), b"shared");
}

// ---------------------------------------------------------------------------------------------
// Errors: permissions, vanishing paths
// ---------------------------------------------------------------------------------------------

/// Restores permissions so the temporary directory can be removed even if a test fails.
#[cfg(unix)]
struct Unlock(Vec<std::path::PathBuf>);

#[cfg(unix)]
impl Drop for Unlock {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        for path in &self.0 {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        }
    }
}

#[cfg(unix)]
fn permission_tree() -> Option<(TempDir, Unlock)> {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp();
    let root = dir.path();
    write(root, "ok.txt", b"ok");
    write(root, "locked/inner.txt", b"inner");
    write(root, "sibling.txt", b"s");
    write(root, "unreadable.txt", b"nope");
    let locked = root.join("locked");
    let unreadable = root.join("unreadable.txt");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("chmod");
    let guard = Unlock(vec![locked.clone(), unreadable]);
    if fs::read_dir(&locked).is_ok() {
        eprintln!("note: running with privileges that bypass permissions; case skipped");
        return None;
    }
    Some((dir, guard))
}

#[cfg(unix)]
#[test]
fn unreadable_paths_are_reported_and_the_rest_is_still_scanned() {
    let Some((dir, _guard)) = permission_tree() else {
        return;
    };
    let scanner = default_scanner(dir.path());
    let mut walk = scanner.scan();
    let items = drain(&mut walk);

    assert_eq!(
        render(&items),
        strings(&["d .", "f ok.txt", "f sibling.txt", "f unreadable.txt"])
    );
    let skipped = skipped(&items);
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].path, Path::new("locked"));
    match &skipped[0].reason {
        SkipReason::Unreadable { operation, kind, .. } => {
            assert_eq!(*operation, "read directory");
            assert_eq!(*kind, std::io::ErrorKind::PermissionDenied);
        }
        other => panic!("unexpected reason {other:?}"),
    }
    assert_eq!(walk.stats().skipped, 1);

    // The file's metadata is readable, its content is not.
    let result = scanner.open(find(&items, "unreadable.txt"));
    assert!(
        matches!(&result, Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::PermissionDenied),
        "{result:?}"
    );
}

#[cfg(unix)]
#[test]
fn the_fail_policy_stops_at_the_first_unreadable_path() {
    let Some((dir, _guard)) = permission_tree() else {
        return;
    };
    let options = ScanOptions {
        on_error: ErrorPolicy::Fail,
        ..ScanOptions::default()
    };
    let scanner = scanner(dir.path(), options);
    let mut walk = scanner.scan();
    assert!(matches!(walk.next(), Some(Ok(ScanItem::Entry(_)))), "the root comes first");
    let err = walk.next().expect("an item").expect_err("the unreadable directory fails");
    assert!(matches!(err, Error::Io { operation: "read directory", .. }), "{err}");
    assert!(walk.next().is_none(), "the stream ends after an error");
}

#[test]
fn paths_that_vanish_during_the_walk_are_skipped_under_every_policy() {
    for on_error in [ErrorPolicy::Skip, ErrorPolicy::Fail] {
        let dir = temp();
        let root = dir.path();
        for name in ["a", "b", "c"] {
            write(root, name, name.as_bytes());
        }
        let options = ScanOptions {
            on_error,
            ..ScanOptions::default()
        };
        let scanner = scanner(root, options);
        let mut walk = scanner.scan();
        // The root entry is produced after its directory has been listed...
        assert!(matches!(walk.next(), Some(Ok(ScanItem::Entry(_)))));
        // ...so changes made now are races with the listing.
        fs::remove_file(root.join("b")).expect("remove");
        write(root, "zz_created_later", b"new");
        let rest = drain(&mut walk);

        assert_eq!(render(&rest), strings(&["f a", "f c"]), "{on_error:?}");
        let skipped = skipped(&rest);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].path, Path::new("b"));
        assert_eq!(skipped[0].reason, SkipReason::Vanished);
    }
}

// ---------------------------------------------------------------------------------------------
// Files that change while being read
// ---------------------------------------------------------------------------------------------

fn changed_error(err: &std::io::Error) -> bool {
    err.get_ref()
        .and_then(|inner| inner.downcast_ref::<Error>())
        .is_some_and(|inner| matches!(inner, Error::FileChanged { .. }))
}

fn one_file(contents: &[u8], options: ScanOptions) -> (TempDir, Scanner, Entry) {
    let dir = temp();
    write(dir.path(), "data.bin", contents);
    let scanner = scanner(dir.path(), options);
    let entry = find(&scan(&scanner), "data.bin").clone();
    (dir, scanner, entry)
}

#[test]
fn an_unchanged_file_reads_cleanly_and_reports_fresh_metadata() {
    let (_dir, scanner, entry) = one_file(&[3_u8; 5000], ScanOptions::default());
    let mut reader = scanner.open(&entry).expect("open");
    assert_eq!(reader.metadata().size, 5000);
    let mut data = Vec::new();
    reader.read_to_end(&mut data).expect("read");
    assert_eq!(data, vec![3_u8; 5000]);
    assert_eq!(reader.bytes_read(), 5000);
    assert!(!reader.changed());
}

#[test]
fn growth_during_the_read_is_detected_at_the_end() {
    let (dir, scanner, entry) = one_file(&[1_u8; 100], ScanOptions::default());
    let mut reader = scanner.open(&entry).expect("open");
    let mut first = [0_u8; 10];
    reader.read_exact(&mut first).expect("read the start");

    let mut file = OpenOptions::new()
        .append(true)
        .open(dir.path().join("data.bin"))
        .expect("open for append");
    file.write_all(b"appended while backing up").expect("append");
    file.sync_all().expect("sync");

    let mut rest = Vec::new();
    let err = reader.read_to_end(&mut rest).expect_err("the change must be reported");
    assert!(changed_error(&err), "{err}");
    assert!(reader.changed());
}

#[test]
fn truncation_during_the_read_is_detected_at_the_end() {
    let (dir, scanner, entry) = one_file(&[1_u8; 4096], ScanOptions::default());
    let mut reader = scanner.open(&entry).expect("open");
    let mut first = [0_u8; 16];
    reader.read_exact(&mut first).expect("read the start");

    let file = OpenOptions::new()
        .write(true)
        .open(dir.path().join("data.bin"))
        .expect("open for writing");
    file.set_len(32).expect("truncate");

    let mut rest = Vec::new();
    let err = reader.read_to_end(&mut rest).expect_err("the change must be reported");
    assert!(changed_error(&err), "{err}");
}

#[test]
fn the_accept_policy_lets_the_read_finish_but_marks_the_reader() {
    let options = ScanOptions {
        changed_files: ChangePolicy::Accept,
        ..ScanOptions::default()
    };
    let (dir, scanner, entry) = one_file(&[1_u8; 100], options);
    let mut reader = scanner.open(&entry).expect("open");
    let mut first = [0_u8; 10];
    reader.read_exact(&mut first).expect("read the start");
    let mut file = OpenOptions::new()
        .append(true)
        .open(dir.path().join("data.bin"))
        .expect("open for append");
    file.write_all(b"more").expect("append");
    drop(file);

    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).expect("accepted");
    assert!(reader.changed());
    assert_eq!(first.len() + rest.len(), 104);
}

#[test]
fn a_file_replaced_after_the_scan_is_refused() {
    let (dir, scanner, entry) = one_file(b"original", ScanOptions::default());
    // Create the replacement first so it cannot reuse the old inode, then swap atomically.
    let replacement = dir.path().join(".replacement");
    fs::write(&replacement, b"imposter").expect("write replacement");
    fs::rename(&replacement, dir.path().join("data.bin")).expect("rename over");

    match scanner.open(&entry) {
        Err(Error::FileChanged { reason, .. }) => {
            assert!(reason.contains("replaced"), "{reason}");
        }
        other => panic!("expected FileChanged, got {other:?}"),
    }
}

#[test]
fn a_file_removed_after_the_scan_is_reported_as_changed() {
    let (dir, scanner, entry) = one_file(b"original", ScanOptions::default());
    fs::remove_file(dir.path().join("data.bin")).expect("remove");
    assert!(matches!(scanner.open(&entry), Err(Error::FileChanged { .. })));
}

#[cfg(unix)]
#[test]
fn a_symlink_swapped_in_after_the_scan_is_never_followed() {
    let (dir, scanner, entry) = one_file(b"original", ScanOptions::default());
    let outside = temp();
    write(outside.path(), "secret.txt", b"must never be read");
    let link = dir.path().join(".link");
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), &link).expect("symlink");
    fs::rename(&link, dir.path().join("data.bin")).expect("rename over");

    match scanner.open(&entry) {
        Err(Error::FileChanged { reason, .. }) => {
            assert!(reason.contains("symbolic link"), "{reason}");
        }
        other => panic!("expected FileChanged, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Special files
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
fn special_tree() -> (TempDir, Vec<&'static str>, Option<std::os::unix::net::UnixListener>) {
    let dir = temp();
    let root = dir.path();
    write(root, "file", b"regular");
    let mut kinds = vec!["f file"];
    let made_fifo = std::process::Command::new("mkfifo")
        .arg(root.join("pipe"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if made_fifo {
        kinds.push("p pipe");
    } else {
        eprintln!("note: mkfifo is unavailable; FIFO case skipped");
    }
    let listener = match std::os::unix::net::UnixListener::bind(root.join("sock")) {
        Ok(listener) => {
            kinds.push("s sock");
            Some(listener)
        }
        Err(err) => {
            eprintln!("note: cannot create a socket here ({err}); socket case skipped");
            None
        }
    };
    (dir, kinds, listener)
}

#[cfg(unix)]
#[test]
fn special_files_are_recorded_without_being_opened() {
    let (dir, kinds, _listener) = special_tree();
    let scanner = default_scanner(dir.path());
    let mut walk = scanner.scan();
    // A FIFO that was opened for reading would block forever; reaching the end proves it was not.
    let items = drain(&mut walk);

    let mut want = vec!["d .".to_owned()];
    want.extend(kinds.iter().map(|kind| (*kind).to_owned()));
    let mut got = render(&items);
    want.sort();
    got.sort();
    assert_eq!(got, want);
    assert_eq!(walk.stats().special_files as usize, kinds.len() - 1);
    for entry in entries(&items) {
        if !matches!(entry.kind, EntryKind::Directory | EntryKind::File) {
            assert!(matches!(scanner.open(entry), Err(Error::InvalidArgument(_))));
        }
    }
}

#[cfg(unix)]
#[test]
fn special_files_can_be_skipped_by_policy() {
    let (dir, kinds, _listener) = special_tree();
    let options = ScanOptions {
        special_files: SpecialFilePolicy::Skip,
        ..ScanOptions::default()
    };
    let items = scan(&scanner(dir.path(), options));
    assert_eq!(render(&items), strings(&["d .", "f file"]));
    let reasons: Vec<_> = skipped(&items)
        .iter()
        .map(|s| s.reason.clone())
        .collect();
    assert_eq!(reasons.len(), kinds.len() - 1);
    assert!(reasons.iter().all(|r| matches!(r, SkipReason::SpecialFile(_))));
}

#[cfg(unix)]
#[test]
fn a_device_node_as_the_root_is_recorded_not_read() {
    let items = scan(&default_scanner(Path::new("/dev/null")));
    assert_eq!(render(&items), strings(&["c ."]));

    let options = ScanOptions {
        special_files: SpecialFilePolicy::Skip,
        ..ScanOptions::default()
    };
    let items = scan(&scanner(Path::new("/dev/null"), options));
    assert!(entries(&items).is_empty());
    assert_eq!(skipped(&items).len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Include and exclude patterns
// ---------------------------------------------------------------------------------------------

fn pattern_tree() -> TempDir {
    let dir = temp();
    let root = dir.path();
    for file in [
        "notes.txt",
        "a.log",
        "src/main.rs",
        "src/lib.rs",
        "src/gen/out.rs",
        "target/debug/app",
        "target/release/app",
        "d/b.log",
        "d/keep.txt",
        "d/notes.txt",
        "cache/data",
        "file_named_cache",
    ] {
        write(root, file, file.as_bytes());
    }
    dir
}

fn patterns(includes: &[&str], excludes: &[&str]) -> ScanOptions {
    ScanOptions {
        includes: includes.iter().map(|p| (*p).to_owned()).collect(),
        excludes: excludes.iter().map(|p| (*p).to_owned()).collect(),
        ..ScanOptions::default()
    }
}

#[test]
fn excluded_directories_are_pruned_with_everything_below_them() {
    let dir = pattern_tree();
    let scanner = scanner(dir.path(), patterns(&[], &["target/", "*.log"]));
    let mut walk = scanner.scan();
    let items = drain(&mut walk);
    assert_eq!(
        render(&items),
        strings(&[
            "d .",
            "d cache",
            "f cache/data",
            "d d",
            "f d/keep.txt",
            "f d/notes.txt",
            "f file_named_cache",
            "f notes.txt",
            "d src",
            "d src/gen",
            "f src/gen/out.rs",
            "f src/lib.rs",
            "f src/main.rs",
        ])
    );
    assert_eq!(walk.stats().excluded, 3, "a.log, d/b.log, and target/ (once)");
}

#[test]
fn include_patterns_filter_files_but_directories_stay_traversable() {
    let dir = pattern_tree();
    let items = scan(&scanner(dir.path(), patterns(&["*.rs"], &["gen/"])));
    assert_eq!(
        render(&items),
        strings(&[
            "d .",
            "d cache",
            "d d",
            "d src",
            "f src/lib.rs",
            "f src/main.rs",
            "d target",
            "d target/debug",
            "d target/release",
        ])
    );
}

#[test]
fn anchored_patterns_match_only_from_the_root() {
    let dir = pattern_tree();
    let items = scan(&scanner(dir.path(), patterns(&[], &["/notes.txt", "src/gen"])));
    let lines = render(&items);
    assert!(!lines.contains(&"f notes.txt".to_owned()));
    assert!(lines.contains(&"f d/notes.txt".to_owned()), "only the top-level file is excluded");
    assert!(lines.iter().all(|line| !line.contains("src/gen")));
    assert!(lines.contains(&"f src/lib.rs".to_owned()));
}

#[test]
fn a_directory_only_pattern_does_not_match_a_file_of_the_same_name() {
    let dir = pattern_tree();
    let items = scan(&scanner(dir.path(), patterns(&[], &["cache/"])));
    let lines = render(&items);
    assert!(!lines.iter().any(|line| line.contains("cache/") || line == "d cache"));
    assert!(lines.contains(&"f file_named_cache".to_owned()));
}

#[test]
fn includes_with_a_directory_glob_select_everything_below_it() {
    let dir = pattern_tree();
    let items = scan(&scanner(dir.path(), patterns(&["src/**"], &[])));
    let files: Vec<_> = render(&items)
        .into_iter()
        .filter(|line| line.starts_with("f "))
        .collect();
    assert_eq!(files, strings(&["f src/gen/out.rs", "f src/lib.rs", "f src/main.rs"]));
}

#[test]
fn exclusion_wins_over_inclusion() {
    let dir = pattern_tree();
    let items = scan(&scanner(dir.path(), patterns(&["*.txt"], &["d/"])));
    let files: Vec<_> = render(&items)
        .into_iter()
        .filter(|line| line.starts_with("f "))
        .collect();
    assert_eq!(files, strings(&["f notes.txt"]));
}

#[cfg(unix)]
#[test]
fn escaped_patterns_match_names_containing_special_characters() {
    let dir = temp();
    write(dir.path(), "[brackets]", b"1");
    write(dir.path(), "b", b"2");
    let items = scan(&scanner(dir.path(), patterns(&[], &[r"\[brackets\]"])));
    assert_eq!(render(&items), strings(&["d .", "f b"]));
}

#[test]
fn invalid_patterns_and_roots_are_rejected_up_front() {
    let dir = temp();
    for bad in ["[", "", "/"] {
        let result = Scanner::new(dir.path(), patterns(&[], &[bad]));
        assert!(matches!(result, Err(Error::InvalidPattern { .. })), "{bad:?}");
    }
    let result = Scanner::new(dir.path(), patterns(&["dir/"], &[]));
    assert!(matches!(result, Err(Error::InvalidPattern { .. })));

    let missing = dir.path().join("does-not-exist");
    assert!(matches!(
        Scanner::new(&missing, ScanOptions::default()),
        Err(Error::Io { .. })
    ));
}

// ---------------------------------------------------------------------------------------------
// Roots, streaming
// ---------------------------------------------------------------------------------------------

#[test]
fn a_single_file_can_be_the_root() {
    let dir = temp();
    write(dir.path(), "only.txt", b"hello");
    let scanner = default_scanner(&dir.path().join("only.txt"));
    let items = scan(&scanner);
    assert_eq!(render(&items), strings(&["f ."]));
    assert_eq!(read_all(&scanner, find(&items, "")), b"hello");
}

#[cfg(unix)]
#[test]
fn a_symlink_given_as_the_root_is_resolved() {
    let dir = temp();
    write(dir.path(), "real/a.txt", b"a");
    std::os::unix::fs::symlink("real", dir.path().join("alias")).expect("symlink");
    let via_alias = default_scanner(&dir.path().join("alias"));
    let direct = default_scanner(&dir.path().join("real"));
    assert_eq!(via_alias.root(), direct.root());
    assert_eq!(render(&scan(&via_alias)), render(&scan(&direct)));
}

#[test]
fn large_files_are_read_as_a_stream_into_the_chunker() {
    let dir = temp();
    let size: u64 = 32 * 1024 * 1024;
    let file = fs::File::create(dir.path().join("big.bin")).expect("create");
    file.set_len(size).expect("extend sparsely");
    drop(file);

    let scanner = default_scanner(dir.path());
    let items = scan(&scanner);
    let entry = find(&items, "big.bin");
    assert_eq!(entry.metadata.size, size);

    let reader = scanner.open(entry).expect("open");
    let mut chunker = Chunker::new(reader, ChunkerConfig::default());
    let (mut total, mut chunks) = (0_u64, 0_u64);
    while let Some(chunk) = chunker.next_chunk().expect("the file did not change") {
        total += chunk.data.len() as u64;
        chunks += 1;
    }
    assert_eq!(total, size);
    assert!(chunks >= 8, "{chunks} chunks");
}

#[test]
fn a_change_during_chunking_surfaces_as_a_read_error() {
    let dir = temp();
    write(dir.path(), "data.bin", &vec![9_u8; 3 * 1024 * 1024]);
    let scanner = default_scanner(dir.path());
    let entry = find(&scan(&scanner), "data.bin").clone();
    let reader = scanner.open(&entry).expect("open");

    // Grow the file after it was opened but before the chunker has read to the end.
    let mut file = OpenOptions::new()
        .append(true)
        .open(dir.path().join("data.bin"))
        .expect("open for append");
    file.write_all(b"tail").expect("append");
    drop(file);

    let mut chunker = Chunker::new(reader, ChunkerConfig::default());
    let mut failure = None;
    loop {
        match chunker.next_chunk() {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    let err = failure.expect("the chunker reports that the file changed");
    assert!(changed_error(&err), "{err}");
}
