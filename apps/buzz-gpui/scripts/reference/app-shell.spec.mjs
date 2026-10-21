// Captures the real buzz-app shell (Messages, timeline, thread, composer
// autocomplete, agents, settings) using buzz-app's synthetic-relay fixture.
const BUZZ_APP = process.env.BUZZ_APP ?? "/home/ben/projects/buzz-app";
const { test, expect } = await import(`${BUZZ_APP}/tests/browser/fixture.mjs`);
const { open, settle } = await import(`${BUZZ_APP}/tests/browser/timeline.mjs`);
const { openPage, selectSettingsSection } = await import(
  `${BUZZ_APP}/tests/browser/navigation.mjs`
);

const OUT =
  process.env.SHOT_DIR ?? "/home/ben/projects/buzz-gpui/docs/port/screenshots";

test.use({
  exactMessages: true,
  readState: true,
  dmLabels: true,

  historyCounts: { alpha: 4, beta: 2 },
});

// Extra viewer-authored messages with varied markdown, appended to the
// synthetic relay history before the page loads.
const VARIED = [
  "Shipping the **sidebar** fix today. See `ChannelList.tsx` and the [design doc](https://example.com/design).",
  "Checklist:\n- rail width\n- sidebar resize\n  - min/max\n1. header\n2. composer",
  "> Quoted context from earlier in the thread\n\nAgreed, let's ~~drop~~ keep it.",
  "```rust\nfn main() {\n    println!(\"hello buzz\");\n}\n```",
  "| Token | Light | Dark |\n| --- | --- | --- |\n| surface | white | #111 |",
  "🎉🚀",
];

function addVaried(app) {
  const alpha = app.histories.get("primary/alpha");
  const base = Math.max(...alpha.map((e) => e.created_at)) + 2 * 3600;
  VARIED.forEach((content, i) =>
    alpha.push(
      app.sign({ kind: 9, tags: [["h", "alpha"]], content, created_at: base + i * 30 }),
    ),
  );
}

const shot = async (page, name, scheme) => {
  await page.waitForTimeout(400);
  await page.evaluate(() => document.activeElement?.blur?.());
  await page.mouse.move(1199, 799);
  await page.screenshot({ path: `${OUT}/${name}-${scheme}.png` });
};

