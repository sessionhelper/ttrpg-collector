"""Local buffer management for in-progress and completed sessions."""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path

import structlog

from collector.config import settings

log = structlog.get_logger()

INDEX_FILE = ".index.json"


def get_buffer_dir() -> Path:
    path = settings.local_buffer_dir
    path.mkdir(parents=True, exist_ok=True)
    return path


def create_session_dir(guild_id: int, session_id: str) -> Path:
    session_dir = get_buffer_dir() / str(guild_id) / session_id
    session_dir.mkdir(parents=True, exist_ok=True)
    return session_dir


def load_index() -> dict:
    index_path = get_buffer_dir() / INDEX_FILE
    if index_path.exists():
        return json.loads(index_path.read_text())
    return {"sessions": {}}


def save_index(index: dict) -> None:
    index_path = get_buffer_dir() / INDEX_FILE
    index_path.write_text(json.dumps(index, indent=2))


def mark_uploaded(session_id: str) -> None:
    index = load_index()
    if session_id in index["sessions"]:
        index["sessions"][session_id]["uploaded_at"] = datetime.now(UTC).isoformat()
        index["sessions"][session_id]["status"] = "uploaded"
        save_index(index)


def mark_recording(guild_id: int, session_id: str, session_dir: str) -> None:
    index = load_index()
    index["sessions"][session_id] = {
        "guild_id": guild_id,
        "session_dir": session_dir,
        "started_at": datetime.now(UTC).isoformat(),
        "status": "recording",
    }
    save_index(index)


def mark_failed(session_id: str, error: str) -> None:
    index = load_index()
    if session_id in index["sessions"]:
        index["sessions"][session_id]["status"] = "upload_failed"
        index["sessions"][session_id]["error"] = error
        save_index(index)


def get_orphaned_sessions() -> list[dict]:
    """Find sessions that were recording or failed upload (for retry on restart)."""
    index = load_index()
    orphans = []
    for sid, info in index["sessions"].items():
        if info["status"] in ("recording", "upload_failed"):
            session_dir = Path(info["session_dir"])
            if session_dir.exists():
                orphans.append({"session_id": sid, **info})
    return orphans


def cleanup_uploaded(max_age_hours: int = 48) -> int:
    """Remove local files for sessions uploaded more than max_age_hours ago."""
    import shutil

    index = load_index()
    cleaned = 0
    now = datetime.now(UTC)

    for sid, info in list(index["sessions"].items()):
        if info["status"] != "uploaded":
            continue
        uploaded_at = datetime.fromisoformat(info["uploaded_at"])
        age_hours = (now - uploaded_at).total_seconds() / 3600
        if age_hours > max_age_hours:
            session_dir = Path(info["session_dir"])
            if session_dir.exists():
                shutil.rmtree(session_dir)
                log.info("cleaned_up", session_id=sid, age_hours=round(age_hours, 1))
            del index["sessions"][sid]
            cleaned += 1

    if cleaned:
        save_index(index)
    return cleaned
