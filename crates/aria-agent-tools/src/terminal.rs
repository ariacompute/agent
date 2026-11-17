//! Terminal tool family (`term_*`).
//!
//! A real PTY is only compiled with the `pty` feature: mobile bindings and the
//! default server build must not link a PTY implementation. Without the feature
//! the family is **not registered** by [`ToolSet`](crate::ToolSet), and if a
//! handler is constructed manually every call degrades to `unsupported` rather
//! than pretending to open a terminal.

#[cfg(feature = "pty")]
use crate::arg_str_opt;
use crate::{arg_str, family_handler, Family};
use agent_core::seams::{SeamError, ShellSeam};
use agent_core::{Tool, ToolCall};
use serde_json::json;
use std::sync::Arc;

/// Sessions are only read by the PTY backend; the default build keeps the same
/// shape so construction and the fail-closed path stay identical.
#[cfg_attr(not(feature = "pty"), allow(dead_code))]
pub struct TerminalToolHandler {
    shell: Option<Arc<dyn ShellSeam>>,
    sessions: std::sync::Mutex<std::collections::BTreeMap<String, TerminalSession>>,
}

#[cfg_attr(not(feature = "pty"), allow(dead_code))]
#[derive(Debug, Default)]
struct TerminalSession {
    /// Output collected so far (bounded).
    buffer: String,
    closed: bool,
}

/// Output cap per terminal session.
pub const MAX_TERMINAL_BYTES: usize = 256 * 1024;

impl TerminalToolHandler {
    pub fn new(shell: Option<Arc<dyn ShellSeam>>) -> Self {
        Self {
            shell,
            sessions: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    #[cfg_attr(not(feature = "pty"), allow(dead_code))]
    fn push_output(&self, id: &str, text: &str) -> Result<(), SeamError> {
        let mut guard = self.sessions.lock().unwrap();
        let session = guard
            .get_mut(id)
            .ok_or_else(|| SeamError::NotFound(format!("terminal {id}")))?;
        session.buffer.push_str(text);
        if session.buffer.len() > MAX_TERMINAL_BYTES {
            let cut = session
                .buffer
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|i| *i <= MAX_TERMINAL_BYTES)
                .last()
                .unwrap_or(0);
            session.buffer.drain(..cut);
        }
        Ok(())
    }
}

pub fn terminal_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "term_open".into(),
            description: "Open an interactive terminal session; returns its id.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "cwd": { "type": "string" } },
                "additionalProperties": false
            }),
        },
        Tool {
            name: "term_write".into(),
            description: "Send a line to a terminal session.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "input": { "type": "string" }
                },
                "required": ["id", "input"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "term_read".into(),
            description: "Read buffered terminal output.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "term_close".into(),
            description: "Close a terminal session.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
    ]
}

impl Family for TerminalToolHandler {
    fn prefix(&self) -> &'static str {
        "term_"
    }

    async fn dispatch(&self, call: &ToolCall, _session: &str) -> Result<String, SeamError> {
        #[cfg(not(feature = "pty"))]
        {
            let _ = (call, arg_str(call, "id"));
            Err(SeamError::Unsupported(
                "terminal support is not compiled in (enable the `pty` feature)".into(),
            ))
        }
        #[cfg(feature = "pty")]
        {
            match call.name.as_str() {
                "term_open" => {
                    let shell = self.shell.clone().ok_or_else(|| {
                        SeamError::Unavailable("no shell seam for the terminal".into())
                    })?;
                    let cwd = arg_str_opt(call, "cwd");
                    let id = pty::open(shell, cwd)?;
                    self.sessions
                        .lock()
                        .unwrap()
                        .insert(id.clone(), TerminalSession::default());
                    Ok(format!("opened terminal {id}"))
                }
                "term_write" => {
                    let id = arg_str(call, "id")?;
                    let input = arg_str(call, "input")?;
                    let out = pty::write(&id, &input)?;
                    self.push_output(&id, &out)?;
                    Ok("sent".into())
                }
                "term_read" => {
                    let id = arg_str(call, "id")?;
                    let fresh = pty::drain(&id)?;
                    self.push_output(&id, &fresh)?;
                    let guard = self.sessions.lock().unwrap();
                    let session = guard
                        .get(&id)
                        .ok_or_else(|| SeamError::NotFound(format!("terminal {id}")))?;
                    Ok(session.buffer.clone())
                }
                "term_close" => {
                    let id = arg_str(call, "id")?;
                    pty::close(&id)?;
                    if let Some(s) = self.sessions.lock().unwrap().get_mut(&id) {
                        s.closed = true;
                    }
                    Ok(format!("closed terminal {id}"))
                }
                other => Err(SeamError::Invalid(format!(
                    "unknown terminal tool: {other}"
                ))),
            }
        }
    }
}

