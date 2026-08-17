//! Persistent PTY terminals, deliberately separate from `ManagedProcess`.

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

const OUTPUT_LIMIT: usize = 256 * 1024;
/// PTY 资源昂贵（fd + 读线程 + 缓冲区），限制上限防止模型无限打开。
const MAX_TERMINALS: usize = 8;
#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    total: u64,
    closed: bool,
}
struct Terminal {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Output>>,
    workspace: Option<crate::workspace::tracked::TrackedWorkspace>,
    journal: Option<(std::path::PathBuf, String)>,
}
#[derive(Default)]
pub struct TerminalRegistry {
    next: u64,
    terminals: HashMap<String, Terminal>,
}
pub struct TerminalRead {
    pub data: Vec<u8>,
    pub next_cursor: u64,
    pub truncated: bool,
    pub closed: bool,
}

impl TerminalRegistry {
    pub fn open(
        &mut self,
        program: &str,
        cwd: &std::path::Path,
        rows: u16,
        cols: u16,
    ) -> Result<String, String> {
        self.open_inner(program, cwd, rows, cols, None, None)
    }

    /// Open a PTY with a workspace snapshot owned for the whole terminal
    /// lifetime. The caller checkpoints it after observable command progress
    /// and on close, so PTY writes cannot bypass the mutation journal.
    pub fn open_tracked(
        &mut self,
        program: &str,
        cwd: &std::path::Path,
        rows: u16,
        cols: u16,
        workspace: crate::workspace::tracked::TrackedWorkspace,
        artifacts_root: std::path::PathBuf,
        session_id: String,
    ) -> Result<String, String> {
        self.open_inner(
            program,
            cwd,
            rows,
            cols,
            Some(workspace),
            Some((artifacts_root, session_id)),
        )
    }

