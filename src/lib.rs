use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

pub mod claude;
pub mod config;
pub mod db;
pub mod discord;
pub mod domain;
pub mod error;

pub struct AppState {
    pub config: Arc<config::AppConfig>,
    pub db: sqlx::SqlitePool,
    pub session_manager: claude::session::SessionManager,
    pub shutdown: CancellationToken,
}

pub type Context<'a> = poise::Context<'a, Arc<AppState>, error::AppError>;

/// Derive project name from cwd path (last path component).
#[inline]
pub fn project_name_from_cwd(cwd: &Path) -> &str {
    cwd.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
}
