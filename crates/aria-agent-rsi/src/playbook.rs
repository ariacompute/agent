//! In-memory, revision-guarded playbook storage.
//!
//! This is the reference implementation of [`PlaybookStore`] and the semantics
//! every other provider (the Postgres one in `aria-agent-cloud`) must match:
//!
//! * `submit` **never** publishes. A proposal is inert until `promote`.
//! * `promote` takes the revision the approver believes is current and refuses a
//!   mismatch with [`SeamError::Conflict`]. Two reviewers approving against a
//!   stale view cannot silently overwrite each other — the pattern
//!   `aria-agent-cloud` established for egress policy revisions extends to
//!   playbooks, and last-writer-wins is never the fallback.
//! * Candidate validation (evidence of improvement + cross-tenant adaptation) is
//!   re-run at promotion time, so a store cannot be tricked into publishing
//!   something that was only valid when it was submitted.
//!
//! It lives in this crate rather than in `core` for the same reason
//! `MemoryTeams` lives in `aria-agent-orchestrator`: the domain crate owns both
//! the contract's reference behaviour and the fake tests use.

use agent_core::context::now_ms;
use agent_core::seams::{
    Playbook, PlaybookCandidate, PlaybookStore, Proposal, RsiScope, SeamError, SeamResult,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

/// `(principal, domain, key)` — the address of one version chain.
type Slot = (String, String, String);

#[derive(Default)]
struct Inner {
    candidates: BTreeMap<String, PlaybookCandidate>,
    /// Proposal ids in submission order, so `pending` is oldest-first.
    order: Vec<String>,
    versions: BTreeMap<Slot, Vec<Playbook>>,
}

fn slot_of(scope: &RsiScope, key: &str) -> Slot {
    (
        scope.principal.clone(),
        scope.domain.clone(),
        key.to_string(),
    )
}

/// Split `"pathology:tool"` back into its parts.
fn split_key(key: &str) -> (String, Option<String>) {
    match key.split_once(':') {
        Some((pathology, tool)) => (pathology.to_string(), Some(tool.to_string())),
        None => (key.to_string(), None),
    }
}

fn proposal_from(candidate: &PlaybookCandidate) -> Proposal {
    let (pathology, tool) = split_key(&candidate.key);
    Proposal {
        id: candidate.proposal_id.clone(),
        scope: candidate.scope.clone(),
        origin: candidate.origin.clone(),
        pathology,
        tool,
        title: candidate.title.clone(),
        body: candidate.body.clone(),
        score: candidate.score,
        baseline: candidate.baseline,
        evidence: candidate.evidence.clone(),
        created_at_ms: now_ms(),
        adaptation: candidate.adaptation.clone(),
    }
}

/// Memory-backed [`PlaybookStore`].
#[derive(Default)]
pub struct MemoryPlaybooks {
    inner: Mutex<Inner>,
}

impl MemoryPlaybooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pending proposals across every placement (test/observability helper).
    pub fn pending_count(&self) -> usize {
        self.lock().candidates.len()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl PlaybookStore for MemoryPlaybooks {
    async fn submit(&self, candidate: PlaybookCandidate) -> SeamResult<Proposal> {
        candidate.validate()?;
        let mut inner = self.lock();
        if let Some(existing) = inner.candidates.get(&candidate.proposal_id) {
            // Submitting the same proposal twice is a retry, not a duplicate.
            return Ok(proposal_from(existing));
        }
        let proposal = proposal_from(&candidate);
        inner.order.push(candidate.proposal_id.clone());
        inner
            .candidates
            .insert(candidate.proposal_id.clone(), candidate);
        Ok(proposal)
    }

    async fn pending(&self, scope: &RsiScope) -> SeamResult<Vec<Proposal>> {
        let inner = self.lock();
        Ok(inner
            .order
            .iter()
            .filter_map(|id| inner.candidates.get(id))
            .filter(|candidate| candidate.scope.same_placement(scope))
            .map(proposal_from)
            .collect())
    }

    async fn promote(&self, proposal_id: &str, expected_revision: u64) -> SeamResult<Playbook> {
        let mut inner = self.lock();
        let candidate = inner
            .candidates
            .get(proposal_id)
            .cloned()
            .ok_or_else(|| SeamError::NotFound(format!("proposal {proposal_id}")))?;
        let slot = slot_of(&candidate.scope, &candidate.key);
        let current = inner
            .versions
            .get(&slot)
            .and_then(|versions| versions.last())
            .map(|playbook| playbook.revision)
            .unwrap_or(0);
        if current != expected_revision {
            // The pending candidate stays pending: the approver can re-read and
            // retry instead of losing the work.
            return Err(SeamError::Conflict(format!(
                "playbook {} is at revision {current}, promotion expected {expected_revision}",
                candidate.key
            )));
        }
        // Re-validate now: an adaptation or a score that was fine at submission
        // time must still be fine at publication time.
        candidate.validate()?;
        let playbook = Playbook {
            key: candidate.key.clone(),
            scope: candidate.scope.clone(),
            origin: candidate.origin.clone(),
            revision: current + 1,
            title: candidate.title.clone(),
            body: candidate.body.clone(),
            score: candidate.score,
            source_proposal: proposal_id.to_string(),
            adaptation: candidate.adaptation.clone(),
            updated_at_ms: now_ms(),
        };
        inner
            .versions
            .entry(slot)
            .or_default()
            .push(playbook.clone());
        inner.candidates.remove(proposal_id);
        inner.order.retain(|id| id != proposal_id);
        Ok(playbook)
    }

    async fn discard(&self, proposal_id: &str) -> SeamResult<Proposal> {
        let mut inner = self.lock();
        let candidate = inner
            .candidates
            .remove(proposal_id)
            .ok_or_else(|| SeamError::NotFound(format!("proposal {proposal_id}")))?;
        inner.order.retain(|id| id != proposal_id);
        Ok(proposal_from(&candidate))
    }

    async fn current(&self, scope: &RsiScope, key: &str) -> SeamResult<Option<Playbook>> {
        Ok(self
            .lock()
            .versions
            .get(&slot_of(scope, key))
            .and_then(|versions| versions.last())
            .cloned())
    }

    async fn list_versions(&self, scope: &RsiScope, key: &str) -> SeamResult<Vec<Playbook>> {
        Ok(self
            .lock()
            .versions
            .get(&slot_of(scope, key))
            .cloned()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> RsiScope {
        RsiScope::new("tenant", "orders")
    }

    fn candidate(id: &str) -> PlaybookCandidate {
        PlaybookCandidate {
            proposal_id: id.into(),
            origin: scope(),
            scope: scope(),
            key: "retry_storm:shell".into(),
            title: "Stop retrying blindly".into(),
            body: "1. read the result first".into(),
            score: 88,
            baseline: 70,
            evidence: Vec::new(),
            adaptation: None,
        }
    }

    #[tokio::test]
    async fn submit_only_queues_and_promote_publishes_revision_one() {
        let store = MemoryPlaybooks::new();
        let proposal = store.submit(candidate("p1")).await.unwrap();
        assert_eq!(proposal.id, "p1");
        assert_eq!(proposal.pathology, "retry_storm");
        assert_eq!(proposal.tool.as_deref(), Some("shell"));
        assert_eq!(store.pending_count(), 1);
        assert!(store
            .current(&scope(), "retry_storm:shell")
            .await
            .unwrap()
            .is_none());

        let playbook = store.promote("p1", 0).await.unwrap();
        assert_eq!(playbook.revision, 1);
        assert_eq!(playbook.key, "retry_storm:shell");
        assert_eq!(playbook.scope, scope());
        assert_eq!(playbook.origin, scope());
        assert_eq!(playbook.source_proposal, "p1");
        assert_eq!(store.pending_count(), 0);
        assert_eq!(
            store
                .current(&scope(), "retry_storm:shell")
                .await
                .unwrap()
                .unwrap(),
            playbook
        );
    }

    #[tokio::test]
    async fn a_stale_expected_revision_conflicts_and_keeps_the_candidate() {
        let store = MemoryPlaybooks::new();
        store.submit(candidate("p1")).await.unwrap();
        store.promote("p1", 0).await.unwrap();

        store.submit(candidate("p2")).await.unwrap();
        let err = store.promote("p2", 0).await.unwrap_err();
        assert!(matches!(err, SeamError::Conflict(_)), "{err:?}");
        assert_eq!(err.code(), "conflict");
        assert_eq!(
            store.pending_count(),
            1,
            "the candidate survives a conflict"
        );

        let second = store.promote("p2", 1).await.unwrap();
        assert_eq!(second.revision, 2);
        let versions = store
            .list_versions(&scope(), "retry_storm:shell")
            .await
            .unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[1].revision, 2);
    }

    #[tokio::test]
    async fn a_key_without_a_tool_becomes_a_scope_wide_proposal() {
        let store = MemoryPlaybooks::new();
        let mut plain = candidate("p1");
        plain.key = "compaction_churn".into();
        let proposal = store.submit(plain).await.unwrap();
        assert_eq!(proposal.pathology, "compaction_churn");
        assert_eq!(proposal.tool, None);
    }

    #[tokio::test]
    async fn submitting_twice_is_idempotent() {
        let store = MemoryPlaybooks::new();
        let first = store.submit(candidate("p1")).await.unwrap();
        let again = store.submit(candidate("p1")).await.unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(store.pending_count(), 1);
    }

    #[tokio::test]
    async fn pending_is_placement_scoped_and_oldest_first() {
        let store = MemoryPlaybooks::new();
        store.submit(candidate("p1")).await.unwrap();
        let mut other = candidate("p2");
        other.scope = RsiScope::new("tenant-b", "orders");
        other.origin = other.scope.clone();
        store.submit(other).await.unwrap();

        let pending = store.pending(&scope()).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "p1");
        let other_pending = store
            .pending(&RsiScope::new("tenant-b", "orders"))
            .await
            .unwrap();
        assert_eq!(other_pending[0].id, "p2");
        assert!(store
            .pending(&RsiScope::new("nobody", "nothing"))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn discard_removes_the_candidate_without_publishing() {
        let store = MemoryPlaybooks::new();
        store.submit(candidate("p1")).await.unwrap();
        let discarded = store.discard("p1").await.unwrap();
        assert_eq!(discarded.id, "p1");
        assert_eq!(store.pending_count(), 0);
        assert!(store
            .current(&scope(), "retry_storm:shell")
            .await
            .unwrap()
            .is_none());
        assert!(matches!(
            store.discard("p1").await,
            Err(SeamError::NotFound(_))
        ));
        assert!(matches!(
            store.promote("missing", 0).await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn invalid_candidates_are_rejected_at_submit_time() {
        let store = MemoryPlaybooks::new();
        let mut no_gain = candidate("p1");
        no_gain.score = no_gain.baseline;
        assert!(matches!(
            store.submit(no_gain).await,
            Err(SeamError::Invalid(_))
        ));
        assert_eq!(store.pending_count(), 0);

        let mut unadapted = candidate("p2");
        unadapted.scope = RsiScope::new("tenant-b", "orders");
        assert!(matches!(
            store.submit(unadapted).await,
            Err(SeamError::Unadapted(_))
        ));
    }

    #[tokio::test]
    async fn versions_are_per_key_and_unknown_keys_are_empty() {
        let store = MemoryPlaybooks::new();
        store.submit(candidate("p1")).await.unwrap();
        store.promote("p1", 0).await.unwrap();
        assert!(store
            .list_versions(&scope(), "other:tool")
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .current(&scope(), "other:tool")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_poisoned_lock_is_recovered() {
        let store = std::sync::Arc::new(MemoryPlaybooks::new());
        let clone = store.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.inner.lock().unwrap();
            panic!("poison the store");
        })
        .join();
        assert_eq!(store.pending_count(), 0);
    }
}
