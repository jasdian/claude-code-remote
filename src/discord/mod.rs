pub mod commands;
pub mod formatter;
pub mod handler;

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::AppState;
use crate::error::AppError;

/// Check if the user's reply is an affirmative ("yes", "y", "yeah", etc.)
#[inline]
pub(crate) fn is_affirmative(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "yes" | "y" | "yeah" | "yep" | "yup" | "sure" | "ok" | "okay"
    )
}

pub async fn start_bot(state: Arc<AppState>) -> Result<(), AppError> {
    let token = state.config.discord.token.clone();
    let shutdown = state.shutdown.clone();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                commands::claude(),
                commands::end(),
                commands::interrupt(),
                commands::projects(),
                commands::sessions(),
                commands::optin(),
                commands::optout(),
                commands::approve(),
                commands::revoke(),
                commands::pending(),
                commands::compact(),
                commands::context(),
                commands::audit(),
                commands::participants(),
                commands::sessionkick(),
                commands::handoff(),
                commands::sessionban(),
            ],
            event_handler: |ctx, event, _fw_ctx, state| {
                Box::pin(async move {
                    if let poise::serenity_prelude::FullEvent::Message { new_message } = event
                        && let Err(e) = handler::handle_message(ctx, new_message, state).await
                    {
                        tracing::error!(error = %e, "message handler error");
                    }
                    Ok(())
                })
            },
            on_error: |error| {
                Box::pin(async move {
                    match error {
                        poise::FrameworkError::Command {
                            ref error, ref ctx, ..
                        } => {
                            tracing::error!(
                                command = ctx.command().name,
                                user = ctx.author().name,
                                error = %error,
                                "command error",
                            );
                            let _ = ctx.say(format!("Error: {error}")).await;
                        }
                        poise::FrameworkError::CommandStructureMismatch {
                            ref description,
                            ref ctx,
                            ..
                        } => {
                            tracing::error!(
                                command = %ctx.command.name,
                                %description,
                                "command structure mismatch",
                            );
                        }
                        other => {
                            if let Err(e) = poise::builtins::on_error(other).await {
                                tracing::error!(error = %e, "error handler failed");
                            }
                        }
                    }
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
