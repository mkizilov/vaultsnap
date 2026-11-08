//! Streaming content-defined chunking (FastCDC with normalized chunking).
//!
//! Files are cut into variable-size chunks at positions chosen by their *content*, so inserting or
//! deleting a few bytes only changes the chunks around the edit and every other chunk keeps its
//! bytes, which is what makes incremental backup efficient. The algorithm and its parameters are
//! part of the repository format; see `docs/FORMAT.md`.
//!
//! This module is independent of disk storage and of the repository: [`Chunker`] works on any
//! [`std::io::Read`], and [`ChunkerConfig::cut_point`] / [`ChunkerConfig::split`] work on plain
//! byte slices for callers that do their own buffering.
//!
//! ```
//! use vaultsnap_core::chunking::{Chunker, ChunkerConfig};
//!
//! let config = ChunkerConfig::new(64, 256, 1024)?;
//! let data = vec![7_u8; 5000];
//! let mut total = 0;
//! for chunk in Chunker::new(&data[..], config) {
//!     total += chunk?.data.len();
//! }
//! assert_eq!(total, data.len());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod config;
mod gear;
mod stream;
#[cfg(test)]
pub(crate) mod test_util;

pub use config::ChunkerConfig;
pub use stream::{Chunk, ChunkBuf, Chunker};
