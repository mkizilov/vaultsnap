//! Cryptographic building blocks, all taken from established RustCrypto crates:
//! Argon2id (`argon2`) turns a passphrase into a key-encryption key, HKDF-SHA256 (`hkdf`,
//! `sha2`) derives one independent key per purpose from the master key, and
//! XChaCha20-Poly1305 (`chacha20poly1305`) provides authenticated encryption. This module only
//! wires them together; it implements no primitive itself. See `docs/FORMAT.md`.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::util::random_bytes;

/// Length of every symmetric key, in bytes.
pub const KEY_LEN: usize = 32;
/// Length of the random XChaCha20 nonce prepended to every sealed message.
pub const NONCE_LEN: usize = 24;
/// Length of the Poly1305 authentication tag appended to every sealed message.
pub const TAG_LEN: usize = 16;
/// Length of the random Argon2id salt.
pub const SALT_LEN: usize = 16;

/// A symmetric key that is zeroed when dropped.
pub type SecretKey = Zeroizing<[u8; KEY_LEN]>;

/// Argon2id cost parameters. They are stored with every key file, so they can be raised for new
/// repositories without breaking existing ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_cost_kib: u32,
    /// Number of passes.
    pub t_cost: u32,
    /// Degree of parallelism (lanes).
    pub p_cost: u32,
}

impl KdfParams {
    /// Largest memory cost accepted when reading a key file (1 GiB), so a modified file cannot
    /// make unlocking exhaust memory.
    pub const MAX_M_COST_KIB: u32 = 1_048_576;
    /// Largest accepted number of passes.
    pub const MAX_T_COST: u32 = 16;
    /// Largest accepted parallelism.
    pub const MAX_P_COST: u32 = 16;

    /// Checks the parameters against Argon2's own limits and VaultSnap's upper bounds.
    pub fn validate(&self) -> Result<()> {
        if self.m_cost_kib > Self::MAX_M_COST_KIB
            || self.t_cost > Self::MAX_T_COST
            || self.p_cost > Self::MAX_P_COST
        {
            return Err(Error::InvalidKdfParams(format!(
                "memory {} KiB, passes {}, lanes {} exceed the limits ({} KiB, {}, {})",
                self.m_cost_kib,
                self.t_cost,
                self.p_cost,
                Self::MAX_M_COST_KIB,
                Self::MAX_T_COST,
                Self::MAX_P_COST
            )));
        }
        self.argon2().map(|_| ())
    }

    fn argon2(&self) -> Result<Argon2<'static>> {
        let params = Params::new(self.m_cost_kib, self.t_cost, self.p_cost, Some(KEY_LEN))
            .map_err(|err| Error::InvalidKdfParams(err.to_string()))?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

impl Default for KdfParams {
    /// 64 MiB, 3 passes, 4 lanes: the RFC 9106 recommendation for memory-constrained use.
    fn default() -> Self {
        Self {
            m_cost_kib: 64 * 1024,
            t_cost: 3,
            p_cost: 4,
        }
    }
}

/// Derives the key-encryption key for a key file from `passphrase` and its random `salt`.
pub fn derive_wrapping_key(
    passphrase: &[u8],
    salt: &[u8; SALT_LEN],
    params: &KdfParams,
) -> Result<SecretKey> {
    let argon2 = params.argon2()?;
    let mut key = Zeroizing::new([0_u8; KEY_LEN]);
    argon2
        .hash_password_into(passphrase, salt, &mut *key)
        .map_err(|err| Error::Kdf(err.to_string()))?;
    Ok(key)
}

/// What a derived key is for. Each purpose gets an independent key, so a weakness or misuse in
/// one place cannot be turned against another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyPurpose {
    /// Keyed hash that names content chunks without revealing whether known data is present.
    ContentId,
    /// Encryption of pack files holding file data.
    DataEncryption,
    /// Encryption of snapshot and index objects.
    MetadataEncryption,
}

impl KeyPurpose {
    /// Every purpose.
    pub const ALL: [Self; 3] = [
        Self::ContentId,
        Self::DataEncryption,
        Self::MetadataEncryption,
    ];

