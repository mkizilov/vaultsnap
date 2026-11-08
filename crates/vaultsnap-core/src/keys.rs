//! The repository master key and the key files that protect it (see `docs/FORMAT.md`).
//!
//! `init` generates a random 256-bit master key. Each file in `keys/` is a *key slot*: the master
//! key encrypted with XChaCha20-Poly1305 under a key-encryption key derived from a passphrase with
//! Argon2id and a per-slot random salt. The slot's own header (version, algorithms, cost
//! parameters, salt) and the repository id are authenticated as associated data, so a slot cannot
//! be edited, downgraded, or moved to another repository without failing to open.

use std::fmt;

use zeroize::Zeroizing;

use crate::crypto::{
    self, KdfParams, KeyPurpose, SecretKey, KEY_LEN, NONCE_LEN, SALT_LEN, TAG_LEN,
};
use crate::error::{Error, Result};
use crate::kv;
use crate::repository::RepositoryLayout;
use crate::secret::Passphrase;
use crate::store::Store;
use crate::util::{fill_random, hex, random_bytes, random_hex};

const MAGIC: &str = "vaultsnap-key";
/// Version of the key file format written by this build.
pub const KEY_FILE_VERSION: u32 = 1;
const KDF_NAME: &str = "argon2id";
const ARGON2_VERSION: u32 = 0x13;
const CIPHER_NAME: &str = "xchacha20poly1305";
const MAX_KEY_FILE_BYTES: u64 = 4096;
const SEALED_LEN: usize = NONCE_LEN + KEY_LEN + TAG_LEN;

/// The unlocked master key. It is zeroed on drop and never printed.
pub struct RepositoryKeys {
    master: SecretKey,
}

impl RepositoryKeys {
    pub(crate) fn generate() -> Result<Self> {
        let mut master = Zeroizing::new([0_u8; KEY_LEN]);
        fill_random(&mut *master)?;
        Ok(Self { master })
    }

    /// The independent key for `purpose`.
    pub fn derive(&self, purpose: KeyPurpose) -> Result<SecretKey> {
        crypto::derive_key(&self.master, purpose)
    }

    /// Encrypts `plaintext` under the key for `purpose`, authenticating `aad` as well.
    pub fn seal(&self, purpose: KeyPurpose, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let key = self.derive(purpose)?;
        crypto::seal(&key, aad, plaintext)
    }

    /// Authenticates and decrypts the output of [`RepositoryKeys::seal`].
    pub fn open(
        &self,
        purpose: KeyPurpose,
        aad: &[u8],
        sealed: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>> {
        let key = self.derive(purpose)?;
        crypto::open(&key, aad, sealed)
    }
}

impl fmt::Debug for RepositoryKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RepositoryKeys(<redacted>)")
    }
}

/// Parsed contents of one file in `keys/`.
#[derive(Debug)]
struct KeySlot {
    kdf: KdfParams,
    salt: [u8; SALT_LEN],
    sealed: Vec<u8>,
}

impl KeySlot {
    fn wrap(
        keys: &RepositoryKeys,
        repository_id: &str,
        passphrase: &Passphrase,
        kdf: KdfParams,
    ) -> Result<Self> {
        let mut slot = Self {
            kdf,
            salt: random_bytes::<SALT_LEN>()?,
            sealed: Vec::new(),
        };
        let kek = crypto::derive_wrapping_key(passphrase.as_bytes(), &slot.salt, &slot.kdf)?;
        slot.sealed = crypto::seal(&kek, &slot.aad(repository_id), &keys.master[..])?;
        Ok(slot)
    }

    fn unwrap_keys(&self, repository_id: &str, passphrase: &Passphrase) -> Result<RepositoryKeys> {
        let kek = crypto::derive_wrapping_key(passphrase.as_bytes(), &self.salt, &self.kdf)?;
        let plain = crypto::open(&kek, &self.aad(repository_id), &self.sealed)?;
        if plain.len() != KEY_LEN {
            return Err(Error::InvalidKeyFile(
                "wrapped key has the wrong length".to_owned(),
            ));
        }
        let mut master = Zeroizing::new([0_u8; KEY_LEN]);
        master.copy_from_slice(&plain);
        Ok(RepositoryKeys { master })
    }

