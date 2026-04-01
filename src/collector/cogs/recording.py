"""Recording cog — /record, /stop, /status slash commands."""

from __future__ import annotations

import uuid

import discord
import structlog
from discord.ext import commands

from collector.config import settings
from collector.consent.manager import ConsentManager, ConsentSession
from collector.consent.types import SessionState
from collector.consent.views import ConsentView, build_consent_embed
from collector.metadata.quality_filters import compute_quality_flags
from collector.metadata.session_meta import SessionMetadata
from collector.storage.local_buffer import (
    create_session_dir,
    mark_failed,
    mark_recording,
    mark_uploaded,
)
from collector.storage.s3_upload import S3Uploader
from collector.storage.session_bundle import SessionBundle
from collector.utils.discord_helpers import format_duration, voice_channel_members
from collector.voice.recorder import VoiceRecorder

log = structlog.get_logger()

COMMON_SYSTEMS = [
    "D&D 5e",
    "D&D 5e (2024)",
    "Pathfinder 2e",
    "Call of Cthulhu",
    "Blades in the Dark",
    "Savage Worlds",
    "FATE",
    "Mothership",
    "Mork Borg",
    "Dungeon World",
    "Dark Heresy 2e",
    "Stars Without Number",
    "Cyberpunk RED",
    "Vampire: The Masquerade 5e",
    "Monster of the Week",
]


