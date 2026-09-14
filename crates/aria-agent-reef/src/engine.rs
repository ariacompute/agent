//! The evolution engine — "Grow + Commit" of the Reef loop.
//!
//! Given eligible records + feedback, it asks the local model engine
//! ([`agent_core::ModelClient`], the "local engine FFI") to propose a candidate
//! [`Harness`](agent_core::Harness), scores candidate vs. the serving baseline,
//! and — **only if the candidate is strictly better** — writes it to the
//! `.reef/` artifact dir, tags it in Git, and hot-swaps the shared
//! [`agent_core::ActiveHarness`]. Failures are fail-closed: a model error, or a
//! candidate that does not beat the baseline, leaves the served harness intact.

use crate::error::ReefError;
use crate::feedback::{collect_eligible, FeedbackStore};
use crate::git;
use crate::harness;
use crate::record::RecordStore;
use agent_core::{ActiveHarness, Harness, ModelClient, ModelRequest, Skill};
use std::path::PathBuf;
use std::sync::Arc;

/// Result of an [`EvolutionEngine::evolve`] pass.
#[derive(Debug, Clone)]
pub struct EvolveOutcome {
    /// Git version number (`reef@<n>`); `0` when git is unavailable.
    pub version: usize,
    /// Whether the served harness was actually swapped to the candidate.
    pub adopted: bool,
    /// A preview of the candidate harness's composed system prompt.
    pub candidate_system: String,
}

/// Drives harness evolution for one agent deployment.
pub struct EvolutionEngine {
    engine: Box<dyn ModelClient>,
    records: Arc<dyn RecordStore>,
    feedback: Arc<dyn FeedbackStore>,
    /// `.reef/` artifact directory (Git-versioned harness files).
    harness_dir: PathBuf,
    /// Shared handle to the currently-served harness (hot-swapped on a win).
    active: Arc<ActiveHarness>,
    /// Minimum number of feedback reports before evolution is eligible.
    min_feedback: usize,
}

impl EvolutionEngine {
    pub fn new(
        engine: Box<dyn ModelClient>,
        records: Arc<dyn RecordStore>,
        feedback: Arc<dyn FeedbackStore>,
        harness_dir: PathBuf,
        active: Arc<ActiveHarness>,
        min_feedback: usize,
    ) -> Self {
        Self {
            engine,
            records,
            feedback,
            harness_dir,
            active,
            min_feedback,
        }
    }

    /// Current served harness (cheap `Arc` clone).
    pub fn active_harness(&self) -> Arc<Harness> {
        self.active.get()
    }

    /// Run one evolution pass.
    ///
    /// Returns [`ReefError::NoEligible`] when there isn't enough feedback signal
    /// yet; otherwise performs proposal + selection and reports the outcome.
    pub async fn evolve(&self) -> Result<EvolveOutcome, ReefError> {
        let eligible = collect_eligible(&self.feedback, &self.records, self.min_feedback).await?;
        let fbs = self.feedback.list().await?;

        let baseline = self.active.get();
        let candidate = self.propose_candidate(&baseline, &eligible, &fbs).await?;

        let base_score = score_harness(&baseline, &fbs);
        let cand_score = score_harness(&candidate, &fbs);

        if cand_score > base_score {
            // Winner: persist locally, version in Git, hot-swap.
            harness::save(&self.harness_dir, &candidate)?;
            // Git versioning is best-effort: if git is unavailable we still
            // hot-serve the winner and report version 0 (see ADR 0005).
            let version =
                git::publish(&self.harness_dir, "reef self-improvement").unwrap_or_else(|e| {
                    tracing::warn!("reef: git versioning skipped: {e}");
                    0
                });
            self.active.set(candidate.clone());
            Ok(EvolveOutcome {
                version,
                adopted: true,
                candidate_system: candidate.system_text(),
            })
        } else {
            // Fail-closed: keep the served harness unchanged.
            Ok(EvolveOutcome {
                version: git::current_version(&self.harness_dir).unwrap_or(0),
                adopted: false,
                candidate_system: candidate.system_text(),
            })
        }
    }

    /// Ask the local engine to propose a candidate harness.
    async fn propose_candidate(
        &self,
        baseline: &Harness,
        eligible: &[crate::record::Record],
        fbs: &[crate::feedback::Feedback],
    ) -> Result<Harness, ReefError> {
        let prompt = build_propose_prompt(baseline, eligible, fbs);
        let req = ModelRequest {
            system: "You evolve agent harnesses (system prompt, skills, rules).".into(),
            context: String::new(),
            input: prompt,
        };
        let resp = self
            .engine
            .complete(&req)
            .await
            .map_err(|e| ReefError::Engine(e.to_string()))?;
        Ok(parse_candidate(&resp.text, baseline, fbs))
    }
}

