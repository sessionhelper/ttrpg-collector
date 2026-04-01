"""Quality flags computed at collection time. Flag but don't discard."""

from __future__ import annotations

from pathlib import Path

import structlog

from collector.config import settings

log = structlog.get_logger()


def compute_quality_flags(
    duration_seconds: float,
    consented_count: int,
    wav_paths: list[Path],
) -> dict:
    """Compute quality flags for a session. These are informational only —
    the curation pipeline decides what to include in the public dataset."""
    flags = {}

    if duration_seconds < settings.min_session_duration_seconds:
        flags["short_session"] = {
            "threshold": settings.min_session_duration_seconds,
            "actual": round(duration_seconds, 1),
        }

    if consented_count < settings.min_participants:
        flags["low_participants"] = {
            "threshold": settings.min_participants,
            "actual": consented_count,
        }

    # Check for empty/tiny audio files
    empty_tracks = []
    for wav in wav_paths:
        if wav.stat().st_size < 1024:  # less than 1KB = essentially empty
            empty_tracks.append(wav.name)
    if empty_tracks:
        flags["empty_tracks"] = empty_tracks

    # Total audio size
    total_audio_bytes = sum(w.stat().st_size for w in wav_paths)
    if total_audio_bytes < 10 * 1024:  # less than 10KB total
        flags["minimal_audio"] = {"total_bytes": total_audio_bytes}

    if flags:
        log.info("quality_flags", flags=flags)

    return flags
