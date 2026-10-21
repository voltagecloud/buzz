// Capture reference screenshots of buzz-app's standalone fixture pages:
// design-system viewer, message gallery, composer gallery, dialog atlas scenes,
// the agent-control fixture and the no-community shell.
//
// Prereqs (see capture-all.sh, which does all of this):
//   web dev server    (pnpm exec vite --port 1531)                         -> WEB
//   design dev server (pnpm exec vite --config vite.design.config.ts ...)  -> DESIGN
//   CHROMIUM=/path/to/chromium
//
// Usage: node capture-fixtures.mjs [filter-regex]
import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";

const BUZZ_APP = process.env.BUZZ_APP ?? "/home/ben/projects/buzz-app";
const require = createRequire(`${BUZZ_APP}/package.json`);
const { chromium } = require("@playwright/test");

const WEB = process.env.WEB ?? "http://localhost:1531";
const DESIGN = process.env.DESIGN ?? "http://localhost:1542";
// Production build of dialog-gallery / agent-control / product-ui fixtures
// (vite.fixtures.config.mjs). Dev-mode CSS layer order drops some paddings.
const FIXTURES = process.env.FIXTURES ?? "http://127.0.0.1:1561";
const OUT =
  process.env.SHOT_DIR ?? "/home/ben/projects/buzz-gpui/docs/port/screenshots";
const FULL_MAX = Number(process.env.FULL_MAX ?? 6000);
const only = process.argv[2] ? new RegExp(process.argv[2]) : null;
mkdirSync(OUT, { recursive: true });

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM });
const written = [];

async function newPage(scheme) {
  const page = await browser.newPage({
    viewport: { width: 1200, height: 800 },
    colorScheme: scheme,
    locale: "en-US",
    timezoneId: "UTC",
  });
  page.on("pageerror", (e) => console.error("  [pageerror]", String(e).slice(0, 200)));
  return page;
}

async function setMode(page, scheme) {
  await page.evaluate((s) => {
    document.documentElement.dataset.colorMode = s;
    document.documentElement.classList.toggle("dark", s === "dark");
  }, scheme);
}

async function settle(page, ms = 700) {
  await page.waitForLoadState("networkidle").catch(() => {});
  await page.evaluate(() => document.activeElement?.blur?.());
  await page.mouse.move(1199, 799);
  await page.waitForTimeout(ms);
}

async function shot(page, name, scheme, { full = false } = {}) {
  if (only && !only.test(name)) return;
  const path = `${OUT}/${name}-${scheme}.png`;
  if (full) {
    const height = await page.evaluate(() =>
      Math.max(document.documentElement.scrollHeight, document.body.scrollHeight),
    );
    await page.screenshot({
      path,
      fullPage: true,
      clip: { x: 0, y: 0, width: 1200, height: Math.min(height, FULL_MAX) },
    });
  } else await page.screenshot({ path });
  written.push(path);
  console.log("  wrote", path);
}

const slug = (s) =>
  s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");

async function shellEmpty(scheme) {
  const page = await newPage(scheme);
  await page.goto(`${WEB}/`, { waitUntil: "networkidle" });
  await settle(page, 2500);
  await shot(page, "app-shell-no-community", scheme);
  await page.close();
}

async function messageGallery(scheme) {
  const page = await newPage(scheme);
  const url = `${WEB}/tests/fixtures/message-gallery.html?theme=${scheme}`;
  await page.goto(url, { waitUntil: "networkidle" });
  await settle(page, 1500);
  await shot(page, "gallery-messages-all", scheme);
  await shot(page, "gallery-messages-all-full", scheme, { full: true });
  const skip = new Set(["All messages", "Narrow preview", "Reset examples"]);
  const labels = (await page.getByRole("button").allInnerTexts())
    .map((t) => t.trim())
    .filter(Boolean);
  const sections = [];
  for (const label of labels) {
    if (skip.has(label) || sections.includes(label)) continue;
    sections.push(label);
    if (sections.length >= 6) break;
  }
  for (const label of sections) {
    try {
      await page.goto(url, { waitUntil: "networkidle" });
      await page.getByRole("button", { name: label, exact: true }).first().click();
      await settle(page, 900);
      await page.evaluate(() => window.scrollTo(0, 0));
      await shot(page, `gallery-messages-${slug(label)}`, scheme, { full: true });
    } catch (e) {
      console.log(`  gallery section ${label} failed`, String(e).slice(0, 160));
    }
  }
  await page.goto(url, { waitUntil: "networkidle" });
  await page.getByRole("button", { name: "Narrow preview", exact: true }).click();
  await settle(page, 900);
  await shot(page, "gallery-messages-narrow", scheme);
  await page.close();
}

