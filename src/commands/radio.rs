//! `/radio`: a pure on/off toggle for radio mode.

use super::playback::reply_public;
use super::{Context, Error};

/// Toggles radio mode: keeps queuing similar tracks once the queue runs dry.
///
/// Seeds from whatever's currently or was most recently playing — a
/// playlist finishing or a one-off `/play` alike, since the seed is always
/// "whatever just played," not something this command sets itself. See
/// [`crate::voice::PlayerRegistry::toggle_radio`].
#[poise::command(slash_command, guild_only)]
pub async fn radio(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .expect("guild_only commands always have a guild");

    let enabled = ctx.data().player.toggle_radio(guild_id).await;
    reply_public(ctx, radio_status_message(enabled)).await
}

/// Formats the reply for the new radio-mode state after a toggle.
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