class RecordingCog(commands.Cog):
    def __init__(self, bot: commands.Bot) -> None:
        self.bot = bot
        self.consent_manager = ConsentManager()
        self.recorders: dict[int, VoiceRecorder] = {}  # guild_id -> recorder
        self.metadata: dict[int, SessionMetadata] = {}  # guild_id -> metadata
        self.bundles: dict[int, SessionBundle] = {}  # guild_id -> bundle
        self.uploader = S3Uploader()

    @discord.slash_command(
        name="record", description="Start recording this voice channel for the TTRPG dataset"
    )
    @discord.option(
        "system",
        description="Game system",
        required=False,
        autocomplete=discord.utils.basic_autocomplete(COMMON_SYSTEMS),
    )
    @discord.option("campaign", description="Campaign name", required=False)
    @discord.option("session_number", description="Session number", required=False, type=int)
    async def record(
        self,
        ctx: discord.ApplicationContext,
        system: str | None = None,
        campaign: str | None = None,
        session_number: int | None = None,
    ) -> None:
        if ctx.guild is None:
            await ctx.respond("This command can only be used in a server.", ephemeral=True)
            return

        if self.consent_manager.has_active_session(ctx.guild.id):
            await ctx.respond(
                "A recording session is already active in this server.", ephemeral=True
            )
            return

        # Find voice channel
        if ctx.author.voice is None or ctx.author.voice.channel is None:
            await ctx.respond("You need to be in a voice channel.", ephemeral=True)
            return

        voice_channel = ctx.author.voice.channel
        members = voice_channel_members(voice_channel)

        if len(members) < settings.min_participants:
            await ctx.respond(
                f"Need at least {settings.min_participants} people in the voice channel.",
                ephemeral=True,
            )
            return

        # Create session
        session_id = str(uuid.uuid4())
        session = self.consent_manager.create_session(
            guild_id=ctx.guild.id,
            channel_id=voice_channel.id,
            text_channel_id=ctx.channel.id,
            initiator_id=ctx.author.id,
            members=members,
        )

        meta = SessionMetadata(
            session_id=session_id,
            guild_id=ctx.guild.id,
            channel_id=voice_channel.id,
            initiator_id=ctx.author.id,
            game_system=system,
            campaign_name=campaign,
            session_number=session_number,
        )
        self.metadata[ctx.guild.id] = meta

        # Post consent embed
        embed = build_consent_embed(session)
        member_mentions = " ".join(f"<@{m.id}>" for m in members)
        view = ConsentView(session, self._on_consent_response)

        await ctx.respond(
            f"Recording requested by <@{ctx.author.id}>. {member_mentions}\nPlease respond below:",
            embed=embed,
            view=view,
        )

        log.info(
            "record_initiated",
            guild=ctx.guild.id,
            channel=voice_channel.name,
            session_id=session_id,
            members=[m.id for m in members],
        )

        # Wait for quorum in background
        self.bot.loop.create_task(self._wait_and_start(ctx, session, voice_channel, session_id))

    async def _wait_and_start(
        self,
        ctx: discord.ApplicationContext,
        session: ConsentSession,
        voice_channel: discord.VoiceChannel,
        session_id: str,
    ) -> None:
        import asyncio

        guild_id = ctx.guild.id
        text_channel = ctx.guild.get_channel(session.text_channel_id)

        try:
            quorum = await session.wait_for_quorum()

            if not quorum:
                self.consent_manager.remove_session(guild_id)
                self.metadata.pop(guild_id, None)
                if text_channel:
                    await text_channel.send(
                        "Recording cancelled — consent requirements not met."
                    )
                return

            session.state = SessionState.RECORDING

            # Set up local buffer
            session_dir = create_session_dir(guild_id, session_id)
            mark_recording(guild_id, session_id, str(session_dir))

            # Create recorder and connect
            recorder = VoiceRecorder(session_dir, session.consented_user_ids)
            await recorder.connect(voice_channel)

            # Retry start_recording — voice connection may need time to stabilize
            for attempt in range(10):
                try:
                    recorder.start_recording(voice_channel_members(voice_channel))
                    break
                except Exception:
                    if attempt == 9:
                        raise
                    log.warning("start_recording_retry", attempt=attempt + 1)
                    await asyncio.sleep(1)

            self.recorders[guild_id] = recorder
        except Exception:
            log.error("recording_start_failed", session_id=session_id, exc_info=True)
            self.consent_manager.remove_session(guild_id)
            self.metadata.pop(guild_id, None)
            if text_channel:
                await text_channel.send("Failed to start recording. Please try again.")
            return

        meta = self.metadata[guild_id]
        bundle = SessionBundle(
            session_id=session_id,
            session_dir=session_dir,
            guild_id=guild_id,
            channel_id=voice_channel.id,
            started_at=meta.started_at,
            game_system=meta.game_system,
            campaign_name=meta.campaign_name,
            session_number=meta.session_number,
        )
        self.bundles[guild_id] = bundle

        await ctx.send(
            f"Recording started. {len(session.consented_user_ids)} participants consented.\n"
            f"Use `/stop` to end the recording."
        )

    async def _on_consent_response(self, session: ConsentSession) -> None:
        """Called when a user responds to consent. Used for mid-session logging."""
        pass

    @discord.slash_command(name="stop", description="Stop the current recording session")
    async def stop(self, ctx: discord.ApplicationContext) -> None:
        if ctx.guild is None:
            await ctx.respond("This command can only be used in a server.", ephemeral=True)
            return

        guild_id = ctx.guild.id
        recorder = self.recorders.get(guild_id)
        session = self.consent_manager.get_session(guild_id)
        meta = self.metadata.get(guild_id)
        bundle = self.bundles.get(guild_id)

        if recorder is None or session is None or session.state != SessionState.RECORDING:
            await ctx.respond("No active recording in this server.", ephemeral=True)
            return

        # Only initiator or admin can stop
        if ctx.author.id != session.initiator_id and not ctx.author.guild_permissions.administrator:
            await ctx.respond(
                "Only the person who started the recording or an admin can stop it.", ephemeral=True
            )
            return

        await ctx.respond("Stopping recording...")

        session.state = SessionState.FINALIZING
        meta.end()
        bundle.ended_at = meta.ended_at

        # Stop recording and convert to WAV
        wav_paths = await recorder.stop()

        # Rename to pseudonymized filenames
        wav_paths = bundle.rename_wav_files(wav_paths)

        # Compute quality flags
        quality_flags = compute_quality_flags(
            duration_seconds=meta.duration_seconds,
            consented_count=len(session.consented_user_ids),
            wav_paths=wav_paths,
        )

        # Write bundle files
        bundle.write_meta(session, recorder.stream_manager, wav_paths, quality_flags)
        bundle.write_consent(session)
        bundle.write_pii(session)

        await recorder.disconnect()

        duration_str = format_duration(meta.duration_seconds)
        await ctx.send(
            f"Recording complete. Duration: {duration_str}, "
            f"Tracks: {len(wav_paths)}\n"
            f"Uploading to storage..."
        )

        # Upload in background
        session.state = SessionState.UPLOADING
        try:
            uploaded = await self.uploader.upload_session(
                bundle.session_dir, guild_id, bundle.session_id
            )
            mark_uploaded(bundle.session_id)
            session.state = SessionState.COMPLETE

            if uploaded:
                await ctx.send(f"Upload complete. {len(uploaded)} files uploaded.")
            else:
                await ctx.send("Upload skipped (S3 not configured). Files saved locally.")

        except Exception as e:
            mark_failed(bundle.session_id, str(e))
            log.error("upload_failed", session_id=bundle.session_id, exc_info=True)
            await ctx.send(f"Upload failed: {e}\nFiles saved locally for retry.")

        # Send PII reminder DM to initiator
        try:
            initiator = ctx.guild.get_member(session.initiator_id)
            if initiator:
                await initiator.send(
                    f"Your recording session ({duration_str}) has been saved.\n\n"
                    f"You can add session notes with `/notes` in the server.\n\n"
                    f"**Reminder:** Do not include real names, addresses, or personal "
                    f"information about your players in session notes."
                )
        except discord.Forbidden:
            pass

        # Cleanup
        self.recorders.pop(guild_id, None)
        self.metadata.pop(guild_id, None)
        self.bundles.pop(guild_id, None)
        self.consent_manager.remove_session(guild_id)

    @discord.slash_command(name="status", description="Show current recording status")
    async def status(self, ctx: discord.ApplicationContext) -> None:
        if ctx.guild is None:
            await ctx.respond("This command can only be used in a server.", ephemeral=True)
            return

        guild_id = ctx.guild.id
        session = self.consent_manager.get_session(guild_id)
        meta = self.metadata.get(guild_id)
        recorder = self.recorders.get(guild_id)

        if session is None:
            await ctx.respond("No active recording session.", ephemeral=True)
            return

        embed = discord.Embed(title="Recording Status", color=0x5A5A5A)
        embed.add_field(name="State", value=session.state.value, inline=True)

        if meta:
            embed.add_field(
                name="Duration",
                value=format_duration(meta.duration_seconds),
                inline=True,
            )
            if meta.game_system:
                embed.add_field(name="System", value=meta.game_system, inline=True)

        embed.add_field(name="Consent", value=session.consent_summary, inline=False)

        if recorder and recorder.sink:
            total_mb = recorder.sink.total_bytes / (1024 * 1024)
            embed.add_field(
                name="Audio buffer",
                value=f"{total_mb:.1f} MB ({len(recorder.sink.active_streams)} tracks)",
                inline=True,
            )

        await ctx.respond(embed=embed, ephemeral=True)

    @commands.Cog.listener()
    async def on_voice_state_update(
        self,
        member: discord.Member,
        before: discord.VoiceState,
        after: discord.VoiceState,
    ) -> None:
        if member.bot:
            return

        guild_id = member.guild.id
        session = self.consent_manager.get_session(guild_id)
        if session is None:
            return

        recorder = self.recorders.get(guild_id)

        # User joined the recorded channel
        if (
            after.channel
            and after.channel.id == session.channel_id
            and (before.channel is None or before.channel.id != session.channel_id)
        ):
            if session.state == SessionState.RECORDING:
                # Mid-session join — need consent
                session.add_participant(member, mid_session=True)
                try:
                    await member.send(
                        embed=discord.Embed(
                            title="Session Recording in Progress",
                            description=(
                                "You joined a voice channel that is being recorded "
                                "for the TTRPG Open Dataset.\n\n"
                                "Your audio will **not** be captured until you consent.\n"
                                "Please respond in the server text channel."
                            ),
                            color=0x5A5A5A,
                        )
                    )
                except discord.Forbidden:
                    pass

                text_channel = member.guild.get_channel(session.text_channel_id)
                if text_channel:
                    view = ConsentView(session, self._on_consent_response)
                    embed = build_consent_embed(session)
                    await text_channel.send(
                        f"<@{member.id}> joined the voice channel. Please consent to recording:",
                        embed=embed,
                        view=view,
                    )

        # User left the recorded channel
        if (
            before.channel
            and before.channel.id == session.channel_id
            and (after.channel is None or after.channel.id != session.channel_id)
        ):
            if session.state == SessionState.AWAITING_CONSENT:
                session.remove_participant(member.id)
            elif session.state == SessionState.RECORDING and recorder:
                recorder.user_left(member.id)

        # User reconnected to the recorded channel
        if (
            before.channel
            and before.channel.id == session.channel_id
            and after.channel
            and after.channel.id == session.channel_id
            and before.channel != after.channel  # actual reconnect, not mute/unmute
        ):
            if recorder and member.id in session.consented_user_ids:
                recorder.user_rejoined(member.id, member.display_name)


def setup(bot: commands.Bot) -> None:
    bot.add_cog(RecordingCog(bot))
