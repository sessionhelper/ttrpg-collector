"""Tests for session bundle assembly and pseudonymization."""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import MagicMock

from collector.consent.manager import ConsentSession
from collector.consent.types import ConsentScope
from collector.storage.session_bundle import SessionBundle
from collector.voice.stream_manager import StreamManager


def make_member(user_id: int, name: str = "TestUser") -> MagicMock:
    m = MagicMock()
    m.id = user_id
    m.display_name = name
    m.bot = False
    return m


class TestPseudonymization:
    def test_pseudonymize_deterministic(self):
        bundle = SessionBundle(
            session_id="test-123",
            session_dir=Path("/tmp/test"),
            guild_id=1,
            channel_id=2,
            started_at=datetime.now(UTC),
        )
        result1 = bundle.pseudonymize(12345)
        result2 = bundle.pseudonymize(12345)
        assert result1 == result2

    def test_pseudonymize_different_users(self):
        bundle = SessionBundle(
            session_id="test-123",
            session_dir=Path("/tmp/test"),
            guild_id=1,
            channel_id=2,
            started_at=datetime.now(UTC),
        )
        result1 = bundle.pseudonymize(12345)
        result2 = bundle.pseudonymize(67890)
        assert result1 != result2

    def test_pseudonymize_different_sessions(self):
        """Different sessions should produce different pseudonyms (different salts)."""
        bundle1 = SessionBundle(
            session_id="test-1",
            session_dir=Path("/tmp/test1"),
            guild_id=1,
            channel_id=2,
            started_at=datetime.now(UTC),
        )
        bundle2 = SessionBundle(
            session_id="test-2",
            session_dir=Path("/tmp/test2"),
            guild_id=1,
            channel_id=2,
            started_at=datetime.now(UTC),
        )
        assert bundle1.pseudonymize(12345) != bundle2.pseudonymize(12345)

    def test_pseudonymize_length(self):
        bundle = SessionBundle(
            session_id="test-123",
            session_dir=Path("/tmp/test"),
            guild_id=1,
            channel_id=2,
            started_at=datetime.now(UTC),
        )
        result = bundle.pseudonymize(12345)
        assert len(result) == 16
        assert all(c in "0123456789abcdef" for c in result)


class TestBundleWriting:
    def test_write_meta(self, tmp_path: Path):
        session_dir = tmp_path / "session"
        session_dir.mkdir()
        audio_dir = session_dir / "audio"
        audio_dir.mkdir()

        # Create a fake audio file
        wav = audio_dir / "12345.flac"
        wav.write_bytes(b"\x00" * 1024)

        bundle = SessionBundle(
            session_id="test-123",
            session_dir=session_dir,
            guild_id=1,
            channel_id=2,
            started_at=datetime(2026, 4, 1, 19, 0, tzinfo=UTC),
            ended_at=datetime(2026, 4, 1, 23, 0, tzinfo=UTC),
            game_system="D&D 5e",
            campaign_name="Test Campaign",
            session_number=1,
        )

        consent_session = ConsentSession(
            guild_id=1, channel_id=2, text_channel_id=3, initiator_id=12345
        )
        consent_session.add_participant(make_member(12345, "GM"))
        consent_session.record_consent(12345, ConsentScope.FULL)

        stream_mgr = StreamManager()
        stream_mgr.track_user(12345, "GM")

        meta_path = bundle.write_meta(consent_session, stream_mgr, [wav])

        assert meta_path.exists()
        meta = json.loads(meta_path.read_text())
        assert meta["session_id"] == "test-123"
        assert meta["game_system"] == "D&D 5e"
        assert meta["duration_seconds"] == 14400.0
        assert meta["consented_audio_count"] == 1
        assert meta["audio_format"]["sample_rate"] == 48000
        assert len(meta["participants"]) == 1

    def test_write_consent(self, tmp_path: Path):
        session_dir = tmp_path / "session"
        session_dir.mkdir()

        bundle = SessionBundle(
            session_id="test-123",
            session_dir=session_dir,
            guild_id=1,
            channel_id=2,
            started_at=datetime.now(UTC),
        )

        consent_session = ConsentSession(
            guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100
        )
        consent_session.add_participant(make_member(100, "GM"))
        consent_session.record_consent(100, ConsentScope.FULL)

        consent_path = bundle.write_consent(consent_session)

        assert consent_path.exists()
        data = json.loads(consent_path.read_text())
        assert data["session_id"] == "test-123"
        assert data["license"] == "CC BY-SA 4.0"
        assert len(data["participants"]) == 1

    def test_write_pii(self, tmp_path: Path):
        session_dir = tmp_path / "session"
        session_dir.mkdir()

        bundle = SessionBundle(
            session_id="test-123",
            session_dir=session_dir,
            guild_id=1,
            channel_id=2,
            started_at=datetime.now(UTC),
        )

        consent_session = ConsentSession(
            guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100
        )
        consent_session.add_participant(make_member(100, "GM"))
        consent_session.record_consent(100, ConsentScope.FULL)

        pii_path = bundle.write_pii(consent_session)

        assert pii_path.exists()
        data = json.loads(pii_path.read_text())
        assert data["guild_id"] == 1
        # PII should contain the real discord ID
        participant = data["participants"]["100"]
        assert participant["discord_id"] == 100
        assert participant["display_name"] == "GM"

    def test_write_notes(self, tmp_path: Path):
        session_dir = tmp_path / "session"
        session_dir.mkdir()

        bundle = SessionBundle(
            session_id="test-123",
            session_dir=session_dir,
            guild_id=1,
            channel_id=2,
            started_at=datetime.now(UTC),
        )

        notes_path = bundle.write_notes("The party fought a dragon.")
        assert notes_path.exists()
        assert notes_path.read_text() == "The party fought a dragon."
