//! Slash command implementations (registered with `poise`).
//!
//! Placeholder — commands such as `/link`, `/play`, and `/queue` will be
//! added in a later task.

/// Shared state made available to every command invocation. Later phases
/// will add a YouTube client, etc.
// `db` isn't read by any command yet — it's wired up here so the OAuth
// linking commands (`/link`, `/unlink`) landing next can use it directly.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Data {
    /// Pool of connections to the token-persistence SQLite database.
    pub db: sqlx::SqlitePool,
}

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
