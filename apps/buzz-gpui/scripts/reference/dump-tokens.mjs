// Dump every CSS custom property defined on :root of the running buzz-app web
// shell, in light and dark mode, with colors resolved by the browser to
// rgb()/rgba() (color-mix/oklch included). Output: docs/port/tokens-resolved.json
//
// Usage: WEB=http://localhost:1531 CHROMIUM=... node dump-tokens.mjs [out.json]
import { createRequire } from "node:module";
import { writeFileSync } from "node:fs";

const BUZZ_APP = process.env.BUZZ_APP ?? "/home/ben/projects/buzz-app";
const require = createRequire(`${BUZZ_APP}/package.json`);
const { chromium } = require("@playwright/test");
const WEB = process.env.WEB ?? "http://localhost:1531";
const out =
  process.argv[2] ?? "/home/ben/projects/buzz-gpui/docs/port/tokens-resolved.json";

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM });
const result = { source: `${WEB}/ (buzz-app shell, 16px root, interface size 100%)`, modes: {} };
for (const scheme of ["light", "dark"]) {
  const page = await browser.newPage({ colorScheme: scheme, viewport: { width: 1200, height: 800 } });
  await page.goto(`${WEB}/`, { waitUntil: "networkidle" });
  // Component stylesheets load lazily; wait until the rule count settles.
  let previous = -1;
  for (let i = 0; i < 30; i++) {
    await page.waitForTimeout(1000);
    const count = await page.evaluate(() =>
      [...document.styleSheets].reduce((n, s) => {
        try {
          return n + s.cssRules.length;
        } catch {
          return n;
        }
      }, 0),
    );
    if (count === previous) break;
    previous = count;
  }
  result.modes[scheme] = await page.evaluate(() => {
    const names = new Set();
    const walk = (rules) => {
      for (const rule of rules) {
        if (rule.style)
          for (const prop of rule.style) if (prop.startsWith("--")) names.add(prop);
        if (rule.cssRules) walk(rule.cssRules);
      }
    };
    for (const sheet of document.styleSheets) {
      try {
        walk(sheet.cssRules);
      } catch {}
    }
    const root = getComputedStyle(document.documentElement);
    const probe = document.createElement("div");
    document.body.append(probe);
    const tokens = {};
    for (const name of [...names].sort()) {
      const raw = root.getPropertyValue(name).trim();
      if (!raw) continue;
      probe.style.backgroundColor = "";
      probe.style.backgroundColor = `var(${name})`;
      const color = probe.style.backgroundColor
        ? getComputedStyle(probe).backgroundColor
        : "";
      const isColor =
        color && !(color === "rgba(0, 0, 0, 0)" && !/transparent|0\)$/.test(raw));
      tokens[name] = isColor && /^(rgb|color\()/.test(color) && !/gradient|url\(/.test(raw)
        ? { value: color, raw }
        : { raw };
    }
    probe.remove();
    return tokens;
  });
  await page.close();
}
await browser.close();
writeFileSync(out, `${JSON.stringify(result, null, 1)}\n`);
console.log(
  `wrote ${out}: ${Object.keys(result.modes.light).length} light / ${Object.keys(result.modes.dark).length} dark tokens`,
);
