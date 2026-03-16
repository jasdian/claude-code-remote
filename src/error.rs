use crate::domain::ThreadId;

/// P3: All string-carrying variants use Box<str> to keep the enum compact.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),

    #[error("config: {0}")]
    Config(Box<str>),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("discord: {0}")]
    Discord(Box<poise::serenity_prelude::Error>),

    #[error("claude process: {0}")]
    Claude(Box<str>),

    #[error("session not found: thread {0:?}")]
    SessionNotFound(ThreadId),

    #[error("unauthorized: {0}")]
    Unauthorized(Box<str>),

    #[error("max sessions reached ({0})")]
    MaxSessions(usize),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<poise::serenity_prelude::Error> for AppError {
    fn from(e: poise::serenity_prelude::Error) -> Self {
        Self::Discord(Box::new(e))
    }
}

impl AppError {
    #[inline]
    pub fn config(msg: &str) -> Self {
        Self::Config(msg.into())
    }
    #[inline]
    pub fn claude(msg: &str) -> Self {
        Self::Claude(msg.into())
    }
    #[inline]
    pub fn unauthorized(msg: &str) -> Self {
        Self::Unauthorized(msg.into())
    }
}