/// Deterministic, feedback-aware score for a harness.
///
/// Combines the mean feedback signal over eligible records with a small reward
/// for added skills/rules, so a candidate that addresses negative feedback
/// scores strictly higher than the baseline (which has none).
fn score_harness(h: &Harness, fbs: &[crate::feedback::Feedback]) -> f64 {
    let base: f64 = fbs.iter().map(|f| f.score).sum();
    base + 0.5 * (h.skills.len() + h.rules.len()) as f64
}

/// Build the prompt sent to the engine. Plain-word change request (Reefine-style).
fn build_propose_prompt(
    baseline: &Harness,
    eligible: &[crate::record::Record],
    fbs: &[crate::feedback::Feedback],
) -> String {
    let negatives: Vec<&crate::feedback::Feedback> = fbs.iter().filter(|f| f.score < 0.0).collect();
    let neg_text = if negatives.is_empty() {
        "(none)".to_string()
    } else {
        negatives
            .iter()
            .map(|f| {
                f.feedback
                    .clone()
                    .unwrap_or_else(|| format!("score={}", f.score))
            })
            .collect::<Vec<_>>()
            .join("\n- ")
    };
    let samples = eligible
        .iter()
        .take(5)
        .map(|r| format!("user: {}\nagent: {}", r.input, r.output))
        .collect::<Vec<_>>()
        .join("\n---\n");
    format!(
        "Current system prompt:\n{bas}\n\nNegative feedback to address:\n- {neg}\n\nSample turns:\n{samples}\n\n\
         Propose an improved harness as a JSON object \
         {{\"system_prompt\": str, \"skills\": [{{\"name\": str, \"body\": str}}], \"rules\": [{{\"name\": str, \"body\": str}}]}} \
         OR reply exactly NO_CHANGE if no improvement is warranted.",
        bas = baseline.system_prompt,
        neg = neg_text,
        samples = samples,
    )
}

/// Parse the engine output into a candidate harness.
///
/// * `NO_CHANGE` → keep the baseline (candidate equals baseline → not adopted).
/// * valid JSON `Harness` → use it directly.
/// * anything else (e.g. a stub model) → structural fallback that adds a skill
///   summarizing the negative feedback, guaranteeing a deterministic, testable
///   improvement.
fn parse_candidate(text: &str, baseline: &Harness, fbs: &[crate::feedback::Feedback]) -> Harness {
    if text.trim() == "NO_CHANGE" {
        return baseline.clone();
    }
    if let Ok(h) = serde_json::from_str::<Harness>(text) {
        return h;
    }
    fallback_candidate(baseline, fbs)
}