async function composerGallery(scheme) {
  const page = await newPage(scheme);
  await page.goto(`${FIXTURES}/tests/fixtures/product-ui.html`, { waitUntil: "networkidle" });
  await setMode(page, scheme);
  await settle(page, 1200);
  await shot(page, "gallery-composer", scheme, { full: true });
  // Mention autocomplete in the first composer.
  const box = page.getByRole("textbox").first();
  await box.click();
  await page.keyboard.type("@Al");
  await page.waitForTimeout(800);
  await page.mouse.move(1199, 799);
  await shot(page, "gallery-composer-mention", scheme);
  await page.close();
}

const DIALOG_SCENES = [
  "search", "join", "create", "edit", "channel", "leave", "delete",
  "enable", "dismiss", "secret", "webhook", "attachment", "review",
];
async function dialogScenes(scheme) {
  const page = await newPage(scheme);
  await page.goto(`${FIXTURES}/tests/fixtures/dialog-gallery.html`, { waitUntil: "networkidle" });
  await setMode(page, scheme);
  await settle(page, 800);
  await shot(page, "dialog-atlas", scheme);
  for (const id of DIALOG_SCENES) {
    if (only && !only.test(`dialog-${id}`)) continue;
    await page.goto(
      `${FIXTURES}/tests/fixtures/dialog-gallery.html?scene=${id}&theme=${scheme}&scale=100&variant=default`,
      { waitUntil: "networkidle" },
    );
    await settle(page, 1200);
    await shot(page, `dialog-${id}`, scheme);
  }
  await page.close();
}

async function agentControl(scheme) {
  const page = await newPage(scheme);
  await page.goto(`${FIXTURES}/tests/fixtures/agent-control.html`, { waitUntil: "networkidle" });
  await setMode(page, scheme);
  await settle(page, 1200);
  await shot(page, "fixture-agents-page", scheme, { full: true });
  try {
    await page.getByRole("button", { name: "Add agent", exact: true }).click();
    await settle(page, 1000);
    await shot(page, "fixture-agents-add-dialog", scheme);
    await page.keyboard.press("Escape");
    await page.waitForTimeout(400);
  } catch (e) {
    console.log("  add agent failed", String(e).slice(0, 160));
  }
  try {
    await page.getByRole("button", { name: /^Manage / }).first().click();
    await settle(page, 1000);
    await shot(page, "fixture-agents-manage-dialog", scheme);
    await page.keyboard.press("Escape");
  } catch (e) {
    console.log("  manage agent failed", String(e).slice(0, 160));
  }
  await page.close();
}

async function designViewer(scheme) {
  const page = await newPage(scheme);
  await page.addInitScript((s) => {
    localStorage.setItem("buzz-design-viewer-color-scheme", s);
  }, scheme);
  await page.goto(`${DESIGN}/tests/fixtures/design-system.html`, { waitUntil: "load", timeout: 90_000 });
  await settle(page, 1200);
  const hrefs = await page.$$eval('a[href*="#/design/"]', (as) =>
    [...new Set(as.map((a) => a.getAttribute("href")))],
  );
  const skip = /design-guide|agents-guide|maintaining|base-ui/;
  for (const href of hrefs) {
    if (skip.test(href)) continue;
    const route = href.split("#/design/")[1].replace(/\/$/, "") || "overview";
    const name = `design-${slug(route.replace(/^components\//, ""))}`;
    if (only && !only.test(name)) continue;
    try {
      await page.goto(`${DESIGN}/tests/fixtures/design-system.html${href.slice(href.indexOf("#"))}`, {
        waitUntil: "load",
        timeout: 90_000,
      });
      // Hash routes on the same document: force a reload so each page starts clean.
      await page.reload({ waitUntil: "load", timeout: 90_000 });
      await settle(page, 1200);
      await shot(page, name, scheme, { full: true });
    } catch (e) {
      console.log(`  design page ${name} failed`, String(e).slice(0, 160));
    }
  }
  await page.close();
}

const jobs = { shellEmpty, messageGallery, composerGallery, dialogScenes, agentControl, designViewer };
const pick = process.env.JOBS ? process.env.JOBS.split(",") : Object.keys(jobs);
for (const scheme of ["light", "dark"])
  for (const job of pick) {
    console.log(`${job} (${scheme})`);
    try {
      await jobs[job](scheme);
    } catch (e) {
      console.error(`  ${job} failed:`, String(e).slice(0, 300));
    }
  }
await browser.close();
console.log(`${written.length} screenshots written`);