    fn open_inner(
        &mut self,
        program: &str,
        cwd: &std::path::Path,
        rows: u16,
        cols: u16,
        workspace: Option<crate::workspace::tracked::TrackedWorkspace>,
        journal: Option<(std::path::PathBuf, String)>,
    ) -> Result<String, String> {
        if self.terminals.len() >= MAX_TERMINALS {
            return Err(format!(
                "terminal limit reached ({MAX_TERMINALS}); close an existing terminal first"
            ));
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("open pty: {e}"))?;
        let mut command = CommandBuilder::new(program);
        command.cwd(cwd);
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| format!("spawn terminal: {e}"))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("terminal writer: {e}"))?;
        #[cfg(windows)]
        {
            // ConPTY-backed shells can issue an initial DSR cursor-position
            // query before accepting normal input. TPI has no terminal
            // emulator frontend to answer it, so provide the harmless
            // top-left response at the PTY boundary.
            writer
                .write_all(b"\x1b[1;1R")
                .and_then(|_| writer.flush())
                .map_err(|e| format!("terminal initialize: {e}"))?;
        }
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("terminal reader: {e}"))?;
        let output = Arc::new(Mutex::new(Output::default()));
        let sink = output.clone();
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        sink.lock().expect("terminal output lock").closed = true;
                        break;
                    }
                    Ok(n) => {
                        let mut out = sink.lock().expect("terminal output lock");
                        out.total = out.total.saturating_add(n as u64);
                        out.bytes.extend_from_slice(&buf[..n]);
                        if out.bytes.len() > OUTPUT_LIMIT {
                            let excess = out.bytes.len() - OUTPUT_LIMIT;
                            out.bytes.drain(..excess);
                        }
                    }
                }
            }
        });
        self.next = self.next.saturating_add(1);
        let id = format!("t{}", self.next);
        self.terminals.insert(
            id.clone(),
            Terminal {
                master: pair.master,
                child,
                writer,
                output,
                workspace,
                journal,
            },
        );
        Ok(id)
    }
    pub fn write(&mut self, id: &str, data: &[u8]) -> Result<(), String> {
        let t = self
            .terminals
            .get_mut(id)
            .ok_or_else(|| "terminal not found".to_string())?;
        t.writer
            .write_all(data)
            .and_then(|_| t.writer.flush())
            .map_err(|e| format!("terminal write: {e}"))
    }

    /// Commit workspace changes since the previous checkpoint, if this
    /// terminal is locally tracked. A terminal without a tracking boundary is
    /// only used by low-level tests; agent-facing terminals always have one.
    pub fn checkpoint_workspace(
        &mut self,
        id: &str,
        artifacts: &std::path::Path,
        session: &str,
    ) -> Result<usize, String> {
        let terminal = self
            .terminals
            .get_mut(id)
            .ok_or_else(|| "terminal not found".to_string())?;
        match &mut terminal.workspace {
            Some(workspace) => workspace.commit(artifacts, session),
            None => Ok(0),
        }
    }
    pub fn read(&self, id: &str, after: u64) -> Result<TerminalRead, String> {
        let t = self
            .terminals
            .get(id)
            .ok_or_else(|| "terminal not found".to_string())?;
        let out = t
            .output
            .lock()
            .map_err(|_| "terminal output lock poisoned".to_string())?;
        let retained = out.total.saturating_sub(out.bytes.len() as u64);
        let start = after.max(retained);
        let offset = start.saturating_sub(retained) as usize;
        Ok(TerminalRead {
            data: out.bytes.get(offset..).unwrap_or_default().to_vec(),
            next_cursor: out.total,
            truncated: after < retained,
            closed: out.closed,
        })
    }
    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<(), String> {
        let t = self
            .terminals
            .get(id)
            .ok_or_else(|| "terminal not found".to_string())?;
        t.master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("terminal resize: {e}"))
    }
    pub fn signal(&mut self, id: &str) -> Result<(), String> {
        self.terminals
            .get_mut(id)
            .ok_or_else(|| "terminal not found".to_string())?
            .child
            .kill()
            .map_err(|e| format!("terminal signal: {e}"))
    }
    pub fn close(&mut self, id: &str) -> Result<(), String> {
        let t = self
            .terminals
            .get_mut(id)
            .ok_or_else(|| "terminal not found".to_string())?;
        let _ = t.child.kill();
        self.terminals.remove(id);
        Ok(())
    }
}
impl Drop for TerminalRegistry {
    fn drop(&mut self) {
        for t in self.terminals.values_mut() {
            let _ = t.child.kill();
            if let (Some(workspace), Some((artifacts_root, session_id))) =
                (&mut t.workspace, &t.journal)
                && let Err(error) = workspace.commit(artifacts_root, session_id)
            {
                tracing::error!(%error, "terminal workspace journal failed during shutdown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TerminalRegistry;
    use std::time::{Duration, Instant};

    #[test]
    fn pty_preserves_input_output_cursor_and_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let shell = if cfg!(windows) {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            "/bin/sh".into()
        };
        let mut registry = TerminalRegistry::default();
        let id = registry.open(&shell, dir.path(), 24, 80).unwrap();
        registry.resize(&id, 30, 100).unwrap();
        registry
            .write(&id, b"echo TPI_PTY_CURSOR_MARKER\r\n")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let first = loop {
            let read = registry.read(&id, 0).unwrap();
            if String::from_utf8_lossy(&read.data).contains("TPI_PTY_CURSOR_MARKER") {
                break read;
            }
            assert!(
                Instant::now() < deadline,
                "PTY did not return written command output: {:?}",
                String::from_utf8_lossy(&read.data)
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        assert!(first.next_cursor > 0);
        assert!(
            registry
                .read(&id, first.next_cursor)
                .unwrap()
                .data
                .is_empty()
        );
        registry.signal(&id).unwrap();
        registry.close(&id).unwrap();
        assert!(registry.read(&id, 0).is_err());
    }
}
