"""Tests for consent manager state machine and quorum logic."""

from __future__ import annotations

from unittest.mock import MagicMock

from collector.consent.manager import ConsentManager, ConsentSession
from collector.consent.types import ConsentScope, SessionState


def make_member(user_id: int, name: str = "TestUser") -> MagicMock:
    m = MagicMock()
    m.id = user_id
    m.display_name = name
    m.bot = False
    return m


class TestConsentSession:
    def test_add_participant(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        member = make_member(100, "GM")
        session.add_participant(member)

        assert 100 in session.participants
        assert session.participants[100].display_name == "GM"
        assert session.participants[100].scope is None

    def test_add_participant_idempotent(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        member = make_member(100, "GM")
        session.add_participant(member)
        session.add_participant(member)

        assert len(session.participants) == 1

    def test_record_consent(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        session.add_participant(make_member(100))
        session.record_consent(100, ConsentScope.FULL)

        assert session.participants[100].scope == ConsentScope.FULL
        assert session.participants[100].consented_at is not None

    def test_record_consent_unknown_user(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        session.record_consent(999, ConsentScope.FULL)  # should not raise

    def test_consented_user_ids(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        session.add_participant(make_member(100))
        session.add_participant(make_member(200))
        session.add_participant(make_member(300))

        session.record_consent(100, ConsentScope.FULL)
        session.record_consent(200, ConsentScope.DECLINE_AUDIO)
        session.record_consent(300, ConsentScope.FULL)

        assert session.consented_user_ids == {100, 300}

    def test_pending_user_ids(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        session.add_participant(make_member(100))
        session.add_participant(make_member(200))

        session.record_consent(100, ConsentScope.FULL)

        assert session.pending_user_ids == {200}

    def test_has_any_decline(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        session.add_participant(make_member(100))
        session.add_participant(make_member(200))

        session.record_consent(100, ConsentScope.FULL)
        assert not session.has_any_decline

        session.record_consent(200, ConsentScope.DECLINE)
        assert session.has_any_decline

    def test_decline_audio_is_not_full_decline(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        session.add_participant(make_member(100))
        session.record_consent(100, ConsentScope.DECLINE_AUDIO)
        assert not session.has_any_decline

    def test_remove_participant_before_consent(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        session.add_participant(make_member(100))
        session.add_participant(make_member(200))

        session.remove_participant(200)
        assert 200 not in session.participants

    def test_remove_participant_after_consent_no_op(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        session.add_participant(make_member(100))
        session.record_consent(100, ConsentScope.FULL)
        session.remove_participant(100)
        # Should NOT remove — already consented
        assert 100 in session.participants

    def test_to_consent_json(self):
        session = ConsentSession(guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100)
        session.add_participant(make_member(100, "GM"))
        session.record_consent(100, ConsentScope.FULL)

        pseudo_map = {100: "abc123"}
        result = session.to_consent_json(pseudo_map)

        assert result["consent_version"] == "1.0"
        assert result["license"] == "CC BY-SA 4.0"
        assert "abc123" in result["participants"]
        assert result["participants"]["abc123"]["scope"] == "full"
        assert result["participants"]["abc123"]["audio_release"] is True


class TestConsentManager:
    def test_create_and_get_session(self):
        mgr = ConsentManager()
        members = [make_member(100), make_member(200)]
        session = mgr.create_session(
            guild_id=1, channel_id=2, text_channel_id=3, initiator_id=100, members=members
        )

        assert mgr.get_session(1) is session
        assert len(session.participants) == 2

    def test_has_active_session(self):
        mgr = ConsentManager()
        assert not mgr.has_active_session(1)

        session = mgr.create_session(
            guild_id=1,
            channel_id=2,
            text_channel_id=3,
            initiator_id=100,
            members=[make_member(100)],
        )
        session.state = SessionState.RECORDING
        assert mgr.has_active_session(1)

    def test_remove_session(self):
        mgr = ConsentManager()
        mgr.create_session(
            guild_id=1,
            channel_id=2,
            text_channel_id=3,
            initiator_id=100,
            members=[make_member(100)],
        )
        mgr.remove_session(1)
        assert mgr.get_session(1) is None

    def test_no_active_for_complete_session(self):
        mgr = ConsentManager()
        session = mgr.create_session(
            guild_id=1,
            channel_id=2,
            text_channel_id=3,
            initiator_id=100,
            members=[make_member(100)],
        )
        session.state = SessionState.COMPLETE
        assert not mgr.has_active_session(1)