family_handler!(TerminalToolHandler);

/// PTY-backed sessions (only compiled with `pty`).
#[cfg(feature = "pty")]
mod pty {
    use agent_core::seams::{SeamError, ShellSeam};
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex, OnceLock};

    struct Session {
        writer: Box<dyn Write + Send>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
        reader: Arc<Mutex<String>>,
    }

    fn registry() -> &'static Mutex<BTreeMap<String, Session>> {
        static REG: OnceLock<Mutex<BTreeMap<String, Session>>> = OnceLock::new();
        REG.get_or_init(|| Mutex::new(BTreeMap::new()))
    }

    pub fn open(_shell: Arc<dyn ShellSeam>, cwd: Option<String>) -> Result<String, SeamError> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize::default())
            .map_err(|e| SeamError::Io(format!("openpty: {e}")))?;
        let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = portable_pty::CommandBuilder::new(program);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| SeamError::Io(format!("spawn pty: {e}")))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| SeamError::Io(format!("pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| SeamError::Io(format!("pty writer: {e}")))?;
        let buffer = Arc::new(Mutex::new(String::new()));
        let sink = buffer.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&chunk[..n]).to_string();
                        if let Ok(mut buf) = sink.lock() {
                            buf.push_str(&text);
                        }
                    }
                }
            }
        });
        let id = uuid::Uuid::new_v4().to_string();
        registry().lock().unwrap().insert(
            id.clone(),
            Session {
                writer,
                child,
                reader: buffer,
            },
        );
        Ok(id)
    }

    pub fn write(id: &str, input: &str) -> Result<String, SeamError> {
        let mut guard = registry().lock().unwrap();
        let session = guard
            .get_mut(id)
            .ok_or_else(|| SeamError::NotFound(format!("terminal {id}")))?;
        session
            .writer
            .write_all(input.as_bytes())
            .and_then(|_| session.writer.write_all(b"\n"))
            .and_then(|_| session.writer.flush())
            .map_err(|e| SeamError::Io(format!("pty write: {e}")))?;
        Ok("sent".into())
    }

    pub fn drain(id: &str) -> Result<String, SeamError> {
        let guard = registry().lock().unwrap();
        let session = guard
            .get(id)
            .ok_or_else(|| SeamError::NotFound(format!("terminal {id}")))?;
        let mut buf = session
            .reader
            .lock()
            .map_err(|_| SeamError::Io("pty reader poisoned".into()))?;
        let text = buf.clone();
        buf.clear();
        Ok(text)
    }

    pub fn close(id: &str) -> Result<(), SeamError> {
        let mut guard = registry().lock().unwrap();
        let mut session = guard
            .remove(id)
            .ok_or_else(|| SeamError::NotFound(format!("terminal {id}")))?;
        let _ = session.child.kill();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ToolHandler;

    async fn run(
        h: &TerminalToolHandler,
        name: &str,
        args: serde_json::Value,
    ) -> agent_core::ToolResult {
        h.run(
            &ToolCall {
                id: "c".into(),
                name: name.into(),
                arguments: args,
            },
            &agent_core::in_memory_context(),
            "s",
        )
        .await
    }

    #[tokio::test]
    async fn without_the_pty_feature_every_call_is_unsupported() {
        let h = TerminalToolHandler::new(None);
        for (name, args) in [
            ("term_open", json!({})),
            ("term_write", json!({"id": "x", "input": "ls"})),
            ("term_read", json!({"id": "x"})),
            ("term_close", json!({"id": "x"})),
        ] {
            let res = run(&h, name, args).await;
            assert!(res.is_error, "{name}");
            assert!(
                res.content.contains("unsupported") || res.content.contains("not found"),
                "{name}: {}",
                res.content
            );
        }
    }

    #[test]
    fn schemas_are_stable() {
        let names: Vec<String> = terminal_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec!["term_open", "term_write", "term_read", "term_close"]
        );
        let h = TerminalToolHandler::new(None);
        assert!(h.handles("term_open"));
        assert!(!h.handles("other"));
    }

    #[test]
    fn output_buffer_is_bounded() {
        let h = TerminalToolHandler::new(None);
        h.sessions
            .lock()
            .unwrap()
            .insert("t".into(), TerminalSession::default());
        h.push_output("t", "small").unwrap();
        let big = "x".repeat(MAX_TERMINAL_BYTES + 10);
        h.push_output("t", &big).unwrap();
        let guard = h.sessions.lock().unwrap();
        assert!(guard["t"].buffer.len() <= MAX_TERMINAL_BYTES);
        drop(guard);
        assert!(h.push_output("missing", "x").is_err());
    }
}
