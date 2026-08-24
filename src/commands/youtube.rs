//! `/link` and `/unlink`: Google account linking commands.

use oauth2::{CsrfToken, RefreshToken, Scope, StandardRevocableToken};

use super::{Context, Error};
use crate::db;
use crate::youtube::oauth::YOUTUBE_READONLY_SCOPE;

/// Links your Google account so the bot can play YouTube videos on your
/// behalf.
#[poise::command(slash_command)]
pub async fn link(ctx: Context<'_>) -> Result<(), Error> {
    let already_linked = db::get_token(
        &ctx.data().db,
        &ctx.author().id.to_string(),
        &ctx.data().token_key,
    )
    .await?
    .is_some();

    let (auth_url, csrf_token) = ctx
        .data()
        .oauth_client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(YOUTUBE_READONLY_SCOPE.to_string()))
        // Request a refresh token, and force the consent screen so Google
        // reissues one even on a re-link (it otherwise only sends a
        // refresh token on a user's very first consent).
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .url();

    ctx.data()
        .pending_links
        .lock()
        .unwrap()
        .insert(csrf_token.secret().clone(), ctx.author().id);

    let mut content = format!(
        "Click to link your Google account (grants read-only YouTube access so the bot can \
         look up and play videos): {auth_url}"
    );
    if already_linked {
        content.push_str("\n\nThis replaces your existing linked account.");
    }

    ctx.send(
        poise::CreateReply::default()
            .content(content)
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

/// Unlinks your Google account, deleting the bot's stored access to it.
#[poise::command(slash_command)]
pub async fn unlink(ctx: Context<'_>) -> Result<(), Error> {
    let discord_user_id = ctx.author().id.to_string();

    let Some(stored) =
        db::get_token(&ctx.data().db, &discord_user_id, &ctx.data().token_key).await?
    else {
        ctx.send(
            poise::CreateReply::default()
                .content("You don't have a linked Google account.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

    // Best-effort: revocation can fail transiently against Google, but the
    // user's actual intent (bot no longer touches their account, local
    // data gone) is still satisfiable by deleting the local row regardless.
    let revocable: StandardRevocableToken = RefreshToken::new(stored.refresh_token).into();
    if let Err(err) = ctx
        .data()
        .oauth_client
        .revoke_token(revocable)?
        .request_async(&ctx.data().oauth_http)
        .await
    {
        tracing::warn!(error = %err, "failed to revoke Google OAuth2 token during /unlink");
    }

    db::delete_token(&ctx.data().db, &discord_user_id).await?;

    ctx.send(
        poise::CreateReply::default()
            .content("Your Google account has been unlinked.")
            .ephemeral(true),
    )
    .await?;

    Ok(())
}
