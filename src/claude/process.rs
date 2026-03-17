use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::ClaudeConfig;
use crate::domain::{ClaudeEvent, ClaudeExitReason};
use crate::error::AppError;

const STDOUT_BUF_CAPACITY: usize = 8 * 1024;
const EVENT_CHANNEL_CAPACITY: usize = 256;

pub struct ClaudeProcessHandle {
    reader_task: JoinHandle<()>,
    cancel: CancellationToken,
}

impl ClaudeProcessHandle {
    /// Kill the process. Cancels the reader task, which drops the child
    /// (triggering `kill_on_drop`).
    pub async fn kill(self) -> Result<(), AppError> {
        self.cancel.cancel();
        self.reader_task.abort();
        // The child lives inside the reader task — aborting drops it,
        // and kill_on_drop(true) ensures the process is killed.
        Ok(())
    }

    /// Signal the process to stop without taking ownership.
    /// The reader task will see the cancellation and close the event channel.
    /// The child process will be killed on drop via `kill_on_drop(true)`.
    pub fn signal_stop(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

pub fn event_channel() -> (mpsc::Sender<ClaudeEvent>, mpsc::Receiver<ClaudeEvent>) {
    mpsc::channel(EVENT_CHANNEL_CAPACITY)
}

/// P4: All IO is async. P1: borrows config and prompt, never clones.
/// `system_prompt_override`: if Some, replaces the config system_prompt entirely
/// (used to append co-author trailers for multi-user sessions).
#[allow(clippy::too_many_arguments)]
pub async fn run_claude(
    config: &ClaudeConfig,
    prompt: &str,
    session_id: Option<&str>,
    cwd: &Path,
    allowed_tools: &[Arc<str>],
    system_prompt_override: Option<&str>,
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

    // System prompt: override wins (includes co-author trailers), else config default
    let effective_prompt = system_prompt_override.or(config.system_prompt.as_deref());
    if let Some(sys_prompt) = effective_prompt {
        cmd.arg("--append-system-prompt").arg(sys_prompt);
    }

    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // P4: Own process group — terminal SIGINT only reaches the bot,
        // not the Claude subprocess. We manage child lifecycle via CancellationToken.
        .process_group(0);

    tracing::info!(
        binary = config.binary.as_ref(),
        ?cwd,
        resume = session_id,
        "spawning claude process",
    );

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let reason = ClaudeExitReason::classify(Some(&e), None, "");
            tracing::error!(?reason, error = %e, "failed to spawn claude");
            // Error propagates to the caller (command handler) which displays it.
            // Don't send events — the formatter task isn't spawned on Err return.
            return Err(AppError::claude(&format!("failed to spawn claude: {e}")));
        }
    };

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::claude("no stdout from claude process"))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::claude("no stderr from claude process"))?;

    let reader_cancel = cancel.clone();
    // Move child into the reader task so we can wait() and classify exit.
    // kill_on_drop(true) ensures cleanup if the task is aborted.
    let reader_task = tokio::spawn(async move {
        let mut child = child;

        // Spawn stderr reader to capture error output
        let stderr_task = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            let mut stderr_buf = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(line, "claude stderr");
                if stderr_buf.len() < 2048 {
                    if !stderr_buf.is_empty() {
                        stderr_buf.push('\n');
                    }
                    stderr_buf.push_str(&line);
                }
            }
            stderr_buf
        });

        let reader = BufReader::with_capacity(STDOUT_BUF_CAPACITY, stdout);
        let mut lines = reader.lines();
        let mut got_events = false;

        loop {
            tokio::select! {
                _ = reader_cancel.cancelled() => {
                    tracing::debug!("claude reader cancelled");
                    break;
                }
                line_result = lines.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            tracing::debug!(line, "claude stdout");
                            if let Some(event) = super::parser::parse_stream_line(&line) {
                                got_events = true;
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

        // Wait for child exit and collect stderr (skip if cancelled/killed)
        if !reader_cancel.is_cancelled() {
            let stderr_output = stderr_task.await.unwrap_or_default();
            let exit_status =
                tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;

            let exit_code = match exit_status {
                Ok(Ok(status)) => status.code(),
                Ok(Err(_)) | Err(_) => None,
            };

            let reason = ClaudeExitReason::classify(None, exit_code, &stderr_output);

            match &reason {
                ClaudeExitReason::Success => {}
                other => {
                    tracing::error!(?other, "claude process exited with error");
                    let _ = event_tx.send(ClaudeEvent::ExitError(reason)).await;
                }
            }

            // Legacy fallback: if no events and no ExitError was sent, send Error
            if !got_events && exit_code == Some(0) {
                if !stderr_output.is_empty() {
                    let _ = event_tx
                        .send(ClaudeEvent::Error(
                            format!("claude error: {stderr_output}").into_boxed_str(),
                        ))
                        .await;
                } else {
                    let _ = event_tx
                        .send(ClaudeEvent::Error(
                            "claude process exited with no output".into(),
                        ))
                        .await;
                }
            }
        }

        let _ = event_tx.send(ClaudeEvent::Done).await;
    });

    Ok(ClaudeProcessHandle {
        reader_task,
        cancel,
    })
}
