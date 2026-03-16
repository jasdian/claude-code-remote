use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::ClaudeConfig;
use crate::domain::ClaudeEvent;
use crate::error::AppError;

const STDOUT_BUF_CAPACITY: usize = 8 * 1024;
const EVENT_CHANNEL_CAPACITY: usize = 256;

pub struct ClaudeProcessHandle {
    child: Child,
    reader_task: JoinHandle<()>,
    cancel: CancellationToken,
}

impl ClaudeProcessHandle {
    pub async fn kill(mut self) -> Result<(), AppError> {
        self.cancel.cancel();
        self.child.kill().await?;
        self.reader_task.abort();
        Ok(())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

pub fn event_channel() -> (mpsc::Sender<ClaudeEvent>, mpsc::Receiver<ClaudeEvent>) {
    mpsc::channel(EVENT_CHANNEL_CAPACITY)
}

/// P4: All IO is async. P1: borrows config and prompt, never clones.
pub async fn run_claude(
    config: &ClaudeConfig,
    prompt: &str,
    session_id: Option<&str>,
    cwd: &Path,
    allowed_tools: &[Arc<str>],
    event_tx: mpsc::Sender<ClaudeEvent>,
    cancel: CancellationToken,
) -> Result<ClaudeProcessHandle, AppError> {
    let mut cmd = Command::new(config.binary.as_ref());
    cmd.arg("-p")
        .arg(prompt)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose");

    if config.dangerously_skip_permissions {
        cmd.arg("--dangerously-skip-permissions");
    }

    if !allowed_tools.is_empty() {
        let tools_str: String = allowed_tools
            .iter()
            .map(|t| t.as_ref())
            .collect::<Vec<_>>()
            .join(",");
        cmd.arg("--allowedTools").arg(tools_str);
    }

    if let Some(sid) = session_id {
        cmd.arg("--resume").arg(sid);
    }

    if let Some(ref sys_prompt) = config.system_prompt {
        cmd.arg("--append-system-prompt").arg(sys_prompt.as_ref());
    }

    cmd.current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::claude(&format!("failed to spawn claude: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::claude("no stdout from claude process"))?;

    let reader_cancel = cancel.clone();
    let reader_task = tokio::spawn(async move {
        let reader = BufReader::with_capacity(STDOUT_BUF_CAPACITY, stdout);
        let mut lines = reader.lines();

        loop {
            tokio::select! {
                _ = reader_cancel.cancelled() => {
                    tracing::debug!("claude reader cancelled");
                    break;
                }
                line_result = lines.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            if let Some(event) = super::parser::parse_stream_line(&line) {
                                if event_tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "claude stdout read error");
                            let _ = event_tx.send(ClaudeEvent::Error(
                                format!("stdout read: {e}").into_boxed_str()
                            )).await;
                            break;
                        }
                    }
                }
            }
        }
        let _ = event_tx.send(ClaudeEvent::Done).await;
    });

    Ok(ClaudeProcessHandle {
        child,
        reader_task,
        cancel,
    })
}
