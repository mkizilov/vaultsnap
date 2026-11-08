//! The binary encoding of a manifest, schema version 1 (specified in `docs/FORMAT.md`).
//!
//! All integers are little endian and fixed width. Byte strings and text carry a `u32` length
//! prefix; lists carry a `u32` count. There are no optional trailing fields and no padding, and
//! the whole input must be consumed. Decoding is defensive: every length and count is checked
//! against the bytes that remain before anything is allocated, so a malformed or hostile manifest
//! fails with an error and cannot exhaust memory or panic.
//!
//! [`decode`] returns a manifest only if it also passes [`Manifest::validate`].

use zeroize::Zeroizing;

use super::id::SnapshotId;
use super::model::{
    Attributes, BackupConfig, ChunkRef, Manifest, ManifestEntry, ManifestStats, NodeKind, Root,
    SkippedReason, SkippedRecord, MANIFEST_FORMAT_VERSION,
};
use super::object::MAX_MANIFEST_BYTES;
use crate::chunk_store::ChunkId;
use crate::chunking::ChunkerConfig;
use crate::error::{Error, Result};
use crate::scan::{
    ChangePolicy, ErrorPolicy, HardLinkPolicy, SpecialFilePolicy, SymlinkPolicy, Timestamp,
};

const NO_PARENT: u32 = u32::MAX;
/// The smallest encoding of an entry: parent, empty name, kind, and attributes with no optional
/// field present. Used to reject lying counts before allocating.
const ENTRY_MIN_BYTES: usize = 28;
const CHUNK_REF_BYTES: usize = 36;
const ROOT_MIN_BYTES: usize = 8;
const SKIPPED_MIN_BYTES: usize = 13;
const STRING_MIN_BYTES: usize = 4;

fn too_large() -> Error {
    Error::InvalidManifest("a length or count does not fit the format".to_owned())
}

// ---------------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------------

struct Writer {
    buf: Zeroizing<Vec<u8>>,
}

impl Writer {
    fn new() -> Self {
        Self {
            buf: Zeroizing::new(Vec::new()),
        }
    }

    fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn flag(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn count(&mut self, count: usize) -> Result<()> {
        self.u32(u32::try_from(count).map_err(|_| too_large())?);
        Ok(())
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.count(bytes.len())?;
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    fn string(&mut self, text: &str) -> Result<()> {
        self.bytes(text.as_bytes())
    }

    fn strings(&mut self, list: &[String]) -> Result<()> {
        self.count(list.len())?;
        for text in list {
            self.string(text)?;
        }
        Ok(())
    }

    fn timestamp(&mut self, time: Timestamp) {
        self.i64(time.secs);
        self.u32(time.nanos);
    }

    fn optional_timestamp(&mut self, time: Option<Timestamp>) {
        self.flag(time.is_some());
        if let Some(time) = time {
            self.timestamp(time);
        }
    }

    fn optional_u32(&mut self, value: Option<u32>) {
        self.flag(value.is_some());
        if let Some(value) = value {
            self.u32(value);
        }
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        self.flag(value.is_some());
        if let Some(value) = value {
            self.u64(value);
        }
    }
}

fn symlink_code(policy: SymlinkPolicy) -> u8 {
    match policy {
        SymlinkPolicy::Preserve => 0,
        SymlinkPolicy::FollowInsideRoot => 1,
        SymlinkPolicy::FollowAll => 2,
    }
}

fn hard_link_code(policy: HardLinkPolicy) -> u8 {
    match policy {
        HardLinkPolicy::Preserve => 0,
        HardLinkPolicy::Duplicate => 1,
    }
}

fn error_code(policy: ErrorPolicy) -> u8 {
    match policy {
        ErrorPolicy::Skip => 0,
        ErrorPolicy::Fail => 1,
    }
}

fn special_code(policy: SpecialFilePolicy) -> u8 {
    match policy {
        SpecialFilePolicy::Record => 0,
        SpecialFilePolicy::Skip => 1,
    }
}

fn change_code(policy: ChangePolicy) -> u8 {
    match policy {
        ChangePolicy::Fail => 0,
        ChangePolicy::Accept => 1,
    }
}

fn write_config(w: &mut Writer, config: &BackupConfig) -> Result<()> {
    for size in [
        config.chunker.min_size(),
        config.chunker.avg_size(),
        config.chunker.max_size(),
    ] {
        w.u32(u32::try_from(size).map_err(|_| too_large())?);
    }
    w.u8(symlink_code(config.symlinks));
    w.u8(hard_link_code(config.hard_links));
    w.u8(error_code(config.on_error));
    w.u8(special_code(config.special_files));
    w.u8(change_code(config.changed_files));
    w.flag(config.cross_filesystems);
    w.strings(&config.includes)?;
    w.strings(&config.excludes)
}

fn write_attributes(w: &mut Writer, attributes: &Attributes) {
    w.u64(attributes.size);
    w.optional_timestamp(attributes.modified);
    w.optional_timestamp(attributes.accessed);
    w.optional_timestamp(attributes.changed);
    w.optional_timestamp(attributes.created);
    w.optional_u32(attributes.mode);
    w.optional_u32(attributes.uid);
    w.optional_u32(attributes.gid);
    w.flag(attributes.readonly);
    w.optional_u64(attributes.device);
    w.optional_u64(attributes.inode);
    w.optional_u64(attributes.links);
}

fn write_entry(w: &mut Writer, entry: &ManifestEntry) -> Result<()> {
    w.u32(entry.parent.unwrap_or(NO_PARENT));
    w.bytes(&entry.name)?;
    match &entry.kind {
        NodeKind::Directory => w.u8(0),
        NodeKind::File { chunks } => {
            w.u8(1);
            w.count(chunks.len())?;
            for chunk in chunks {
                w.buf.extend_from_slice(chunk.id.as_bytes());
                w.u32(chunk.len);
            }
        }
        NodeKind::Symlink { target } => {
            w.u8(2);
            w.bytes(target)?;
        }
        NodeKind::HardLink { target } => {
            w.u8(3);
            w.u32(*target);
        }
        NodeKind::Fifo => w.u8(4),
        NodeKind::Socket => w.u8(5),
        NodeKind::CharDevice => w.u8(6),
        NodeKind::BlockDevice => w.u8(7),
        NodeKind::Other => w.u8(8),
    }
    write_attributes(w, &entry.attributes);
    Ok(())
}

fn write_skipped(w: &mut Writer, record: &SkippedRecord) -> Result<()> {
    w.u32(record.root);
    w.bytes(&record.path)?;
    match &record.reason {
        SkippedReason::Vanished => {
            w.u8(0);
            w.string("")
        }
        SkippedReason::Unreadable { detail } => {
            w.u8(1);
            w.string(detail)
        }
        SkippedReason::OtherFilesystem => {
            w.u8(2);
            w.string("")
        }
        SkippedReason::SpecialFile { kind } => {
            w.u8(3);
            w.string(kind)
        }
    }
}

/// Serializes `manifest`. It does not validate: callers validate before storing, and tests rely
/// on being able to encode manifests that are deliberately invalid.
pub(super) fn encode(manifest: &Manifest) -> Result<Zeroizing<Vec<u8>>> {
    let mut w = Writer::new();
    w.u32(manifest.format_version);
    w.buf.extend_from_slice(&manifest.id.to_bytes());
    w.string(&manifest.repository_id)?;
    w.timestamp(manifest.created);
    w.string(&manifest.host)?;
    w.strings(&manifest.tags)?;
    w.string(&manifest.program)?;
    write_config(&mut w, &manifest.config)?;

    w.count(manifest.roots.len())?;
    for root in &manifest.roots {
        w.bytes(&root.source)?;
        w.count(root.entries.len())?;
        for entry in &root.entries {
            write_entry(&mut w, entry)?;
        }
    }

    w.count(manifest.skipped.len())?;
    for record in &manifest.skipped {
        write_skipped(&mut w, record)?;
    }

    let stats = &manifest.stats;
    for value in [
        stats.directories,
        stats.files,
        stats.symlinks,
        stats.hard_links,
        stats.special_files,
        stats.bytes,
        stats.skipped,
    ] {
        w.u64(value);
    }

    if w.buf.len() > MAX_MANIFEST_BYTES {
        return Err(Error::InvalidManifest(
            "the manifest is larger than the format allows".to_owned(),
        ));
    }
    Ok(w.buf)
}

// ---------------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------------

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn fail<T>(&self, what: &str) -> Result<T> {
        Err(Error::InvalidManifest(format!(
            "{what} (at byte {})",
            self.pos
        )))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.data.len());
        let Some(end) = end else {
            return self.fail("the manifest is truncated");
        };
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let mut raw = [0_u8; 4];
        raw.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64> {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(raw))
    }

