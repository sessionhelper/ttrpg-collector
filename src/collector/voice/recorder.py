"""Voice channel recorder — joins, starts sink, handles lifecycle."""

from __future__ import annotations

import asyncio
from pathlib import Path

import discord
import structlog

from collector.voice.sink import DiskSink
from collector.voice.stream_manager import StreamManager

log = structlog.get_logger()


class VoiceRecorder:
    """Manages a single recording session in a voice channel."""

    def __init__(self, session_dir: Path, consented_user_ids: set[int]) -> None:
        self.session_dir = session_dir
        self.sink = DiskSink(session_dir, consented_user_ids)
        self.stream_manager = StreamManager()
        self.voice_client: discord.VoiceClient | None = None
        self._recording = False

    async def connect(self, channel: discord.VoiceChannel) -> None:
        self.voice_client = await channel.connect()
        loop = asyncio.get_event_loop()
        connected = await loop.run_in_executor(
            None, lambda: self.voice_client.wait_until_connected(timeout=30)
        )
        if not connected:
            raise RuntimeError("Voice connection timed out")
        log.info(
            "voice_connected",
            channel=channel.name,
            guild=channel.guild.name,
        )

    def start_recording(self, members: list[discord.Member]) -> None:
        if self.voice_client is None:
            raise RuntimeError("Not connected to voice channel")

        for m in members:
            if m.id in self.sink.consented_user_ids:
                self.stream_manager.track_user(m.id, m.display_name)

        self.voice_client.start_recording(self.sink, self._on_recording_stopped)
        self._recording = True
        log.info("recording_started", user_count=len(self.sink.consented_user_ids))

    def _on_recording_stopped(self, exception: Exception | None = None) -> None:
        """Callback when recording stops."""
        if exception:
            log.error("recording_stopped_with_error", error=str(exception))
        else:
            log.info("recording_stopped_callback")

    def add_mid_session_user(self, member: discord.Member) -> None:
        """Add a user who joined and consented mid-session."""
        self.sink.add_consented_user(member.id)
        self.stream_manager.track_user(member.id, member.display_name)
        log.info("mid_session_user_added", user_id=member.id)

    def user_left(self, user_id: int) -> None:
        self.stream_manager.user_left(user_id)

    def user_rejoined(self, user_id: int, display_name: str) -> None:
        self.stream_manager.user_rejoined(user_id)

    async def stop(self) -> list[Path]:
        """Stop recording and convert PCM to FLAC. Returns FLAC paths."""
        if self.voice_client and self._recording:
            self.voice_client.stop_recording()
            self._recording = False

        audio_paths = await self.sink.convert_to_flac()
        log.info("recording_finalized", track_count=len(audio_paths))
        return audio_paths

    async def disconnect(self) -> None:
        if self.voice_client and self.voice_client.is_connected():
            await self.voice_client.disconnect()
            log.info("voice_disconnected")

    @property
    def is_recording(self) -> bool:
        return self._recording
