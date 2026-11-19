//! Credentials seam: the agent writes a literal placeholder, the boundary
//! substitutes the real value.
//!
//! The placeholder spelling is a **contract**, not a convention: it is exactly
//! `"${credential}"` and it is only ever replaced inside header values at the
//! egress boundary (ADR-0020). Nothing on the agent side — process env, logs,
//! session events, audit payloads — ever sees the plaintext.

use super::egress::HostPattern;
use super::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The one and only credential placeholder.
pub const CREDENTIAL_PLACEHOLDER: &str = "${credential}";

/// A secret value that refuses to leak through `Debug` or `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Explicit, auditable access to the plaintext. Call sites are the header
    /// substitution in the egress gateway and the AEAD writer; nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Metadata only — never the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMeta {
    pub name: String,
    /// Bumped on every rotation; also the cache-invalidation key.
    pub version: u64,
    pub updated_at_ms: u64,
    /// Automatic rotation deadline, when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate_after_secs: Option<u64>,
}

/// One substitution rule: "when talking to `host` (optionally under `path`),
/// put this credential into this header using this template".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretStub {
    pub id: String,
    pub host: HostPattern,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Header name, e.g. `authorization`.
    pub header: String,
    /// Template containing [`CREDENTIAL_PLACEHOLDER`], e.g.
    /// `"Bearer ${credential}"`.
    pub value_template: String,
    /// Name of the stored secret this stub resolves.
    pub credential: String,
}

impl SecretStub {
    /// Validate the template. A stub without the placeholder is a configuration
    /// error: it would send the literal credential *name* instead of the value.
    pub fn validate(&self) -> SeamResult<()> {
        if self.id.trim().is_empty() {
            return Err(SeamError::Invalid(
                "secret stub id must not be empty".into(),
            ));
        }
        if self.header.trim().is_empty() {
            return Err(SeamError::Invalid(
                "secret stub header must not be empty".into(),
            ));
        }
        if self.credential.trim().is_empty() {
            return Err(SeamError::Invalid(
                "secret stub credential name must not be empty".into(),
            ));
        }
        if !self.value_template.contains(CREDENTIAL_PLACEHOLDER) {
            return Err(SeamError::Invalid(format!(
                "secret stub `{}` template must contain {CREDENTIAL_PLACEHOLDER}",
                self.id
            )));
        }
        Ok(())
    }

    /// Does this stub apply to `host` + `path`?
    pub fn matches(&self, host: &str, path: &str) -> bool {
        if !self.host.matches(host) {
            return false;
        }
        match &self.path_prefix {
            Some(prefix) => path.starts_with(prefix.as_str()),
            None => true,
        }
    }

    /// Render the header value with the plaintext substituted.
    pub fn render(&self, value: &SecretValue) -> String {
        self.value_template
            .replace(CREDENTIAL_PLACEHOLDER, value.expose())
    }
}

/// How many headers one substitution pass touched.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubstitutionReport {
    pub applied: usize,
    pub stub_ids: Vec<String>,
}

/// The seam.
#[async_trait]
pub trait CredentialsSeam: Send + Sync {
    /// Metadata for every stored secret (never the values).
    async fn list(&self) -> SeamResult<Vec<SecretMeta>>;

    /// Resolve one secret for boundary substitution.
    async fn resolve(&self, name: &str) -> SeamResult<SecretValue>;

    /// Atomically replace a secret. The new version takes effect immediately:
    /// there is no TTL window in which the old value is still served.
    async fn rotate(&self, name: &str, value: SecretValue) -> SeamResult<SecretMeta>;

    /// Remove a secret.
    async fn delete(&self, name: &str) -> SeamResult<()>;

    /// Substitution rules, tenant-wide.
    async fn stubs(&self) -> SeamResult<Vec<SecretStub>>;

    /// Insert or replace a stub (same `id` replaces, never duplicates).
    async fn put_stub(&self, stub: SecretStub) -> SeamResult<()>;

    /// Remove a stub by id.
    async fn remove_stub(&self, id: &str) -> SeamResult<()>;

    /// Substitute every matching stub into `headers` (name → value).
    ///
    /// Provided method so providers only implement storage; the matching and
    /// replacement logic lives in one place.
    async fn substitute(
        &self,
        host: &str,
        path: &str,
        headers: &mut Vec<(String, String)>,
    ) -> SeamResult<SubstitutionReport> {
        let mut report = SubstitutionReport::default();
        for stub in self.stubs().await? {
            if !stub.matches(host, path) {
                continue;
            }
            let value = self.resolve(&stub.credential).await?;
            let rendered = stub.render(&value);
            let header = stub.header.trim().to_ascii_lowercase();
            match headers
                .iter_mut()
                .find(|(name, _)| name.eq_ignore_ascii_case(&header))
            {
                Some((_, slot)) => *slot = rendered,
                None => headers.push((header, rendered)),
            }
            report.applied += 1;
            report.stub_ids.push(stub.id);
        }
        Ok(report)
    }
}