test("reference shell screenshots", async ({ page, app }, info) => {
  const scheme = info.project.name;
  addVaried(app);
  await open(page, app);
  await page.evaluate(() => document.activeElement?.blur());
  await shot(page, "app-channel-timeline", scheme);

  // Same timeline scrolled to the top: other-author group, day divider.
  await page
    .getByRole("region", { name: "Channel message history" })
    .evaluate((el) => el.scrollTo(0, 0));
  await page.waitForTimeout(800);
  await shot(page, "app-channel-timeline-top", scheme);
  await page.getByRole("button", { name: "Jump to latest" }).click();
  await page.waitForTimeout(1200);
  await settle(page);

  // Hover action bar on the latest message.
  const rows = page.locator("[data-message-id]");
  const last = rows.last();
  await last.hover();
  await page.waitForTimeout(400);
  await page.screenshot({ path: `${OUT}/app-message-hover-${scheme}.png` });

  // Thread panel: the action bar is visible now, so use its Reply button.
  try {
    await page.getByRole("button", { name: "Reply", exact: true }).first().click({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Thread", exact: true }).waitFor({ timeout: 10_000 });
    await page.waitForTimeout(1200);
    await shot(page, "app-thread-panel", scheme);
    const thread = page.getByRole("tab", { name: "Thread", exact: true }).locator("..");
    await thread.getByRole("button", { name: /close/i }).first().click({ timeout: 5_000 });
    await page.waitForTimeout(800);
  } catch (error) {
    console.log("thread failed", String(error).slice(0, 300));
  }

  // Composer with @mention autocomplete.
  const composer = page.getByRole("textbox", { name: "Message #Alpha", exact: true });
  await composer.click();
  await page.keyboard.type("@");
  await page.waitForTimeout(800);
  await page.screenshot({ path: `${OUT}/app-composer-mention-${scheme}.png` });
  await page.keyboard.press("Escape");
  await page.keyboard.press("Backspace");
  await page.keyboard.type("#");
  await page.waitForTimeout(800);
  await page.screenshot({ path: `${OUT}/app-composer-channel-${scheme}.png` });
  await page.keyboard.press("Escape");
  await page.keyboard.press("Backspace");
  await page.keyboard.type(":smi");
  await page.waitForTimeout(800);
  await page.screenshot({ path: `${OUT}/app-composer-emoji-${scheme}.png` });
  await page.keyboard.press("Escape");
  await page.keyboard.press("ControlOrMeta+a");
  await page.keyboard.press("Backspace");

  // Composer toolbar popovers: emoji picker and formatting toolbar.
  const composerRegion = page;
  for (const [label, name] of [
    [/emoji/i, "app-composer-emoji-picker"],
    [/format/i, "app-composer-formatting"],
  ]) {
    try {
      await composerRegion.getByRole("button", { name: label }).first().click();
      await page.waitForTimeout(900);
      await page.mouse.move(1199, 799);
      await page.screenshot({ path: `${OUT}/${name}-${scheme}.png` });
      await page.keyboard.press("Escape");
      if (name.endsWith("formatting"))
        await page.getByRole("button", { name: /format/i }).first().click();
      await page.waitForTimeout(300);
    } catch (error) {
      console.log(`${name} failed`, String(error).slice(0, 200));
    }
  }

  // Channel header overflow menu and members panel.
  try {
    await page.getByRole("button", { name: "Channel actions", exact: true }).first().click();
    await page.waitForTimeout(600);
    await page.mouse.move(1199, 799);
    await page.screenshot({ path: `${OUT}/app-channel-menu-${scheme}.png` });
    await page.keyboard.press("Escape");
  } catch (error) {
    console.log("channel menu failed", String(error).slice(0, 200));
  }
  try {
    await page.getByRole("button", { name: /member/i }).first().click();
    await page.waitForTimeout(1200);
    await shot(page, "app-channel-members", scheme);
    await page.keyboard.press("Escape");
  } catch (error) {
    console.log("members failed", String(error).slice(0, 200));
  }

  // Search palette (header magnifier).
  try {
    await page.getByRole("button", { name: /search/i }).first().click();
    await page.waitForTimeout(900);
    await page.mouse.move(1199, 799);
    await page.screenshot({ path: `${OUT}/app-search-dialog-${scheme}.png` });
    await page.keyboard.press("Escape");
    await page.waitForTimeout(400);
  } catch (error) {
    console.log("search failed", String(error).slice(0, 200));
  }

  // Beta channel (sparser) to show sidebar selection change.
  const beta = page.locator('button[data-channel-id="beta"]');
  if (await beta.count()) {
    await beta.click();
    await page.waitForTimeout(1200);
    await shot(page, "app-channel-beta", scheme);
  }

  // Agents page.
  try {
    await openPage(page, "Agents");
    await page.waitForTimeout(1500);
    await shot(page, "app-agents", scheme);
  } catch (error) {
    console.log("agents page failed", String(error).slice(0, 300));
  }

  // Settings page (profile menu -> Settings).
  try {
    await page.keyboard.press("Escape").catch(() => {});
    await page.getByRole("button", { name: "Your profile", exact: true }).click();
    await page.waitForTimeout(400);
    await page.screenshot({ path: `${OUT}/app-profile-menu-${scheme}.png` });
    await page.getByRole("menuitem", { name: "Settings", exact: true }).click();
    await page.waitForTimeout(1200);
    await shot(page, "app-settings", scheme);
    const sections = await page
      .getByRole("complementary", { name: "Settings sidebar", exact: true })
      .getByRole("button")
      .allInnerTexts();
    console.log("settings sections:", sections.join(" | "));
    for (const name of ["Appearance", "Notifications", "Agents"]) {
      try {
        await selectSettingsSection(page, name);
        await page.waitForTimeout(600);
        await shot(page, `app-settings-${name.toLowerCase()}`, scheme);
      } catch (error) {
        console.log(`settings section ${name} failed`, String(error).slice(0, 200));
      }
    }
  } catch (error) {
    console.log("settings failed", String(error).slice(0, 300));
  }
});
