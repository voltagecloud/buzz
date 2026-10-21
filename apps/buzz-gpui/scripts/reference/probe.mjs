// Usage: node probe.mjs <url> <out.png> [light|dark] [waitMs]
// Loads a page in headless Chromium and saves a screenshot + prints visible text.
import { createRequire } from "node:module";
const BUZZ_APP = process.env.BUZZ_APP ?? "/home/ben/projects/buzz-app";
const require = createRequire(`${BUZZ_APP}/package.json`);
const { chromium } = require("@playwright/test");
const [url, out, scheme = "light", wait = "2500"] = process.argv.slice(2);
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM });
const page = await browser.newPage({ viewport: { width: 1200, height: 800 }, colorScheme: scheme, locale: "en-US", timezoneId: "UTC" });
page.on("console", (m) => { if (m.type() === "error") console.error("[console]", m.text().slice(0, 300)); });
page.on("pageerror", (e) => console.error("[pageerror]", String(e).slice(0, 300)));
await page.goto(url, { waitUntil: "networkidle" });
await page.waitForTimeout(Number(wait));
await page.screenshot({ path: out });
console.log((await page.innerText("body")).slice(0, 3000));
await browser.close();
