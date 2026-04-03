use serenity::all::{CreateEmbed, CreateButton, ButtonStyle, CreateActionRow};

use super::manager::{ConsentSession, ConsentScope};

const CONSENT_TEXT: &str = "\
This session will be recorded for the **Open Voice Project**.\n\n\
After accepting, you can set restrictions on how your audio is used.\n\n\
You can decline without leaving the voice channel.";

pub fn build_consent_embed(session: &ConsentSession) -> CreateEmbed {
    let mut pending = Vec::new();
    let mut accepted = Vec::new();
    let mut declined = Vec::new();

    for p in session.participants.values() {
        match p.scope {
            None => pending.push(p.display_name.as_str()),
            Some(ConsentScope::Full) => accepted.push(p.display_name.as_str()),
            Some(ConsentScope::DeclineAudio | ConsentScope::Decline) => {
                declined.push(p.display_name.clone())
            }
        }
    }

    let mut embed = CreateEmbed::new()
        .title("Session Recording — Open Dataset")
        .description(CONSENT_TEXT)
        .color(0x5A5A5A);

    if !pending.is_empty() {
        embed = embed.field("Waiting for", pending.join("\n"), true);
    }
    if !accepted.is_empty() {
        embed = embed.field("Accepted", accepted.join("\n"), true);
    }
    if !declined.is_empty() {
        let declined_strs: Vec<&str> = declined.iter().map(|s| s.as_str()).collect();
        embed = embed.field("Declined", declined_strs.join("\n"), true);
    }

    embed
}

pub fn consent_buttons() -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new("consent_accept")
            .label("Accept")
            .style(ButtonStyle::Success),
        CreateButton::new("consent_decline")
            .label("Decline")
            .style(ButtonStyle::Danger),
    ])
}
