"""Tests for stream manager join/leave/reconnect tracking."""

from __future__ import annotations

from collector.voice.stream_manager import StreamManager


class TestStreamManager:
    def test_track_new_user(self):
        mgr = StreamManager()
        state = mgr.track_user(100, "TestUser")
        assert state.user_id == 100
        assert state.connected is True
        assert len(state.events) == 1
        assert state.events[0].event == "join"

    def test_track_user_idempotent(self):
        mgr = StreamManager()
        state1 = mgr.track_user(100, "TestUser")
        state2 = mgr.track_user(100, "TestUser")
        assert state1 is state2

    def test_user_leave(self):
        mgr = StreamManager()
        mgr.track_user(100, "TestUser")
        mgr.user_left(100)
        state = mgr.get_user(100)
        assert state.connected is False

    def test_user_reconnect(self):
        mgr = StreamManager()
        mgr.track_user(100, "TestUser")
        mgr.user_left(100)
        mgr.user_rejoined(100)

        state = mgr.get_user(100)
        assert state.connected is True
        assert len(state.gaps) == 1
        assert state.gaps[0]["reason"] == "disconnect"

    def test_connected_users(self):
        mgr = StreamManager()
        mgr.track_user(100, "User1")
        mgr.track_user(200, "User2")
        mgr.user_left(100)

        connected = mgr.connected_users
        assert len(connected) == 1
        assert connected[0].user_id == 200

    def test_multiple_disconnects(self):
        mgr = StreamManager()
        mgr.track_user(100, "User1")
        mgr.user_left(100)
        mgr.user_rejoined(100)
        mgr.user_left(100)
        mgr.user_rejoined(100)

        state = mgr.get_user(100)
        assert len(state.gaps) == 2
