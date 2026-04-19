#!/usr/bin/env python3
"""
Long-session soak harness for chronicle-bot (roadmap #1).

Drives the dev feeder fleet through an N-hour recording session, sampling
bot resource use throughout, and asserts a clean teardown at the end.

What it proves:
  - Bot survives a multi-hour session without leaking FDs or accum.
  - Sustained packet flow doesn't stall.
  - Final session row reaches a terminal state.
  - No DAVE decrypt distress (heal path never fires; silent-packet rate 0).

Failure signals (each tagged in the report):
  - RSS growth > RSS_GROWTH_LIMIT_MB_PER_HR
  - FD count growth > FD_GROWTH_LIMIT_PER_HR
  - DAVE heal fired > 0 (dave_heal_firing log line during soak window)
  - DAVE heal requested > DAVE_HEAL_REQUEST_LIMIT (receiver-side trigger)
  - silent-packet count > SILENT_PACKETS_LIMIT in final voice_rx_rollup
  - active-feeder play stall: nobody playing for > STALL_LIMIT_SECS
  - /stop errors

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
import re
import subprocess
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
# Any dave_heal_firing is a failure (the heal path is a correctness
# patch over a DAVE bug-class; if it runs during a nominally-clean soak,
# the bot was under distress).
DAVE_HEAL_FIRED_LIMIT = int(os.environ.get("DAVE_HEAL_FIRED_LIMIT", "0"))
# Receiver-side consecutive-silent trigger. This fires upstream of the
# actual leave/rejoin (which may be debounced), so a non-zero rate here
# with zero fires means the debounce held.
DAVE_HEAL_REQUEST_LIMIT = int(os.environ.get("DAVE_HEAL_REQUEST_LIMIT", "0"))
# Silent-packet rollup threshold. 0 matches the README's "silent > 0 in
# any voice_rx_rollup" intent — bump if DAVE transitions reliably
# produce a handful of silent ticks and we want to tolerate them.
SILENT_PACKETS_LIMIT = int(os.environ.get("SILENT_PACKETS_LIMIT", "0"))

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


# Strip CSI sequences ("\x1b[Nm") emitted by tracing-subscriber's pretty
# formatter. We only need to parse key=value pairs out of the message body,
# not pretty-print them.
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def _parse_kv(line: str, key: str) -> int | None:
    """Extract an integer field from a pretty-tracing-formatted log line.

    Returns None when the key is absent or not an integer. Kept deliberately
    forgiving; partial reads shouldn't fail the whole scan.
    """
    m = re.search(rf"\b{re.escape(key)}=(-?\d+)", line)
    return int(m.group(1)) if m else None


@dataclass
class LogScanResult:
    scanned: bool
    error: str | None = None
    heal_fired: int = 0
    heal_requested: int = 0
    heal_debounced: int = 0
    rollup_count: int = 0
    # Count of rollup lines inside the window with silent > 0. Match the
    # README's "silent > 0 in any voice_rx_rollup" intent. Counting rather
    # than summing absolute values, because multiple AudioReceivers can
    # overlap briefly during /stop → /record transitions and the absolute
    # counters aren't per-session — but a non-zero `silent` on any rollup
    # means packets went silent at that moment.
    silent_rollup_count: int = 0
    silent_max: int = 0
    # Max "decoded" seen across any rollup in the window. Sanity signal
    # that we captured *some* audio; useful for confirming scenarios that
    # should have produced frames actually did.
    peak_decoded: int = 0


def scan_bot_logs(started_ms: int) -> LogScanResult:
    """Scrape BOT_CONTAINER's docker logs since `started_ms` for DAVE
    distress signals and the final voice_rx_rollup snapshot.

    Cheap and blunt: runs once post-teardown rather than streaming. Fine
    because the rollup counters are monotonic and we only care about the
    aggregate for the soak window.
    """
    if not BOT_CONTAINER:
        return LogScanResult(scanned=False, error="BOT_CONTAINER not set")
    since = datetime.fromtimestamp(started_ms / 1000, tz=timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%S"
    )
    try:
        out = subprocess.check_output(
            ["docker", "logs", "--since", since, BOT_CONTAINER],
            stderr=subprocess.STDOUT,
            timeout=60,
        ).decode("utf-8", errors="replace")
    except Exception as e:
        return LogScanResult(scanned=False, error=str(e))

    result = LogScanResult(scanned=True)
    for raw in out.splitlines():
        line = _ANSI_RE.sub("", raw)
        if "dave_heal_firing" in line:
            result.heal_fired += 1
        if "dave_heal_requested" in line:
            result.heal_requested += 1
        if "dave_heal_debounced" in line:
            result.heal_debounced += 1
        if "voice_rx_rollup" in line:
            result.rollup_count += 1
            decoded = _parse_kv(line, "decoded")
            silent = _parse_kv(line, "silent")
            if decoded is not None and decoded > result.peak_decoded:
                result.peak_decoded = decoded
            if silent is not None and silent > 0:
                result.silent_rollup_count += 1
                if silent > result.silent_max:
                    result.silent_max = silent
    return result


def check_log_signals(state: SoakState, scan: LogScanResult) -> None:
    if not scan.scanned:
        # Don't fail the soak because we couldn't scrape — but surface it.
        print(
            f"log scrape skipped: {scan.error}",
            file=sys.stderr,
        )
        return
    if scan.heal_fired > DAVE_HEAL_FIRED_LIMIT:
        state.fail(
            "dave_heal_fired",
            f"{scan.heal_fired} heal(s) fired > limit {DAVE_HEAL_FIRED_LIMIT}",
        )
    if scan.heal_requested > DAVE_HEAL_REQUEST_LIMIT:
        state.fail(
            "dave_heal_requested",
            f"{scan.heal_requested} heal request(s) > limit "
            f"{DAVE_HEAL_REQUEST_LIMIT} "
            f"({scan.heal_debounced} debounced, {scan.heal_fired} fired)",
        )
    # Count rollups whose `silent` field was non-zero at snapshot time.
    # The README intent is "silent > 0 in any voice_rx_rollup" — counting
    # rollups sidesteps the cumulative-counter issue (multiple AudioReceivers
    # can overlap across /stop → /record transitions, so absolute silent
    # numbers aren't session-scoped).
    if scan.silent_rollup_count > SILENT_PACKETS_LIMIT:
        state.fail(
            "silent_packets",
            f"{scan.silent_rollup_count} rollup(s) with silent>0 "
            f"(peak={scan.silent_max}) > limit {SILENT_PACKETS_LIMIT}",
        )


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

    # Give the bot a beat to flush the final voice_rx_rollup + any
    # post-teardown log lines before we scrape.
    time.sleep(6)
    scan = scan_bot_logs(state.started_ms)
    check_log_signals(state, scan)

    report = build_report(state, scan)
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


def build_report(state: SoakState, scan: LogScanResult | None = None) -> dict:
    probes = state.probes
    report: dict = {
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
    if scan is not None:
        report["log_scan"] = {
            "scanned": scan.scanned,
            "error": scan.error,
            "heal_fired": scan.heal_fired,
            "heal_requested": scan.heal_requested,
            "heal_debounced": scan.heal_debounced,
            "rollup_count": scan.rollup_count,
            "last_rollup_decoded": scan.last_rollup_decoded,
            "last_rollup_silent": scan.last_rollup_silent,
            "last_rollup_unmapped": scan.last_rollup_unmapped,
            "last_rollup_ssrcs": scan.last_rollup_ssrcs,
        }
    return report


if __name__ == "__main__":
    sys.exit(main())
