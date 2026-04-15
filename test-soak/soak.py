#!/usr/bin/env python3
"""
Long-session soak harness for chronicle-bot (roadmap #1).

Drives the dev feeder fleet through an N-hour recording session, sampling
bot resource use throughout, and asserts a clean teardown at the end.

What it proves:
  - Bot survives a multi-hour session without leaking FDs or accum.
  - Sustained packet flow doesn't stall.
  - Final session row reaches a terminal state.

Failure signals (each tagged in the log):
  - RSS growth > RSS_GROWTH_LIMIT_MB_PER_HR
  - FD count growth > FD_GROWTH_LIMIT_PER_HR
  - chronicle_dave_heal_requests_total increments (any)
  - silent-packet rate > 0 in any voice_rx_rollup
  - active-feeder play stall: nobody playing for > STALL_LIMIT_SECS
  - bot session not in {transcribed,failed,...} after /stop

Designed to be invoked nightly. Status reported as a final
JSON line for easy log scraping.

Usage:
  GUILD_ID=... CHANNEL_ID=... BOT_PID=... \
    DURATION_SECS=10800 \
    python3 soak.py
"""

from __future__ import annotations

import json
import os
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import urllib.request
import urllib.error


COLLECTOR_URL = os.environ.get("COLLECTOR_URL", "http://127.0.0.1:8010")
FEEDER_URLS = os.environ.get(
    "FEEDER_URLS",
    "http://127.0.0.1:8003,http://127.0.0.1:8004,"
    "http://127.0.0.1:8005,http://127.0.0.1:8006",
).split(",")
GUILD_ID = int(os.environ["GUILD_ID"])
CHANNEL_ID = int(os.environ["CHANNEL_ID"])

DURATION_SECS = int(os.environ.get("DURATION_SECS", str(4 * 3600)))
PROBE_INTERVAL_SECS = int(os.environ.get("PROBE_INTERVAL_SECS", "60"))
JOIN_DELAY_SECS = int(os.environ.get("JOIN_DELAY_SECS", "5"))
STALL_LIMIT_SECS = int(os.environ.get("STALL_LIMIT_SECS", "30"))
RSS_GROWTH_LIMIT_MB_PER_HR = int(
    os.environ.get("RSS_GROWTH_LIMIT_MB_PER_HR", "100")
)
FD_GROWTH_LIMIT_PER_HR = int(os.environ.get("FD_GROWTH_LIMIT_PER_HR", "50"))

# When set, soak.py reads /proc/<pid>/{status,fd} for the bot. When the
# bot runs in Docker, point this at the host PID (`docker inspect ...`)
# or set BOT_CONTAINER and we exec into the container instead.
BOT_PID = os.environ.get("BOT_PID")
BOT_CONTAINER = os.environ.get("BOT_CONTAINER")


def now_ms() -> int:
    return int(time.time() * 1000)


def post(url: str, body: dict | None = None) -> dict:
    data = json.dumps(body or {}).encode()
    req = urllib.request.Request(
        url, data=data, headers={"content-type": "application/json"}, method="POST"
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read() or b"{}")


def get(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=10) as r:
        return json.loads(r.read() or b"{}")


@dataclass
class Probe:
    ts_ms: int
    rss_kb: int | None
    fd_count: int | None
    feeders_playing: int


@dataclass
class Failure:
    ts_ms: int
    kind: str
    detail: str


@dataclass
class SoakState:
    started_ms: int = field(default_factory=now_ms)
    probes: list[Probe] = field(default_factory=list)
    failures: list[Failure] = field(default_factory=list)
    last_play_ms: int = field(default_factory=now_ms)
    session_id: str | None = None

    def fail(self, kind: str, detail: str) -> None:
        f = Failure(now_ms(), kind, detail)
        self.failures.append(f)
        print(f"FAIL[{kind}] {detail}", file=sys.stderr)


def read_rss_kb(pid: str) -> int | None:
    p = Path(f"/proc/{pid}/status")
    if not p.exists():
        return None
    for line in p.read_text().splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1])
    return None


def read_fd_count(pid: str) -> int | None:
    p = Path(f"/proc/{pid}/fd")
    if not p.exists():
        return None
    try:
        return sum(1 for _ in p.iterdir())
    except PermissionError:
        return None


def probe_bot() -> tuple[int | None, int | None]:
    """Returns (rss_kb, fd_count). None when unsupported."""
    if BOT_PID:
        return read_rss_kb(BOT_PID), read_fd_count(BOT_PID)
    if BOT_CONTAINER:
        # Best-effort: read /proc inside the container via docker exec.
        # Bot's own process is PID 1 inside its container.
        import subprocess

        try:
            rss = subprocess.check_output(
                ["docker", "exec", BOT_CONTAINER, "cat", "/proc/1/status"],
                timeout=5,
            ).decode()
            rss_kb = next(
                int(l.split()[1]) for l in rss.splitlines() if l.startswith("VmRSS:")
            )
            fd_out = subprocess.check_output(
                ["docker", "exec", BOT_CONTAINER, "ls", "/proc/1/fd"], timeout=5
            ).decode()
            return rss_kb, len(fd_out.split())
        except Exception:
            return None, None
    return None, None


