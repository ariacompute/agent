//! Filesystem tool family (`fs_*`).
//!
//! All paths go through the `fs` seam, which enforces the single
//! [`SandboxPolicy`](agent_core::seams::SandboxPolicy) root.

use crate::{arg_str, family_handler, Family};
use agent_core::seams::{FsSeam, SeamError};
use agent_core::{Tool, ToolCall};
use serde_json::json;
use std::sync::Arc;

pub struct FsToolHandler {
    fs: Arc<dyn FsSeam>,
}

impl FsToolHandler {
    pub fn new(fs: Arc<dyn FsSeam>) -> Self {
        Self { fs }
    }
}

pub fn fs_tools() -> Vec<Tool> {
    let path = json!({ "type": "string", "description": "Path relative to the workspace root." });
    vec![
        Tool {
            name: "fs_read".into(),
            description: "Read a UTF-8 text file from the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": path },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "fs_write".into(),
            description: "Create or overwrite a text file in the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": path,
                    "contents": { "type": "string", "description": "Full file contents." }
                },
                "required": ["path", "contents"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "fs_edit".into(),
            description: "Replace the first occurrence of `old` with `new` in a file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": path,
                    "old": { "type": "string", "description": "Exact text to replace." },
                    "new": { "type": "string", "description": "Replacement text." }
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "fs_list".into(),
            description: "List a directory (directories end with `/`).".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": path },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "fs_stat".into(),
            description: "Stat a path (size, directory flag, mtime).".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": path },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
    ]
}

impl Family for FsToolHandler {
    fn prefix(&self) -> &'static str {
        "fs_"
    }

    async fn dispatch(&self, call: &ToolCall, _session: &str) -> Result<String, SeamError> {
        match call.name.as_str() {
            "fs_read" => self.fs.read(&arg_str(call, "path")?).await,
            "fs_write" => {
                let path = arg_str(call, "path")?;
                let contents = arg_str(call, "contents")?;
                self.fs.write(&path, &contents).await?;
                Ok(format!("wrote {} bytes to {path}", contents.len()))
            }
            "fs_edit" => {
                let path = arg_str(call, "path")?;
                let old = arg_str(call, "old")?;
                let new = arg_str(call, "new")?;
                let n = self.fs.edit(&path, &old, &new).await?;
                Ok(format!("applied {n} edit(s) to {path}"))
            }
            "fs_list" => {
                let entries = self.fs.list(&arg_str(call, "path")?).await?;
                if entries.is_empty() {
                    Ok("(empty)".into())
                } else {
                    Ok(entries.join("\n"))
                }
            }
            "fs_stat" => {
                let stat = self.fs.stat(&arg_str(call, "path")?).await?;
                serde_json::to_string(&stat).map_err(|e| SeamError::Io(e.to_string()))
            }
            other => Err(SeamError::Invalid(format!("unknown fs tool: {other}"))),
        }
    }
}

family_handler!(FsToolHandler);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::{LocalFs, SandboxPolicy};
    use agent_core::ToolHandler;
    use serde_json::json;

    fn handler() -> (tempfile::TempDir, FsToolHandler) {
        let dir = tempfile::tempdir().unwrap();
        let policy = Arc::new(SandboxPolicy::workspace(dir.path()).unwrap());
        (dir, FsToolHandler::new(Arc::new(LocalFs::new(policy))))
    }

    async fn run(h: &FsToolHandler, name: &str, args: serde_json::Value) -> agent_core::ToolResult {
        let ctx = agent_core::in_memory_context();
        h.run(
            &ToolCall {
                id: "c".into(),
                name: name.into(),
                arguments: args,
            },
            &ctx,
            "s",
        )
        .await
    }

    #[tokio::test]
    async fn write_read_edit_list_stat() {
        let (_dir, h) = handler();
        assert!(
            run(&h, "fs_write", json!({"path": "a.txt", "contents": "hi"}))
                .await
                .content
                .contains("wrote 2 bytes")
        );
        assert_eq!(
            run(&h, "fs_read", json!({"path": "a.txt"})).await.content,
            "hi"
        );
        assert!(run(
            &h,
            "fs_edit",
            json!({"path": "a.txt", "old": "hi", "new": "hey"})
        )
        .await
        .content
        .contains("1 edit"));
        assert_eq!(
            run(&h, "fs_list", json!({"path": "."})).await.content,
            "a.txt"
        );
        let stat = run(&h, "fs_stat", json!({"path": "a.txt"})).await;
        assert!(stat.content.contains("\"len\":3"), "{}", stat.content);
    }

    #[tokio::test]
    async fn empty_listing_and_path_escape() {
        let (_dir, h) = handler();
        assert_eq!(
            run(&h, "fs_list", json!({"path": "."})).await.content,
            "(empty)"
        );
        let escaped = run(&h, "fs_read", json!({"path": "../x"})).await;
        assert!(escaped.is_error);
        assert!(escaped.content.contains("denied"), "{}", escaped.content);
    }

    #[tokio::test]
    async fn missing_argument_and_unknown_tool_are_invalid() {
        let (_dir, h) = handler();
        let missing = run(&h, "fs_read", json!({})).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("invalid"));
        let unknown = run(&h, "fs_nope", json!({})).await;
        assert!(unknown.is_error);
        assert!(unknown.content.contains("unknown fs tool"));
    }

    #[test]
    fn handles_only_the_family_prefix() {
        let (_dir, h) = handler();
        assert!(h.handles("fs_read"));
        assert!(!h.handles("shell"));
    }
}
