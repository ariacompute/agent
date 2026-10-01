//! Pluggable tool families, each routed by a [`ToolHandler`] (see
//! `aria-agent-core`) before the shell fallback. Add a new family as a sub-module
//! here and re-export its constructor + handler at the crate root.

pub mod memo;
