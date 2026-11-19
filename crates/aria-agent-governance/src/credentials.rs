//! Encrypted-at-rest credential store with **rotation invalidates immediately**.
//!
//! Design notes (ADR-0023):
//! * The value is sealed with CHACHA20-POLY1305 under a key derived from the
//!   master key in an environment variable. The master key itself is never
//!   written to disk.
//! * There is **no read cache**. Runta keeps a TTL cache, which means a rotated
//!   secret keeps working for up to TTL seconds — a real incident-response
//!   footgun. Here every `resolve` decrypts the current file version, so a
//!   rotation is effective on the next request.
//! * Every metadata read returns version/updated-at only, never the value, and
//!   `SecretValue`'s `Debug` is redacted.

use agent_core::seams::{
    now_ms, CredentialsSeam, SeamError, SeamResult, SecretMeta, SecretStub, SecretValue,
};
use async_trait::async_trait;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const STORE_FILE: &str = "credentials.json";
const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSecret {
    version: u64,
    updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rotate_after_secs: Option<u64>,
    nonce_hex: String,
    ciphertext_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    secrets: BTreeMap<String, StoredSecret>,
    #[serde(default)]
    stubs: Vec<SecretStub>,
}

impl Default for StoreFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            secrets: BTreeMap::new(),
            stubs: Vec::new(),
        }
    }
}

/// File-backed credential store.
pub struct FileCredentialStore {
    dir: PathBuf,
    key: LessSafeKey,
    state: Mutex<StoreFile>,
    rotate_after_secs: u64,
}

