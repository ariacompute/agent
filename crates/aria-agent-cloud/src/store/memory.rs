//! In-memory implementation of every store trait.
//!
//! Semantics intentionally mirror the Postgres implementation so the API tests
//! exercise the same behaviour without a database:
//!
//! * `principal_id` scoping (admins read across tenants, users do not),
//! * newest-first listing with a `limit`,
//! * turn/item ordering (`created_at`, then `seq`),
//! * revoking hides a key from listings and from resolution.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::auth::{ApiKeyScopes, Principal, PrincipalKind};
use crate::store::{
    AgentStore, ApiKeyRow, ApiKeyStore, ItemRow, NewApiKeyRow, ResolvedKey, SessionRow,
    SessionStore, TurnRow,
};
use crate::types::{Agent, AGENT_OBJECT};
use crate::AppError;

#[derive(Default)]
struct Inner {
    agents: Vec<Agent>,
    /// Owning `principal_id` per agent id (the owner is not part of the wire
    /// type, exactly like the `agents.principal_id` column).
    tenants: std::collections::HashMap<String, String>,
    sessions: Vec<SessionRow>,
    turns: Vec<TurnRow>,
    items: Vec<ItemRow>,
    keys: Vec<(NewApiKeyRow, Option<String>)>,
}

/// One lock-backed store implementing [`AgentStore`], [`SessionStore`] and
/// [`ApiKeyStore`]. Cheap `Clone` is not provided on purpose: wrap it in an
/// `Arc` and coerce to the trait objects.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

