"""Session metadata collection."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime


@dataclass
class SessionMetadata:
    session_id: str
    guild_id: int
    channel_id: int
    initiator_id: int
    started_at: datetime = field(default_factory=lambda: datetime.now(UTC))
    ended_at: datetime | None = None
    game_system: str | None = None
    campaign_name: str | None = None
    session_number: int | None = None

    def end(self) -> None:
        self.ended_at = datetime.now(UTC)

    @property
    def duration_seconds(self) -> float:
        if self.ended_at is None:
            return (datetime.now(UTC) - self.started_at).total_seconds()
        return (self.ended_at - self.started_at).total_seconds()
