//! Encrypted, deduplicated, content-addressed chunk storage.
//!
//! A [`ChunkStore`] stores byte chunks (typically produced by [`crate::chunking`]) as
//! individual objects below a repository's `data/` directory. Each object is named by a keyed
//! BLAKE3 hash of its plaintext ([`ChunkId`]), optionally compressed with zstd, and sealed with
//! XChaCha20-Poly1305 under a random nonce, with the object header and the chunk id
//! authenticated. The byte-level format is specified in `docs/FORMAT.md`.
//!
//! ```
//! # use vaultsnap_core::{ChunkStore, KdfParams, Passphrase, Repository};
//! # async fn demo(root: &std::path::Path) -> vaultsnap_core::Result<()> {
//! let repo = Repository::at(root);
//! let passphrase = Passphrase::new(b"correct horse battery staple".to_vec())?;
//! repo.init_with_kdf(&passphrase, KdfParams { m_cost_kib: 64, t_cost: 1, p_cost: 1 }).await?;
//! let keys = repo.unlock(&passphrase).await?;
//! let chunks: ChunkStore = repo.chunk_store(&keys)?;
//! let stored = chunks.put(b"some chunk of file data")?;
//! assert_eq!(chunks.get(&stored.id)?, b"some chunk of file data");
//! # Ok(())
//! # }
//! ```

mod id;
mod object;
mod store;

pub use id::ChunkId;
pub use object::{Compression, MAX_CHUNK_BYTES, OBJECT_FORMAT_VERSION};
pub use store::{ChunkStore, PutOutcome, PutResult};
