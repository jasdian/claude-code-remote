use std::path::PathBuf;
use std::sync::Arc;

use crate::config::AppConfig;
use crate::domain::{ClaudeSessionId, ThreadId, UserId, UserMessage};
use crate::error::AppError;
use dashmap::DashMap;
use smallvec::SmallVec;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::process::ClaudeProcessHandle;

struct ActiveSession {
    handle: ClaudeProcessHandle,
    started_at: Instant,
    claude_session_id: Option<ClaudeSessionId>,
    project_cwd: PathBuf,
    worktree_path: Option<PathBuf>,
}

pub struct SessionManager {
    active: DashMap<ThreadId, ActiveSession>,
    pending: DashMap<ThreadId, Vec<UserMessage>>,
    /// Oneshot channels for routing user replies to waiting control_requests.
    reply_waiters: DashMap<ThreadId, oneshot::Sender<String>>,
    /// Original prompts awaiting "start new session?" confirmation, scoped to the requesting user.
    confirm_new: DashMap<ThreadId, (UserId, String)>,
    /// Tracks which user triggered the current Claude run (for tool audit attribution).
    current_user: DashMap<ThreadId, (UserId, Arc<str>)>,
    config: Arc<AppConfig>,
}

impl SessionManager {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            active: DashMap::with_capacity(config.claude.max_sessions),
            pending: DashMap::new(),
            reply_waiters: DashMap::new(),
            confirm_new: DashMap::new(),
            current_user: DashMap::new(),
            config,
        }
    }

    #[inline]
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    #[inline]
    pub fn has_session(&self, thread_id: ThreadId) -> bool {
        self.active.contains_key(&thread_id)
    }

    pub fn register(
        &self,
        thread_id: ThreadId,
        handle: ClaudeProcessHandle,
        cwd: PathBuf,
        worktree_path: Option<PathBuf>,
    ) -> Result<(), AppError> {
        if self.active.len() >= self.config.claude.max_sessions {
            tokio::spawn(async move {
                let _ = handle.kill().await;
            });
            return Err(AppError::MaxSessions(self.config.claude.max_sessions));
        }
        self.active.insert(
            thread_id,
            ActiveSession {
                handle,
                started_at: Instant::now(),
                claude_session_id: None,
                project_cwd: cwd,
                worktree_path,
            },
        );
        Ok(())
    }

    pub fn set_session_id(&self, thread_id: ThreadId, sid: ClaudeSessionId) {
        if let Some(mut entry) = self.active.get_mut(&thread_id) {
            entry.claude_session_id = Some(sid);
        }
    }

    pub fn get_session_id(&self, thread_id: ThreadId) -> Option<ClaudeSessionId> {
        self.active
            .get(&thread_id)
            .and_then(|entry| entry.claude_session_id.clone())
    }

    /// Get the cwd for an active session.
    pub fn get_cwd(&self, thread_id: ThreadId) -> Option<PathBuf> {
        self.active.get(&thread_id).map(|e| e.project_cwd.clone())
    }

    pub fn remove(&self, thread_id: ThreadId) -> Option<(ClaudeProcessHandle, Option<PathBuf>)> {
        self.active
            .remove(&thread_id)
            .map(|(_, s)| (s.handle, s.worktree_path))
    }

    /// Queue a follow-up message for a busy session.
    pub fn queue_message(&self, thread_id: ThreadId, message: UserMessage) {
        self.pending.entry(thread_id).or_default().push(message);
    }

    /// Take all pending messages for a session.
    pub fn take_pending(&self, thread_id: ThreadId) -> Option<Vec<UserMessage>> {
        self.pending
            .remove(&thread_id)
            .map(|(_, msgs)| msgs)
            .filter(|msgs| !msgs.is_empty())
    }

    /// Store a oneshot sender to route the next user reply to a waiting control_request.
    pub fn set_reply_waiter(&self, thread_id: ThreadId, tx: oneshot::Sender<String>) {
        self.reply_waiters.insert(thread_id, tx);
    }

    /// Take the reply waiter for a thread (if any). Returns None if no one is waiting.
    pub fn take_reply_waiter(&self, thread_id: ThreadId) -> Option<oneshot::Sender<String>> {
        self.reply_waiters.remove(&thread_id).map(|(_, tx)| tx)
    }

    /// Check if a thread has a pending reply waiter.
    pub fn has_reply_waiter(&self, thread_id: ThreadId) -> bool {
        self.reply_waiters.contains_key(&thread_id)
    }

    /// Store an original prompt awaiting "new session?" confirmation, scoped to user.
    pub fn set_confirm_new(&self, thread_id: ThreadId, user_id: UserId, prompt: String) {
        self.confirm_new.insert(thread_id, (user_id, prompt));
    }

    /// Take the stored prompt if the confirming user matches the original requester.
    pub fn take_confirm_new(&self, thread_id: ThreadId, user_id: UserId) -> Option<String> {
        // Only let the same user who initiated the confirmation consume it
        if let Some(entry) = self.confirm_new.get(&thread_id)
            && entry.0 == user_id
        {
            return self.confirm_new.remove(&thread_id).map(|(_, (_, p))| p);
        }
        None
    }

    /// Remove a stale confirmation (timeout cleanup).
    pub fn remove_confirm_new(&self, thread_id: ThreadId) {
        self.confirm_new.remove(&thread_id);
    }

    /// Set which user triggered the current Claude run (for tool audit attribution).
    pub fn set_current_user(&self, thread_id: ThreadId, user_id: UserId, username: Arc<str>) {
        self.current_user.insert(thread_id, (user_id, username));
    }

    /// Get the current user for a thread.
    pub fn get_current_user(&self, thread_id: ThreadId) -> Option<UserId> {
        self.current_user.get(&thread_id).map(|e| e.0)
    }

    /// Clear current user tracking for a thread.
    pub fn clear_current_user(&self, thread_id: ThreadId) {
        self.current_user.remove(&thread_id);
    }

    /// Interrupt: signal the active process to stop without removing it.
    /// `stream_to_discord` will handle cleanup and pick up pending messages.
    pub fn interrupt(&self, thread_id: ThreadId) {
        if let Some(entry) = self.active.get(&thread_id) {
            entry.handle.signal_stop();
            tracing::info!(?thread_id, "session interrupted");
        }
    }

    /// Kill all active sessions. Used during graceful shutdown.
    pub async fn kill_all(&self) {
        let keys: SmallVec<[ThreadId; 4]> = self.active.iter().map(|e| *e.key()).collect();
        for tid in keys {
            if let Some((_, session)) = self.active.remove(&tid) {
                let _ = session.handle.kill().await;
                if let Some(ref wt) = session.worktree_path {
                    super::worktree::remove_worktree(wt).await;
                }
                tracing::info!(?tid, "killed session on shutdown");
            }
        }
    }

    /// Kill sessions older than timeout. Returns reaped thread IDs for DB cleanup.
    pub async fn reap_expired(&self) -> SmallVec<[ThreadId; 4]> {
        let timeout =
            std::time::Duration::from_secs(self.config.claude.session_timeout_minutes * 60);

        let expired: SmallVec<[ThreadId; 4]> = self
            .active
            .iter()
            .filter(|entry| entry.started_at.elapsed() > timeout)
            .map(|entry| *entry.key())
            .collect();

        for tid in &expired {
            if let Some((_, session)) = self.active.remove(tid) {
                let _ = session.handle.kill().await;
                if let Some(ref wt) = session.worktree_path {
                    super::worktree::remove_worktree(wt).await;
                }
                tracing::info!(?tid, "reaped expired session");
            }
        }
        expired
    }
}