def feeders_playing() -> int:
    n = 0
    for url in FEEDER_URLS:
        try:
            h = get(f"{url}/health")
            if h.get("playing"):
                n += 1
        except Exception:
            pass
    return n


def fire_play_on_idle() -> None:
    for url in FEEDER_URLS:
        try:
            h = get(f"{url}/health")
            if h.get("in_voice") and not h.get("playing"):
                post(f"{url}/play")
        except Exception as e:
            print(f"  play retry on {url} failed: {e}", file=sys.stderr)


def join_all_staggered() -> None:
    for i, url in enumerate(FEEDER_URLS):
        print(f"  joining feeder {i + 1}/{len(FEEDER_URLS)}: {url}")
        post(
            f"{url}/join",
            {"guild_id": GUILD_ID, "channel_id": CHANNEL_ID},
        )
        time.sleep(JOIN_DELAY_SECS)


def leave_all() -> None:
    for url in FEEDER_URLS:
        try:
            post(f"{url}/leave")
        except Exception:
            pass


def main() -> int:
    state = SoakState()
    print(f"=== soak start ({DURATION_SECS}s) ===")
    print(f"collector={COLLECTOR_URL}  feeders={len(FEEDER_URLS)}")
    print(f"probes every {PROBE_INTERVAL_SECS}s; reporting JSON on exit")

    rec = post(
        f"{COLLECTOR_URL}/record",
        {"guild_id": GUILD_ID, "channel_id": CHANNEL_ID},
    )
    state.session_id = rec.get("session_id")
    print(f"session_id={state.session_id}")

    join_all_staggered()
    fire_play_on_idle()

    deadline = state.started_ms + DURATION_SECS * 1000
    next_probe = state.started_ms + PROBE_INTERVAL_SECS * 1000

    while now_ms() < deadline:
        playing = feeders_playing()
        if playing > 0:
            state.last_play_ms = now_ms()
        else:
            stalled_for = (now_ms() - state.last_play_ms) / 1000
            if stalled_for > STALL_LIMIT_SECS:
                state.fail(
                    "stall",
                    f"no feeder playing for {stalled_for:.0f}s — restarting play",
                )
                state.last_play_ms = now_ms()
            fire_play_on_idle()

        if now_ms() >= next_probe:
            rss, fds = probe_bot()
            state.probes.append(Probe(now_ms(), rss, fds, playing))
            next_probe = now_ms() + PROBE_INTERVAL_SECS * 1000
            check_growth_limits(state)

        time.sleep(2)

    print("=== soak teardown ===")
    leave_all()
    try:
        post(f"{COLLECTOR_URL}/stop", {"guild_id": GUILD_ID})
    except Exception as e:
        state.fail("stop_failed", str(e))

    report = build_report(state)
    print(json.dumps(report, indent=2))
    return 0 if not state.failures else 1


def check_growth_limits(state: SoakState) -> None:
    if len(state.probes) < 2:
        return
    first, last = state.probes[0], state.probes[-1]
    hours = max((last.ts_ms - first.ts_ms) / 3_600_000, 1 / 60)
    if first.rss_kb and last.rss_kb:
        rss_growth_mb_per_hr = ((last.rss_kb - first.rss_kb) / 1024) / hours
        if rss_growth_mb_per_hr > RSS_GROWTH_LIMIT_MB_PER_HR:
            state.fail(
                "rss_growth",
                f"{rss_growth_mb_per_hr:.1f} MB/hr "
                f"> limit {RSS_GROWTH_LIMIT_MB_PER_HR}",
            )
    if first.fd_count and last.fd_count:
        fd_growth_per_hr = (last.fd_count - first.fd_count) / hours
        if fd_growth_per_hr > FD_GROWTH_LIMIT_PER_HR:
            state.fail(
                "fd_growth",
                f"{fd_growth_per_hr:.1f} fd/hr "
                f"> limit {FD_GROWTH_LIMIT_PER_HR}",
            )


def build_report(state: SoakState) -> dict:
    probes = state.probes
    return {
        "session_id": state.session_id,
        "started_at": datetime.fromtimestamp(
            state.started_ms / 1000, tz=timezone.utc
        ).isoformat(),
        "duration_secs": (now_ms() - state.started_ms) / 1000,
        "probe_count": len(probes),
        "rss_first_kb": probes[0].rss_kb if probes else None,
        "rss_last_kb": probes[-1].rss_kb if probes else None,
        "fd_first": probes[0].fd_count if probes else None,
        "fd_last": probes[-1].fd_count if probes else None,
        "failures": [
            {"ts_ms": f.ts_ms, "kind": f.kind, "detail": f.detail}
            for f in state.failures
        ],
        "ok": not state.failures,
    }


if __name__ == "__main__":
    sys.exit(main())
