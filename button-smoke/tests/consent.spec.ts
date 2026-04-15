import { test, expect, Page } from "@playwright/test";

/**
 * Runs the Discord-client side of the /record + consent-button flow.
 *
 * Prereqs:
 *  - `.storageState.json` exists (run `npm run login` once).
 *  - Env vars:
 *      DISCORD_GUILD_ID    — the test guild
 *      DISCORD_CHANNEL_ID  — a TEXT channel in that guild (for /record invocation)
 *      DISCORD_VOICE_ID    — a VOICE channel in the same guild (test user must be in it)
 *      DATA_API_URL        — tunnelled to data-api (default http://127.0.0.1:18001)
 *      DATA_API_SECRET     — shared secret (mirrors the bot)
 */

const GUILD = process.env.DISCORD_GUILD_ID!;
const CHANNEL = process.env.DISCORD_CHANNEL_ID!;
const DATA_API_URL = process.env.DATA_API_URL || "http://127.0.0.1:18001";
const DATA_API_SECRET = process.env.DATA_API_SECRET!;

test.beforeAll(() => {
  if (!GUILD || !CHANNEL || !DATA_API_SECRET) {
    throw new Error(
      "Missing env: DISCORD_GUILD_ID, DISCORD_CHANNEL_ID, DATA_API_SECRET are required.",
    );
  }
});

test("`/record` → click Accept → participant consent_scope flips to 'full'", async ({ page }) => {
  await page.goto(`/channels/${GUILD}/${CHANNEL}`);

  // Wait for the message input to appear — this is the slowest thing.
  const input = page.getByRole("textbox", { name: /message/i });
  await expect(input).toBeVisible({ timeout: 30_000 });

  await runSlashCommand(page, input, "record");

  // Bot's /record bails with "You need to be in a voice channel." if the
  // test account isn't joined. Catch that early with a clearer message
  // than a 15-second toBeVisible timeout on the Accept button.
  const voiceWarning = page.locator(
    'article:has-text("You need to be in a voice channel"), li:has-text("You need to be in a voice channel")',
  );
  if (await voiceWarning.first().isVisible({ timeout: 5_000 }).catch(() => false)) {
    throw new Error(
      "Test account isn't in a voice channel. Join the dev guild's voice channel " +
        "with the test user (manually, once) before running this suite. See README.",
    );
  }

  // The bot replies with an ephemeral message containing Accept/Decline.
  // Ephemeral messages in Discord web have an "Only you can see this"
  // footer hint we can anchor to.
  const ephemeralBlock = page.locator(
    'article:has-text("Only you can see this"), li:has-text("Only you can see this")',
  );
  await expect(ephemeralBlock.first()).toBeVisible({ timeout: 20_000 });

  // Click the Accept button.
  const acceptBtn = ephemeralBlock.first().getByRole("button", { name: /accept|full/i });
  await expect(acceptBtn).toBeVisible();
  await acceptBtn.click();

  // Ask the data-api what it thinks. We don't need to parse Discord's
  // UI state — the bot's consent flush will have landed by the time
  // this call returns.
  const updated = await waitForParticipantConsent(page);
  expect(updated).toBe("full");
});

async function runSlashCommand(page: Page, input: ReturnType<Page["locator"]>, name: string) {
  await input.click();
  await input.fill(`/${name}`);

  // Wait for Discord's command autocomplete popup, then pick the first
  // matching entry. The popup has the command name in a known row.
  const autocomplete = page.locator(`[role="listbox"] [role="option"]:has-text("/${name}")`);
  await expect(autocomplete.first()).toBeVisible({ timeout: 5_000 });
  await autocomplete.first().click();

  // Some slash commands require a follow-up Enter to submit (no args).
  await page.keyboard.press("Enter");
}

async function waitForParticipantConsent(page: Page): Promise<string | null> {
  // Give the bot + data-api a moment to process the click.
  await page.waitForTimeout(2_000);

  const token = await authDataApi();
  // Discover the most-recent session in the target guild, then read its
  // participants and return the current test-user's consent_scope.
  const sessions = await fetchJson(`${DATA_API_URL}/internal/sessions?limit=5`, token);
  const latest = sessions.find((s: any) => String(s.guild_id) === GUILD);
  if (!latest) return null;

  const parts = await fetchJson(
    `${DATA_API_URL}/internal/sessions/${latest.id}/participants`,
    token,
  );
  // We can't trivially know the test user's pseudo_id from here; assume
  // a single-human session (MIN_PARTICIPANTS=1 on dev) and return the
  // first participant's scope.
  return parts[0]?.consent_scope ?? null;
}

async function authDataApi(): Promise<string> {
  const r = await fetch(`${DATA_API_URL}/internal/auth`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      shared_secret: DATA_API_SECRET,
      service_name: "button-smoke",
    }),
  });
  if (!r.ok) throw new Error(`auth failed: ${r.status}`);
  const j = (await r.json()) as { session_token: string };
  return j.session_token;
}

async function fetchJson(url: string, token: string): Promise<any> {
  const r = await fetch(url, { headers: { Authorization: `Bearer ${token}` } });
  if (!r.ok) throw new Error(`GET ${url} → ${r.status}`);
  return r.json();
}