    fn i64(&mut self) -> Result<i64> {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(self.take(8)?);
        Ok(i64::from_le_bytes(raw))
    }

    fn flag(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => self.fail("a flag byte is neither 0 nor 1"),
        }
    }

    /// A list length, checked against the bytes left so a lie cannot cause a huge allocation.
    fn count(&mut self, min_item_bytes: usize) -> Result<usize> {
        let count = usize::try_from(self.u32()?).unwrap_or(usize::MAX);
        let remaining = self.data.len() - self.pos;
        if count
            .checked_mul(min_item_bytes)
            .is_none_or(|needed| needed > remaining)
        {
            return self.fail("a count is larger than the data that follows");
        }
        Ok(count)
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = usize::try_from(self.u32()?).unwrap_or(usize::MAX);
        self.take(len)
    }

    fn string(&mut self) -> Result<String> {
        let bytes = self.bytes()?;
        match String::from_utf8(bytes.to_vec()) {
            Ok(text) => Ok(text),
            Err(_) => self.fail("text is not valid UTF-8"),
        }
    }

    fn strings(&mut self) -> Result<Vec<String>> {
        let count = self.count(STRING_MIN_BYTES)?;
        let mut list = Vec::with_capacity(count);
        for _ in 0..count {
            list.push(self.string()?);
        }
        Ok(list)
    }

    fn timestamp(&mut self) -> Result<Timestamp> {
        let secs = self.i64()?;
        let nanos = self.u32()?;
        if nanos >= 1_000_000_000 {
            return self.fail("a timestamp has out-of-range nanoseconds");
        }
        Ok(Timestamp { secs, nanos })
    }

    fn optional_timestamp(&mut self) -> Result<Option<Timestamp>> {
        if self.flag()? {
            Ok(Some(self.timestamp()?))
        } else {
            Ok(None)
        }
    }

    fn optional_u32(&mut self) -> Result<Option<u32>> {
        if self.flag()? {
            Ok(Some(self.u32()?))
        } else {
            Ok(None)
        }
    }

    fn optional_u64(&mut self) -> Result<Option<u64>> {
        if self.flag()? {
            Ok(Some(self.u64()?))
        } else {
            Ok(None)
        }
    }

    fn finish(&self) -> Result<()> {
        if self.pos == self.data.len() {
            Ok(())
        } else {
            self.fail("unexpected data after the end of the manifest")
        }
    }
}

fn read_config(r: &mut Reader<'_>) -> Result<BackupConfig> {
    let mut sizes = [0_usize; 3];
    for size in &mut sizes {
        *size = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
    }
    let chunker = ChunkerConfig::new(sizes[0], sizes[1], sizes[2])
        .map_err(|err| Error::InvalidManifest(format!("invalid chunker settings: {err}")))?;
    let symlinks = match r.u8()? {
        0 => SymlinkPolicy::Preserve,
        1 => SymlinkPolicy::FollowInsideRoot,
        2 => SymlinkPolicy::FollowAll,
        _ => return r.fail("unknown symbolic link policy"),
    };
    let hard_links = match r.u8()? {
        0 => HardLinkPolicy::Preserve,
        1 => HardLinkPolicy::Duplicate,
        _ => return r.fail("unknown hard link policy"),
    };
    let on_error = match r.u8()? {
        0 => ErrorPolicy::Skip,
        1 => ErrorPolicy::Fail,
        _ => return r.fail("unknown error policy"),
    };
    let special_files = match r.u8()? {
        0 => SpecialFilePolicy::Record,
        1 => SpecialFilePolicy::Skip,
        _ => return r.fail("unknown special file policy"),
    };
    let changed_files = match r.u8()? {
        0 => ChangePolicy::Fail,
        1 => ChangePolicy::Accept,
        _ => return r.fail("unknown changed-file policy"),
    };
    Ok(BackupConfig {
        chunker,
        symlinks,
        hard_links,
        on_error,
        special_files,
        changed_files,
        cross_filesystems: r.flag()?,
        includes: r.strings()?,
        excludes: r.strings()?,
    })
}

