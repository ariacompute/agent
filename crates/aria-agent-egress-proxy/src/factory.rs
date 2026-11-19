//! Per-connection identity: turn the sandbox's source IP into a tenant, then
//! build a gateway that answers from the control plane.
//!
//! An unknown peer is an error, never a default tenant. A cache keeps the hot
//! path cheap; it is bounded and short-lived because Docker recycles IPs.

use crate::audit::RemoteAudit;
use crate::remote::{PeerIdentity, RemoteControl, RemoteCredentials, RemoteEgress};
use agent_core::seams::{SeamError, SeamResult};
use agent_governance::{EgressGateway, GatewayFactory};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Upper bound on cached peers. IPs are recycled, so the map is capped and the
/// TTL is short (default 10s).
const MAX_CACHED_PEERS: usize = 4096;

struct Entry {
    identity: PeerIdentity,
    at: Instant,
}

pub struct PeerGatewayFactory {
    control: Arc<RemoteControl>,
    audit: Arc<RemoteAudit>,
    ttl: Duration,
    peers: Mutex<HashMap<IpAddr, Entry>>,
}

impl PeerGatewayFactory {
    pub fn new(control: Arc<RemoteControl>, audit: Arc<RemoteAudit>, ttl: Duration) -> Self {
        Self {
            control,
            audit,
            ttl,
            peers: Mutex::new(HashMap::new()),
        }
    }

    async fn identity(&self, ip: IpAddr) -> SeamResult<PeerIdentity> {
        if let Some(entry) = self.lookup(ip) {
            return Ok(entry);
        }
        let identity = self.control.whoami(&ip.to_string()).await?;
        self.remember(ip, identity.clone());
        Ok(identity)
    }

    fn lookup(&self, ip: IpAddr) -> Option<PeerIdentity> {
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        match peers.get(&ip) {
            Some(entry) if entry.at.elapsed() < self.ttl => Some(entry.identity.clone()),
            Some(_) => {
                peers.remove(&ip);
                None
            }
            None => None,
        }
    }

    fn remember(&self, ip: IpAddr, identity: PeerIdentity) {
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        if peers.len() >= MAX_CACHED_PEERS {
            // Bounded and honest: drop the map rather than grow without limit.
            peers.clear();
        }
        peers.insert(
            ip,
            Entry {
                identity,
                at: Instant::now(),
            },
        );
    }
}

#[async_trait::async_trait]
impl GatewayFactory for PeerGatewayFactory {
    async fn for_peer(&self, peer: SocketAddr) -> SeamResult<Arc<EgressGateway>> {
        let identity = self.identity(peer.ip()).await.map_err(|e| match e {
            SeamError::NotFound(_) | SeamError::Denied(_) => {
                SeamError::Denied(format!("unknown egress peer {}", peer.ip()))
            }
            other => other,
        })?;
        let peer_ip = peer.ip().to_string();
        let egress = Arc::new(RemoteEgress::new(
            self.control.clone(),
            identity.principal_id.clone(),
            peer_ip.clone(),
        ));
        let credentials = Arc::new(RemoteCredentials::new(
            self.control.clone(),
            identity.principal_id.clone(),
            peer_ip,
        ));
        let mut gateway = EgressGateway::new(
            egress,
            credentials,
            self.audit.clone(),
            identity.principal_id,
        );
        if let Some(session) = identity.session_id {
            gateway = gateway.with_session(session);
        }
        Ok(Arc::new(gateway))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Counts how many times the proxy asked who a peer is.
    struct Counting {
        calls: Arc<Mutex<usize>>,
    }

    impl Respond for Counting {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            *self.calls.lock().unwrap() += 1;
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "principal_id": "tenant-a",
                "sandbox_id": "sbx-1"
            }))
        }
    }

    fn addr(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 40000)
    }

    async fn factory(server: &MockServer, calls: Arc<Mutex<usize>>) -> PeerGatewayFactory {
        Mock::given(method("POST"))
            .and(path("/internal/egress/whoami"))
            .respond_with(Counting { calls })
            .mount(server)
            .await;
        let control = Arc::new(RemoteControl::new(server.uri(), "t").unwrap());
        let audit = RemoteAudit::spawn(control.clone());
        PeerGatewayFactory::new(control, audit, Duration::from_secs(10))
    }

    #[tokio::test]
    async fn a_peer_is_resolved_once_within_the_ttl() {
        let server = MockServer::start().await;
        let calls = Arc::new(Mutex::new(0));
        let factory = factory(&server, calls.clone()).await;
        let first = factory.for_peer(addr("10.0.0.5")).await.unwrap();
        let second = factory.for_peer(addr("10.0.0.5")).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1, "cache hit, not a second RPC");
        assert!(format!("{first:?}").contains("tenant-a"));
        assert!(format!("{second:?}").contains("tenant-a"));
    }

    #[tokio::test]
    async fn an_unknown_peer_is_denied() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/whoami"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let control = Arc::new(RemoteControl::new(server.uri(), "t").unwrap());
        let audit = RemoteAudit::spawn(control.clone());
        let factory = PeerGatewayFactory::new(control, audit, Duration::from_secs(10));
        assert!(matches!(
            factory.for_peer(addr("10.0.0.9")).await,
            Err(SeamError::Denied(_))
        ));
    }

    #[tokio::test]
    async fn transport_failures_pass_through_as_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/whoami"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let control = Arc::new(RemoteControl::new(server.uri(), "t").unwrap());
        let audit = RemoteAudit::spawn(control.clone());
        let factory = PeerGatewayFactory::new(control, audit, Duration::from_secs(10));
        // Not a peer problem: the control plane is broken, and the caller must
        // see that rather than "unknown tenant".
        assert!(matches!(
            factory.for_peer(addr("10.0.0.5")).await,
            Err(SeamError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_full_cache_drops_itself_instead_of_growing() {
        let server = MockServer::start().await;
        let control = Arc::new(RemoteControl::new(server.uri(), "t").unwrap());
        let audit = RemoteAudit::spawn(control.clone());
        let factory = PeerGatewayFactory::new(control, audit, Duration::from_secs(10));
        let identity = PeerIdentity {
            principal_id: "tenant-a".into(),
            sandbox_id: "sbx".into(),
            session_id: None,
        };
        for last_octet in 0..=MAX_CACHED_PEERS {
            let ip: IpAddr = format!("10.1.{}.{}", last_octet / 256, last_octet % 256)
                .parse()
                .unwrap();
            factory.remember(ip, identity.clone());
        }
        assert_eq!(factory.peers.lock().unwrap().len(), 1, "the map restarts");
    }

    #[tokio::test]
    async fn the_cache_is_bounded_and_expires() {
        let server = MockServer::start().await;
        let calls = Arc::new(Mutex::new(0));
        Mock::given(method("POST"))
            .and(path("/internal/egress/whoami"))
            .respond_with(Counting {
                calls: calls.clone(),
            })
            .mount(&server)
            .await;
        let control = Arc::new(RemoteControl::new(server.uri(), "t").unwrap());
        let audit = RemoteAudit::spawn(control.clone());
        let factory = PeerGatewayFactory::new(control, audit, Duration::from_millis(40));

        factory.for_peer(addr("10.0.0.1")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        factory.for_peer(addr("10.0.0.1")).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 2, "an expired entry is re-resolved");
    }
}
