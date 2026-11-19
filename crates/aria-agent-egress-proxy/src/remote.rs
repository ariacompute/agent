//! Remote seams: the proxy asks the control plane instead of holding any state.
//!
//! Only five calls exist, and none of them stores a secret locally:
//!
//! | call | direction | why |
//! |---|---|---|
//! | `whoami(peer)` | proxy → control plane | turns a source IP into a tenant |
//! | `policy()` | proxy → control plane | the snapshot the gateway decides with |
//! | `stubs()` | proxy → control plane | substitution rules (no values) |
//! | `resolve(name)` | proxy → control plane | the plaintext, at the boundary only |
//! | `audit(events)` | proxy → control plane | one audit trail, not two |
//!
//! Everything else on `CredentialsSeam` is `Unsupported`: a proxy that could
//! rotate a secret would be a second writer of the credential store.

use agent_core::seams::{
    AuditEvent, CredentialsSeam, EgressPolicy, EgressSeam, SeamError, SeamResult, SecretMeta,
    SecretStub, SecretValue,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// Identity of one connected sandbox, as resolved by the control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerIdentity {
    pub principal_id: String,
    pub sandbox_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WhoamiBody<'a> {
    peer_ip: &'a str,
}

#[derive(Debug, Serialize)]
struct PrincipalBody<'a> {
    principal_id: &'a str,
    /// The control plane re-derives the principal from this IP; it is the
    /// anti-lateral-movement check, not a hint.
    peer_ip: &'a str,
}

#[derive(Debug, Serialize)]
struct SecretBody<'a> {
    principal_id: &'a str,
    peer_ip: &'a str,
    name: &'a str,
}

#[derive(Debug, Deserialize)]
struct SecretResponse {
    value: String,
}

#[derive(Debug, Serialize)]
struct AuditBody<'a> {
    events: &'a [AuditEvent],
}

