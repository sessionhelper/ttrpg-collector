//! License preference button handler (`No LLM Training` / `No Public Release`).
//!
//! These buttons appear in an ephemeral followup once recording is active.
//! Each click toggles one license flag.
//!
//! # Optimistic UI
//!
//! A naive implementation does: Acknowledge → PATCH /participants → edit
//! the button row. That's **two sequential Discord API round trips** per
//! click (Acknowledge + edit_response), each at ~100-300ms — user stares
//! at unchanged buttons for 200-600ms and complains that the bot "feels
//! laggy".
//!
//! The optimistic pattern here computes the new flag state locally from
//! the cached `ParticipantConsent` values, responds with
//! `UpdateMessage` (type 7) **immediately** carrying the new button
//! styling, and fires the Data API PATCH in a background task. The user
//! sees the button flip after **one** round trip. The Data API write
//! happens off the click's critical path.
//!
//! Failure mode: if the PATCH fails, the local UI state diverges from
//! the server. Logged as an error; corrected on the next successful
//! click because `toggle_license_flag_by_id` returns the server's
//! authoritative post-patch state.

use serenity::all::*;
use tracing::{error, info};

use crate::state::AppState;

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
    let guild_id = component.guild_id.unwrap().get();
    let user_id = component.user.id;

    let field = match component.data.custom_id.as_str() {
        "license_no_llm" => "no_llm_training",
        "license_no_public" => "no_public_release",
        _ => return Ok(()),
    };

    // Snapshot current cached state + participant UUID under one lock.
    // The UUID may be missing on cold cache (bot restart mid-session) —
    // handled below by falling back to the slow path.
    let (cached_pid, current_flags) = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id) {
            Some(s) => (s.participant_uuid(user_id), s.license_flags(user_id)),
            None => (None, (false, false)),
        }
    };

    // Compute the new flag state optimistically. This is what the user
    // will see on their buttons after the next line — no data-api call
    // has happened yet.
    let new_flags = match field {
        "no_llm_training" => (!current_flags.0, current_flags.1),
        "no_public_release" => (current_flags.0, !current_flags.1),
        _ => current_flags,
    };

    info!(
        field = field,
        from = ?current_flags,
        to = ?new_flags,
        cached = cached_pid.is_some(),
        "license_toggle_optimistic",
    );

    // Write the optimistic state to the local cache immediately so any
    // concurrent reader (e.g. session finalization pulling meta.json)
    // sees the user's intended state even before the server confirms.
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(s) = sessions.get_mut(guild_id) {
            s.set_license_flags(user_id, new_flags.0, new_flags.1);
        }
    }

    // Respond to Discord with UpdateMessage (type 7) — single round
    // trip that BOTH acknowledges the interaction AND updates the
    // button row. This is the user-visible latency path; everything
    // else happens in the background.
    component
        .create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content("Toggle restrictions on your audio:")
                    .components(vec![build_license_buttons(new_flags.0, new_flags.1)]),
            ),
        )
        .await?;

    // Fire the Data API PATCH in a background task so it doesn't block
    // the click's critical path. On success, log at info so we can trace
    // click → server-sync latency. On error, log and note the state
    // divergence — the next click will correct it because
    // toggle_license_flag_by_id returns the authoritative post-patch
    // state.
    if let Some(pid) = cached_pid {
        let api = state.api.clone();
        let field_owned = field.to_string();
        let (cur_llm, cur_public) = current_flags;
        tokio::spawn(async move {
            let start = std::time::Instant::now();
            match api
                .toggle_license_flag_by_id(pid, &field_owned, cur_llm, cur_public)
                .await
            {
                Ok(server_flags) => {
                    info!(
                        field = %field_owned,
                        server_flags = ?server_flags,
                        sync_ms = start.elapsed().as_millis(),
                        "license_toggle_synced",
                    );
                }
                Err(e) => {
                    error!(
                        error = %e,
                        field = %field_owned,
                        "license_toggle_sync_failed — local cache may diverge from server",
                    );
                }
            }
        });
    } else {
        // Cold cache fallback. We still have to sync, but we don't have
        // the participant UUID — use the slow 3-hop path. Spawn so this
        // also doesn't block the click.
        let api = state.api.clone();
        let field_owned = field.to_string();
        let session_id_str = {
            let sessions = state.sessions.lock().await;
            sessions.get(guild_id).map(|s| s.id.clone())
        };
        tokio::spawn(async move {
            let Some(sid_str) = session_id_str else { return };
            let Ok(sid) = uuid::Uuid::parse_str(&sid_str) else {
                return;
            };
            let start = std::time::Instant::now();
            match api.toggle_license_flag(sid, user_id.get(), &field_owned).await {
                Ok(server_flags) => {
                    info!(
                        field = %field_owned,
                        server_flags = ?server_flags,
                        sync_ms = start.elapsed().as_millis(),
                        "license_toggle_synced_cold_cache",
                    );
                }
                Err(e) => {
                    error!(
                        error = %e,
                        field = %field_owned,
                        "license_toggle_cold_cache_sync_failed",
                    );
                }
            }
        });
    }

    Ok(())
}

/// Build the two-button license preference row, styling each button
/// based on whether its flag is set (Danger = restriction active,
/// Secondary = no restriction).
fn build_license_buttons(no_llm: bool, no_public: bool) -> CreateActionRow {
    let llm_style = if no_llm { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let public_style = if no_public { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let llm_label = if no_llm { "No LLM Training ✓" } else { "No LLM Training" };
    let public_label = if no_public { "No Public Release ✓" } else { "No Public Release" };

    CreateActionRow::Buttons(vec![
        CreateButton::new("license_no_llm")
            .label(llm_label)
            .style(llm_style),
        CreateButton::new("license_no_public")
            .label(public_label)
            .style(public_style),
    ])
}
