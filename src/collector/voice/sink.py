"""Custom AudioSink that writes per-user PCM to disk files.

py-cord's voice receive delivers decoded PCM (signed 16-bit LE, 48kHz, stereo)
per-user via the AudioSink interface. This sink writes each user's audio to a
separate raw PCM file on disk, avoiding memory accumulation for long sessions.
"""

from __future__ import annotations

import asyncio
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

import discord
import structlog

log = structlog.get_logger()


@dataclass
class UserStream:
    user_id: int
    pcm_path: Path
    file: object = field(default=None, repr=False)  # IO handle
    bytes_written: int = 0
    part: int = 1
    _last_seq: int = field(default=-1, repr=False)

    def open(self) -> None:
        self.pcm_path.parent.mkdir(parents=True, exist_ok=True)
        self.file = open(self.pcm_path, "ab")  # append for reconnects

    def write(self, data: bytes) -> None:
        if self.file is None:
            return
        self.file.write(data)
        self.bytes_written += len(data)

    def close(self) -> None:
        if self.file is not None:
            self.file.close()
            self.file = None


class DiskSink(discord.sinks.Sink):
    """Writes per-user PCM audio directly to disk.

    Each consented user gets a raw PCM file at:
        {session_dir}/pcm/{user_id}.pcm

    On finalization, PCM files are converted to WAV via ffmpeg.
    """

    def __init__(self, session_dir: Path, consented_user_ids: set[int]) -> None:
        super().__init__()
        self.session_dir = session_dir
        self.consented_user_ids = set(consented_user_ids)
        self.pcm_dir = session_dir / "pcm"
        self.pcm_dir.mkdir(parents=True, exist_ok=True)
        self.audio_dir = session_dir / "audio"
        self.audio_dir.mkdir(parents=True, exist_ok=True)
        self._streams: dict[int, UserStream] = {}

    def _get_or_create_stream(self, user_id: int) -> UserStream | None:
        if user_id not in self.consented_user_ids:
            return None
        if user_id not in self._streams:
            stream = UserStream(
                user_id=user_id,
                pcm_path=self.pcm_dir / f"{user_id}.pcm",
            )
            stream.open()
            self._streams[user_id] = stream
            log.info("stream_opened", user_id=user_id, path=str(stream.pcm_path))
        return self._streams[user_id]

    # PCM frame size: 48kHz * 2 channels * 2 bytes/sample * 20ms = 3840 bytes
    FRAME_SIZE = 3840
    SILENCE_FRAME = b"\x00" * FRAME_SIZE

    def write(self, data, user) -> None:
        """Called by py-cord for each chunk of decoded PCM audio.

        In the DAVE branch, data is a VoiceData object with .pcm bytes,
        and user is a Member/User object (not an int).
        """
        # Handle both old API (bytes, int) and new DAVE API (VoiceData, Member)
        pcm = data.pcm if hasattr(data, "pcm") else data
        user_id = user.id if hasattr(user, "id") else user

        # Track sequence numbers and fill gaps with silence
        if hasattr(data, "packet"):
            seq = data.packet.sequence
            stream = self._get_or_create_stream(user_id)
            if stream is not None:
                if hasattr(stream, "_last_seq") and stream._last_seq >= 0:
                    gap = (seq - stream._last_seq - 1) & 0xFFFF
                    if 0 < gap < 100:  # reasonable gap, not a wrap-around glitch
                        stream.write(self.SILENCE_FRAME * gap)
                stream._last_seq = seq
                stream.write(pcm)
        else:
            stream = self._get_or_create_stream(user_id)
            if stream is not None:
                stream.write(pcm)

    def add_consented_user(self, user_id: int) -> None:
        """Allow a mid-session joiner to be recorded."""
        self.consented_user_ids.add(user_id)

    def cleanup(self) -> None:
        """Close all file handles."""
        for stream in self._streams.values():
            stream.close()

    async def convert_to_flac(self) -> list[Path]:
        """Convert all PCM files to FLAC. Returns list of FLAC paths."""
        self.cleanup()
        flac_paths = []
        loop = asyncio.get_event_loop()

        for stream in self._streams.values():
            if stream.bytes_written == 0:
                continue
            flac_path = self.audio_dir / f"{stream.user_id}.flac"
            await loop.run_in_executor(None, self._pcm_to_flac, stream.pcm_path, flac_path)
            flac_paths.append(flac_path)
            log.info(
                "flac_converted",
                user_id=stream.user_id,
                pcm_bytes=stream.bytes_written,
                flac_path=str(flac_path),
            )
        return flac_paths

    @staticmethod
    def _pcm_to_flac(pcm_path: Path, flac_path: Path) -> None:
        """Convert raw PCM to FLAC using ffmpeg."""
        cmd = [
            "ffmpeg",
            "-y",
            "-f",
            "s16le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-i",
            str(pcm_path),
            "-ac",
            "1",  # downmix to mono
            "-ar",
            "48000",
            str(flac_path),
        ]
        result = subprocess.run(cmd, capture_output=True, text=True)
        if result.returncode != 0:
            log.error("ffmpeg_error", stderr=result.stderr, pcm=str(pcm_path))
            raise RuntimeError(f"ffmpeg failed: {result.stderr}")

    @property
    def active_streams(self) -> dict[int, UserStream]:
        return dict(self._streams)

    @property
    def total_bytes(self) -> int:
        return sum(s.bytes_written for s in self._streams.values())
