pub mod commands;
pub mod formatter;
pub mod handler;

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::error::AppError;
use crate::AppState;

pub async fn start_bot(state: Arc<AppState>) -> Result<(), AppError> {
    let token = state.config.discord.token.clone();
    let shutdown = state.shutdown.clone();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                commands::claude(),
                commands::stop(),
                commands::sessions(),
            ],
            event_handler: |ctx, event, _fw_ctx, state| {
                Box::pin(async move {
                    if let poise::serenity_prelude::FullEvent::Message { new_message } = event {
                        if let Err(e) =
                            handler::handle_message(ctx, new_message, state).await
                        {
                            tracing::error!(error = %e, "message handler error");
                        }
                    }
                    Ok(())
                })
            },
            ..Default::default()
        })
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                tracing::info!("bot ready, commands registered");
                Ok(state)
            })
        })
        .build();

    let intents = serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::DIRECT_MESSAGES
        | serenity::GatewayIntents::MESSAGE_CONTENT;

    let mut client = serenity::ClientBuilder::new(&*token, intents)
        .framework(framework)
        .await?;

    tokio::select! {
        result = client.start() => {
            result?;
        }
        _ = shutdown.cancelled() => {
            tracing::info!("discord bot shutting down");
            client.shard_manager.shutdown_all().await;
        }
    }

    Ok(())
}
