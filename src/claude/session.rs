use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use smallvec::SmallVec;
use tokio::time::Instant;
use crate::config::AppConfig;
use crate::domain::{ClaudeSessionId, ThreadId};
use crate::error::AppError;

use super::process::ClaudeProcessHandle;

struct ActiveSession {
    handle: ClaudeProcessHandle,
    started_at: Instant,
    claude_session_id: Option<ClaudeSessionId>,
    _project_cwd: PathBuf,
}

pub struct SessionManager {
    active: DashMap<ThreadId, ActiveSession>,
    config: Arc<AppConfig>,
}

impl SessionManager {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            active: DashMap::with_capacity(config.claude.max_sessions),
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
                _project_cwd: cwd,
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

    pub fn remove(&self, thread_id: ThreadId) -> Option<ClaudeProcessHandle> {
        self.active.remove(&thread_id).map(|(_, s)| s.handle)
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
                tracing::info!(?tid, "reaped expired session");
            }
        }
        expired
    }
}