    /// Everything about the slot except the ciphertext, plus the repository it belongs to.
    fn aad(&self, repository_id: &str) -> Vec<u8> {
        format!(
            "{MAGIC}\nkey_version={KEY_FILE_VERSION}\nrepository_id={repository_id}\n\
             kdf={KDF_NAME}\nkdf_argon2_version={ARGON2_VERSION}\n\
             kdf_m_cost_kib={}\nkdf_t_cost={}\nkdf_p_cost={}\nkdf_salt={}\n\
             cipher={CIPHER_NAME}\n",
            self.kdf.m_cost_kib,
            self.kdf.t_cost,
            self.kdf.p_cost,
            hex(&self.salt)
        )
        .into_bytes()
    }

    fn to_bytes(&self) -> Vec<u8> {
        format!(
            "{MAGIC}\nkey_version={KEY_FILE_VERSION}\nkdf={KDF_NAME}\n\
             kdf_argon2_version={ARGON2_VERSION}\nkdf_m_cost_kib={}\nkdf_t_cost={}\n\
             kdf_p_cost={}\nkdf_salt={}\ncipher={CIPHER_NAME}\nsealed={}\n",
            self.kdf.m_cost_kib,
            self.kdf.t_cost,
            self.kdf.p_cost,
            hex(&self.salt),
            hex(&self.sealed)
        )
        .into_bytes()
    }

    /// The key file version is checked before anything else, like the repository `config`.
    fn parse(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::InvalidKeyFile("file is not valid UTF-8".to_owned()))?;
        let invalid = Error::InvalidKeyFile;
        let mut fields = kv::parse(text, MAGIC).map_err(invalid)?;

        let version = kv::take_number::<u32>(&mut fields, "key_version").map_err(invalid)?;
        if version != KEY_FILE_VERSION {
            return Err(Error::UnsupportedKeyVersion {
                found: version,
                supported: KEY_FILE_VERSION,
            });
        }
        kv::take_exact(&mut fields, "kdf", KDF_NAME).map_err(invalid)?;
        let argon2_version =
            kv::take_number::<u32>(&mut fields, "kdf_argon2_version").map_err(invalid)?;
        if argon2_version != ARGON2_VERSION {
            return Err(Error::InvalidKeyFile(format!(
                "unsupported Argon2 version {argon2_version}"
            )));
        }
        let kdf = KdfParams {
            m_cost_kib: kv::take_number(&mut fields, "kdf_m_cost_kib").map_err(invalid)?,
            t_cost: kv::take_number(&mut fields, "kdf_t_cost").map_err(invalid)?,
            p_cost: kv::take_number(&mut fields, "kdf_p_cost").map_err(invalid)?,
        };
        kdf.validate()?;
        let salt_bytes = kv::take_hex(&mut fields, "kdf_salt", SALT_LEN).map_err(invalid)?;
        let mut salt = [0_u8; SALT_LEN];
        salt.copy_from_slice(&salt_bytes);
        kv::take_exact(&mut fields, "cipher", CIPHER_NAME).map_err(invalid)?;
        let sealed = kv::take_hex(&mut fields, "sealed", SEALED_LEN).map_err(invalid)?;
        kv::reject_unknown(&fields).map_err(invalid)?;

        Ok(Self { kdf, salt, sealed })
    }
}

/// Wraps `keys` under `passphrase` and stores the result as a new file in `keys/`, never
/// overwriting an existing one. Returns the store name of the new file.
pub(crate) fn write_key_slot(
    store: &Store,
    repository_id: &str,
    keys: &RepositoryKeys,
    passphrase: &Passphrase,
    kdf: KdfParams,
) -> Result<String> {
    let slot = KeySlot::wrap(keys, repository_id, passphrase, kdf)?;
    let name = format!("{}/{}", RepositoryLayout::KEYS_DIR, random_hex(16)?);
    store.write_new(&name, &slot.to_bytes())?;
    Ok(name)
}