fn read_attributes(r: &mut Reader<'_>) -> Result<Attributes> {
    Ok(Attributes {
        size: r.u64()?,
        modified: r.optional_timestamp()?,
        accessed: r.optional_timestamp()?,
        changed: r.optional_timestamp()?,
        created: r.optional_timestamp()?,
        mode: r.optional_u32()?,
        uid: r.optional_u32()?,
        gid: r.optional_u32()?,
        readonly: r.flag()?,
        device: r.optional_u64()?,
        inode: r.optional_u64()?,
        links: r.optional_u64()?,
    })
}

fn read_chunk_id(r: &mut Reader<'_>) -> Result<ChunkId> {
    let mut raw = [0_u8; ChunkId::LEN];
    raw.copy_from_slice(r.take(ChunkId::LEN)?);
    Ok(ChunkId::from_bytes(raw))
}

fn read_entry(r: &mut Reader<'_>) -> Result<ManifestEntry> {
    let parent = match r.u32()? {
        NO_PARENT => None,
        index => Some(index),
    };
    let name = r.bytes()?.to_vec();
    let kind = match r.u8()? {
        0 => NodeKind::Directory,
        1 => {
            let count = r.count(CHUNK_REF_BYTES)?;
            let mut chunks = Vec::with_capacity(count);
            for _ in 0..count {
                let id = read_chunk_id(r)?;
                chunks.push(ChunkRef { id, len: r.u32()? });
            }
            NodeKind::File { chunks }
        }
        2 => NodeKind::Symlink {
            target: r.bytes()?.to_vec(),
        },
        3 => NodeKind::HardLink { target: r.u32()? },
        4 => NodeKind::Fifo,
        5 => NodeKind::Socket,
        6 => NodeKind::CharDevice,
        7 => NodeKind::BlockDevice,
        8 => NodeKind::Other,
        _ => return r.fail("unknown entry kind"),
    };
    Ok(ManifestEntry {
        parent,
        name,
        kind,
        attributes: read_attributes(r)?,
    })
}

fn read_root(r: &mut Reader<'_>) -> Result<Root> {
    let source = r.bytes()?.to_vec();
    let count = r.count(ENTRY_MIN_BYTES)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push(read_entry(r)?);
    }
    Ok(Root { source, entries })
}

fn read_skipped(r: &mut Reader<'_>) -> Result<SkippedRecord> {
    let root = r.u32()?;
    let path = r.bytes()?.to_vec();
    let code = r.u8()?;
    let detail = r.string()?;
    let reason = match (code, detail.is_empty()) {
        (0, true) => SkippedReason::Vanished,
        (1, _) => SkippedReason::Unreadable { detail },
        (2, true) => SkippedReason::OtherFilesystem,
        (3, _) => SkippedReason::SpecialFile { kind: detail },
        _ => return r.fail("unknown or malformed skip reason"),
    };
    Ok(SkippedRecord { root, path, reason })
}

fn decode_v1(r: &mut Reader<'_>) -> Result<Manifest> {
    let mut raw_id = [0_u8; 32];
    raw_id.copy_from_slice(r.take(32)?);
    let id = SnapshotId::from_bytes(&raw_id);
    let repository_id = r.string()?;
    let created = r.timestamp()?;
    let host = r.string()?;
    let tags = r.strings()?;
    let program = r.string()?;
    let config = read_config(r)?;

    let root_count = r.count(ROOT_MIN_BYTES)?;
    let mut roots = Vec::with_capacity(root_count);
    for _ in 0..root_count {
        roots.push(read_root(r)?);
    }
    let skipped_count = r.count(SKIPPED_MIN_BYTES)?;
    let mut skipped = Vec::with_capacity(skipped_count);
    for _ in 0..skipped_count {
        skipped.push(read_skipped(r)?);
    }
    let stats = ManifestStats {
        directories: r.u64()?,
        files: r.u64()?,
        symlinks: r.u64()?,
        hard_links: r.u64()?,
        special_files: r.u64()?,
        bytes: r.u64()?,
        skipped: r.u64()?,
    };
    Ok(Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        id,
        repository_id,
        created,
        host,
        tags,
        program,
        config,
        roots,
        skipped,
        stats,
    })
}

