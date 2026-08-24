//! Slash command implementations (registered with `poise`).
//!
//! Placeholder — commands such as `/link`, `/play`, and `/queue` will be
//! added in a later task.

/// Shared state made available to every command invocation. Empty for now;
/// later phases will add a database pool, YouTube client, etc.
#[derive(Debug, Default)]
pub struct Data;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Context<'a> = poise::Context<'a, Data, Error>;

/// Trivial connectivity check.
#[poise::command(slash_command)]
async fn ping(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("Pong!").await?;
    Ok(())
}

/// All commands registered with the framework.
pub fn commands() -> Vec<poise::Command<Data, Error>> {
    vec![ping()]
}
