//! Shared helpers for unit tests.

use crate::config::RepositoryConfig;
use crate::crypto::KdfParams;
use crate::error::Result;
use crate::repository::Repository;
use crate::secret::Passphrase;

pub(crate) const PASSPHRASE: &str = "correct horse battery staple";

pub(crate) fn passphrase() -> Passphrase {
    Passphrase::new(PASSPHRASE.as_bytes().to_vec()).expect("valid passphrase")
}

/// The cheapest parameters Argon2 accepts, so tests do not spend time or memory on hashing.
pub(crate) fn fast_kdf() -> KdfParams {
    KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    }
}

pub(crate) async fn init(repo: &Repository) -> Result<RepositoryConfig> {
    repo.init_with_kdf(&passphrase(), fast_kdf()).await
}
