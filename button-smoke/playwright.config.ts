import { defineConfig } from "@playwright/test";

const STORAGE = "./.storageState.json";

export default defineConfig({
  testDir: "./tests",
  timeout: 60_000,
  expect: { timeout: 15_000 },
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  use: {
    baseURL: "https://discord.com",
    // Discord flags web-scraping user-agents; stick with a normal one.
    userAgent:
      "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    // Real browser features — Discord is an SPA and needs them.
    javaScriptEnabled: true,
    actionTimeout: 15_000,
    // Slow the automated clicks to something that looks human-ish;
    // Discord flags rapid interaction patterns.
    launchOptions: { slowMo: 100 },
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
  },
  projects: [
    {
      // `npm run login` once — captures your Discord web session into
      // .storageState.json so subsequent runs can skip the login flow.
      // Hand-walk the 2FA / captcha in the popped browser; Playwright
      // watches for a successful channel load and saves state.
      name: "login",
      testMatch: /auth\.setup\.ts/,
    },
    {
      name: "smoke",
      testMatch: /.*\.spec\.ts/,
      use: { storageState: STORAGE },
      dependencies: [],
    },
  ],
});