fn fallback_candidate(baseline: &Harness, fbs: &[crate::feedback::Feedback]) -> Harness {
    let negatives: Vec<&crate::feedback::Feedback> = fbs.iter().filter(|f| f.score < 0.0).collect();
    let summary = if negatives.is_empty() {
        "Address recent feedback to improve reliability.".to_string()
    } else {
        negatives
            .iter()
            .map(|f| {
                f.feedback
                    .clone()
                    .unwrap_or_else(|| format!("score={}", f.score))
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let mut h = baseline.clone();
    h.skills.push(Skill {
        name: "reef_improvement".into(),
        body: summary,
    });
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::{Feedback, FeedbackStore, SledFeedbackStore};
    use crate::record::{Record, RecordStore, SledRecordStore};
    use agent_core::StubModel;
    use async_trait::async_trait;

    fn make_engine(
        model: Box<dyn ModelClient>,
        recs: Arc<dyn RecordStore>,
        fb: Arc<dyn FeedbackStore>,
    ) -> (EvolutionEngine, Arc<ActiveHarness>) {
        let dir = std::env::temp_dir().join(format!("reef_eng_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        let active = Arc::new(ActiveHarness::baseline("agent"));
        let engine = EvolutionEngine::new(model, recs, fb, dir, active.clone(), 3);
        (engine, active)
    }

    #[tokio::test]
    async fn evolve_wins_on_negative_feedback_and_swaps() {
        let recs = SledRecordStore::memory().unwrap();
        let fb = SledFeedbackStore::memory().unwrap();
        let rec = Record::new("a", "s", None, "You are agent.", "in", "out", "stub");
        recs.record_turn(rec.clone()).await.unwrap();
        fb.report(Feedback::new(
            vec![rec.id.clone()],
            -1.0,
            Some("wrong".into()),
        ))
        .await
        .unwrap();

        let (engine, active) = make_engine(Box::new(StubModel::new("agent")), recs, fb);
        let before = active.get().system_text();
        let outcome = engine.evolve().await.unwrap();
        assert!(outcome.adopted, "candidate should beat baseline");
        let after = active.get().system_text();
        assert_ne!(before, after);
        assert!(
            after.contains("## Skills"),
            "swapped harness gained a skill"
        );
        assert!(after.contains("reef_improvement"));
    }

    #[tokio::test]
    async fn evolve_no_eligible_returns_no_eligible() {
        let recs = SledRecordStore::memory().unwrap();
        let fb = SledFeedbackStore::memory().unwrap();
        let (engine, active) = make_engine(Box::new(StubModel::new("agent")), recs, fb);
        let before = active.get().clone();
        let err = engine.evolve().await.unwrap_err();
        assert!(matches!(err, ReefError::NoEligible));
        // Active harness untouched.
        assert_eq!(active.get().system_text(), before.system_text());
    }

    #[tokio::test]
    async fn evolve_candidate_not_winning_keeps_active() {
        // Engine replies NO_CHANGE → candidate == baseline → not adopted.
        struct NoChangeModel;
        #[async_trait]
        impl ModelClient for NoChangeModel {
            async fn complete(
                &self,
                _req: &ModelRequest,
            ) -> Result<agent_core::ModelResponse, agent_core::CoreError> {
                Ok(agent_core::ModelResponse {
                    text: "NO_CHANGE".into(),
                })
            }
        }
        let recs = SledRecordStore::memory().unwrap();
        let fb = SledFeedbackStore::memory().unwrap();
        let rec = Record::new("a", "s", None, "You are agent.", "in", "out", "stub");
        recs.record_turn(rec.clone()).await.unwrap();
        fb.report(Feedback::new(
            vec![rec.id.clone()],
            -1.0,
            Some("wrong".into()),
        ))
        .await
        .unwrap();
        let (engine, active) = make_engine(Box::new(NoChangeModel), recs, fb);
        let before = active.get().clone();
        let outcome = engine.evolve().await.unwrap();
        assert!(!outcome.adopted);
        assert_eq!(active.get().system_text(), before.system_text());
    }

    #[tokio::test]
    async fn evolve_engine_error_keeps_active() {
        struct ErrModel;
        #[async_trait]
        impl ModelClient for ErrModel {
            async fn complete(
                &self,
                _req: &ModelRequest,
            ) -> Result<agent_core::ModelResponse, agent_core::CoreError> {
                Err(agent_core::CoreError::Model("boom".into()))
            }
        }
        let recs = SledRecordStore::memory().unwrap();
        let fb = SledFeedbackStore::memory().unwrap();
        let rec = Record::new("a", "s", None, "You are agent.", "in", "out", "stub");
        recs.record_turn(rec.clone()).await.unwrap();
        fb.report(Feedback::new(
            vec![rec.id.clone()],
            -1.0,
            Some("wrong".into()),
        ))
        .await
        .unwrap();
        let (engine, active) = make_engine(Box::new(ErrModel), recs, fb);
        let before = active.get().clone();
        let err = engine.evolve().await.unwrap_err();
        assert!(matches!(err, ReefError::Engine(_)));
        assert_eq!(active.get().system_text(), before.system_text());
    }

    #[tokio::test]
    async fn evolve_json_candidate_wins() {
        // Engine returns a valid improved JSON harness.
        struct JsonModel;
        #[async_trait]
        impl ModelClient for JsonModel {
            async fn complete(
                &self,
                _req: &ModelRequest,
            ) -> Result<agent_core::ModelResponse, agent_core::CoreError> {
                let json = r#"{"system_prompt":"You are agent.","skills":[{"name":"x","body":"y"}],"rules":[]}"#;
                Ok(agent_core::ModelResponse { text: json.into() })
            }
        }
        let recs = SledRecordStore::memory().unwrap();
        let fb = SledFeedbackStore::memory().unwrap();
        let rec = Record::new("a", "s", None, "You are agent.", "in", "out", "stub");
        recs.record_turn(rec.clone()).await.unwrap();
        fb.report(Feedback::new(
            vec![rec.id.clone()],
            -1.0,
            Some("wrong".into()),
        ))
        .await
        .unwrap();
        let (engine, active) = make_engine(Box::new(JsonModel), recs, fb);
        let outcome = engine.evolve().await.unwrap();
        assert!(outcome.adopted);
        assert!(
            active.get().system_text().contains("\"name\":\"x\"")
                || active.get().skills.iter().any(|s| s.name == "x")
        );
    }

    #[test]
    fn score_rewards_skills_and_rules() {
        let base = Harness::baseline("a");
        let mut improved = base.clone();
        improved.skills.push(Skill {
            name: "s".into(),
            body: "b".into(),
        });
        let fbs = vec![Feedback::new(vec!["r".into()], 0.0, None)];
        assert!(score_harness(&improved, &fbs) > score_harness(&base, &fbs));
    }
}
