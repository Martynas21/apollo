use super::playback::{join_for_play, reply_error, reply_public};
use super::{Context, Error};

#[poise::command(slash_command, guild_only)]
pub async fn radio(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        ctx.send(
            poise::CreateReply::default()
                .content("This command can only be used in a server.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

    if let Err(err) = join_for_play(ctx, guild_id).await {
        return reply_error(ctx, err.to_string()).await;
    }

    let enabled = ctx.data().player.toggle_radio(guild_id).await;
    reply_public(ctx, radio_status_message(enabled)).await
}

fn radio_status_message(enabled: bool) -> &'static str {
    if enabled {
        "📻 Radio mode is on — I'll keep queuing similar tracks once the queue runs out."
    } else {
        "📻 Radio mode is off."
    }
}

#[cfg(test)]
mod tests {
    use super::radio_status_message;

    #[test]
    fn message_when_enabled_mentions_on_and_explains_behavior() {
        let message = radio_status_message(true);
        assert!(message.contains("on"));
        assert!(message.contains("queuing similar tracks"));
    }

    #[test]
    fn message_when_disabled_mentions_off() {
        let message = radio_status_message(false);
        assert!(message.contains("off"));
        assert!(!message.contains("queuing similar tracks"));
    }
}
