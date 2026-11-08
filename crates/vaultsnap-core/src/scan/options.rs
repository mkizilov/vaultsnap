//! The explicit policies of the scanner. Every default is the conservative choice; see
//! `docs/SCANNING.md` for the reasoning and the platform differences.

/// What to do with symbolic links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymlinkPolicy {
    /// Record each symbolic link as a link (its target text) and never follow it. A link never
    /// causes anything outside the root, or anything twice, to be read. This is the default.
    #[default]
    Preserve,
    /// Follow a link only if what it ultimately points to lies inside the scan root; the target is
    /// then recorded as an ordinary file or directory at the link's path. Links that point
    /// outside the root, dangling links, link loops, and links to an ancestor directory are
    /// recorded as links instead.
    FollowInsideRoot,
    /// Follow every link that resolves, including to places outside the root. Dangling links,
    /// loops, and links to an ancestor directory are still recorded as links. This can read data
    /// the user never asked to back up; it must be chosen explicitly.
    FollowAll,
}

/// What to do with files that have several hard links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HardLinkPolicy {
    /// The first path (in scan order) to reach a file with more than one link is recorded as a
    /// regular file; later paths to the same file are recorded as links to it, so its data is
    /// stored once and the links can be recreated. This is the default.
    #[default]
    Preserve,
    /// Record every path as an independent regular file with its own copy of the data.
    Duplicate,
}

/// What to do when part of the tree cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorPolicy {
    /// Report the path as skipped and carry on with the rest of the tree. This is the default;
    /// the caller sees every skipped path and decides how to treat a partial result.
    #[default]
    Skip,
    /// Stop at the first error and return it.
    Fail,
}

/// What to do with FIFOs, sockets, and device nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpecialFilePolicy {
    /// Record them as entries carrying only metadata. They are never opened or read, so a FIFO
    /// cannot block the scan. This is the default.
    #[default]
    Record,
    /// Report them as skipped.
    Skip,
}

/// What to do when a file changes while it is being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChangePolicy {
    /// Fail the read at end of file, so the caller cannot mistake torn data for a consistent
    /// copy. This is the default.
    #[default]
    Fail,
    /// Let the read succeed but mark the reader as changed.
    Accept,
}

/// Everything that controls a scan.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Symbolic link handling.
    pub symlinks: SymlinkPolicy,
    /// Hard link handling.
    pub hard_links: HardLinkPolicy,
    /// Handling of unreadable paths.
    pub on_error: ErrorPolicy,
    /// Handling of FIFOs, sockets, and devices.
    pub special_files: SpecialFilePolicy,
    /// Handling of files that change while being read.
    pub changed_files: ChangePolicy,
    /// Descend into directories on other filesystems (mount points). Off by default: such
    /// directories are reported as skipped.
    pub cross_filesystems: bool,
    /// Glob patterns; if any are given, only non-directory entries matching one are recorded.
    pub includes: Vec<String>,
    /// Glob patterns; entries (and everything below excluded directories) matching one are
    /// omitted. Exclusion wins over inclusion.
    pub excludes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_conservative_choices() {
        let options = ScanOptions::default();
        assert_eq!(options.symlinks, SymlinkPolicy::Preserve);
        assert_eq!(options.hard_links, HardLinkPolicy::Preserve);
        assert_eq!(options.on_error, ErrorPolicy::Skip);
        assert_eq!(options.special_files, SpecialFilePolicy::Record);
        assert_eq!(options.changed_files, ChangePolicy::Fail);
        assert!(!options.cross_filesystems);
        assert!(options.includes.is_empty() && options.excludes.is_empty());
    }
}
