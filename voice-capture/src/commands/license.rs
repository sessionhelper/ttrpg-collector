//! License preference button handler (`No LLM Training` / `No Public Release`).
//!
//! These buttons appear in an ephemeral followup once recording is active.
//! Each click toggles one license flag in the Data API and re-renders the
//! button row so the current state is visible through button style.

use serenity::all::*;
use tracing::error;

use crate::state::AppState;

/// Handle a click on one of the license preference buttons.
///
/// Acknowledges the component interaction immediately (type-6 deferred
/// update message, no visible "thinking" state), then does the Data API
/// round-trips and edits the original button row via `edit_response`. If
/// we tried to `create_response(UpdateMessage)` at the end instead, two
/// sequential Data API calls could overrun Discord's 3-second interaction
/// window and fail with "Unknown interaction".
#[tracing::instrument(
    skip_all,
    fields(
        guild_id = component.guild_id.map(|g| g.get()),
        user_id = %component.user.id,
    )
)]
pub async fn handle_license_button(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    component
        .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
        .await?;

    let guild_id = component.guild_id.unwrap().get();
    let user_id = component.user.id;

    let field = match component.data.custom_id.as_str() {
        "license_no_llm" => "no_llm_training",
        "license_no_public" => "no_public_release",
        _ => return Ok(()),
    };

    let session_id_str = {
        let sessions = state.sessions.lock().await;
        sessions.get(guild_id).map(|s| s.id.clone())
    };

    // Toggle then read-back: two Data API round-trips. Both phase-aware
    // inside the client via find_participant.
    let (no_llm, no_public) = if let Some(sid_str) = &session_id_str {
        if let Ok(sid) = uuid::Uuid::parse_str(sid_str) {
            if let Err(e) = state.api.toggle_license_flag(sid, user_id.get(), field).await {
                error!("API call failed (toggle_license_flag): {e}");
            }
            state
                .api
                .get_license_flags(sid, user_id.get())
                .await
                .unwrap_or((false, false))
        } else {
            (false, false)
        }
    } else {
        (false, false)
    };

    // Re-render button row — state expressed through button style alone.
    let llm_style = if no_llm { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let public_style = if no_public { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let llm_label = if no_llm { "No LLM Training ✓" } else { "No LLM Training" };
    let public_label = if no_public { "No Public Release ✓" } else { "No Public Release" };

    component
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new()
                .content("Toggle restrictions on your audio:")
                .components(vec![CreateActionRow::Buttons(vec![
                    CreateButton::new("license_no_llm")
                        .label(llm_label)
                        .style(llm_style),
                    CreateButton::new("license_no_public")
                        .label(public_label)
                        .style(public_style),
                ])]),
        )
        .await?;

    Ok(())
}
