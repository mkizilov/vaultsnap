//! A robust filesystem scanner: deterministic, streaming, policy-driven, and independent of
//! storage.
//!
//! [`Scanner`] walks a directory (or a single file) and yields [`ScanItem`]s: [`Entry`]s with
//! type, size, timestamps, permissions, ownership, and link information, and [`Skipped`] records
//! for anything left out and why. File content is read separately and as a stream through
//! [`Scanner::open`], which returns a [`FileReader`] that plugs into
//! [`crate::chunking::Chunker`].
//!
//! Every behavior that depends on a judgment call is an explicit option in [`ScanOptions`]:
//! symbolic links ([`SymlinkPolicy`]: never followed by default, and never outside the root unless
//! `FollowAll` is chosen), hard links ([`HardLinkPolicy`]), unreadable paths ([`ErrorPolicy`]),
//! files that change ([`ChangePolicy`], plus vanishing during the walk), filesystem boundaries
//! (`cross_filesystems`), and special files ([`SpecialFilePolicy`]). Include and exclude patterns
//! are described in [`ScanOptions::includes`] and the `filter` module. The full policy table,
//! cross-platform behavior, and limitations are in `docs/SCANNING.md`.
//!
//! ```
//! use vaultsnap_core::scan::{ScanItem, ScanOptions, Scanner};
//!
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let scanner = Scanner::new(".", ScanOptions::default())?;
//! for item in scanner.scan() {
//!     if let ScanItem::Entry(entry) = item? {
//!         if entry.is_file() {
//!             let mut reader = scanner.open(&entry)?;
//!             std::io::copy(&mut reader, &mut std::io::sink())?;
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```

mod entry;
mod filter;
mod options;
mod reader;
mod walker;

pub use entry::{Entry, EntryKind, Metadata, ScanItem, ScanStats, SkipReason, Skipped, Timestamp};
pub use options::{
    ChangePolicy, ErrorPolicy, HardLinkPolicy, ScanOptions, SpecialFilePolicy, SymlinkPolicy,
};
pub use reader::FileReader;
pub use walker::{Scan, Scanner};
