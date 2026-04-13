//! `/record` slash command handler.
//!
//! Thin glue that validates the channel, builds a `Session`, spawns the
//! per-guild actor, and posts the consent embed. All subsequent mutation
//! of the session happens inside the actor — this handler never touches
//! session state after `spawn_session` returns.

use std::sync::Arc;

use serenity::all::*;
use tracing::{error, info};

use crate::session::actor::{request, spawn_session, SessionCmd, SessionHandle};
use crate::session::{consent_buttons, Session};
use crate::state::AppState;

/// Handle the /record slash command.
///
/// 1. Defer the interaction IMMEDIATELY to claim Discord's 3-second ack
///    window.
/// 2. Validate the user's voice channel (cache lookup, no await).
/// 3. Enumerate voice channel members (cache).
/// 4. Build a Session and spawn its actor. DashMap's vacant-entry semantics
///    reject concurrent /record for the same guild atomically.
/// 5. Persist session + participants to the Data API.
/// 6. Cache participant UUIDs on the actor via `SetParticipantUuid`.
/// 7. Apply bypass consent (if any) via `BypassConsent`.
/// 8. If bypass quorum met → send `StartRecording` and await outcome.
///    Otherwise post the consent embed and let the consent-button path
///    drive the rest.
#[tracing::instrument(skip(ctx, command, state), fields(guild_id = %command.guild_id.unwrap()))]
pub async fn handle_record(
    ctx: &Context,
    command: &CommandInteraction,
    state: &Arc<AppState>,
) -> Result<(), serenity::Error> {
    let guild_id = command.guild_id.unwrap();

    // Defer IMMEDIATELY. Discord gives us exactly 3 seconds from click to
    // ack. Deferring first eliminates the whole class of "Unknown
    // interaction" errors when the cache lookup or Data API call takes
    // more than a couple hundred ms.
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(
                CreateInteractionResponseMessage::new().ephemeral(true),
            ),
        )
        .await?;

    // Find the user's voice channel (cache lookup, instant).
    let guild = ctx.cache.guild(guild_id).unwrap().clone();
    let channel_id = guild
        .voice_states
        .get(&command.user.id)
        .and_then(|vs| vs.channel_id);

    let channel_id = match channel_id {
        Some(id) => id,
        None => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .content("You need to be in a voice channel."),
                )
                .await?;
            return Ok(());
        }
    };

    // Enumerate members. Bots are filtered unless on the bypass list.
    let bypass_ids = state.config.bypass_consent_user_ids();
    let members: Vec<(UserId, String)> = guild
        .voice_states
        .iter()
        .filter(|(_, vs)| vs.channel_id == Some(channel_id))
        .filter_map(|(uid, _)| {
            let member = guild.members.get(uid)?;
            if member.user.bot && !bypass_ids.contains(&uid.get()) {
                return None;
            }
            Some((*uid, member.display_name().to_string()))
        })
        .collect();

    // Build the Session locally (no network calls).
    let mut session = Session::new(
        guild_id.get(),
        channel_id.get(),
        command.channel_id.get(),
        command.user.id,
        state.config.min_participants,
        state.config.require_all_consent,
    );
    for (uid, name) in &members {
        session.add_participant(*uid, name.clone(), false);
    }
    let session_id = session.id.clone();

    // Spawn the actor and insert its handle into AppState.sessions. If
    // another actor is already running for this guild, the DashMap's
    // vacant-entry CAS rejects the insert.
    let handle = match spawn_session(state.clone(), ctx.clone(), session) {
        Ok(h) => h,
        Err(_returned_session) => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .content("A recording session is already active in this server."),
                )
                .await?;
            return Ok(());
        }
    };

    // From here on any error means we need to ask the actor to self-destruct
    // before returning, otherwise the DashMap entry leaks.
    let rollback = |_state: Arc<AppState>, handle: SessionHandle| async move {
        // Tell the actor to stop. The actor's initiator check would reject
        // an arbitrary user, so send AutoStop which bypasses that check.
        let _ = handle.send(SessionCmd::AutoStop).await;
    };

    // Persist to Data API.
    let session_uuid = match uuid::Uuid::parse_str(&session_id) {
        Ok(u) => u,
        Err(_) => {
            rollback(state.clone(), handle.clone()).await;
            return Ok(());
        }
    };
    let s3_prefix = format!("sessions/{}/{}", guild_id.get(), session_id);
    if let Err(e) = state
        .api
        .create_session(
            session_uuid,
            guild_id.get() as i64,
            chrono::Utc::now(),
            None,
            None,
            Some(s3_prefix),
        )
        .await
    {
        error!("API call failed (create_session): {e}");
        rollback(state.clone(), handle.clone()).await;
        let _ = command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content("Couldn't reach the storage backend. Try `/record` again in a moment."),
            )
            .await;
        return Ok(());
    }

    // Blocklist check and batch participant insert.
    let mut accepted: Vec<(UserId, u64, String)> = Vec::with_capacity(members.len());
    for (uid, name) in &members {
        match state.api.check_blocklist(uid.get()).await {
            Ok(true) => {
                tracing::info!(
                    user_id = %uid,
                    "participant_blocked — user opted out globally, skipping"
                );
                continue;
            }
            Err(e) => error!("API call failed (check_blocklist): {e}"),
            _ => {}
        }
        accepted.push((*uid, uid.get(), name.clone()));
    }

    let batch_input: Vec<(u64, bool, Option<String>)> = accepted
        .iter()
        .map(|(_, raw, name)| (*raw, false, Some(name.clone())))
        .collect();
    match state.api.add_participants_batch(session_uuid, &batch_input).await {
        Ok(rows) => {
            for ((user_id, _, _), row) in accepted.iter().zip(rows.iter()) {
                let _ = handle
                    .send(SessionCmd::SetParticipantUuid {
                        user_id: *user_id,
                        participant_uuid: row.id,
                    })
                    .await;
            }
            tracing::info!(
                participants = rows.len(),
                "participants_batched_and_cached"
            );
        }
        Err(e) => error!("API call failed (add_participants_batch): {e}"),
    }

    // Apply bypass consent if configured. `BypassConsent` flips the local
    // state inside the actor; we separately fire the remote PATCH per
    // user via the discord-id fallback (we don't have the participant
    // UUID available here without another round trip through the actor).
    let bypass = state.config.bypass_consent_user_ids();
    let mut bypass_quorum_met = false;
    if !bypass.is_empty() {
        let bypass_users: Vec<UserId> = accepted
            .iter()
            .filter(|(_, raw, _)| bypass.contains(raw))
            .map(|(uid, _, _)| *uid)
            .collect();
        if !bypass_users.is_empty() {
            let _ = handle
                .send(SessionCmd::BypassConsent {
                    users: bypass_users.clone(),
                })
                .await;
            for uid in &bypass_users {
                let api = state.api.clone();
                let user_get = uid.get();
                tokio::spawn(async move {
                    if let Err(e) = api.record_consent(session_uuid, user_get, "full").await {
                        error!(
                            user_id = user_get,
                            error = %e,
                            "API call failed (record_consent bypass)",
                        );
                    }
                });
            }
            tracing::info!(
                bypassed = bypass_users.len(),
                "bypass_consent_applied"
            );
            // Check if bypass completed the full participant set → bypass quorum met.
            // All accepted users that ARE in bypass counts.
            bypass_quorum_met = accepted
                .iter()
                .all(|(_, raw, _)| bypass.contains(raw))
                && !accepted.is_empty();
        }
    }

    // Bypass-quorum-met fast path: jump straight to StartRecording.
    if bypass_quorum_met {
        info!(
            session_id = %session_id,
            "bypass_quorum_met — skipping consent embed, starting recording directly",
        );
        let _ = command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content("Recording started — all participants auto-consented via bypass."),
            )
            .await;
        let outcome = request(&handle, |reply| SessionCmd::StartRecording { reply }).await;
        tracing::info!(outcome = ?outcome, "bypass_quorum_recording_pipeline_complete");
        return Ok(());
    }

    // Build the consent embed via a snapshot through the actor. The actor
    // returns its own embed from RecordConsent replies on subsequent clicks;
    // for the initial render we build one from the session we just created.
    // Simplest approach: have the actor emit one by wrapping consent_embed
    // as part of GetSnapshot — but to avoid bloating the snapshot and to
    // keep the actor API minimal, we rebuild the embed from `members` here.
    let embed = build_initial_consent_embed(&members);
    let buttons = consent_buttons();

    let mentions: String = members
        .iter()
        .map(|(uid, _)| format!("<@{}>", uid))
        .collect::<Vec<_>>()
        .join(" ");

    match command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new()
                .content(mentions)
                .embed(embed)
                .components(vec![buttons]),
        )
        .await
    {
        Ok(msg) => {
            metrics::counter!("chronicle_sessions_total", "outcome" => "started").increment(1);
            let _ = handle
                .send(SessionCmd::SetConsentMessage {
                    channel_id: msg.channel_id,
                    message_id: msg.id,
                })
                .await;
        }
        Err(e) => {
            error!(error = %e, "edit_response_failed — rolling back reservation");
            rollback(state.clone(), handle.clone()).await;
            if let Err(api_e) = state.api.abandon_session(session_uuid).await {
                error!("API call failed (abandon_session): {api_e}");
            }
            return Err(e);
        }
    }

    Ok(())
}

/// Initial consent embed rendered from `members` alone, before the actor
/// has observed any clicks. The button-click handler gets the updated
/// embed back from the actor via `RecordConsent`'s `ConsentOutcome`.
fn build_initial_consent_embed(members: &[(UserId, String)]) -> CreateEmbed {
    let pending: Vec<&str> = members.iter().map(|(_, n)| n.as_str()).collect();

    let mut embed = CreateEmbed::new()
        .title("Session Recording — Open Dataset")
        .description(
            "This session will be recorded for the **Open Voice Project**.\n\n\
             After accepting, you can set restrictions on how your audio is used.\n\n\
             You can decline without leaving the voice channel.",
        )
        .color(0x5A5A5A);
    if !pending.is_empty() {
        embed = embed.field("Waiting for", pending.join("\n"), true);
    }
    embed
}
