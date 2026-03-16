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