impl std::fmt::Debug for FileCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileCredentialStore")
            .field("dir", &self.dir)
            .field("key", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl FileCredentialStore {
    /// Open (or create) the store under `dir`.
    ///
    /// `master_key_env` names the environment variable holding the passphrase.
    /// A missing or empty key is a hard error: silently writing plaintext would
    /// be worse than not starting.
    pub fn open(
        dir: impl AsRef<Path>,
        master_key_env: &str,
        rotate_after_secs: u64,
    ) -> SeamResult<Self> {
        let dir = dir.as_ref().to_path_buf();
        let master = std::env::var(master_key_env).map_err(|_| {
            SeamError::Unavailable(format!(
                "credential store needs a master key in ${master_key_env}; refusing to \
                 write secrets without one"
            ))
        })?;
        if master.trim().is_empty() {
            return Err(SeamError::Unavailable(format!(
                "${master_key_env} is empty; refusing to write secrets without a key"
            )));
        }
        let key = derive_key(&master)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| SeamError::Io(format!("credential dir {}: {e}", dir.display())))?;
        let path = dir.join(STORE_FILE);
        let state = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<StoreFile>(&raw).map_err(|e| {
                SeamError::Invalid(format!(
                    "credential store {} is corrupt: {e}",
                    path.display()
                ))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreFile::default(),
            Err(e) => {
                return Err(SeamError::Io(format!(
                    "credential store {}: {e}",
                    path.display()
                )))
            }
        };
        if state.version != STORE_VERSION {
            return Err(SeamError::Invalid(format!(
                "credential store version {} is not supported (expected {STORE_VERSION})",
                state.version
            )));
        }
        Ok(Self {
            dir,
            key,
            state: Mutex::new(state),
            rotate_after_secs,
        })
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(STORE_FILE)
    }

    /// Names whose rotation deadline has passed, for the scheduler to rotate.
    pub fn due_for_rotation(&self, now: u64) -> Vec<String> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        state
            .secrets
            .iter()
            .filter_map(|(name, secret)| {
                // A zero period means "no rotation": it is not a deadline.
                let period = secret.rotate_after_secs.filter(|period| *period > 0)?;
                let due_at = secret
                    .updated_at_ms
                    .saturating_add(period.saturating_mul(1000));
                (now >= due_at).then(|| name.clone())
            })
            .collect()
    }

    fn persist(&self, state: &StoreFile) -> SeamResult<()> {
        let path = self.path();
        let tmp = self.dir.join(format!("{STORE_FILE}.tmp"));
        // `StoreFile` is plain data: serialising it cannot fail.
        let body =
            serde_json::to_vec_pretty(state).expect("the credential store is a plain struct");
        std::fs::write(&tmp, body)
            .map_err(|e| SeamError::Io(format!("write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| SeamError::Io(format!("replace {}: {e}", path.display())))?;
        Ok(())
    }

    fn seal(&self, name: &str, value: &[u8]) -> SeamResult<(String, String)> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        // The OS CSPRNG is the only source of nonces; without it there is no
        // safe way to write a secret, so this is fatal rather than silent.
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .expect("the OS CSPRNG must be available to store credentials");
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = value.to_vec();
        self.key
            .seal_in_place_append_tag(nonce, Aad::from(name.as_bytes()), &mut in_out)
            .expect("sealing with a fresh random nonce cannot fail");
        Ok((hex_encode(&nonce_bytes), hex_encode(&in_out)))
    }

    fn open_secret(&self, name: &str, stored: &StoredSecret) -> SeamResult<SecretValue> {
        let nonce_bytes = hex_decode(&stored.nonce_hex)?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(SeamError::Invalid(format!(
                "credential `{name}` has a malformed nonce"
            )));
        }
        let mut in_out = hex_decode(&stored.ciphertext_hex)?;
        let nonce = Nonce::assume_unique_for_key(
            nonce_bytes
                .as_slice()
                .try_into()
                .expect("nonce length is checked above"),
        );
        let plaintext = self
            .key
            .open_in_place(nonce, Aad::from(name.as_bytes()), &mut in_out)
            .map_err(|_| {
                SeamError::Denied(format!(
                    "credential `{name}` could not be decrypted (wrong master key or tampering)"
                ))
            })?;
        let text = String::from_utf8(plaintext.to_vec())
            .map_err(|_| SeamError::Invalid(format!("credential `{name}` is not valid UTF-8")))?;
        Ok(SecretValue::new(text))
    }
}

fn derive_key(master: &str) -> SeamResult<LessSafeKey> {
    let material = ring::digest::digest(&ring::digest::SHA256, master.as_bytes());
    // A SHA-256 digest is always a valid ChaCha20-Poly1305 key length.
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, material.as_ref())
        .expect("SHA-256 output is a valid key length");
    Ok(LessSafeKey::new(unbound))
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn hex_decode(raw: &str) -> SeamResult<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return Err(SeamError::Invalid("hex payload has odd length".into()));
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    let bytes = raw.as_bytes();
    for chunk in bytes.chunks(2) {
        let hi = (chunk[0] as char).to_digit(16);
        let lo = (chunk[1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => return Err(SeamError::Invalid("hex payload has non-hex digits".into())),
        }
    }
    Ok(out)
}

#[async_trait]
impl CredentialsSeam for FileCredentialStore {
    async fn list(&self) -> SeamResult<Vec<SecretMeta>> {
        let state = self
            .state
            .lock()
            .map_err(|_| SeamError::Io("credential store lock poisoned".into()))?;
        Ok(state
            .secrets
            .iter()
            .map(|(name, secret)| SecretMeta {
                name: name.clone(),
                version: secret.version,
                updated_at_ms: secret.updated_at_ms,
                rotate_after_secs: secret.rotate_after_secs,
            })
            .collect())
    }

    async fn resolve(&self, name: &str) -> SeamResult<SecretValue> {
        let state = self
            .state
            .lock()
            .map_err(|_| SeamError::Io("credential store lock poisoned".into()))?;
        let stored = state
            .secrets
            .get(name)
            .ok_or_else(|| SeamError::NotFound(format!("credential `{name}`")))?;
        self.open_secret(name, stored)
    }

    async fn rotate(&self, name: &str, value: SecretValue) -> SeamResult<SecretMeta> {
        if name.trim().is_empty() {
            return Err(SeamError::Invalid(
                "credential name must not be empty".into(),
            ));
        }
        if value.is_empty() {
            return Err(SeamError::Invalid(format!(
                "refusing to store an empty credential for `{name}`"
            )));
        }
        let (nonce_hex, ciphertext_hex) = self.seal(name, value.expose().as_bytes())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SeamError::Io("credential store lock poisoned".into()))?;
        let next_version = state
            .secrets
            .get(name)
            .map(|s| s.version.saturating_add(1))
            .unwrap_or(1);
        let meta = SecretMeta {
            name: name.to_string(),
            version: next_version,
            updated_at_ms: now_ms(),
            rotate_after_secs: (self.rotate_after_secs > 0).then_some(self.rotate_after_secs),
        };
        state.secrets.insert(
            name.to_string(),
            StoredSecret {
                version: meta.version,
                updated_at_ms: meta.updated_at_ms,
                rotate_after_secs: meta.rotate_after_secs,
                nonce_hex,
                ciphertext_hex,
            },
        );
        self.persist(&state)?;
        Ok(meta)
    }

    async fn delete(&self, name: &str) -> SeamResult<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SeamError::Io("credential store lock poisoned".into()))?;
        if state.secrets.remove(name).is_none() {
            return Err(SeamError::NotFound(format!("credential `{name}`")));
        }
        self.persist(&state)
    }

    async fn stubs(&self) -> SeamResult<Vec<SecretStub>> {
        let state = self
            .state
            .lock()
            .map_err(|_| SeamError::Io("credential store lock poisoned".into()))?;
        Ok(state.stubs.clone())
    }

    async fn put_stub(&self, stub: SecretStub) -> SeamResult<()> {
        stub.validate()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SeamError::Io("credential store lock poisoned".into()))?;
        match state.stubs.iter_mut().find(|s| s.id == stub.id) {
            Some(slot) => *slot = stub,
            None => state.stubs.push(stub),
        }
        self.persist(&state)
    }

    async fn remove_stub(&self, id: &str) -> SeamResult<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SeamError::Io("credential store lock poisoned".into()))?;
        let before = state.stubs.len();
        state.stubs.retain(|s| s.id != id);
        if state.stubs.len() == before {
            return Err(SeamError::NotFound(format!("secret stub `{id}`")));
        }
        self.persist(&state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::HostPattern;

    /// Serialises tests that mutate process-wide environment variables. Async so
    /// the guard may be held across `await` without tripping
    /// `clippy::await_holding_lock`.
    async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    fn store(dir: &Path, rotate: u64) -> FileCredentialStore {
        std::env::set_var("TEST_CRED_KEY", "master-passphrase");
        FileCredentialStore::open(dir, "TEST_CRED_KEY", rotate).unwrap()
    }

    fn stub(id: &str) -> SecretStub {
        SecretStub {
            id: id.into(),
            host: HostPattern::parse("api.openai.com").unwrap(),
            path_prefix: Some("/v1/".into()),
            header: "authorization".into(),
            value_template: "Bearer ${credential}".into(),
            credential: "openai".into(),
        }
    }

    #[test]
    fn hex_round_trips_and_rejects_garbage() {
        assert_eq!(
            hex_decode(&hex_encode(b"\x00\xff\x10")).unwrap(),
            b"\x00\xff\x10"
        );
        assert!(hex_decode("abc").is_err());
        assert!(hex_decode("zz").is_err());
    }

    #[test]
    fn a_missing_master_key_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        std::env::remove_var("TEST_CRED_MISSING");
        let err = FileCredentialStore::open(dir.path(), "TEST_CRED_MISSING", 0).unwrap_err();
        assert!(matches!(err, SeamError::Unavailable(_)), "{err:?}");
        std::env::set_var("TEST_CRED_MISSING", "   ");
        assert!(FileCredentialStore::open(dir.path(), "TEST_CRED_MISSING", 0).is_err());
    }

    #[tokio::test]
    async fn rotate_then_resolve_returns_the_new_value_immediately() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 0);
        let first = store
            .rotate("openai", SecretValue::new("v1"))
            .await
            .unwrap();
        assert_eq!(first.version, 1);
        assert_eq!(store.resolve("openai").await.unwrap().expose(), "v1");

        let second = store
            .rotate("openai", SecretValue::new("v2"))
            .await
            .unwrap();
        assert_eq!(second.version, 2);
        assert_eq!(
            store.resolve("openai").await.unwrap().expose(),
            "v2",
            "no cache window: rotation is effective on the next read"
        );
        let debug = format!("{:?}", store);
        assert!(debug.contains("redacted"));
        assert!(!debug.contains("master-passphrase"));
    }

    #[tokio::test]
    async fn the_file_never_contains_the_plaintext() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 0);
        store
            .rotate("openai", SecretValue::new("sk-super-secret"))
            .await
            .unwrap();
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("sk-super-secret"), "plaintext on disk: {raw}");
        assert!(raw.contains("ciphertext_hex"));
    }

    #[tokio::test]
    async fn a_wrong_master_key_cannot_decrypt() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        {
            let store = store(dir.path(), 0);
            store.rotate("k", SecretValue::new("v")).await.unwrap();
        }
        std::env::set_var("TEST_CRED_KEY", "different-passphrase");
        let reopened = FileCredentialStore::open(dir.path(), "TEST_CRED_KEY", 0).unwrap();
        assert!(matches!(
            reopened.resolve("k").await,
            Err(SeamError::Denied(_))
        ));
        std::env::set_var("TEST_CRED_KEY", "master-passphrase");
    }

    #[tokio::test]
    async fn tampering_is_detected() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        {
            let store = store(dir.path(), 0);
            store.rotate("k", SecretValue::new("v")).await.unwrap();
        }
        let path = dir.path().join(STORE_FILE);
        let mut file: StoreFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = file.secrets.get_mut("k").unwrap();
        entry.ciphertext_hex = hex_encode(b"tampered");
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let reopened = FileCredentialStore::open(dir.path(), "TEST_CRED_KEY", 0).unwrap();
        assert!(reopened.resolve("k").await.is_err());
    }

    #[tokio::test]
    async fn credentials_can_be_deleted_and_empty_values_refused() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 0);
        assert!(matches!(
            store.rotate(" ", SecretValue::new("v")).await,
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            store.rotate("k", SecretValue::new("  ")).await,
            Err(SeamError::Invalid(_))
        ));
        store.rotate("k", SecretValue::new("v")).await.unwrap();
        store.delete("k").await.unwrap();
        assert!(matches!(
            store.delete("k").await,
            Err(SeamError::NotFound(_))
        ));
        assert!(matches!(
            store.resolve("k").await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn stubs_round_trip_and_substitute_once() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 0);
        store
            .rotate("openai", SecretValue::new("sk-1"))
            .await
            .unwrap();
        store.put_stub(stub("s1")).await.unwrap();
        store.put_stub(stub("s1")).await.unwrap();
        assert_eq!(store.stubs().await.unwrap().len(), 1);
        assert_eq!(store.list().await.unwrap()[0].name, "openai");

        let mut headers = Vec::new();
        let report = store
            .substitute("api.openai.com", "/v1/chat", &mut headers)
            .await
            .unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(headers[0].1, "Bearer sk-1");

        store.remove_stub("s1").await.unwrap();
        assert!(store.stubs().await.unwrap().is_empty());
        assert!(matches!(
            store.remove_stub("s1").await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn rotation_deadlines_are_reported() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 60);
        store.rotate("k", SecretValue::new("v")).await.unwrap();
        let meta = store.list().await.unwrap();
        assert_eq!(meta[0].rotate_after_secs, Some(60));
        assert!(store.due_for_rotation(now_ms()).is_empty());
        assert_eq!(
            store.due_for_rotation(now_ms() + 61_000),
            vec!["k".to_string()]
        );
    }

    #[tokio::test]
    async fn a_corrupt_store_is_reported_not_silently_reset() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CRED_KEY", "master-passphrase");
        std::fs::write(dir.path().join(STORE_FILE), "{not json").unwrap();
        let err = FileCredentialStore::open(dir.path(), "TEST_CRED_KEY", 0).unwrap_err();
        assert!(matches!(err, SeamError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn an_unusable_directory_or_store_is_a_reported_error() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CRED_KEY", "master-passphrase");
        // A file where the directory should go.
        let blocker = dir.path().join("creds");
        std::fs::write(&blocker, "x").unwrap();
        let err = FileCredentialStore::open(&blocker, "TEST_CRED_KEY", 0).unwrap_err();
        assert!(matches!(err, SeamError::Io(_)), "{err:?}");

        // A directory where the store file should go.
        let shadowed = dir.path().join("shadowed");
        std::fs::create_dir_all(shadowed.join(STORE_FILE)).unwrap();
        let err = FileCredentialStore::open(&shadowed, "TEST_CRED_KEY", 0).unwrap_err();
        assert!(matches!(err, SeamError::Io(_)), "{err:?}");

        // A store written by a newer build is refused, not reset.
        let future = dir.path().join("future");
        std::fs::create_dir_all(&future).unwrap();
        std::fs::write(
            future.join(STORE_FILE),
            r#"{"version":9999,"secrets":{},"stubs":[]}"#,
        )
        .unwrap();
        let err = FileCredentialStore::open(&future, "TEST_CRED_KEY", 0).unwrap_err();
        assert!(matches!(err, SeamError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_vanished_directory_is_reported_on_write() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 0);
        std::fs::remove_dir_all(dir.path()).unwrap();
        assert!(matches!(
            store.rotate("k", SecretValue::new("v")).await,
            Err(SeamError::Io(_))
        ));
    }

    #[tokio::test]
    async fn a_failed_atomic_replace_is_reported() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 0);
        // The temp file can still be written, but renaming it over a directory
        // fails on every platform. This is the only way `persist` can fail
        // after the write succeeded, so the error must name the target path.
        std::fs::create_dir(store.path()).unwrap();
        let err = store.rotate("k", SecretValue::new("v")).await.unwrap_err();
        assert!(matches!(err, SeamError::Io(_)), "{err:?}");
        assert!(err.to_string().contains("replace"), "{err}");
    }

    #[tokio::test]
    async fn a_malformed_nonce_is_refused() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        {
            let store = store(dir.path(), 0);
            store.rotate("k", SecretValue::new("v")).await.unwrap();
        }
        let path = dir.path().join(STORE_FILE);
        let mut file: StoreFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        file.secrets.get_mut("k").unwrap().nonce_hex = "00".into();
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let store = FileCredentialStore::open(dir.path(), "TEST_CRED_KEY", 0).unwrap();
        let err = store.resolve("k").await.unwrap_err();
        assert!(err.to_string().contains("malformed nonce"), "{err}");
    }

    #[tokio::test]
    async fn rotation_deadlines_skip_disabled_and_future_windows() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        // Rotation disabled: the entry never becomes due.
        let disabled = store(dir.path(), 0);
        disabled.rotate("k", SecretValue::new("v")).await.unwrap();
        assert!(disabled.due_for_rotation(u64::MAX).is_empty());

        // Rotation enabled: due only once the window has passed.
        let timed = store(dir.path(), 60);
        timed.rotate("k", SecretValue::new("v")).await.unwrap();
        assert!(timed.due_for_rotation(0).is_empty(), "not due yet");
        assert_eq!(timed.due_for_rotation(u64::MAX), vec!["k".to_string()]);
    }

    #[tokio::test]
    async fn a_poisoned_store_lock_is_reported_as_io() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 0);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = store.state.lock().unwrap();
            panic!("poison the credential store lock");
        }));
        assert!(poisoned.is_err());

        assert!(matches!(store.list().await, Err(SeamError::Io(_))));
        assert!(matches!(store.resolve("k").await, Err(SeamError::Io(_))));
        assert!(matches!(
            store.rotate("k", SecretValue::new("v")).await,
            Err(SeamError::Io(_))
        ));
        assert!(matches!(store.delete("k").await, Err(SeamError::Io(_))));
        assert!(matches!(store.stubs().await, Err(SeamError::Io(_))));
        assert!(matches!(
            store.put_stub(stub("s")).await,
            Err(SeamError::Io(_))
        ));
        assert!(matches!(
            store.remove_stub("s").await,
            Err(SeamError::Io(_))
        ));
        let mut headers = Vec::new();
        assert!(matches!(
            store
                .substitute("api.openai.com", "/v1/x", &mut headers)
                .await,
            Err(SeamError::Io(_))
        ));
        // The rotation scheduler degrades to "nothing due" instead of panicking.
        assert!(store.due_for_rotation(u64::MAX).is_empty());
    }

    #[tokio::test]
    async fn a_payload_that_is_not_utf8_is_refused() {
        let _guard = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), 0);
        store.rotate("k", SecretValue::new("v")).await.unwrap();
        // Same key and AAD, but a payload no `rotate` call could have produced.
        let (nonce_hex, ciphertext_hex) = store.seal("k", &[0xff, 0xfe]).unwrap();
        let path = store.path();
        let mut file: StoreFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = file.secrets.get_mut("k").unwrap();
        entry.nonce_hex = nonce_hex;
        entry.ciphertext_hex = ciphertext_hex;
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

        let reopened = FileCredentialStore::open(dir.path(), "TEST_CRED_KEY", 0).unwrap();
        assert!(matches!(
            reopened.resolve("k").await,
            Err(SeamError::Invalid(_))
        ));
    }
}
