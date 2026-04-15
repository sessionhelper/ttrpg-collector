import { test, expect } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";

/**
 * One-time login flow.
 *
 * Run `npm run login` (which invokes this with --headed). A real Chromium
 * window pops. Log in with a test Discord account — including any MFA
 * challenge — and let Playwright detect a channel load, at which point
 * it dumps storage state to `.storageState.json` and exits.
 *
 * Rerun this whenever the session expires (Discord keeps them alive for
 * weeks but nukes them on password change or "Log out all devices").
 */
test("capture Discord session", async ({ page, context }) => {
  test.setTimeout(600_000); // 10 min — manual login window

  await page.goto("/login");

  // Wait for the URL to hit `/channels/@me` or a guild channel, which
  // indicates auth completed. That's more reliable than scraping DOM.
  await page.waitForURL(/discord\.com\/channels\//, {
    timeout: 580_000,
    waitUntil: "domcontentloaded",
  });

  // Give a second for client-side state to hydrate before saving.
  await page.waitForTimeout(2_000);

  const out = path.resolve("./.storageState.json");
  await context.storageState({ path: out });
  console.log(`storage state saved → ${out}`);

  // Sanity check the saved file contains a discord.com cookie.
  const raw = fs.readFileSync(out, "utf8");
  expect(raw).toContain("discord.com");
});
