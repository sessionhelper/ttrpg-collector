test-soak
=========

Nightly soak + scenario-matrix harness for `chronicle-bot`.

Two entry points:

- `soak.py` — long single-scenario soak (multi-hour). Validates stability.
- `matrix.py` — short-scenario matrix (A3/C2/D1 from `docs/e2e-voice-test-plan.md`). Validates specific voice-state edge cases.

Both scrape the bot container's docker logs at the end of each run to
detect DAVE distress signals and silent-packet rollups (see below).

soak.py
-------

Drives the dev feeder fleet through an N-hour session:

1. POSTs `/record` to the bot harness.
2. Joins each feeder one-at-a-time (5s stagger — MLS constraint).
3. Loops for `DURATION_SECS`, re-firing `/play` on idle feeders every
   ~2s and sampling bot RSS + FD count every `PROBE_INTERVAL_SECS`.
4. POSTs `/stop`, calls `/leave` on all feeders, scrapes docker logs,
   prints a JSON report.

Failure signals (exit 1 if any fire):

- RSS growth > `RSS_GROWTH_LIMIT_MB_PER_HR` (default 100)
- FD growth > `FD_GROWTH_LIMIT_PER_HR` (default 50)
- No feeder playing for > `STALL_LIMIT_SECS` (default 30)
- `/stop` errors
- `dave_heal_firing` count > `DAVE_HEAL_FIRED_LIMIT` (default 0)
- `dave_heal_requested` count > `DAVE_HEAL_REQUEST_LIMIT` (default 0)
- final `voice_rx_rollup` silent-packet count > `SILENT_PACKETS_LIMIT` (default 0)

Usage:

```
GUILD_ID=... CHANNEL_ID=... \
  BOT_CONTAINER=ovp-collector-1 \
  DURATION_SECS=10800 \
  python3 soak.py
```

Either `BOT_PID` (host PID) or `BOT_CONTAINER` (docker container name)
is required for resource probes. `BOT_CONTAINER` is also what the
log-scan uses for DAVE/silent-rollup detection; without it the
`log_scan.scanned` field in the report is `false` and those checks
are skipped.

matrix.py
---------

Runs A3, C2, D1 back-to-back as short (default 3-minute) scenarios,
each with its own `/record` -> consent -> play -> `/stop` cycle.
Auto-consents every feeder participant via the data-api (required
under `require_all_consent=true`).

- **A3**: 4 feeders join staggered, everyone plays to end, stop.
- **C2**: A3, then one feeder leaves at the halfway mark; session
  should keep recording the remaining three.
- **D1**: A3, then one feeder leaves + rejoins before stop; track
  should resume in the same per-speaker stream.

Usage:

```
GUILD_ID=... CHANNEL_ID=... \
  BOT_CONTAINER=ovp-collector-1 \
  SHARED_SECRET=$(ssh root@dev 'docker exec ovp-data-api-1 printenv SHARED_SECRET') \
  python3 matrix.py
```

Optional env:

- `MATRIX` — comma-separated scenario ids, default `A3,C2,D1`
- `SCENARIO_DURATION_SECS` — per scenario, default 180
- `DATA_API_URL` — default `http://127.0.0.1:8001`

Exit 0 only if every scenario passes its log-scan assertions.

Log scan
--------

Both runners post-process `docker logs --since <start>` for the
`BOT_CONTAINER`, stripping tracing-subscriber ANSI escapes and
parsing key=value pairs. Detected:

- `dave_heal_firing` — the actual leave/rejoin heal fired
- `dave_heal_requested` — receiver-side consecutive-silent trigger (upstream of the fire)
- `dave_heal_debounced` — trigger suppressed by the debounce
- last `voice_rx_rollup`'s absolute counters (decoded, silent, unmapped, ssrcs_seen)

These are monotonic session-scoped counters (reset per songbird call),
so the final rollup's numbers describe the entire soak window.