    /// The HKDF `info` string. These labels are part of the on-disk format and must never change
    /// or be reused for another purpose.
    fn label(self) -> &'static [u8] {
        match self {
            Self::ContentId => b"vaultsnap/v1/content-id",
            Self::DataEncryption => b"vaultsnap/v1/data-encryption",
            Self::MetadataEncryption => b"vaultsnap/v1/metadata-encryption",
        }
    }
}

/// Derives the key for `purpose` from `master` with HKDF-SHA256.
pub fn derive_key(master: &[u8; KEY_LEN], purpose: KeyPurpose) -> Result<SecretKey> {
    let hkdf = Hkdf::<Sha256>::new(None, master);
    let mut key = Zeroizing::new([0_u8; KEY_LEN]);
    hkdf.expand(purpose.label(), &mut *key)
        .map_err(|_| Error::Crypto("key derivation failed"))?;
    Ok(key)
}

/// Encrypts and authenticates `plaintext` with XChaCha20-Poly1305 under `key`, binding `aad`
/// (data that is authenticated but not encrypted). Returns `nonce || ciphertext || tag`; the
/// 192-bit nonce is random per call, which is safe at any realistic volume.
pub fn seal(key: &[u8; KEY_LEN], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher =
        XChaCha20Poly1305::new_from_slice(key).map_err(|_| Error::Crypto("invalid key length"))?;
    let nonce = random_bytes::<NONCE_LEN>()?;
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Crypto("encryption failed"))?;
    let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

