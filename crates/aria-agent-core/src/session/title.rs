//! Session titles.
//!
//! dsh's `ctx.sessionTitle` is a seam with a **deterministic fallback**: the
//! first prompt, truncated. An async provider (e.g. a small LLM call) may
//! replace it, but the fallback is always available so a title never blocks a
//! turn (ADR-0015).

use async_trait::async_trait;

/// `TitleSet.source` for the deterministic first-prompt fallback.
pub const SOURCE_FIRST_PROMPT: &str = "first_prompt";
/// `TitleSet.source` for an async provider override.
pub const SOURCE_PROVIDER: &str = "provider";

/// Longest deterministic title we generate.
pub const MAX_TITLE_CHARS: usize = 80;

/// Deterministic fallback: first non-empty line, whitespace-collapsed, truncated.
pub fn first_prompt_title(input: &str) -> String {
    let line = input
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "Untitled session".into();
    }
    if collapsed.chars().count() <= MAX_TITLE_CHARS {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(MAX_TITLE_CHARS - 1).collect();
    out.push('…');
    out
}

/// Optional async title provider (one provider per deployment, ADR-0015).
#[async_trait]
pub trait SessionTitleProvider: Send + Sync {
    /// Return a title for the session, or `None` to keep the fallback.
    async fn title(&self, session: &str, first_input: &str) -> Option<String>;
}

/// Provider that always declines, keeping the deterministic fallback.
#[derive(Debug, Clone, Copy, Default)]
pub struct FallbackOnly;

#[async_trait]
impl SessionTitleProvider for FallbackOnly {
    async fn title(&self, _session: &str, _first_input: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_uses_the_first_non_empty_line() {
        assert_eq!(
            first_prompt_title("\n\n  list the files  \nmore"),
            "list the files"
        );
        assert_eq!(first_prompt_title(""), "Untitled session");
        assert_eq!(first_prompt_title("   \n\t\n"), "Untitled session");
    }

    #[test]
    fn long_titles_are_truncated_on_char_boundaries() {
        let long = "é".repeat(MAX_TITLE_CHARS + 10);
        let title = first_prompt_title(&long);
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(first_prompt_title("a    b\tc"), "a b c");
    }

    #[tokio::test]
    async fn fallback_provider_declines() {
        assert!(FallbackOnly.title("s", "hi").await.is_none());
    }
}
