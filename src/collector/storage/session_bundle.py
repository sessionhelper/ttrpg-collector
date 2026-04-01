"""Assembles a session directory into the final bundle with meta.json, consent.json, pii.json."""

from __future__ import annotations

import hashlib
import json
import secrets
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

import structlog

from collector.consent.manager import ConsentSession
from collector.voice.stream_manager import StreamManager

log = structlog.get_logger()

COLLECTOR_VERSION = "0.1.0"


@dataclass
class SessionBundle:
    session_id: str
    session_dir: Path
    guild_id: int
    channel_id: int
    started_at: datetime
    ended_at: datetime | None = None
    game_system: str | None = None
    campaign_name: str | None = None
    session_number: int | None = None

    def __post_init__(self) -> None:
        self._salt = secrets.token_hex(16)

    def pseudonymize(self, user_id: int) -> str:
        """One-way hash of user snowflake with session-scoped salt."""
        return hashlib.sha256(f"{user_id}:{self._salt}".encode()).hexdigest()[:16]

    def build_pseudo_map(self, user_ids: set[int]) -> dict[int, str]:
        return {uid: self.pseudonymize(uid) for uid in user_ids}

    def write_meta(
        self,
        consent_session: ConsentSession,
        stream_manager: StreamManager,
        wav_paths: list[Path],
        quality_flags: dict | None = None,
    ) -> Path:
        pseudo_map = self.build_pseudo_map(
            {p.user_id for p in consent_session.participants.values()}
        )

        duration = (self.ended_at - self.started_at).total_seconds() if self.ended_at else 0

        participants = []
        for uid, p in consent_session.participants.items():
            pseudo_id = pseudo_map[uid]
            stream_state = stream_manager.get_user(uid)
            wav_file = f"audio/{uid}.wav"
            has_wav = any(w.name == f"{uid}.wav" for w in wav_paths)

            entry = {
                "pseudo_id": pseudo_id,
                "track_file": wav_file if has_wav else None,
                "consent_scope": p.scope.value if p.scope else None,
                "gaps": stream_state.gaps if stream_state else [],
            }
            participants.append(entry)

        meta = {
            "session_id": self.session_id,
            "started_at": self.started_at.isoformat(),
            "ended_at": self.ended_at.isoformat() if self.ended_at else None,
            "duration_seconds": round(duration, 1),
            "game_system": self.game_system,
            "campaign_name": self.campaign_name,
            "session_number": self.session_number,
            "participant_count": len(consent_session.participants),
            "consented_audio_count": len(consent_session.consented_user_ids),
            "collector_version": COLLECTOR_VERSION,
            "audio_format": {
                "sample_rate": 48000,
                "bit_depth": 16,
                "channels": 1,
                "codec": "pcm_s16le",
                "container": "wav",
            },
            "participants": participants,
            "quality_flags": quality_flags or {},
        }

        meta_path = self.session_dir / "meta.json"
        meta_path.write_text(json.dumps(meta, indent=2))
        log.info("meta_written", path=str(meta_path))
        return meta_path

    def write_consent(self, consent_session: ConsentSession) -> Path:
        pseudo_map = self.build_pseudo_map(
            {p.user_id for p in consent_session.participants.values()}
        )
        consent_data = consent_session.to_consent_json(pseudo_map)
        consent_data["session_id"] = self.session_id

        consent_path = self.session_dir / "consent.json"
        consent_path.write_text(json.dumps(consent_data, indent=2))
        log.info("consent_written", path=str(consent_path))
        return consent_path

    def write_pii(self, consent_session: ConsentSession) -> Path:
        """Write PII file with real snowflakes and display names. NEVER in public dataset."""
        pseudo_map = self.build_pseudo_map(
            {p.user_id for p in consent_session.participants.values()}
        )
        pii_data = {
            "session_id": self.session_id,
            "guild_id": self.guild_id,
            "channel_id": self.channel_id,
            "participants": {
                str(uid): {
                    "pseudo_id": pseudo_map[uid],
                    "display_name": p.display_name,
                    "discord_id": uid,
                }
                for uid, p in consent_session.participants.items()
            },
        }

        pii_path = self.session_dir / "pii.json"
        pii_path.write_text(json.dumps(pii_data, indent=2))
        log.info("pii_written", path=str(pii_path))
        return pii_path

    def write_notes(self, notes_text: str) -> Path:
        notes_path = self.session_dir / "notes.md"
        notes_path.write_text(notes_text)
        log.info("notes_written", path=str(notes_path))
        return notes_path

    def rename_wav_files(self, wav_paths: list[Path]) -> list[Path]:
        """Rename WAV files from user_id.wav to pseudo_id.wav for the public dataset."""
        renamed = []
        for wav in wav_paths:
            user_id = int(wav.stem)
            pseudo_id = self.pseudonymize(user_id)
            new_path = wav.parent / f"{pseudo_id}.wav"
            wav.rename(new_path)
            renamed.append(new_path)
            log.info("wav_renamed", old=wav.name, new=new_path.name)
        return renamed
