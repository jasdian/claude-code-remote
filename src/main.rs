use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use claude_remote_chat::AppState;
use claude_remote_chat::claude::session::SessionManager;
use claude_remote_chat::config::AppConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = AppConfig::from_file("config.toml").await?;

    // Init tracing
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(config.logging.level.as_ref()));

    match config.logging.format.as_ref() {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        _ => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }

    // Connect DB
    let pool = sqlx::SqlitePool::connect(config.database.url.as_ref()).await?;
    claude_remote_chat::db::run_migrations(&pool).await?;

    // Build shared state
    let config = Arc::new(config);
    let shutdown = CancellationToken::new();

    let state = Arc::new(AppState {
        session_manager: SessionManager::new(Arc::clone(&config)),
        config: Arc::clone(&config),
        db: pool,
        shutdown: shutdown.clone(),
    });

    // Spawn background reaper
    let reaper_state = Arc::clone(&state);
    let reaper_cancel = shutdown.child_token();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = reaper_cancel.cancelled() => {
                    tracing::info!("reaper shutting down");
                    break;
                }
                _ = interval.tick() => {
                    let reaped = reaper_state.session_manager.reap_expired().await;
                    if !reaped.is_empty() {
                        tracing::info!(count = reaped.len(), "reaped expired sessions");
                        for tid in reaped {
                            let _ = claude_remote_chat::db::update_session_status(
                                &reaper_state.db, tid, "expired",
                            ).await;
                        }
                    }
                }
            }
        }
    });

    // Signal handler
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => tracing::info!("received SIGINT"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        }

        tracing::info!("initiating graceful shutdown");
        signal_shutdown.cancel();
    });

    // Start Discord bot
    tracing::info!("starting discord bot");
    claude_remote_chat::discord::start_bot(state).await?;

    tracing::info!("shutdown complete");
    Ok(())
}