/// Parses the key file `name` (a file in `keys/`) without trying to open it, which would need a
/// passphrase, and describes what is wrong with it, if anything. The description never contains
/// key material.
pub(crate) fn key_file_problem(store: &Store, name: &str) -> Option<String> {
    let path = format!("{}/{name}", RepositoryLayout::KEYS_DIR);
    match store.read(&path, MAX_KEY_FILE_BYTES) {
        Ok(bytes) => KeySlot::parse(&bytes).err().map(|err| err.to_string()),
        Err(err) => Some(err.to_string()),
    }
}

/// Opens the master key of the repository `repository_id` with `passphrase`.
///
/// Every slot is tried. A slot that does not authenticate is skipped, which also ignores slots
/// left behind by an interrupted or losing `init` (they belong to another repository id). A slot
/// that cannot be parsed is an error. If no slot opens, the passphrase is reported as wrong; a
/// modified slot is indistinguishable from a wrong passphrase by design.
pub(crate) fn unlock(
    store: &Store,
    repository_id: &str,
    passphrase: &Passphrase,
) -> Result<RepositoryKeys> {
    let mut slots = 0_usize;
    for name in store.list(RepositoryLayout::KEYS_DIR)? {
        if Store::is_temp_name(&name) {
            continue;
        }
        let path = format!("{}/{name}", RepositoryLayout::KEYS_DIR);
        let slot = KeySlot::parse(&store.read(&path, MAX_KEY_FILE_BYTES)?)?;
        slots += 1;
        match slot.unwrap_keys(repository_id, passphrase) {
            Ok(keys) => return Ok(keys),
            Err(Error::Decryption) => {}
            Err(err) => return Err(err),
        }
    }
    if slots == 0 {
        return Err(Error::Corrupt("the repository has no key files".to_owned()));
    }
    Err(Error::WrongPassphrase)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::testing::{fast_kdf, passphrase};

    const REPO_ID: &str = "0123456789abcdef0123456789abcdef";

    struct Fixture {
        _dir: TempDir,
        store: Store,
        keys: RepositoryKeys,
        slot: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let store = Store::local(dir.path());
        store.create_dir(RepositoryLayout::KEYS_DIR).expect("mkdir");
        let keys = RepositoryKeys::generate().expect("generate");
        let slot = write_key_slot(&store, REPO_ID, &keys, &passphrase(), fast_kdf()).expect("slot");
        Fixture {
            _dir: dir,
            store,
            keys,
            slot,
        }
    }

    fn slot_text(f: &Fixture) -> String {
        String::from_utf8(f.store.read(&f.slot, 4096).expect("read")).expect("utf-8")
    }

    fn overwrite(f: &Fixture, text: &str) {
        f.store
            .write_replace(&f.slot, text.as_bytes())
            .expect("write");
    }

    fn assert_same_keys(a: &RepositoryKeys, b: &RepositoryKeys) {
        for purpose in KeyPurpose::ALL {
            let left = a.derive(purpose).expect("derive");
            let right = b.derive(purpose).expect("derive");
            assert_eq!(*left, *right, "{purpose:?}");
        }
    }

    #[test]
    fn the_right_passphrase_unlocks_the_same_master_key() {
        let f = fixture();
        let unlocked = unlock(&f.store, REPO_ID, &passphrase()).expect("unlock");
        assert_same_keys(&f.keys, &unlocked);
    }

    #[test]
    fn wrong_passphrases_are_rejected() {
        let f = fixture();
        let attempts: [&[u8]; 5] = [
            b"wrong",
            b"correct horse battery stapl",
            b"correct horse battery staple ",
            b"Correct horse battery staple",
            b"x",
        ];
        for attempt in attempts {
            let wrong = Passphrase::new(attempt.to_vec()).expect("valid passphrase");
            let result = unlock(&f.store, REPO_ID, &wrong);
            assert!(matches!(result, Err(Error::WrongPassphrase)), "{attempt:?}");
        }
    }

    #[test]
    fn keys_persist_across_store_handles() {
        let f = fixture();
        let purpose = KeyPurpose::DataEncryption;
        let sealed = f
            .keys
            .seal(purpose, b"pack-1", b"chunk data")
            .expect("seal");
        let reopened = Store::local(f.store.root());
        let unlocked = unlock(&reopened, REPO_ID, &passphrase()).expect("unlock");
        let opened = unlocked.open(purpose, b"pack-1", &sealed).expect("open");
        assert_eq!(opened.as_slice(), b"chunk data");
        assert!(unlocked
            .open(KeyPurpose::MetadataEncryption, b"pack-1", &sealed)
            .is_err());
    }

    #[test]
    fn each_slot_has_its_own_salt_and_nonce() {
        let f = fixture();
        let second = write_key_slot(&f.store, REPO_ID, &f.keys, &passphrase(), fast_kdf())
            .expect("second slot");
        let a = slot_text(&f);
        let b = String::from_utf8(f.store.read(&second, 4096).expect("read")).expect("utf-8");
        let field = |text: &str, key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .expect("field")
                .to_owned()
        };
        assert_ne!(field(&a, "kdf_salt="), field(&b, "kdf_salt="));
        assert_ne!(field(&a, "sealed="), field(&b, "sealed="));
        let unlocked = unlock(&f.store, REPO_ID, &passphrase()).expect("unlock");
        assert_same_keys(&f.keys, &unlocked);
    }

    #[test]
    fn key_files_contain_no_secrets() {
        let f = fixture();
        let text = slot_text(&f);
        let master_hex = hex(&f.keys.master[..]);
        assert!(!text.contains(&master_hex));
        assert!(!text.contains("correct horse"));
        for line in ["key_version=1", "kdf=argon2id", "cipher=xchacha20poly1305"] {
            assert!(text.lines().any(|l| l == line), "missing {line}:\n{text}");
        }
        assert!(text.starts_with("vaultsnap-key\n"));
    }

    #[test]
    fn flipping_any_ciphertext_digit_breaks_unlocking() {
        let f = fixture();
        let original = slot_text(&f);
        let start = original.find("sealed=").expect("sealed field") + "sealed=".len();
        for index in start..start + SEALED_LEN * 2 {
            let mut bytes = original.clone().into_bytes();
            bytes[index] = if bytes[index] == b'0' { b'1' } else { b'0' };
            f.store.write_replace(&f.slot, &bytes).expect("write");
            let result = unlock(&f.store, REPO_ID, &passphrase());
            assert!(
                matches!(result, Err(Error::WrongPassphrase)),
                "digit {index}"
            );
        }
    }

    #[test]
    fn changing_authenticated_header_fields_breaks_unlocking() {
        let f = fixture();
        let original = slot_text(&f);
        let salt_line = original
            .lines()
            .find(|line| line.starts_with("kdf_salt="))
            .expect("salt line");
        let last = salt_line.chars().last().expect("salt digit");
        let swapped = if last == '0' { '1' } else { '0' };
        let flipped_salt = format!("{}{swapped}", &salt_line[..salt_line.len() - 1]);
        let edits = [
            ("kdf_m_cost_kib=64", "kdf_m_cost_kib=65"),
            ("kdf_t_cost=1", "kdf_t_cost=2"),
            ("kdf_p_cost=1", "kdf_p_cost=2"),
            (salt_line, flipped_salt.as_str()),
        ];
        for (from, to) in edits {
            overwrite(&f, &original.replace(from, to));
            let result = unlock(&f.store, REPO_ID, &passphrase());
            assert!(
                matches!(result, Err(Error::WrongPassphrase)),
                "{from} -> {to}: {result:?}"
            );
        }
        overwrite(&f, &original);
        unlock(&f.store, REPO_ID, &passphrase()).expect("restored file unlocks again");
    }

    #[test]
    fn a_key_file_is_bound_to_its_repository() {
        let f = fixture();
        let other = "fedcba9876543210fedcba9876543210";
        let result = unlock(&f.store, other, &passphrase());
        assert!(matches!(result, Err(Error::WrongPassphrase)), "{result:?}");
    }

    #[test]
    fn slots_from_other_repositories_are_ignored() {
        let f = fixture();
        let stray = RepositoryKeys::generate().expect("generate");
        let other = "fedcba9876543210fedcba9876543210";
        write_key_slot(&f.store, other, &stray, &passphrase(), fast_kdf()).expect("stray slot");
        let unlocked = unlock(&f.store, REPO_ID, &passphrase()).expect("unlock");
        assert_same_keys(&f.keys, &unlocked);
    }

    #[test]
    fn malformed_key_files_are_rejected() {
        let f = fixture();
        let original = slot_text(&f);
        let truncated = &original[..original.len() - 10];
        let cases = [
            String::new(),
            "garbage".to_owned(),
            truncated.to_owned(),
            original.replace("kdf=argon2id", "kdf=argon2i"),
            original.replace("cipher=xchacha20poly1305", "cipher=aes256gcm"),
            original.replace("kdf_argon2_version=19", "kdf_argon2_version=16"),
            original.replace("kdf_t_cost=1", "kdf_t_cost=one"),
            original.replace("sealed=", "sealed=zz"),
            original.replace("sealed=", "sealed=ab"),
            format!("{original}extra=1\n"),
            format!("{original}sealed=00\n"),
            original.replace("kdf_salt=", "kdf_salt=00"),
        ];
        for case in cases {
            overwrite(&f, &case);
            let result = unlock(&f.store, REPO_ID, &passphrase());
            assert!(
                matches!(result, Err(Error::InvalidKeyFile(_))),
                "{case:?} gave {result:?}"
            );
        }
    }

    #[test]
    fn newer_key_file_versions_are_reported_as_such() {
        let f = fixture();
        let text = slot_text(&f).replace("key_version=1", "key_version=2");
        overwrite(&f, &format!("{text}from_the_future=1\n"));
        let result = unlock(&f.store, REPO_ID, &passphrase());
        assert!(
            matches!(
                result,
                Err(Error::UnsupportedKeyVersion {
                    found: 2,
                    supported: 1
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn excessive_cost_parameters_are_refused_before_any_work() {
        let f = fixture();
        let text = slot_text(&f).replace("kdf_m_cost_kib=64", "kdf_m_cost_kib=4000000");
        overwrite(&f, &text);
        let result = unlock(&f.store, REPO_ID, &passphrase());
        assert!(
            matches!(result, Err(Error::InvalidKdfParams(_))),
            "{result:?}"
        );
    }

    #[test]
    fn a_repository_without_key_files_is_reported() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let store = Store::local(dir.path());
        store.create_dir(RepositoryLayout::KEYS_DIR).expect("mkdir");
        let result = unlock(&store, REPO_ID, &passphrase());
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    }

    #[cfg(unix)]
    #[test]
    fn key_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let f = fixture();
        let path = f.store.root().join(&f.slot);
        let mode = std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn debug_output_hides_key_material() {
        let f = fixture();
        assert_eq!(format!("{:?}", f.keys), "RepositoryKeys(<redacted>)");
    }

    #[test]
    fn key_files_are_inspected_without_a_passphrase() {
        let f = fixture();
        let name = f
            .store
            .list(RepositoryLayout::KEYS_DIR)
            .expect("list")
            .remove(0);
        assert_eq!(key_file_problem(&f.store, &name), None);

        overwrite(&f, "vaultsnap-key\nkey_version=1\n");
        let problem = key_file_problem(&f.store, &name).expect("a problem");
        assert!(!problem.is_empty());
        assert!(!problem.contains("sealed="), "{problem}");

        let missing = key_file_problem(&f.store, "0123456789abcdef0123456789abcdef");
        assert!(missing.is_some());
    }

    #[test]
    fn key_slot_serialization_round_trips() {
        let keys = RepositoryKeys::generate().expect("generate");
        let slot = KeySlot::wrap(&keys, REPO_ID, &passphrase(), fast_kdf()).expect("wrap");
        let parsed = KeySlot::parse(&slot.to_bytes()).expect("parse");
        assert_eq!(parsed.kdf, slot.kdf);
        assert_eq!(parsed.salt, slot.salt);
        assert_eq!(parsed.sealed, slot.sealed);
        assert_eq!(parsed.sealed.len(), SEALED_LEN);
        let unwrapped = parsed.unwrap_keys(REPO_ID, &passphrase()).expect("unwrap");
        assert_same_keys(&keys, &unwrapped);
    }
}
