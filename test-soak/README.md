test-soak
=========

Long-session soak harness for `chronicle-bot` (roadmap #1).

Status: **scaffolded**, end-to-end run not yet validated.

What it does
------------

`soak.py` drives the dev feeder fleet through an N-hour session:

1. POSTs `/record` to the bot harness.
2. Joins each feeder one-at-a-time (5s stagger) — same MLS-stagger
   reasoning as `chronicle-bot/scripts/e2e-run.sh`.
3. Loops for `DURATION_SECS`, re-firing `/play` on idle feeders every
   ~2s and sampling bot RSS + FD count every `PROBE_INTERVAL_SECS`.
4. POSTs `/stop`, calls `/leave` on all feeders, prints a JSON report.

Failure signals (exit 1):

- RSS growth > `RSS_GROWTH_LIMIT_MB_PER_HR` (default 100)
- FD growth > `FD_GROWTH_LIMIT_PER_HR` (default 50)
- No feeder playing for > `STALL_LIMIT_SECS` (default 30)
- `/stop` errors

Not yet wired in
----------------

- DAVE-heal counter scrape (need a metrics endpoint or log scraper)
- Rollup `silent > 0` detection (log scrape)
- Final session row check via data-api (need session token threading)
- `verify-timeline.py` invocation post-stop

These will land as follow-ups; the current shape gets the orchestration
honest first.

Usage
-----

```
GUILD_ID=... CHANNEL_ID=... \
  BOT_CONTAINER=chronicle-bot-dev \
  DURATION_SECS=10800 \
  python3 soak.py
```

Either `BOT_PID` (host PID) or `BOT_CONTAINER` (docker container name)
is required for resource probes; without either, the harness still runs
the session but the JSON report's `rss_*` / `fd_*` fields stay null.