/// Parses and validates a manifest. The schema version is read first and an unknown version is
/// reported as such, before anything else is interpreted. New versions add a branch here and keep
/// the old ones, so historical snapshots stay readable.
pub(super) fn decode(bytes: &[u8]) -> Result<Manifest> {
    let mut reader = Reader {
        data: bytes,
        pos: 0,
    };
    let version = reader.u32()?;
    let manifest = match version {
        MANIFEST_FORMAT_VERSION => decode_v1(&mut reader)?,
        found => {
            return Err(Error::UnsupportedManifestVersion {
                found,
                supported: MANIFEST_FORMAT_VERSION,
            })
        }
    };
    reader.finish()?;
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::super::model::testing::*;
    use super::*;

    /// The documented layout, assembled by hand and independently of the encoder.
    struct Golden;

    impl Golden {
        fn header() -> Vec<u8> {
            let mut b = Vec::new();
            b.extend(1_u32.to_le_bytes()); // schema version
            b.extend([0xcd_u8; 32]); // snapshot id
            b.extend(32_u32.to_le_bytes()); // repository id: length, text
            b.extend(REPO_ID.as_bytes());
            b.extend(0_i64.to_le_bytes()); // created: seconds
            b.extend(0_u32.to_le_bytes()); // created: nanoseconds
            b.extend(0_u32.to_le_bytes()); // host (empty)
            b.extend(0_u32.to_le_bytes()); // tags (none)
            b.extend(0_u32.to_le_bytes()); // program (empty)
            b.extend(262_144_u32.to_le_bytes()); // chunker minimum, 256 KiB
            b.extend(1_048_576_u32.to_le_bytes()); // chunker average, 1 MiB
            b.extend(4_194_304_u32.to_le_bytes()); // chunker maximum, 4 MiB
            b.extend([0_u8; 5]); // five policies: all defaults, code 0
            b.push(0); // cross_filesystems = false
            b.extend(0_u32.to_le_bytes()); // includes (none)
            b.extend(0_u32.to_le_bytes()); // excludes (none)
            b
        }

        /// One root `/r` holding one empty directory entry with no optional attributes.
        fn minimal_root() -> Vec<u8> {
            let mut b = Vec::new();
            b.extend(2_u32.to_le_bytes()); // source: length, text
            b.extend(b"/r");
            b.extend(1_u32.to_le_bytes()); // entry count
            b.extend(u32::MAX.to_le_bytes()); // parent: none
            b.extend(0_u32.to_le_bytes()); // name (empty)
            b.push(0); // kind: directory
            b.extend(0_u64.to_le_bytes()); // size
            b.extend([0_u8; 11]); // eleven optional/flag fields, all absent or false
            b
        }

        fn assemble(root_count: u32, roots: &[u8], skipped_count: u32, stats: [u64; 7]) -> Vec<u8> {
            let mut b = Self::header();
            b.extend(root_count.to_le_bytes());
            b.extend(roots);
            b.extend(skipped_count.to_le_bytes());
            for value in stats {
                b.extend(value.to_le_bytes());
            }
            b
        }

        fn minimal() -> Vec<u8> {
            Self::assemble(1, &Self::minimal_root(), 0, [1, 0, 0, 0, 0, 0, 0])
        }
    }

    fn encoded(manifest: &Manifest) -> Vec<u8> {
        encode(manifest).expect("encode").to_vec()
    }

    fn rejects(bytes: &[u8], why: &str) {
        let result = decode(bytes);
        assert!(result.is_err(), "{why}: unexpectedly decoded");
    }

    #[test]
    fn the_encoder_produces_exactly_the_documented_layout() {
        let golden = Golden::minimal();
        assert_eq!(
            golden.len(),
            224,
            "the hand-assembled layout has the expected size"
        );
        assert_eq!(encoded(&minimal()), golden);
        assert_eq!(decode(&golden).expect("decodes"), minimal());
    }

    #[test]
    fn manifests_round_trip() {
        for manifest in [minimal(), rich()] {
            let bytes = encoded(&manifest);
            assert_eq!(decode(&bytes).expect("decodes"), manifest);
            assert_eq!(
                encoded(&decode(&bytes).expect("decodes")),
                bytes,
                "stable re-encoding"
            );
        }
    }

    #[test]
    fn names_and_paths_survive_as_exact_bytes() {
        let manifest = rich();
        let decoded = decode(&encoded(&manifest)).expect("decodes");
        assert_eq!(decoded.roots[0].entries[6].name, b"bad-\xff\xfe-name");
        assert_eq!(
            decoded.roots[0].entries[7].name,
            "unicode-日本語".as_bytes()
        );
        assert_eq!(decoded.roots[0].source, b"/home/me/Documents");
    }

    #[test]
    fn encoding_is_deterministic_and_does_not_embed_plaintext_padding() {
        let manifest = rich();
        assert_eq!(encoded(&manifest), encoded(&manifest));
    }

    #[test]
    fn the_minimum_sizes_used_to_reject_lying_counts_are_exact() {
        let mut w = Writer::new();
        let entry = ManifestEntry {
            parent: None,
            name: Vec::new(),
            kind: NodeKind::Directory,
            attributes: bare_attributes(),
        };
        write_entry(&mut w, &entry).expect("encode entry");
        assert_eq!(w.buf.len(), ENTRY_MIN_BYTES);

        let mut w = Writer::new();
        w.buf.extend_from_slice(chunk(1, 1).id.as_bytes());
        w.u32(1);
        assert_eq!(w.buf.len(), CHUNK_REF_BYTES);

        let mut w = Writer::new();
        w.bytes(b"").expect("encode");
        w.count(0).expect("encode");
        assert_eq!(w.buf.len(), ROOT_MIN_BYTES);

        let mut w = Writer::new();
        let record = SkippedRecord {
            root: 0,
            path: Vec::new(),
            reason: SkippedReason::Vanished,
        };
        write_skipped(&mut w, &record).expect("encode skipped");
        assert_eq!(w.buf.len(), SKIPPED_MIN_BYTES);

        let mut w = Writer::new();
        w.string("").expect("encode");
        assert_eq!(w.buf.len(), STRING_MIN_BYTES);
    }

    #[test]
    fn every_truncation_is_rejected() {
        let bytes = encoded(&rich());
        for len in 0..bytes.len() {
            rejects(&bytes[..len], &format!("truncated to {len} bytes"));
        }
    }

    #[test]
    fn trailing_data_is_rejected() {
        let mut bytes = Golden::minimal();
        bytes.push(0);
        rejects(&bytes, "one trailing byte");
        let mut bytes = encoded(&rich());
        bytes.extend_from_slice(b"garbage");
        rejects(&bytes, "trailing garbage");
    }

    #[test]
    fn unknown_schema_versions_are_reported_before_anything_else_is_read() {
        for version in [0_u32, 2, 3, u32::MAX] {
            let mut bytes = Golden::minimal();
            bytes[..4].copy_from_slice(&version.to_le_bytes());
            let result = decode(&bytes);
            assert!(
                matches!(result, Err(Error::UnsupportedManifestVersion { found, supported: 1 }) if found == version),
                "{version}: {result:?}"
            );
        }
        // Even a body that makes no sense in any version is judged by its version first.
        let mut nonsense = vec![0xff; 40];
        nonsense[..4].copy_from_slice(&7_u32.to_le_bytes());
        assert!(matches!(
            decode(&nonsense),
            Err(Error::UnsupportedManifestVersion { found: 7, .. })
        ));
    }

    #[test]
    fn lying_counts_fail_without_allocating() {
        let huge = u32::MAX;
        rejects(&Golden::assemble(huge, &[], 0, [0; 7]), "root count");

        let mut root = Vec::new();
        root.extend(2_u32.to_le_bytes());
        root.extend(b"/r");
        root.extend(huge.to_le_bytes());
        rejects(&Golden::assemble(1, &root, 0, [0; 7]), "entry count");

        let mut root = Golden::minimal_root();
        let entry_start = 4 + 2 + 4;
        root.truncate(entry_start + 4 + 4);
        root.push(1); // kind: file
        root.extend(huge.to_le_bytes()); // chunk count
        rejects(&Golden::assemble(1, &root, 0, [0; 7]), "chunk count");

        rejects(
            &Golden::assemble(1, &Golden::minimal_root(), huge, [0; 7]),
            "skipped count",
        );

        let mut bytes = Golden::minimal();
        bytes[114..118].copy_from_slice(&huge.to_le_bytes()); // includes count
        rejects(&bytes, "list count");

        let mut bytes = Golden::minimal();
        bytes[36..40].copy_from_slice(&huge.to_le_bytes()); // repository id length
        rejects(&bytes, "string length");
    }

    #[test]
    fn single_bytes_with_impossible_values_are_rejected() {
        let patches: [(usize, u8, &str); 9] = [
            (108, 9, "symbolic link policy"),
            (109, 9, "hard link policy"),
            (110, 9, "error policy"),
            (111, 9, "special file policy"),
            (112, 9, "changed-file policy"),
            (113, 2, "cross-filesystems flag"),
            (144, 99, "entry kind"),
            (153, 7, "optional field flag"),
            (160, 3, "read-only flag"),
        ];
        for (offset, value, what) in patches {
            let mut bytes = Golden::minimal();
            bytes[offset] = value;
            rejects(&bytes, what);
        }
    }

    #[test]
    fn invalid_text_and_timestamps_are_rejected() {
        let mut bytes = Golden::minimal();
        bytes[40] = 0xff; // first byte of the repository id: not UTF-8
        rejects(&bytes, "invalid UTF-8");

        let mut bytes = Golden::minimal();
        bytes[40] = b'X'; // valid UTF-8 but not a lowercase hex repository id
        rejects(&bytes, "malformed repository id");

        let mut bytes = Golden::minimal();
        bytes[80..84].copy_from_slice(&1_000_000_000_u32.to_le_bytes());
        rejects(&bytes, "creation nanoseconds");
    }

    #[test]
    fn structurally_invalid_manifests_fail_to_decode_even_if_well_encoded() {
        let mut manifest = rich();
        manifest.roots[0].entries[2].name = b"a/b".to_vec();
        rejects(&encoded(&manifest), "slash in a name");

        let mut manifest = rich();
        manifest.roots[0].entries[2].parent = Some(11);
        rejects(&encoded(&manifest), "child before its parent");

        let mut manifest = rich();
        manifest.stats.files = 99;
        rejects(&encoded(&manifest), "wrong totals");

        let mut manifest = rich();
        manifest.config = BackupConfig {
            chunker: ChunkerConfig::new(1024, 4096, 16_384).expect("valid"),
            ..manifest.config
        };
        decode(&encoded(&manifest)).expect("a different valid chunker configuration is fine");
    }

    #[test]
    fn an_invalid_chunker_configuration_in_the_bytes_is_rejected() {
        let mut bytes = Golden::minimal();
        bytes[96..100].copy_from_slice(&10_u32.to_le_bytes()); // minimum below the limit
        rejects(&bytes, "chunker minimum");
        let mut bytes = Golden::minimal();
        bytes[100..104].copy_from_slice(&u32::MAX.to_le_bytes()); // average above the maximum
        rejects(&bytes, "chunker average");
    }

    #[test]
    fn random_garbage_never_panics() {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for round in 0..300_usize {
            let len = (round * 7) % 400;
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                bytes.push((state >> 32) as u8);
            }
            let _ = decode(&bytes);
            if bytes.len() >= 4 {
                bytes[..4].copy_from_slice(&1_u32.to_le_bytes());
                let _ = decode(&bytes);
            }
        }
    }

    #[test]
    fn mutating_any_single_byte_never_panics() {
        let bytes = encoded(&rich());
        for index in 0..bytes.len() {
            for flip in [0x01_u8, 0x80, 0xff] {
                let mut mutated = bytes.clone();
                mutated[index] ^= flip;
                let _ = decode(&mutated);
            }
        }
    }
}
