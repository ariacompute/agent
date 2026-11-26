//! Recursive self-improvement (RSI) for the agent runtime.
//!
//! The loop this crate implements is the one the reference material describes,
//! made deterministic and auditable:
//!
//! ```text
//! turn events ──► ledger ──► diagnosis ──► proposal ──► replay validation
//!                                                            │
//!                            ┌───────────────────────────────┘
//!                            ▼
//!                    pending (human gate) ──► promote ──► playbook rN + SKILL.md
//!                                                            │
//!                            ┌───────────────────────────────┘
//!                            ▼
//!              injected as a prompt section on the next turn
//! ```
//!
//! Boundaries this crate refuses to cross:
//!
//! * **No hot swap.** Promotion writes a versioned asset that the next turn
//!   reads; it never rewrites the prompt of a running session (the removed Reef
//!   design, `requirements.md §7`).
//! * **No unvalidated publishing.** A proposal that does not beat the baseline
//!   score is discarded, and an unanswered gate stays pending.
//! * **No silent cross-tenant reuse.** A playbook only reaches another tenant
//!   with an explicit, validated [`Adaptation`](agent_core::seams::Adaptation).
//!
//! Everything here is offline-testable: the diagnosis is a pure fold, proposals
//! come from a rule table, validation is a re-fold, and the only provider that
//! needs a model (a real re-run) is an optional trait.

pub mod engine;
pub mod ledger;
pub mod playbook;
pub mod propose;
pub mod skill;
pub mod sweep;
pub mod validate;

pub use engine::DeterministicReflection;
pub use ledger::{LedgerSink, LedgerStats, RsiLedger};
pub use playbook::MemoryPlaybooks;
pub use propose::propose_for_report;
pub use skill::{FilesystemSkillSink, SkillSink};
pub use sweep::{PromotionGate, ReportSink, RsiSweep, SweepOutcome};
pub use validate::{replay_score, ReplayValidator};
