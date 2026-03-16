use std::sync::Arc;

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use uuid::Uuid;

// P6: Newtypes prevent mixing up bare u64 IDs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClaudeSessionId(pub Arc<str>);

impl ThreadId {
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl UserId {
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl ClaudeSessionId {
    pub fn new(s: &str) -> Self {
        Self(Arc::from(s))
    }
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<serenity::ChannelId> for ThreadId {
    fn from(id: serenity::ChannelId) -> Self {
        Self(id.get())
    }
}

impl From<serenity::UserId> for UserId {
    fn from(id: serenity::UserId) -> Self {
        Self(id.get())
    }
}

// Session: hot/cold field separation (P5)

#[derive(Debug, Clone)]
pub struct Session {
    pub id: Uuid,
    pub thread_id: ThreadId,
    pub user_id: UserId,
    // Hot path fields first (P5: locality)
    pub status: SessionStatus,
    pub last_active_at: DateTime<Utc>,
    // Cold path fields
    pub claude_session_id: Option<ClaudeSessionId>,
    pub project: Arc<str>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Active,
    Idle,
    Stopped,
    Expired,
}

impl SessionStatus {
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Stopped => "stopped",
            Self::Expired => "expired",
        }
    }

    #[inline]
    pub fn parse(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "idle" => Self::Idle,
            "stopped" => Self::Stopped,
            _ => Self::Expired,
        }
    }
}

// ClaudeEvent: compact enum (P3)

#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    TextDelta(Arc<str>),
    ToolUse {
        tool: Arc<str>,
        input_preview: Arc<str>,
    },
    ToolResult {
        tool: Arc<str>,
        is_error: bool,
    },
    SessionId(ClaudeSessionId),
    Done,
    Error(Box<str>),
}
