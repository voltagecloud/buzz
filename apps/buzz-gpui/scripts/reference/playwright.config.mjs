// Playwright config for capturing buzz-app reference screenshots of the real
// built app against buzz-app's own synthetic relay fixture (tests/browser/fixture.mjs).
// Run via scripts/reference/capture-app.sh.
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const executablePath = process.env.CHROMIUM || undefined;

export default {
  testDir: here,
  testMatch: "app-shell.spec.mjs",
  outputDir: "/tmp/buzz-reference-results",
  workers: 1,
  retries: 0,
  timeout: 240_000,
  expect: { timeout: 20_000 },
  reporter: [["list"]],
  use: {
    headless: true,
    viewport: { width: 1200, height: 800 },
    locale: "en-US",
    timezoneId: "UTC",
    serviceWorkers: "block",
    browserName: "chromium",
    launchOptions: { executablePath },
  },
  projects: [
    { name: "light", use: { colorScheme: "light" } },
    { name: "dark", use: { colorScheme: "dark" } },
  ],
};