fn can_see(principal: &Principal, owner: &str) -> bool {
    principal.kind == PrincipalKind::Admin || principal.id == owner
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AgentStore for MemoryStore {
    async fn insert_agent(&self, principal: &Principal, agent: &Agent) -> Result<(), AppError> {
        let mut g = self.inner.lock().unwrap();
        let mut stored = agent.clone();
        stored.object = AGENT_OBJECT.into();
        // The owner is not part of the wire type; keep the mapping explicit.
        g.agents.push(stored);
        g.tenants.insert(agent.id.clone(), principal.id.clone());
        Ok(())
    }

    async fn list_agents(&self, principal: &Principal, limit: i64) -> Result<Vec<Agent>, AppError> {
        let g = self.inner.lock().unwrap();
        let mut visible: Vec<&Agent> = g
            .agents
            .iter()
            .filter(|a| {
                can_see(
                    principal,
                    g.tenants.get(&a.id).map(String::as_str).unwrap_or(""),
                )
            })
            .collect();
        visible.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(visible
            .into_iter()
            .take(limit.max(0) as usize)
            .cloned()
            .collect())
    }

    async fn get_agent(&self, principal: &Principal, id_or_name: &str) -> Result<Agent, AppError> {
        let g = self.inner.lock().unwrap();
        let by = |pred: &dyn Fn(&Agent) -> bool| {
            g.agents
                .iter()
                .find(|a| {
                    pred(a)
                        && can_see(
                            principal,
                            g.tenants.get(&a.id).map(String::as_str).unwrap_or(""),
                        )
                })
                .cloned()
        };
        by(&|a| a.id == id_or_name)
            .or_else(|| by(&|a| a.name.as_deref() == Some(id_or_name)))
            .ok_or(AppError::NotFound)
    }

    async fn save_agent(&self, agent: &Agent) -> Result<(), AppError> {
        let mut g = self.inner.lock().unwrap();
        match g.agents.iter_mut().find(|a| a.id == agent.id) {
            Some(slot) => *slot = agent.clone(),
            None => return Err(AppError::NotFound),
        }
        Ok(())
    }

    async fn delete_agent(&self, principal: &Principal, id: &str) -> Result<String, AppError> {
        let mut g = self.inner.lock().unwrap();
        let idx = g
            .agents
            .iter()
            .position(|a| {
                a.id == id
                    && can_see(
                        principal,
                        g.tenants.get(&a.id).map(String::as_str).unwrap_or(""),
                    )
            })
            .ok_or(AppError::NotFound)?;
        let removed = g.agents.remove(idx);
        g.tenants.remove(&removed.id);
        Ok(removed.id)
    }
}

#[async_trait]
impl SessionStore for MemoryStore {
    async fn insert_session(
        &self,
        _principal: &Principal,
        session: &SessionRow,
    ) -> Result<(), AppError> {
        self.inner.lock().unwrap().sessions.push(session.clone());
        Ok(())
    }

    async fn list_sessions(
        &self,
        principal: &Principal,
        limit: i64,
    ) -> Result<Vec<SessionRow>, AppError> {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<&SessionRow> = g
            .sessions
            .iter()
            .filter(|s| can_see(principal, &s.principal_id))
            .collect();
        rows.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(rows
            .into_iter()
            .take(limit.max(0) as usize)
            .cloned()
            .collect())
    }

    async fn get_session(&self, principal: &Principal, id: &str) -> Result<SessionRow, AppError> {
        let g = self.inner.lock().unwrap();
        g.sessions
            .iter()
            .find(|s| s.id == id && can_see(principal, &s.principal_id))
            .cloned()
            .ok_or(AppError::NotFound)
    }

    async fn set_instructions(&self, id: &str, instructions: Option<&str>) -> Result<(), AppError> {
        let mut g = self.inner.lock().unwrap();
        let row = g
            .sessions
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(AppError::NotFound)?;
        row.instructions = instructions.map(str::to_string);
        Ok(())
    }

    async fn delete_session(&self, id: &str) -> Result<(), AppError> {
        let mut g = self.inner.lock().unwrap();
        g.sessions.retain(|s| s.id != id);
        g.turns.retain(|t| t.session_id != id);
        g.items.retain(|i| i.session_id != id);
        Ok(())
    }

    async fn insert_turn(&self, turn: &TurnRow) -> Result<(), AppError> {
        self.inner.lock().unwrap().turns.push(turn.clone());
        Ok(())
    }

    async fn finish_turn(
        &self,
        id: &str,
        status: &str,
        output: Option<&str>,
    ) -> Result<(), AppError> {
        let mut g = self.inner.lock().unwrap();
        let row = g
            .turns
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(AppError::NotFound)?;
        row.status = status.to_string();
        row.output = output.map(str::to_string);
        row.completed_at = Some(crate::util::now_ms());
        Ok(())
    }

    async fn list_turns(
        &self,
        principal: &Principal,
        session: &str,
    ) -> Result<Vec<TurnRow>, AppError> {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<&TurnRow> = g
            .turns
            .iter()
            .filter(|t| t.session_id == session && can_see(principal, &t.principal_id))
            .collect();
        rows.sort_by_key(|t| (t.created_at, t.id.clone()));
        Ok(rows.into_iter().cloned().collect())
    }

    async fn get_turn(
        &self,
        principal: &Principal,
        session: &str,
        turn: &str,
    ) -> Result<Option<TurnRow>, AppError> {
        let g = self.inner.lock().unwrap();
        Ok(g.turns
            .iter()
            .find(|t| {
                t.id == turn && t.session_id == session && can_see(principal, &t.principal_id)
            })
            .cloned())
    }

    async fn insert_item(&self, item: &ItemRow) -> Result<(), AppError> {
        self.inner.lock().unwrap().items.push(item.clone());
        Ok(())
    }

    async fn list_items(
        &self,
        principal: &Principal,
        session: &str,
    ) -> Result<Vec<ItemRow>, AppError> {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<&ItemRow> = g
            .items
            .iter()
            .filter(|i| i.session_id == session && can_see(principal, &i.principal_id))
            .collect();
        rows.sort_by_key(|i| (i.seq, i.created_at));
        Ok(rows.into_iter().cloned().collect())
    }
}

#[async_trait]
impl ApiKeyStore for MemoryStore {
    async fn insert_key(&self, row: &NewApiKeyRow) -> Result<(), AppError> {
        self.inner.lock().unwrap().keys.push((row.clone(), None));
        Ok(())
    }

    async fn list_keys(&self) -> Result<Vec<ApiKeyRow>, AppError> {
        let g = self.inner.lock().unwrap();
        Ok(g.keys
            .iter()
            .filter(|(_, revoked)| revoked.is_none())
            .map(|(k, _)| ApiKeyRow {
                id: k.id.clone(),
                key_prefix: k.key_prefix.clone(),
                principal_id: k.principal_id.clone(),
                label: k.label.clone(),
                scopes: k.scopes.clone(),
                created_at: crate::util::now_ms().to_string(),
            })
            .collect())
    }

    async fn revoke_key(&self, id: &str) -> Result<Option<String>, AppError> {
        let mut g = self.inner.lock().unwrap();
        for (key, revoked) in g.keys.iter_mut() {
            if key.id == id && revoked.is_none() {
                *revoked = Some(crate::util::now_ms().to_string());
                return Ok(Some(key.key_hash.clone()));
            }
        }
        Ok(None)
    }

    async fn resolve_key(&self, key_hash: &str) -> Result<Option<ResolvedKey>, AppError> {
        let g = self.inner.lock().unwrap();
        Ok(g.keys
            .iter()
            .find(|(k, revoked)| revoked.is_none() && k.key_hash == key_hash)
            .map(|(k, _)| ResolvedKey {
                principal_id: k.principal_id.clone(),
                scopes: ApiKeyScopes::from_tokens(&k.scopes),
            }))
    }
}

/// Test-only decorator that fails a chosen call, used to prove the handlers map
/// storage failures to `500` instead of panicking.
#[cfg(test)]
pub mod faulty {
    use async_trait::async_trait;
    use std::sync::Arc;

    use crate::auth::Principal;
    use crate::store::{
        AgentStore, ApiKeyRow, ApiKeyStore, ItemRow, NewApiKeyRow, ResolvedKey, SessionRow,
        SessionStore, TurnRow,
    };
    use crate::types::Agent;
    use crate::AppError;
    use agent_core::CoreError;

    fn boom() -> AppError {
        AppError::Core(CoreError::Config("injected store failure".into()))
    }

    /// Delegates every call to the wrapped store; the flags select failures.
    pub struct FaultyStore {
        pub inner: Arc<dyn AgentStore>,
        pub sessions: Arc<dyn SessionStore>,
        pub keys: Arc<dyn ApiKeyStore>,
    }

    #[async_trait]
    impl AgentStore for FaultyStore {
        async fn insert_agent(&self, _: &Principal, _: &Agent) -> Result<(), AppError> {
            Err(boom())
        }
        async fn list_agents(&self, _: &Principal, _: i64) -> Result<Vec<Agent>, AppError> {
            Err(boom())
        }
        async fn get_agent(&self, _: &Principal, _: &str) -> Result<Agent, AppError> {
            Err(boom())
        }
        async fn save_agent(&self, _: &Agent) -> Result<(), AppError> {
            Err(boom())
        }
        async fn delete_agent(&self, _: &Principal, _: &str) -> Result<String, AppError> {
            Err(boom())
        }
    }

    #[async_trait]
    impl SessionStore for FaultyStore {
        async fn insert_session(&self, _: &Principal, _: &SessionRow) -> Result<(), AppError> {
            Err(boom())
        }
        async fn list_sessions(&self, _: &Principal, _: i64) -> Result<Vec<SessionRow>, AppError> {
            Err(boom())
        }
        async fn get_session(&self, _: &Principal, _: &str) -> Result<SessionRow, AppError> {
            Err(boom())
        }
        async fn set_instructions(&self, _: &str, _: Option<&str>) -> Result<(), AppError> {
            Err(boom())
        }
        async fn delete_session(&self, _: &str) -> Result<(), AppError> {
            Err(boom())
        }
        async fn insert_turn(&self, _: &TurnRow) -> Result<(), AppError> {
            Err(boom())
        }
        async fn finish_turn(&self, _: &str, _: &str, _: Option<&str>) -> Result<(), AppError> {
            Err(boom())
        }
        async fn list_turns(&self, _: &Principal, _: &str) -> Result<Vec<TurnRow>, AppError> {
            Err(boom())
        }
        async fn get_turn(
            &self,
            _: &Principal,
            _: &str,
            _: &str,
        ) -> Result<Option<TurnRow>, AppError> {
            Err(boom())
        }
        async fn insert_item(&self, _: &ItemRow) -> Result<(), AppError> {
            Err(boom())
        }
        async fn list_items(&self, _: &Principal, _: &str) -> Result<Vec<ItemRow>, AppError> {
            Err(boom())
        }
    }

    #[async_trait]
    impl ApiKeyStore for FaultyStore {
        async fn insert_key(&self, _: &NewApiKeyRow) -> Result<(), AppError> {
            Err(boom())
        }
        async fn list_keys(&self) -> Result<Vec<ApiKeyRow>, AppError> {
            Err(boom())
        }
        async fn revoke_key(&self, _: &str) -> Result<Option<String>, AppError> {
            Err(boom())
        }
        async fn resolve_key(&self, _: &str) -> Result<Option<ResolvedKey>, AppError> {
            Err(boom())
        }
    }

    /// Delegating session store that fails only the named operations, so deep
    /// error branches (e.g. `finish_turn` after a successful model run) can be
    /// reached without breaking the reads that lead there.
    ///
    /// Every method funnels through [`FlakySessionStore::check`] so the double
    /// adds a single failing path instead of one per operation.
    pub struct FlakySessionStore {
        pub inner: Arc<dyn SessionStore>,
        pub agents: Arc<dyn AgentStore>,
        pub keys: Arc<dyn ApiKeyStore>,
        pub fail: &'static [&'static str],
    }

    impl FlakySessionStore {
        fn check(&self, op: &str) -> Result<(), AppError> {
            if self.fail.contains(&op) {
                Err(boom())
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl AgentStore for FlakySessionStore {
        async fn insert_agent(&self, p: &Principal, a: &Agent) -> Result<(), AppError> {
            self.agents.insert_agent(p, a).await
        }
        async fn list_agents(&self, p: &Principal, limit: i64) -> Result<Vec<Agent>, AppError> {
            self.agents.list_agents(p, limit).await
        }
        async fn get_agent(&self, p: &Principal, id: &str) -> Result<Agent, AppError> {
            self.agents.get_agent(p, id).await
        }
        async fn save_agent(&self, a: &Agent) -> Result<(), AppError> {
            self.agents.save_agent(a).await
        }
        async fn delete_agent(&self, p: &Principal, id: &str) -> Result<String, AppError> {
            self.agents.delete_agent(p, id).await
        }
    }

    #[async_trait]
    impl ApiKeyStore for FlakySessionStore {
        async fn insert_key(&self, row: &NewApiKeyRow) -> Result<(), AppError> {
            self.keys.insert_key(row).await
        }
        async fn list_keys(&self) -> Result<Vec<ApiKeyRow>, AppError> {
            self.keys.list_keys().await
        }
        async fn revoke_key(&self, id: &str) -> Result<Option<String>, AppError> {
            self.keys.revoke_key(id).await
        }
        async fn resolve_key(&self, hash: &str) -> Result<Option<ResolvedKey>, AppError> {
            self.keys.resolve_key(hash).await
        }
    }

    #[async_trait]
    impl SessionStore for FlakySessionStore {
        async fn insert_session(
            &self,
            principal: &Principal,
            session: &SessionRow,
        ) -> Result<(), AppError> {
            self.check("insert_session")?;
            self.inner.insert_session(principal, session).await
        }
        async fn list_sessions(
            &self,
            principal: &Principal,
            limit: i64,
        ) -> Result<Vec<SessionRow>, AppError> {
            self.check("list_sessions")?;
            self.inner.list_sessions(principal, limit).await
        }
        async fn get_session(
            &self,
            principal: &Principal,
            id: &str,
        ) -> Result<SessionRow, AppError> {
            self.check("get_session")?;
            self.inner.get_session(principal, id).await
        }
        async fn set_instructions(
            &self,
            id: &str,
            instructions: Option<&str>,
        ) -> Result<(), AppError> {
            self.check("set_instructions")?;
            self.inner.set_instructions(id, instructions).await
        }
        async fn delete_session(&self, id: &str) -> Result<(), AppError> {
            self.check("delete_session")?;
            self.inner.delete_session(id).await
        }
        async fn insert_turn(&self, turn: &TurnRow) -> Result<(), AppError> {
            self.check("insert_turn")?;
            self.inner.insert_turn(turn).await
        }
        async fn finish_turn(
            &self,
            id: &str,
            status: &str,
            output: Option<&str>,
        ) -> Result<(), AppError> {
            self.check("finish_turn")?;
            self.inner.finish_turn(id, status, output).await
        }
        async fn list_turns(
            &self,
            principal: &Principal,
            session: &str,
        ) -> Result<Vec<TurnRow>, AppError> {
            self.check("list_turns")?;
            self.inner.list_turns(principal, session).await
        }
        async fn get_turn(
            &self,
            principal: &Principal,
            session: &str,
            turn: &str,
        ) -> Result<Option<TurnRow>, AppError> {
            self.check("get_turn")?;
            self.inner.get_turn(principal, session, turn).await
        }
        async fn insert_item(&self, item: &ItemRow) -> Result<(), AppError> {
            self.check("insert_item")?;
            self.inner.insert_item(item).await
        }
        async fn list_items(
            &self,
            principal: &Principal,
            session: &str,
        ) -> Result<Vec<ItemRow>, AppError> {
            self.check("list_items")?;
            self.inner.list_items(principal, session).await
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::store::MemoryStore;
        use crate::store::{AgentStore, ApiKeyStore, ItemRow, NewApiKeyRow, SessionStore, TurnRow};
        use crate::types::{AgentReasoning, MultiAgentConfig};
        use std::sync::Arc;

        /// The double must fail *every* operation, so each handler's error path
        /// has a genuine counterpart on the storage side.
        #[tokio::test]
        async fn faulty_store_fails_every_operation() {
            let mem = Arc::new(MemoryStore::new());
            let store = FaultyStore {
                inner: mem.clone(),
                sessions: mem.clone(),
                keys: mem.clone(),
            };
            let principal = Principal::admin();
            let agent = Agent {
                id: "a1".into(),
                object: crate::types::AGENT_OBJECT.into(),
                name: None,
                model: None,
                instructions: None,
                reasoning: AgentReasoning::default(),
                multi_agent: MultiAgentConfig::default(),
                service_tier: "auto".into(),
                text: None,
                tools: vec![],
                created_at: 0,
            };
            let session = SessionRow {
                id: "s1".into(),
                principal_id: principal.id.clone(),
                agent_id: agent.id.clone(),
                instructions: None,
                status: "idle".into(),
                created_at: 0,
            };
            let turn = TurnRow {
                id: "t1".into(),
                principal_id: principal.id.clone(),
                session_id: session.id.clone(),
                agent_id: agent.id.clone(),
                status: "in_progress".into(),
                output: None,
                created_at: 0,
                completed_at: None,
            };
            let item = ItemRow {
                id: "i1".into(),
                principal_id: principal.id.clone(),
                session_id: session.id.clone(),
                turn_id: turn.id.clone(),
                kind: "message".into(),
                role: Some("user".into()),
                status: "completed".into(),
                content: serde_json::json!("hi"),
                seq: 0,
                created_at: 0,
            };
            let key = NewApiKeyRow {
                id: "k1".into(),
                key_prefix: "aria-x".into(),
                key_hash: "hash".into(),
                principal_id: principal.id.clone(),
                label: None,
                scopes: vec!["read".into()],
            };

            assert!(AgentStore::insert_agent(&store, &principal, &agent)
                .await
                .is_err());
            assert!(AgentStore::list_agents(&store, &principal, 10)
                .await
                .is_err());
            assert!(AgentStore::get_agent(&store, &principal, "a1")
                .await
                .is_err());
            assert!(AgentStore::save_agent(&store, &agent).await.is_err());
            assert!(AgentStore::delete_agent(&store, &principal, "a1")
                .await
                .is_err());

            assert!(SessionStore::insert_session(&store, &principal, &session)
                .await
                .is_err());
            assert!(SessionStore::list_sessions(&store, &principal, 10)
                .await
                .is_err());
            assert!(SessionStore::get_session(&store, &principal, "s1")
                .await
                .is_err());
            assert!(SessionStore::set_instructions(&store, "s1", None)
                .await
                .is_err());
            assert!(SessionStore::delete_session(&store, "s1").await.is_err());
            assert!(SessionStore::insert_turn(&store, &turn).await.is_err());
            assert!(SessionStore::finish_turn(&store, "t1", "completed", None)
                .await
                .is_err());
            assert!(SessionStore::list_turns(&store, &principal, "s1")
                .await
                .is_err());
            assert!(SessionStore::get_turn(&store, &principal, "s1", "t1")
                .await
                .is_err());
            assert!(SessionStore::insert_item(&store, &item).await.is_err());
            assert!(SessionStore::list_items(&store, &principal, "s1")
                .await
                .is_err());

            assert!(ApiKeyStore::insert_key(&store, &key).await.is_err());
            assert!(ApiKeyStore::list_keys(&store).await.is_err());
            assert!(ApiKeyStore::revoke_key(&store, "k1").await.is_err());
            assert!(ApiKeyStore::resolve_key(&store, "hash").await.is_err());
        }
    }
}
