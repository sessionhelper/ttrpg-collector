"""Notes cog — /notes command for GM to submit session notes."""

from __future__ import annotations

import json
from pathlib import Path

import discord
import structlog
from discord.ext import commands

from collector.config import settings
from collector.storage.local_buffer import get_buffer_dir

log = structlog.get_logger()


class NotesModal(discord.ui.Modal):
    def __init__(self, session_id: str, session_dir: Path, guild_id: int) -> None:
        super().__init__(title="Session Notes")
        self.session_id = session_id
        self.session_dir = session_dir
        self.guild_id = guild_id

        self.notes_input = discord.ui.InputText(
            label="Session Notes",
            placeholder=(
                "What happened this session? Key events, NPC interactions, "
                "plot developments, combat encounters..."
            ),
            style=discord.InputTextStyle.long,
            max_length=4000,
            required=True,
        )
        self.add_item(self.notes_input)

    async def callback(self, interaction: discord.Interaction) -> None:
        notes_text = self.notes_input.value

        # Write notes to session dir
        notes_path = self.session_dir / "notes.md"
        notes_path.write_text(notes_text)
        log.info("notes_saved", session_id=self.session_id, length=len(notes_text))

        # Upload notes to S3
        try:
            key = f"sessions/{self.guild_id}/{self.session_id}/notes.md"
            if settings.s3_access_key and settings.s3_secret_key:
                import aioboto3

                s = aioboto3.Session()
                async with s.client(
                    "s3",
                    endpoint_url=settings.s3_endpoint,
                    aws_access_key_id=settings.s3_access_key,
                    aws_secret_access_key=settings.s3_secret_key,
                ) as s3:
                    await s3.put_object(
                        Bucket=settings.s3_bucket,
                        Key=key,
                        Body=notes_text.encode(),
                        ContentType="text/markdown",
                    )
                log.info("notes_uploaded", key=key)
        except Exception:
            log.error("notes_upload_failed", session_id=self.session_id, exc_info=True)

        await interaction.response.send_message(
            "Notes saved. Thank you!\n\n"
            "**Reminder:** Please ensure your notes do not contain real names, "
            "addresses, or personal information about your players.",
            ephemeral=True,
        )


class NotesCog(commands.Cog):
    def __init__(self, bot: commands.Bot) -> None:
        self.bot = bot

    async def _session_autocomplete(self, ctx: discord.AutocompleteContext) -> list[str]:
        if ctx.interaction.guild is None:
            return []

        guild_dir = get_buffer_dir() / str(ctx.interaction.guild.id)
        if not guild_dir.exists():
            return []

        sessions = []
        for d in sorted(guild_dir.iterdir(), reverse=True):
            if d.is_dir() and (d / "meta.json").exists():
                meta = json.loads((d / "meta.json").read_text())
                label = d.name[:8]
                if meta.get("game_system"):
                    label = f"{meta['game_system']} — {label}"
                if meta.get("started_at"):
                    date = meta["started_at"][:10]
                    label = f"{label} ({date})"
                sessions.append(discord.OptionChoice(name=label, value=d.name))
        return sessions[:25]

    @discord.slash_command(name="notes", description="Submit session notes for the dataset")
    @discord.option(
        "session",
        description="Session to add notes to",
        required=True,
        autocomplete=_session_autocomplete,
    )
    async def notes(self, ctx: discord.ApplicationContext, session: str) -> None:
        if ctx.guild is None:
            await ctx.respond("This command can only be used in a server.", ephemeral=True)
            return

        guild_id = ctx.guild.id
        session_dir = get_buffer_dir() / str(guild_id) / session

        if not session_dir.exists():
            await ctx.respond("Session not found.", ephemeral=True)
            return

        # Check that meta.json exists (session was completed)
        meta_path = session_dir / "meta.json"
        if not meta_path.exists():
            await ctx.respond("This session hasn't been completed yet.", ephemeral=True)
            return

        modal = NotesModal(session_id=session, session_dir=session_dir, guild_id=guild_id)
        await ctx.send_modal(modal)


def setup(bot: commands.Bot) -> None:
    bot.add_cog(NotesCog(bot))
