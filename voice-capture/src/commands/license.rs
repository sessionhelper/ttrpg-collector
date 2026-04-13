//! License preference button handler (`No LLM Training` / `No Public Release`).
//!
//! Post-refactor: the actor owns the authoritative license-flag state and
//! performs the local toggle in `SessionCmd::ToggleLicense`'s reply. This
//! handler acks the interaction (within Discord's 3s window), forwards the
//! click to the actor, and schedules the Data API sync in a background
//! task. No `state.sessions.lock()` here.

use std::sync::Arc;

use serenity::all::*;
use tracing::{error, info};

use crate::session::actor::{request, LicenseField, SessionCmd};
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
    state: &Arc<AppState>,
) -> Result<(), serenity::Error> {
    let guild_id = match component.guild_id {
        Some(g) => g.get(),
        None => return Ok(()),
    };
    let user_id = component.user.id;

    let field = match component.data.custom_id.as_str() {
        "license_no_llm" => LicenseField::NoLlmTraining,
        "license_no_public" => LicenseField::NoPublicRelease,
        _ => return Ok(()),
    };

    // Ack the component interaction IMMEDIATELY before any other work.
    // DeferredUpdateMessage / Acknowledge is the lightest possible ack —
    // no message changes, just "yes, got it." The real button refresh
    // happens via edit_response once the actor replies with the new
    // flag state.
    component
        .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
        .await?;

    let handle = match state.sessions.get(&guild_id) {
        Some(entry) if !entry.is_shutting_down() => entry.clone(),
        _ => return Ok(()),
    };

    let (new_llm, new_public, cached_pid) = match request(&handle, |reply| {
        SessionCmd::ToggleLicense {
            user: user_id,
            field,
            reply,
        }
    })
    .await
    {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    info!(
        ?field,
        new_llm,
        new_public,
        cached = cached_pid.is_some(),
        "license_toggle_optimistic"
    );

    if let Err(e) = component
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new()
                .components(vec![build_license_buttons(new_llm, new_public)]),
        )
        .await
    {
        tracing::warn!(error = %e, "license_button_edit_response_failed");
    }

    // Fire the Data API PATCH in a background task.
    let api = state.api.clone();
    let field_str = match field {
        LicenseField::NoLlmTraining => "no_llm_training",
        LicenseField::NoPublicRelease => "no_public_release",
    };
    let field_owned = field_str.to_string();
    // The actor's optimistic new flags are what the PATCH should produce;
    // we call toggle_license_flag_by_id with the PRE-toggle values so the
    // server flips the same way.
    let pre_llm = if matches!(field, LicenseField::NoLlmTraining) {
        !new_llm
    } else {
        new_llm
    };
    let pre_public = if matches!(field, LicenseField::NoPublicRelease) {
        !new_public
    } else {
        new_public
    };

    if let Some(pid) = cached_pid {
        tokio::spawn(async move {
            let start = std::time::Instant::now();
            match api
                .toggle_license_flag_by_id(pid, &field_owned, pre_llm, pre_public)
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
        // Cold cache fallback: slower discord-id lookup. We don't have
        // the session UUID here without another actor round trip — use
        // the handle's stored session_id.
        let session_id = handle.session_id.clone();
        let user_get = user_id.get();
        tokio::spawn(async move {
            let Ok(sid) = uuid::Uuid::parse_str(&session_id) else {
                return;
            };
            let start = std::time::Instant::now();
            match api.toggle_license_flag(sid, user_get, &field_owned).await {
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