/// Fail-closed default: nothing to resolve, nothing to substitute.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCredentials;

#[async_trait]
impl CredentialsSeam for NoCredentials {
    async fn list(&self) -> SeamResult<Vec<SecretMeta>> {
        Ok(Vec::new())
    }

    async fn resolve(&self, name: &str) -> SeamResult<SecretValue> {
        Err(SeamError::Unavailable(format!(
            "no credential store is configured for this deployment (requested `{name}`)"
        )))
    }

    async fn rotate(&self, _name: &str, _value: SecretValue) -> SeamResult<SecretMeta> {
        Err(SeamError::Unavailable(
            "no credential store is configured for this deployment".into(),
        ))
    }

    async fn delete(&self, _name: &str) -> SeamResult<()> {
        Err(SeamError::Unavailable(
            "no credential store is configured for this deployment".into(),
        ))
    }

    async fn stubs(&self) -> SeamResult<Vec<SecretStub>> {
        Ok(Vec::new())
    }

    async fn put_stub(&self, _stub: SecretStub) -> SeamResult<()> {
        Err(SeamError::Unavailable(
            "no credential store is configured for this deployment".into(),
        ))
    }

    async fn remove_stub(&self, _id: &str) -> SeamResult<()> {
        Err(SeamError::Unavailable(
            "no credential store is configured for this deployment".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn stub(host: &str, path: Option<&str>, credential: &str) -> SecretStub {
        SecretStub {
            id: format!("stub-{credential}"),
            host: HostPattern::parse(host).unwrap(),
            path_prefix: path.map(|p| p.to_string()),
            header: "Authorization".into(),
            value_template: format!("Bearer {CREDENTIAL_PLACEHOLDER}"),
            credential: credential.into(),
        }
    }

    #[test]
    fn secrets_never_render_through_debug() {
        let secret = SecretValue::new("sk-live-123");
        assert_eq!(format!("{secret:?}"), "[redacted]");
        assert_eq!(secret.expose(), "sk-live-123");
        assert!(!secret.is_empty());
        assert!(SecretValue::new("  ").is_empty());
    }

    #[test]
    fn templates_must_contain_the_placeholder() {
        let mut s = stub("api.openai.com", Some("/v1/"), "openai");
        assert!(s.validate().is_ok());
        assert!(s.matches("api.openai.com", "/v1/chat"));
        assert!(!s.matches("api.openai.com", "/v2/chat"), "path prefix");
        assert!(!s.matches("openai.com", "/v1/chat"), "host must match");

        let rendered = s.render(&SecretValue::new("sk-1"));
        assert_eq!(rendered, "Bearer sk-1");

        s.value_template = "Bearer hardcoded".into();
        assert!(matches!(s.validate(), Err(SeamError::Invalid(_))));

        s.value_template = format!("Bearer {CREDENTIAL_PLACEHOLDER}");
        s.header = " ".into();
        assert!(s.validate().is_err());
        s.header = "authorization".into();
        s.credential = " ".into();
        assert!(s.validate().is_err());
        s.credential = "openai".into();
        s.id = " ".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn stub_without_path_matches_every_path() {
        let s = stub("api.github.com", None, "gh");
        assert!(s.matches("api.github.com", "/"));
        assert!(s.matches("api.github.com", "/repos/x"));
    }

    struct Memory {
        secrets: Mutex<Vec<(String, String, u64)>>,
        stubs: Mutex<Vec<SecretStub>>,
    }

    impl Memory {
        fn new() -> Self {
            Self {
                secrets: Mutex::new(Vec::new()),
                stubs: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CredentialsSeam for Memory {
        async fn list(&self) -> SeamResult<Vec<SecretMeta>> {
            Ok(self
                .secrets
                .lock()
                .unwrap()
                .iter()
                .map(|(name, _, version)| SecretMeta {
                    name: name.clone(),
                    version: *version,
                    updated_at_ms: 0,
                    rotate_after_secs: None,
                })
                .collect())
        }

        async fn resolve(&self, name: &str) -> SeamResult<SecretValue> {
            self.secrets
                .lock()
                .unwrap()
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, v, _)| SecretValue::new(v.clone()))
                .ok_or_else(|| SeamError::NotFound(name.to_string()))
        }

        async fn rotate(&self, name: &str, value: SecretValue) -> SeamResult<SecretMeta> {
            let mut secrets = self.secrets.lock().unwrap();
            match secrets.iter_mut().find(|(n, _, _)| n == name) {
                Some((_, v, version)) => {
                    *v = value.expose().to_string();
                    *version += 1;
                    Ok(SecretMeta {
                        name: name.into(),
                        version: *version,
                        updated_at_ms: 0,
                        rotate_after_secs: None,
                    })
                }
                None => {
                    secrets.push((name.to_string(), value.expose().to_string(), 1));
                    Ok(SecretMeta {
                        name: name.into(),
                        version: 1,
                        updated_at_ms: 0,
                        rotate_after_secs: None,
                    })
                }
            }
        }

        async fn delete(&self, name: &str) -> SeamResult<()> {
            self.secrets.lock().unwrap().retain(|(n, _, _)| n != name);
            Ok(())
        }

        async fn stubs(&self) -> SeamResult<Vec<SecretStub>> {
            Ok(self.stubs.lock().unwrap().clone())
        }

        async fn put_stub(&self, stub: SecretStub) -> SeamResult<()> {
            stub.validate()?;
            let mut stubs = self.stubs.lock().unwrap();
            match stubs.iter_mut().find(|s| s.id == stub.id) {
                Some(slot) => *slot = stub,
                None => stubs.push(stub),
            }
            Ok(())
        }

        async fn remove_stub(&self, id: &str) -> SeamResult<()> {
            self.stubs.lock().unwrap().retain(|s| s.id != id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn substitution_is_scoped_and_append_or_replace() {
        let store = Memory::new();
        store
            .rotate("openai", SecretValue::new("sk-1"))
            .await
            .unwrap();
        store
            .put_stub(stub("api.openai.com", Some("/v1/"), "openai"))
            .await
            .unwrap();

        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        let report = store
            .substitute("api.openai.com", "/v1/chat", &mut headers)
            .await
            .unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.stub_ids, vec!["stub-openai"]);
        assert_eq!(
            headers,
            vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("authorization".to_string(), "Bearer sk-1".to_string()),
            ]
        );

        // An existing header is replaced, not duplicated.
        let mut headers = vec![("Authorization".into(), "Bearer ${credential}".into())];
        store
            .substitute("api.openai.com", "/v1/x", &mut headers)
            .await
            .unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].1, "Bearer sk-1");

        // No match → untouched.
        let mut headers = Vec::new();
        let report = store
            .substitute("example.com", "/v1/x", &mut headers)
            .await
            .unwrap();
        assert_eq!(report.applied, 0);
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn rotation_bumps_the_version_and_never_keeps_the_old_value() {
        let store = Memory::new();
        let stubs = store.stubs().await.unwrap();
        assert!(stubs.is_empty());
        let first = store.rotate("k", SecretValue::new("v1")).await.unwrap();
        assert_eq!(first.version, 1);
        let second = store.rotate("k", SecretValue::new("v2")).await.unwrap();
        assert_eq!(second.version, 2);
        assert_eq!(store.resolve("k").await.unwrap().expose(), "v2");
        assert_eq!(store.list().await.unwrap()[0].version, 2);
        store.delete("k").await.unwrap();
        assert!(matches!(
            store.resolve("k").await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn stub_crud_and_validation() {
        let store = Memory::new();
        let mut s = stub("api.openai.com", None, "openai");
        s.id = "s1".into();
        store.put_stub(s.clone()).await.unwrap();
        store.put_stub(s.clone()).await.unwrap();
        assert_eq!(store.stubs().await.unwrap().len(), 1, "id replaces");
        store.remove_stub("s1").await.unwrap();
        assert!(store.stubs().await.unwrap().is_empty());

        let mut bad = stub("api.openai.com", None, "openai");
        bad.value_template = "nope".into();
        assert!(store.put_stub(bad).await.is_err());
    }

    #[tokio::test]
    async fn default_provider_is_fail_closed() {
        let seam = NoCredentials;
        assert!(seam.list().await.unwrap().is_empty());
        assert!(seam.stubs().await.unwrap().is_empty());
        assert!(matches!(
            seam.resolve("x").await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            seam.rotate("x", SecretValue::new("v")).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            seam.delete("x").await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            seam.put_stub(stub("a.com", None, "x")).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            seam.remove_stub("x").await,
            Err(SeamError::Unavailable(_))
        ));
        let mut headers = Vec::new();
        let report = seam.substitute("a.com", "/", &mut headers).await.unwrap();
        assert_eq!(report.applied, 0);
    }
}
