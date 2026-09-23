//! Who is listening in the bot's voice channel, read from the serenity
//! cache's voice states. Other bots never count as listeners.

use serenity::all::{Cache, ChannelId, GuildId, UserId, VoiceState};

/// Counts the users in `channel` other than the bot itself and anyone
/// `is_bot` reports as a bot.
pub fn listeners_in<'a>(
    voice_states: impl IntoIterator<Item = &'a VoiceState>,
    channel: ChannelId,
    bot_id: UserId,
    is_bot: impl Fn(&VoiceState) -> bool,
) -> usize {
    voice_states
        .into_iter()
        .filter(|state| state.channel_id == Some(channel))
        .filter(|state| state.user_id != bot_id && !is_bot(state))
        .count()
}

/// Whether the bot has `channel` to itself. A guild missing from the cache
/// reads as not alone, so an incomplete cache never causes a disconnect.
pub fn is_alone(cache: &Cache, guild_id: GuildId, channel: ChannelId) -> bool {
    let bot_id = cache.current_user().id;
    let Some(guild) = cache.guild(guild_id) else {
        return false;
    };
    let is_bot = |state: &VoiceState| {
        state
            .member
            .as_ref()
            .or_else(|| guild.members.get(&state.user_id))
            .is_some_and(|member| member.user.bot)
    };
    listeners_in(guild.voice_states.values(), channel, bot_id, is_bot) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: u64 = 10;
    const CHANNEL: u64 = 2;

    fn voice_state(user_id: u64, channel_id: u64, bot: bool) -> VoiceState {
        serde_json::from_value(serde_json::json!({
            "channel_id": channel_id.to_string(),
            "guild_id": "1",
            "user_id": user_id.to_string(),
            "session_id": "session",
            "deaf": false,
            "mute": false,
            "self_deaf": false,
            "self_mute": false,
            "self_video": false,
            "suppress": false,
            "request_to_speak_timestamp": null,
            "member": {
                "user": {
                    "id": user_id.to_string(),
                    "username": "user",
                    "discriminator": "0",
                    "bot": bot,
                },
                "roles": [],
                "joined_at": "2024-01-01T00:00:00Z",
                "deaf": false,
                "mute": false,
                "flags": 0,
            },
        }))
        .unwrap()
    }

    fn count(states: &[VoiceState]) -> usize {
        let is_bot =
            |state: &VoiceState| state.member.as_ref().is_some_and(|member| member.user.bot);
        listeners_in(states, ChannelId::new(CHANNEL), UserId::new(BOT), is_bot)
    }

    #[test]
    fn the_bot_alone_has_no_listeners() {
        assert_eq!(count(&[voice_state(BOT, CHANNEL, true)]), 0);
    }

    #[test]
    fn a_user_in_the_channel_is_a_listener() {
        let states = [
            voice_state(BOT, CHANNEL, true),
            voice_state(20, CHANNEL, false),
        ];
        assert_eq!(count(&states), 1);
    }

    #[test]
    fn other_bots_are_not_listeners() {
        let states = [
            voice_state(BOT, CHANNEL, true),
            voice_state(30, CHANNEL, true),
        ];
        assert_eq!(count(&states), 0);
    }

    #[test]
    fn users_in_other_channels_are_not_listeners() {
        let states = [voice_state(BOT, CHANNEL, true), voice_state(20, 99, false)];
        assert_eq!(count(&states), 0);
    }
}
