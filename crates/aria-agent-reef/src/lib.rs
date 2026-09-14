//! `agent-reef` — Reef-style continual self-improvement for agents.
//!
//! The loop (mirroring [Human-Agent-Society/reef](https://github.com/Human-Agent-Society/reef)):
//! 1. **Serve** — [`record`] every turn and surface a receipt id.
//! 2. **Observe** — [`feedback`] binds a score to those receipts.
//! 3. **Grow + Commit** — [`engine`] proposes candidate [`Harness`](agent_core::Harness)
//!    updates via the local model engine ([`agent_core::ModelClient`]), keeps
//!    only the winner, versions it in Git ([`git`]), and hot-serves it back
//!    through the shared [`agent_core::ActiveHarness`].
//!
//! Records/feedback live in a local sled store (never Postgres/memo); versions
//! live in a local `.reef/` git repo. See `docs/adr/0005-reef-self-improvement.md`.

pub mod engine;
pub mod error;
pub mod feedback;
pub mod git;
pub mod harness;
pub mod record;

pub use error::ReefError;
