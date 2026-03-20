use std::sync::Arc;

use tokio::signal;
use tokio_util::sync::CancellationToken;

use claude_crew::AppState;
use claude_crew::claude::session::SessionManager;
use claude_crew::config::AppConfig;

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

    // Connect DB with WAL mode and foreign keys
    let pool_opts = config
        .database
        .url
        .parse::<sqlx::sqlite::SqliteConnectOptions>()?
        .pragma("journal_mode", "WAL")
        .pragma("foreign_keys", "ON");
    let pool = sqlx::SqlitePool::connect_with(pool_opts).await?;
    claude_crew::db::run_migrations(&pool).await?;

    // Reconcile sessions left "active" by a previous crash/shutdown —
    // mark them "idle" so they can be resumed on the next message.
    let reconciled = claude_crew::db::reconcile_stale_sessions(&pool).await?;
    if reconciled > 0 {
        tracing::info!(
            count = reconciled,
            "reconciled stale active sessions to idle"
        );
    }

    // Clean up orphaned worktrees from previous crash/shutdown
    claude_crew::claude::worktree::cleanup_orphaned(&pool, &config.claude).await;

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
                    // Expire idle sessions that haven't been used within timeout.
                    // Active sessions (Claude running) are never auto-expired.
                    let timeout_mins = reaper_state.config.claude.session_timeout_minutes;
                    if let Ok(stale) = claude_crew::db::find_stale_idle_sessions(
                        &reaper_state.db, timeout_mins,
                    ).await {
                        if !stale.is_empty() {
                            tracing::info!(count = stale.len(), "expiring stale idle sessions");
                            for (tid, worktree_path) in stale {
                                let _ = claude_crew::db::update_session_status(
                                    &reaper_state.db, tid, claude_crew::domain::SessionStatus::Expired,
                                ).await;
                                let _ = claude_crew::db::mark_summary_status(
                                    &reaper_state.db, tid, claude_crew::domain::SessionStatus::Expired,
                                ).await;
                                if let Some(ref wt) = worktree_path {
                                    claude_crew::claude::worktree::remove_worktree(
                                        std::path::Path::new(wt), false,
                                    ).await;
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    // Spawn context summarizer (if enabled)
    if state.config.claude.context_sharing.enabled {
        let ctx_state = Arc::clone(&state);
        let ctx_cancel = shutdown.child_token();
        let interval_secs = state.config.claude.context_sharing.interval_seconds;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            loop {
                tokio::select! {
                    _ = ctx_cancel.cancelled() => {
                        tracing::info!("context summarizer shutting down");
                        break;
                    }
                    _ = interval.tick() => {
                        claude_crew::claude::context::update_summaries(
                            &ctx_state.db,
                        ).await;
                    }
                }
            }
        });
        tracing::info!(interval_secs, "context summarizer started");
    }

    // Spawn Discord bot as a separate task (Tokio recommended pattern)
    tracing::info!("starting discord bot");
    let bot_handle = tokio::spawn(claude_crew::discord::start_bot(Arc::clone(&state)));

    // Wait for shutdown signal in main task
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = signal::ctrl_c() => {
            tracing::info!("received SIGINT");
        }
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
        }
    }

    // Signal all tasks to shut down
    tracing::info!("initiating graceful shutdown");
    shutdown.cancel();

    // Kill all active Claude subprocesses
    state.session_manager.kill_all().await;

    // Wait for bot to finish with timeout
    tokio::select! {
        result = bot_handle => {
            if let Err(e) = result {
                tracing::error!(error = %e, "bot task panicked");
            }
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
            tracing::warn!("graceful shutdown timed out, forcing exit");
            std::process::exit(0);
        }
    }

    tracing::info!("shutdown complete");
    Ok(())
}