/// Verifies and decrypts the output of [`seal`]. Any change to the key, `aad`, or sealed bytes
/// yields [`Error::Decryption`]; no plaintext is released unless authentication succeeds.
pub fn open(key: &[u8; KEY_LEN], aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(Error::Decryption);
    }
    let cipher =
        XChaCha20Poly1305::new_from_slice(key).map_err(|_| Error::Crypto("invalid key length"))?;
    let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| Error::Decryption)?;
    Ok(Zeroizing::new(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; KEY_LEN] = [7; KEY_LEN];

    fn fast() -> KdfParams {
        KdfParams {
            m_cost_kib: 64,
            t_cost: 1,
            p_cost: 1,
        }
    }

    #[test]
    fn seal_then_open_round_trips() {
        let sealed = seal(&KEY, b"context", b"attack at dawn").expect("seal");
        assert_eq!(sealed.len(), NONCE_LEN + b"attack at dawn".len() + TAG_LEN);
        let opened = open(&KEY, b"context", &sealed).expect("open");
        assert_eq!(opened.as_slice(), b"attack at dawn");
    }

    #[test]
    fn empty_plaintext_round_trips() {
        let sealed = seal(&KEY, b"", b"").expect("seal");
        assert_eq!(sealed.len(), NONCE_LEN + TAG_LEN);
        assert!(open(&KEY, b"", &sealed).expect("open").is_empty());
    }

    #[test]
    fn plaintext_does_not_appear_in_the_output() {
        let sealed = seal(&KEY, b"", b"very recognizable plaintext").expect("seal");
        let needle = b"recognizable";
        assert!(!sealed.windows(needle.len()).any(|window| window == needle));
    }

    #[test]
    fn nonces_are_fresh_for_every_message() {
        let a = seal(&KEY, b"", b"same").expect("seal");
        let b = seal(&KEY, b"", b"same").expect("seal");
        assert_ne!(a[..NONCE_LEN], b[..NONCE_LEN]);
        assert_ne!(a, b);
    }

    #[test]
    fn a_wrong_key_wrong_aad_or_truncation_is_rejected() {
        let sealed = seal(&KEY, b"aad", b"secret").expect("seal");
        let other = [8_u8; KEY_LEN];
        assert!(matches!(
            open(&other, b"aad", &sealed),
            Err(Error::Decryption)
        ));
        assert!(matches!(
            open(&KEY, b"AAD", &sealed),
            Err(Error::Decryption)
        ));
        assert!(matches!(open(&KEY, b"", &sealed), Err(Error::Decryption)));
        for len in [0, 1, NONCE_LEN, NONCE_LEN + TAG_LEN - 1, sealed.len() - 1] {
            assert!(
                matches!(open(&KEY, b"aad", &sealed[..len]), Err(Error::Decryption)),
                "{len}"
            );
        }
    }

    #[test]
    fn flipping_any_single_bit_is_detected() {
        let sealed = seal(&KEY, b"aad", b"secret message").expect("seal");
        for index in 0..sealed.len() {
            for bit in 0..8 {
                let mut tampered = sealed.clone();
                tampered[index] ^= 1 << bit;
                let result = open(&KEY, b"aad", &tampered);
                assert!(
                    matches!(result, Err(Error::Decryption)),
                    "byte {index} bit {bit}"
                );
            }
        }
    }

    #[test]
    fn purposes_yield_distinct_deterministic_keys() {
        let master = [1_u8; KEY_LEN];
        let keys: Vec<SecretKey> = KeyPurpose::ALL
            .iter()
            .map(|purpose| derive_key(&master, *purpose).expect("derive"))
            .collect();
        for (i, a) in keys.iter().enumerate() {
            assert_ne!(**a, master, "a derived key must differ from the master key");
            for b in &keys[i + 1..] {
                assert_ne!(**a, **b, "purposes must not share a key");
            }
        }
        let again = derive_key(&master, KeyPurpose::DataEncryption).expect("derive");
        assert_eq!(*again, *keys[1]);
    }

    #[test]
    fn derived_keys_depend_on_the_master_key() {
        let a = derive_key(&[1; KEY_LEN], KeyPurpose::ContentId).expect("derive");
        let b = derive_key(&[2; KEY_LEN], KeyPurpose::ContentId).expect("derive");
        assert_ne!(*a, *b);
    }

    #[test]
    fn a_key_for_one_purpose_cannot_open_another_purposes_data() {
        let master = [3_u8; KEY_LEN];
        let data_key = derive_key(&master, KeyPurpose::DataEncryption).expect("derive");
        let meta_key = derive_key(&master, KeyPurpose::MetadataEncryption).expect("derive");
        let sealed = seal(&data_key, b"", b"chunk").expect("seal");
        assert!(matches!(
            open(&meta_key, b"", &sealed),
            Err(Error::Decryption)
        ));
    }

    #[test]
    fn wrapping_key_depends_on_passphrase_salt_and_parameters() {
        let salt = [9_u8; SALT_LEN];
        let base = derive_wrapping_key(b"pass", &salt, &fast()).expect("derive");
        assert_eq!(
            *base,
            *derive_wrapping_key(b"pass", &salt, &fast()).expect("derive")
        );
        assert_ne!(
            *base,
            *derive_wrapping_key(b"Pass", &salt, &fast()).expect("derive")
        );
        assert_ne!(
            *base,
            *derive_wrapping_key(b"pass", &[8; SALT_LEN], &fast()).expect("derive")
        );
        let slower = KdfParams {
            t_cost: 2,
            ..fast()
        };
        assert_ne!(
            *base,
            *derive_wrapping_key(b"pass", &salt, &slower).expect("derive")
        );
    }

    #[test]
    fn kdf_parameters_are_range_checked() {
        assert!(KdfParams::default().validate().is_ok());
        assert!(fast().validate().is_ok());
        let bad = [
            KdfParams {
                m_cost_kib: KdfParams::MAX_M_COST_KIB + 1,
                ..fast()
            },
            KdfParams {
                t_cost: KdfParams::MAX_T_COST + 1,
                ..fast()
            },
            KdfParams {
                p_cost: KdfParams::MAX_P_COST + 1,
                ..fast()
            },
            KdfParams {
                t_cost: 0,
                ..fast()
            },
            KdfParams {
                p_cost: 0,
                ..fast()
            },
            KdfParams {
                m_cost_kib: 1,
                ..fast()
            },
        ];
        for params in bad {
            assert!(
                matches!(params.validate(), Err(Error::InvalidKdfParams(_))),
                "{params:?}"
            );
        }
    }
}
