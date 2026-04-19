#!/usr/bin/env python3
"""
Short-scenario matrix runner for chronicle-bot.

Runs the A3/C2/D1 scenarios from docs/e2e-voice-test-plan.md back-to-back
with short durations, one /record -> consent -> play -> stop cycle per
scenario. Writes a combined JSON report at the end.

Complements soak.py (long-session single scenario); does NOT replace it.

Scenarios:
  A3 — 4 feeders join staggered, all play, stop (multi-user happy path)
  C2 — A3, then one feeder leaves mid-recording; session keeps going
  D1 — A3, then one feeder leaves + rejoins before stop

Each scenario auto-consents every participant via data-api (required by
`require_all_consent=true` on dev; otherwise chunks stay Pending).

Required env:
  GUILD_ID, CHANNEL_ID      — Discord ids
  SHARED_SECRET             — data-api service-auth secret
  BOT_CONTAINER             — docker container name for log-scrape

Optional env:
  COLLECTOR_URL             — default http://127.0.0.1:8010
  DATA_API_URL              — default http://127.0.0.1:8001
  FEEDER_URLS               — comma-separated, default feeder ports 8003-8006
  SCENARIO_DURATION_SECS    — default 180 (per scenario)
  JOIN_DELAY_SECS           — default 5 (MLS stagger)
  MATRIX                    — comma-separated scenario ids, default A3,C2,D1
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.request
from dataclasses import asdict
from datetime import datetime, timezone

# Reuse primitives from soak.py. Importing pulls in its module-level env
# reads, so the caller must set GUILD_ID/CHANNEL_ID before invoking.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import soak  # noqa: E402


DATA_API_URL = os.environ.get("DATA_API_URL", "http://127.0.0.1:8001")
SHARED_SECRET = os.environ["SHARED_SECRET"]
SCENARIO_DURATION_SECS = int(os.environ.get("SCENARIO_DURATION_SECS", "180"))
MATRIX = os.environ.get("MATRIX", "A3,C2,D1").split(",")


def _auth_headers(token: str) -> dict:
    return {
        "authorization": f"Bearer {token}",
        "content-type": "application/json",
    }


def mint_service_token() -> str:
    """Exchange SHARED_SECRET for a data-api service session token."""
    req = urllib.request.Request(
        f"{DATA_API_URL}/internal/auth",
        data=json.dumps(
            {"shared_secret": SHARED_SECRET, "service_name": "matrix-harness"}
        ).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())["session_token"]


def fetch_participants(token: str, session_id: str) -> list[dict]:
    req = urllib.request.Request(
        f"{DATA_API_URL}/internal/sessions/{session_id}/participants",
        headers={"authorization": f"Bearer {token}"},
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())


CONSENT_POLL_TIMEOUT_SECS = int(os.environ.get("CONSENT_POLL_TIMEOUT_SECS", "120"))
CONSENT_POLL_INTERVAL_SECS = int(os.environ.get("CONSENT_POLL_INTERVAL_SECS", "3"))


def poll_for_participants(
    token: str, session_id: str, min_expected: int
) -> list[dict]:
    """Poll the data-api until at least `min_expected` participants
    are registered, or the timeout expires. Returns whatever the last
    fetch saw.

    The bot defers participant registration until audio frames pass the
    stabilization gate, which is typically ~15-30s after /record. A
    fixed sleep would either be too short or waste soak time.
    """
    deadline = time.time() + CONSENT_POLL_TIMEOUT_SECS
    participants: list[dict] = []
    while time.time() < deadline:
        try:
            participants = fetch_participants(token, session_id)
        except Exception as e:
            print(f"  participant poll error: {e}", file=sys.stderr)
            participants = []
        if len(participants) >= min_expected:
            return participants
        time.sleep(CONSENT_POLL_INTERVAL_SECS)
    return participants


def consent_all(token: str, session_id: str, min_expected: int = 0) -> int:
    """Mint a consent token per participant and PATCH consent=full.

    `min_expected` drives the pre-fetch poll: if set, we wait until at
    least that many participants register (up to CONSENT_POLL_TIMEOUT_SECS)
    before issuing consent PATCHes. Default 0 polls once for whatever's
    there — useful for re-consent after a mid-session rejoin.

    Returns the number of participants consented.
    """
    consented = 0
    if min_expected > 0:
        participants = poll_for_participants(token, session_id, min_expected)
    else:
        participants = fetch_participants(token, session_id)
    for p in participants:
        try:
            create_req = urllib.request.Request(
                f"{DATA_API_URL}/internal/consent-tokens",
                data=json.dumps(
                    {
                        "session_id": session_id,
                        "participant_id": p["id"],
                        "pseudo_id": p["pseudo_id"],
                    }
                ).encode(),
                headers=_auth_headers(token),
                method="POST",
            )
            with urllib.request.urlopen(create_req, timeout=10) as r:
                consent_token = json.loads(r.read())["token"]

            patch_req = urllib.request.Request(
                f"{DATA_API_URL}/public/consent/{consent_token}",
                data=json.dumps({"consent_scope": "full"}).encode(),
                headers={"content-type": "application/json"},
                method="PATCH",
            )
            with urllib.request.urlopen(patch_req, timeout=10) as r:
                r.read()
            consented += 1
        except Exception as e:
            print(
                f"  consent failed for participant {p.get('pseudo_id')}: {e}",
                file=sys.stderr,
            )
    return consented


def run_scenario(scenario_id: str) -> dict:
    """Execute one scenario end-to-end. Returns a per-scenario report dict.

    Mints a fresh service session token per scenario — the data-api
    expires them after ~90s of no activity, which is shorter than any
    useful scenario duration.
    """
    print(f"\n=== scenario {scenario_id} start ===")
    started_ms = soak.now_ms()
    failures: list[dict] = []

    try:
        svc_token = mint_service_token()
    except Exception as e:
        failures.append(
            {"ts_ms": soak.now_ms(), "kind": "auth_failed", "detail": str(e)}
        )
        print(f"FAIL[auth_failed] {e}", file=sys.stderr)
        return _build_scenario_report(
            scenario_id, started_ms, None, failures, None, 0
        )

    def fail(kind: str, detail: str) -> None:
        failures.append({"ts_ms": soak.now_ms(), "kind": kind, "detail": detail})
        print(f"FAIL[{kind}] {detail}", file=sys.stderr)

    # 1. /record
    try:
        rec = soak.post(
            f"{soak.COLLECTOR_URL}/record",
            {"guild_id": soak.GUILD_ID, "channel_id": soak.CHANNEL_ID},
        )
        session_id = rec.get("session_id")
        print(f"  session_id={session_id}")
    except Exception as e:
        fail("record_failed", str(e))
        return _build_scenario_report(
            scenario_id, started_ms, None, failures, None, 0
        )

    # 2. Feeders join staggered
    try:
        soak.join_all_staggered()
    except Exception as e:
        fail("join_failed", str(e))

    # 3. Fire play FIRST so voice frames stream into the stabilization gate.
    #    Participants aren't registered in the data-api until the gate opens,
    #    which needs ~15-30s of continuous frames. Without playback here the
    #    poll below will time out with no participants.
    soak.fire_play_on_idle()

    # 4. Poll for participant rows (registered post-stabilization) and
    #    consent each. Under require_all_consent=true the chunk-upload
    #    gate stays closed until every participant consents, so this step
    #    is what lets the rest of the scenario actually produce chunks.
    expected = len(soak.FEEDER_URLS)
    try:
        consented = consent_all(svc_token, session_id, min_expected=expected)
        print(f"  consented {consented}/{expected} participants")
        if consented == 0:
            fail("consent_zero", "no participants consented; chunks will stay Pending")
        elif consented < expected:
            fail(
                "consent_partial",
                f"only {consented}/{expected} participants consented",
            )
    except Exception as e:
        fail("consent_failed", str(e))
        consented = 0

    # 5. Scenario-specific action, interleaved with the wait period
    half = SCENARIO_DURATION_SECS // 2

    if scenario_id == "A3":
        # Baseline: everyone stays, session runs its course.
        time.sleep(SCENARIO_DURATION_SECS)

    elif scenario_id == "C2":
        # Accepted leaves mid-recording; session should continue recording
        # the others. Pick the second feeder so we're not the initiator.
        time.sleep(half)
        victim = soak.FEEDER_URLS[1] if len(soak.FEEDER_URLS) >= 2 else None
        if victim:
            try:
                soak.post(f"{victim}/leave")
                print(f"  C2: {victim} left mid-recording")
            except Exception as e:
                fail("c2_leave_failed", str(e))
        # Let the remaining feeders keep playing for the second half.
        soak.fire_play_on_idle()
        time.sleep(SCENARIO_DURATION_SECS - half)

    elif scenario_id == "D1":
        # Accepted leaves + rejoins before stop.
        time.sleep(half)
        target = soak.FEEDER_URLS[1] if len(soak.FEEDER_URLS) >= 2 else None
        if target:
            try:
                soak.post(f"{target}/leave")
                print(f"  D1: {target} left, will rejoin in 10s")
                time.sleep(10)
                soak.post(
                    f"{target}/join",
                    {
                        "guild_id": soak.GUILD_ID,
                        "channel_id": soak.CHANNEL_ID,
                    },
                )
                soak.post(f"{target}/play")
                print(f"  D1: {target} rejoined")
            except Exception as e:
                fail("d1_rejoin_failed", str(e))
        # Re-consent any freshly-registered participant row from the rejoin.
        # Re-mint the service token first — the initial one is often stale
        # by this point in D1 (runs late in the scenario).
        try:
            consent_all(mint_service_token(), session_id)
        except Exception as e:
            fail("d1_reconsent_failed", str(e))
        soak.fire_play_on_idle()
        time.sleep(SCENARIO_DURATION_SECS - half - 10)

    else:
        fail("unknown_scenario", scenario_id)

    # 6. Teardown
    print(f"  scenario {scenario_id} teardown")
    soak.leave_all()
    try:
        soak.post(f"{soak.COLLECTOR_URL}/stop", {"guild_id": soak.GUILD_ID})
    except Exception as e:
        fail("stop_failed", str(e))

    # 7. Log scan for this scenario's window. The silent-rollup count
    #    (how many rollups during this window reported silent>0) is the
    #    per-window signal; absolute counters are cumulative and not
    #    session-scoped, so we can't easily delta them.
    time.sleep(6)
    scan = soak.scan_bot_logs(started_ms)
    if scan.scanned:
        if scan.heal_fired > 0:
            fail(
                "dave_heal_fired",
                f"{scan.heal_fired} heal(s) during {scenario_id}",
            )
        if scan.silent_rollup_count > 0:
            fail(
                "silent_packets",
                f"{scan.silent_rollup_count} rollup(s) with silent>0 "
                f"in {scenario_id} (peak={scan.silent_max})",
            )

    return _build_scenario_report(
        scenario_id, started_ms, session_id, failures, scan, consented
    )


def _build_scenario_report(
    scenario_id: str,
    started_ms: int,
    session_id: str | None,
    failures: list[dict],
    scan: soak.LogScanResult | None,
    consented: int,
) -> dict:
    report = {
        "scenario": scenario_id,
        "session_id": session_id,
        "started_at": datetime.fromtimestamp(
            started_ms / 1000, tz=timezone.utc
        ).isoformat(),
        "duration_secs": (soak.now_ms() - started_ms) / 1000,
        "consented_participants": consented,
        "failures": failures,
        "ok": not failures,
    }
    if scan is not None:
        report["log_scan"] = asdict(scan)
    return report


def main() -> int:
    print(f"=== matrix start: {MATRIX} ===")

    results = []
    for sid in MATRIX:
        sid = sid.strip()
        if not sid:
            continue
        try:
            results.append(run_scenario(sid))
        except Exception as e:
            results.append(
                {"scenario": sid, "ok": False, "failures": [{"kind": "crash", "detail": str(e)}]}
            )
        # Cooldown between scenarios so a trailing voice-state event from
        # scenario N doesn't get attributed to scenario N+1.
        time.sleep(10)

    summary = {
        "completed_at": datetime.now(tz=timezone.utc).isoformat(),
        "scenarios_run": len(results),
        "scenarios_ok": sum(1 for r in results if r.get("ok")),
        "results": results,
        "ok": all(r.get("ok") for r in results),
    }
    print("\n=== matrix summary ===")
    print(json.dumps(summary, indent=2))
    return 0 if summary["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