/// HTTP client for the control plane. One instance per process.
pub struct RemoteControl {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for RemoteControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The shared token is a credential: never render it, even in a panic
        // message or a `dbg!`.
        f.debug_struct("RemoteControl")
            .field("base", &self.base)
            .field("token", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl RemoteControl {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> SeamResult<Self> {
        let base = base.into().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err(SeamError::Unavailable(
                "CONTROL_PLANE_URL must be set for the egress proxy".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("the client builder only fails on invalid TLS configuration");
        Ok(Self {
            base,
            token: token.into(),
            http,
        })
    }

    async fn post<B: Serialize + ?Sized, T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> SeamResult<T> {
        let url = format!("{}{}", self.base, path);
        let response = self
            .http
            .post(&url)
            .header("X-Aria-Proxy-Token", &self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| SeamError::Unavailable(format!("control plane {path}: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            // Never echo the body: it may contain a substituted secret.
            return Err(match status.as_u16() {
                401 | 403 => SeamError::Denied(format!("control plane refused {path}")),
                404 => SeamError::NotFound(format!("control plane has no peer for {path}")),
                other => SeamError::Unavailable(format!("control plane {path} returned {other}")),
            });
        }
        response
            .json::<T>()
            .await
            .map_err(|e| SeamError::Invalid(format!("control plane {path}: {e}")))
    }

    pub async fn whoami(&self, peer_ip: &str) -> SeamResult<PeerIdentity> {
        self.post("/internal/egress/whoami", &WhoamiBody { peer_ip })
            .await
    }

    pub async fn policy(&self, principal_id: &str, peer_ip: &str) -> SeamResult<EgressPolicy> {
        let body = PrincipalBody {
            principal_id,
            peer_ip,
        };
        self.post("/internal/egress/policy", &body).await
    }

    pub async fn stubs(&self, principal_id: &str, peer_ip: &str) -> SeamResult<Vec<SecretStub>> {
        let body = PrincipalBody {
            principal_id,
            peer_ip,
        };
        self.post("/internal/egress/stubs", &body).await
    }

    pub async fn secret(
        &self,
        principal_id: &str,
        peer_ip: &str,
        name: &str,
    ) -> SeamResult<SecretValue> {
        let body = SecretBody {
            principal_id,
            peer_ip,
            name,
        };
        let response: SecretResponse = self.post("/internal/egress/secret", &body).await?;
        Ok(SecretValue::new(response.value))
    }

    pub async fn audit(&self, events: &[AuditEvent]) -> SeamResult<()> {
        let _: serde_json::Value = self
            .post("/internal/egress/audit", &AuditBody { events })
            .await?;
        Ok(())
    }
}

/// `EgressSeam` bound to one tenant.
pub struct RemoteEgress {
    control: Arc<RemoteControl>,
    principal_id: String,
    peer_ip: String,
}

impl RemoteEgress {
    pub fn new(
        control: Arc<RemoteControl>,
        principal_id: impl Into<String>,
        peer_ip: impl Into<String>,
    ) -> Self {
        Self {
            control,
            principal_id: principal_id.into(),
            peer_ip: peer_ip.into(),
        }
    }
}

#[async_trait::async_trait]
impl EgressSeam for RemoteEgress {
    async fn policy(&self) -> SeamResult<EgressPolicy> {
        self.control.policy(&self.principal_id, &self.peer_ip).await
    }
}

/// `CredentialsSeam` bound to one tenant. Read-only by construction.
pub struct RemoteCredentials {
    control: Arc<RemoteControl>,
    principal_id: String,
    peer_ip: String,
}

impl RemoteCredentials {
    pub fn new(
        control: Arc<RemoteControl>,
        principal_id: impl Into<String>,
        peer_ip: impl Into<String>,
    ) -> Self {
        Self {
            control,
            principal_id: principal_id.into(),
            peer_ip: peer_ip.into(),
        }
    }

    fn readonly<T>(what: &str) -> SeamResult<T> {
        Err(SeamError::Unsupported(format!(
            "the egress proxy cannot {what}: credentials are written by the control plane"
        )))
    }
}

#[async_trait::async_trait]
impl CredentialsSeam for RemoteCredentials {
    async fn list(&self) -> SeamResult<Vec<SecretMeta>> {
        Self::readonly("list secrets")
    }

    async fn resolve(&self, name: &str) -> SeamResult<SecretValue> {
        self.control
            .secret(&self.principal_id, &self.peer_ip, name)
            .await
    }

    async fn rotate(&self, _name: &str, _value: SecretValue) -> SeamResult<SecretMeta> {
        Self::readonly("rotate a secret")
    }

    async fn delete(&self, _name: &str) -> SeamResult<()> {
        Self::readonly("delete a secret")
    }

    async fn stubs(&self) -> SeamResult<Vec<SecretStub>> {
        self.control.stubs(&self.principal_id, &self.peer_ip).await
    }

    async fn put_stub(&self, _stub: SecretStub) -> SeamResult<()> {
        Self::readonly("write a stub")
    }

    async fn remove_stub(&self, _id: &str) -> SeamResult<()> {
        Self::readonly("remove a stub")
    }

    // `substitute` keeps the provided implementation: matching and rendering
    // live in exactly one place (agent-core), so the proxy and the control plane
    // cannot drift.
}

// Audit is forwarded in batches by `crate::audit::RemoteAudit`, which owns the
// queue; this module only carries the transport.

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::EgressMode;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn control(server: &MockServer) -> Arc<RemoteControl> {
        Arc::new(RemoteControl::new(server.uri(), "token-1").unwrap())
    }

    #[tokio::test]
    async fn whoami_and_policy_round_trip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/whoami"))
            .and(header("X-Aria-Proxy-Token", "token-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "principal_id": "tenant-a",
                "sandbox_id": "sbx-1",
                "session_id": "s1"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/policy"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "revision": 4,
                "mode": "allowlist",
                "allowed": ["api.openai.com"],
                "denied": []
            })))
            .mount(&server)
            .await;

        let control = control(&server);
        let who = control.whoami("10.0.0.5").await.unwrap();
        assert_eq!(who.principal_id, "tenant-a");
        assert_eq!(who.session_id.as_deref(), Some("s1"));

        let egress = RemoteEgress::new(control, "tenant-a", "10.0.0.5");
        let policy = egress.policy().await.unwrap();
        assert_eq!(policy.mode, EgressMode::Allowlist);
        assert_eq!(policy.revision, 4);
        assert!(policy.decide("api.openai.com").is_allowed());
    }

    #[tokio::test]
    async fn secrets_resolve_but_never_write() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/secret"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"value": "sk-live"})),
            )
            .mount(&server)
            .await;
        let credentials = RemoteCredentials::new(control(&server), "tenant-a", "10.0.0.5");
        let value = credentials.resolve("openai").await.unwrap();
        assert_eq!(value.expose(), "sk-live");
        assert!(matches!(
            credentials.rotate("openai", SecretValue::new("x")).await,
            Err(SeamError::Unsupported(_))
        ));
        assert!(matches!(
            credentials
                .put_stub(SecretStub {
                    id: "s".into(),
                    host: agent_core::seams::HostPattern::parse("a.com").unwrap(),
                    path_prefix: None,
                    credential: "openai".into(),
                    header: "authorization".into(),
                    value_template: "Bearer ${credential}".into(),
                })
                .await,
            Err(SeamError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn control_plane_errors_are_mapped_not_swallowed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/whoami"))
            .respond_with(ResponseTemplate::new(403).set_body_string("secret-looking-body"))
            .mount(&server)
            .await;
        let err = control(&server).whoami("10.0.0.5").await.unwrap_err();
        assert!(matches!(err, SeamError::Denied(_)), "{err}");
        assert!(
            !err.to_string().contains("secret-looking-body"),
            "an error must not echo the response body"
        );

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/policy"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert!(matches!(
            RemoteEgress::new(control(&server), "t", "10.0.0.5")
                .policy()
                .await,
            Err(SeamError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn transport_errors_and_bad_bodies_are_mapped() {
        // Nothing is listening: the send itself fails.
        let unreachable = RemoteControl::new("http://127.0.0.1:1", "t").unwrap();
        assert!(matches!(
            unreachable.whoami("10.0.0.5").await,
            Err(SeamError::Unavailable(_))
        ));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;
        assert!(matches!(
            control(&server).whoami("10.0.0.5").await,
            Err(SeamError::Invalid(_))
        ));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/whoami"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(matches!(
            control(&server).whoami("10.0.0.5").await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn stubs_are_forwarded_while_writes_stay_local_refusals() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/stubs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        let credentials = RemoteCredentials::new(control(&server), "tenant-a", "10.0.0.5");
        assert!(credentials.stubs().await.unwrap().is_empty());
        assert!(matches!(
            credentials.list().await,
            Err(SeamError::Unsupported(_))
        ));
        assert!(matches!(
            credentials.delete("openai").await,
            Err(SeamError::Unsupported(_))
        ));
        assert!(matches!(
            credentials.remove_stub("s").await,
            Err(SeamError::Unsupported(_))
        ));
    }

    #[test]
    fn an_empty_base_url_is_refused() {
        assert!(matches!(
            RemoteControl::new("", "t"),
            Err(SeamError::Unavailable(_))
        ));
    }

    #[test]
    fn the_token_never_appears_in_debug_output() {
        let control = RemoteControl::new("http://cp:8080", "super-secret-token").unwrap();
        let debug = format!("{control:?}");
        assert!(!debug.contains("super-secret-token"), "{debug}");
    }
}
