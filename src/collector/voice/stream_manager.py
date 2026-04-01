"""Manages per-user voice stream state: joins, leaves, reconnects."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime

import structlog

log = structlog.get_logger()


@dataclass
class StreamEvent:
    timestamp: datetime
    event: str  # "join", "leave", "reconnect"


@dataclass
class UserStreamState:
    user_id: int
    display_name: str
    connected: bool = True
    events: list[StreamEvent] = field(default_factory=list)
    gaps: list[dict] = field(default_factory=list)
    _last_disconnect: datetime | None = field(default=None, repr=False)

    def record_join(self) -> None:
        now = datetime.now(UTC)
        if self._last_disconnect is not None:
            gap_seconds = (now - self._last_disconnect).total_seconds()
            self.gaps.append(
                {
                    "start": self._last_disconnect.isoformat(),
                    "end": now.isoformat(),
                    "duration_seconds": round(gap_seconds, 1),
                    "reason": "disconnect",
                }
            )
            self._last_disconnect = None
            self.events.append(StreamEvent(timestamp=now, event="reconnect"))
            log.info("user_reconnected", user_id=self.user_id, gap_seconds=gap_seconds)
        else:
            self.events.append(StreamEvent(timestamp=now, event="join"))
        self.connected = True

    def record_leave(self) -> None:
        now = datetime.now(UTC)
        self._last_disconnect = now
        self.events.append(StreamEvent(timestamp=now, event="leave"))
        self.connected = False
        log.info("user_left", user_id=self.user_id)


class StreamManager:
    """Track per-user connection state during a recording session."""

    def __init__(self) -> None:
        self._users: dict[int, UserStreamState] = {}

    def track_user(self, user_id: int, display_name: str) -> UserStreamState:
        if user_id not in self._users:
            state = UserStreamState(user_id=user_id, display_name=display_name)
            state.record_join()
            self._users[user_id] = state
        return self._users[user_id]

    def user_left(self, user_id: int) -> None:
        if user_id in self._users:
            self._users[user_id].record_leave()

    def user_rejoined(self, user_id: int) -> None:
        if user_id in self._users:
            self._users[user_id].record_join()

    def get_user(self, user_id: int) -> UserStreamState | None:
        return self._users.get(user_id)

    @property
    def connected_users(self) -> list[UserStreamState]:
        return [u for u in self._users.values() if u.connected]

    @property
    def all_users(self) -> dict[int, UserStreamState]:
        return dict(self._users)

    def get_gaps(self, user_id: int) -> list[dict]:
        user = self._users.get(user_id)
        return user.gaps if user else []
