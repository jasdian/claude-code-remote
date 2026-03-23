use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::ClaudeConfig;
use crate::domain::{ClaudeEvent, ClaudeExitReason};
use crate::error::AppError;

const STDOUT_BUF_CAPACITY: usize = 8 * 1024;
const EVENT_CHANNEL_CAPACITY: usize = 256;
const STDIN_CHANNEL_CAPACITY: usize = 4;

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

pub fn stdin_channel() -> (mpsc::Sender<String>, mpsc::Receiver<String>) {
    mpsc::channel(STDIN_CHANNEL_CAPACITY)
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
    stdin_rx: mpsc::Receiver<String>,
) -> Result<ClaudeProcessHandle, AppError> {
    // Build initial prompt as stream-json user message for stdin delivery.
    // We use --input-format stream-json instead of -p to avoid Claude blocking
    // on piped stdin (Claude hangs when stdin is piped+open with -p mode).
    let initial_message = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": prompt
        }
    })
    .to_string();

    let mut cmd = Command::new(config.binary.as_ref());
    cmd.arg("--output-format")
        .arg("stream-json")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--include-partial-messages");

    if config.dangerously_skip_permissions {
        cmd.arg("--dangerously-skip-permissions");
    } else {
        // Route permission prompts through stdin/stdout as control_request events.
        // Without this, tools not in --allowedTools are silently auto-denied.
        cmd.arg("--permission-prompt-tool")
            .arg("stdio")
            .arg("--permission-mode")
            .arg("default");
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
        .stdin(Stdio::piped())
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
        stdin = "piped",
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

    let stdin_handle = child.stdin.take();

    let reader_cancel = cancel.clone();
    // Move child into the reader task so we can wait() and classify exit.
    // kill_on_drop(true) ensures cleanup if the task is aborted.
    let reader_task = tokio::spawn(async move {
        let mut child = child;

        // Spawn stdin writer task: sends initial prompt, then relays control_responses
        if let Some(stdin) = stdin_handle {
            let stdin_cancel = reader_cancel.clone();
            tokio::spawn(async move {
                let mut stdin = stdin;
                let mut stdin_rx = stdin_rx;

                // Send initial prompt as stream-json user message
                let mut buf = initial_message.into_bytes();
                buf.push(b'\n');
                if stdin.write_all(&buf).await.is_err() || stdin.flush().await.is_err() {
                    tracing::error!("failed to write initial prompt to stdin");
                    return;
                }
                tracing::debug!("initial prompt sent to stdin");

                // Relay control_response messages
                loop {
                    tokio::select! {
                        _ = stdin_cancel.cancelled() => break,
                        msg = stdin_rx.recv() => {
                            let Some(line) = msg else { break };
                            tracing::debug!(bytes = line.len(), "writing to stdin");
                            let mut buf = line.into_bytes();
                            buf.push(b'\n');
                            if stdin.write_all(&buf).await.is_err() || stdin.flush().await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

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
        let mut got_content = false;
        let mut line_count: u32 = 0;

        'reader: loop {
            tokio::select! {
                _ = reader_cancel.cancelled() => {
                    tracing::debug!("claude reader cancelled");
                    break;
                }
                line_result = lines.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            line_count += 1;
                            if line_count <= 3 {
                                tracing::debug!(line_count, line, "claude stdout (first lines)");
                            } else {
                                tracing::trace!(line, "claude stdout");
                            }
                            let parsed = super::parser::parse_stream_line(&line);

                            // If result event has text and we haven't sent content yet,
                            // emit it as TextDelta (avoids duplicating assistant events).
                            if let Some(result_text) = parsed.result_text
                                && !got_content
                            {
                                tracing::info!("emitting result text as fallback content");
                                got_content = true;
                                if event_tx.send(ClaudeEvent::TextDelta(result_text)).await.is_err() {
                                    break 'reader;
                                }
                            }

                            for event in &parsed.events {
                                if matches!(event,
                                    ClaudeEvent::TextDelta(_)
                                    | ClaudeEvent::ToolUse { .. }
                                    | ClaudeEvent::ToolResult { .. }
                                    | ClaudeEvent::ControlRequest(_)
                                    | ClaudeEvent::Error(_)
                                    | ClaudeEvent::ExitError(_)
                                ) {
                                    got_content = true;
                                }
                            }
                            for event in parsed.events {
                                if event_tx.send(event).await.is_err() {
                                    break 'reader;
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

            tracing::debug!(
                line_count,
                got_content,
                ?exit_code,
                "claude process finished"
            );

            // Fallback: if no content events were produced, send Error
            if !got_content && exit_code == Some(0) {
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

