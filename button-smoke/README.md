# chronicle-bot button-smoke

Playwright-driven smoke of the Discord `/record` + consent-button flow.
Validates the interaction/ack/webhook path end-to-end without a human in
the loop. Complements `sessionhelper-hub/infra/dev-compose.yml`'s feeder +
harness capture loop, which covers voice end-to-end but bypasses Discord's
slash-command UI.

## Scope

- Launches headless Chromium, loads a persisted Discord web session.
- Types `/record` in a configured text channel.
- Clicks the **Accept** button on the ephemeral consent embed.
- Asserts the participant's `consent_scope` flipped to `full` by querying
  `chronicle-data-api` over an SSH tunnel.

It does **not** test voice. That's the feeder fleet's job.

It does **not** simulate a Discord user account beyond clicking buttons in
a real Chromium. Don't point it at a personal account — create a dedicated
test user.

## One-time setup

1. **Install**
   ```
   cd chronicle-bot/button-smoke
   npm install
   npm run install-browsers
   ```

2. **Capture a Discord session**
   ```
   npm run login
   ```
   A real Chromium window pops. Log in with your test account. The script
   watches for a successful `/channels/...` navigation and writes
   `.storageState.json` (gitignored). Takes about 30 seconds once you've
   typed your password + any MFA code.

3. **Configure env**
   ```
   cp env.example .env
   ```
   Fill in `DISCORD_CHANNEL_ID` (any text channel in the test guild) and
   `DATA_API_SECRET` (mirror the bot's `SHARED_SECRET` — pull with
   `ssh root@<dev-vps> 'grep ^SHARED_SECRET /opt/ovp/.env | cut -d= -f2'`).

4. **Tunnel data-api**
   Run in another shell:
   ```
   ssh -fN -L 18001:127.0.0.1:8001 root@46.225.94.152
   ```
   (Or set `DATA_API_URL` to whatever you tunnel on.)

## Prerequisites for the test account

The test asserts the consent flow on the response to `/record`. The bot
only posts the consent embed if the invoker is **already in a voice
channel** of the test guild. Two ways to satisfy this:

- Pop a real Discord client open with the test user and join any voice
  channel in the guild. Stay in. Run the suite.
- (Future) auto-join via Playwright by clicking a voice channel name
  in the sidebar — works for muted "presence join" without needing a
  real WebRTC stream. Not implemented yet.

If the test account isn't in voice when the suite runs, you'll get a
fast, explicit failure pointing here.

## Running

Human-visible (useful when debugging selector breakage):
```
npm run smoke:headed
```

Headless (CI / local watch loop):
```
npm run smoke
```

## When it breaks

Discord web is a minified SPA; its DOM is not a public contract.
Selectors in `tests/consent.spec.ts` are written against stable anchors
(`role="textbox"`, `role="button"`, the "Only you can see this" ephemeral
footer text) but will still drift over time. When a selector fails:

1. Re-run with `--headed` to see what Discord actually renders.
2. Use `page.pause()` in-test to poke interactively.
3. Prefer accessible-name-based selectors (`getByRole`, `getByLabel`)
   over class/structure.

## Future work

- A second spec for the **Decline** path (asserts session is cancelled).
- A re-auth spec that pins down the "ephemeral is stale on subsequent
  clicks" class of regressions (the bug this suite was born to catch).
- Wire into CI on label trigger, with the storage state stored as a
  GitHub encrypted secret.
