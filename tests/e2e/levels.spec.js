// @ts-check
// intercom#34: the cutter's level sliders, end to end on the REAL hub binary (start-levels-hub.js
// runs it on levels.toml). The browser drags a slider, uses the keyboard on it and toggles a mute;
// each result is read back from the hub's /api/levels AND from the state file it wrote (the
// backend effect), the page shows the hub's version, the cans' meter is live, a reload keeps the
// values, and the console stays completely empty.
const fs = require("fs");
const path = require("path");
const { test, expect } = require("@playwright/test");
const env = require("./levels-env");
const { startTone } = require("./vban-tone");

const STATE_FILE = path.join(env.RUN_DIR, "state.toml");
const WORKSPACE_VERSION = /\[workspace\.package\]\s*\nversion = "([^"]+)"/.exec(
  fs.readFileSync(path.resolve(__dirname, "../../Cargo.toml"), "utf8"),
)[1];

function watchConsole(page) {
  const seen = [];
  page.on("console", (msg) => seen.push(`${msg.type()}: ${msg.text()}`));
  page.on("pageerror", (err) => seen.push(`pageerror: ${err.message}`));
  return seen;
}

async function apiLevel(request, name) {
  const r = await request.get("/api/levels");
  expect(r.ok()).toBe(true);
  const body = await r.json();
  return body.levels.find((l) => l.name === name);
}

/** The state file's entry for one slider ({db, muted}), or null before the first save. */
function storedLevel(name) {
  if (!fs.existsSync(STATE_FILE)) return null;
  const text = fs.readFileSync(STATE_FILE, "utf8");
  const m = new RegExp(`\\[levels\\.${name}\\]\\s*\\ndb = ([-0-9.e]+)\\s*\\nmuted = (true|false)`).exec(text);
  return m ? { db: Number(m[1]), muted: m[2] === "true" } : null;
}

const strip = (page, name) => page.locator(`[data-role="strip"][data-name="${name}"]`);

// One hub serves every test: each starts from the shipped 0 dB, unmuted.
test.beforeEach(async ({ request }) => {
  for (const name of ["program", "click"]) {
    const r = await request.put(`/api/levels/${name}`, { data: { db: 0, muted: false } });
    expect(r.ok()).toBe(true);
  }
});

test("the cutter drags, steps and mutes the sliders; the hub applies and stores them", async ({ page, request }) => {
  const consoleSeen = watchConsole(page);
  await page.goto("/");

  // The hub's own version, from the binary under test.
  await expect(page.locator('[data-role="version"]')).toHaveText(`v${WORKSPACE_VERSION}`);
  const ver = await (await request.get("/api/version")).json();
  expect(ver.version).toBe(WORKSPACE_VERSION);

  await expect(page.locator('[data-role="status"]')).toHaveAttribute("data-state", "good");
  await expect(page.locator('[data-role="strip"]')).toHaveCount(2);
  await expect(strip(page, "program").locator('[data-role="label"]')).toHaveText("Program");
  await expect(strip(page, "click").locator('[data-role="label"]')).toHaveText("Click");
  await expect(strip(page, "program").locator('[data-role="db"]')).toHaveText("0.0 dB");

  // Drag the Program slider (vertical, -60..+12 dB) from its bottom to near its top.
  const slider = strip(page, "program").locator('[data-role="slider"]');
  const box = await slider.boundingBox();
  if (!box) throw new Error("the program slider has no box");
  const x = box.x + box.width / 2;
  await page.mouse.move(x, box.y + box.height - 2);
  await page.mouse.down();
  await page.mouse.move(x, box.y + box.height * 0.5, { steps: 8 });
  await page.mouse.move(x, box.y + 4, { steps: 8 });
  await page.mouse.up();

  const shown = Number(await strip(page, "program").getAttribute("data-db"));
  expect(shown).toBeGreaterThan(6);
  expect(shown).toBeLessThanOrEqual(12);
  await expect.poll(async () => (await apiLevel(request, "program")).db).toBe(shown);
  await expect.poll(() => storedLevel("program")).toEqual({ db: shown, muted: false });
  await expect(strip(page, "program")).toHaveAttribute("data-saved", "true");

  // The keyboard steps it by 0.5 dB.
  await slider.focus();
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  const stepped = shown - 1;
  await expect(strip(page, "program")).toHaveAttribute("data-db", String(stepped));
  await expect.poll(async () => (await apiLevel(request, "program")).db).toBe(stepped);
  await expect.poll(() => storedLevel("program")).toEqual({ db: stepped, muted: false });

  // Mute Click: the button shows it, the hub and the file hold it, the dB is kept.
  const mute = strip(page, "click").locator('[data-role="mute"]');
  await mute.click();
  await expect(mute).toHaveAttribute("aria-pressed", "true");
  await expect(strip(page, "click").locator('[data-role="db"]')).toHaveText("stlmené");
  await expect.poll(async () => (await apiLevel(request, "click")).muted).toBe(true);
  await expect.poll(() => storedLevel("click")).toEqual({ db: 0, muted: true });
  expect((await apiLevel(request, "program")).db).toBe(stepped);

  // The cans' meter is the hub's live output reading (no input here: silence).
  await expect(page.locator('[data-role="meter-value"]')).toHaveText("ticho", { timeout: 5000 });
  await expect(page.locator('[data-role="meter"]')).toHaveAttribute("data-db", "-120");

  // A reload shows what the hub holds.
  await page.reload();
  await expect(strip(page, "program")).toHaveAttribute("data-db", String(stepped));
  await expect(strip(page, "click")).toHaveAttribute("data-muted", "true");

  // Unmute again: back to 0 dB for Click.
  await strip(page, "click").locator('[data-role="mute"]').click();
  await expect.poll(() => storedLevel("click")).toEqual({ db: 0, muted: false });

  expect(consoleSeen).toEqual([]);
});

test("a value the hub refuses changes nothing the page shows", async ({ page, request }) => {
  const consoleSeen = watchConsole(page);
  await page.goto("/");
  await expect(page.locator('[data-role="strip"]')).toHaveCount(2);
  const before = await apiLevel(request, "click");

  const r = await request.put("/api/levels/click", { data: { db: 99, muted: false } });
  expect(r.status()).toBe(400);
  expect(await r.text()).toContain("outside -60 .. 12 dB");
  expect(await apiLevel(request, "click")).toEqual(before);

  // The page still shows the hub's value after a reload.
  await page.reload();
  await expect(strip(page, "click")).toHaveAttribute("data-db", String(before.db));
  await expect(strip(page, "click")).toHaveAttribute("data-muted", String(before.muted));

  expect(consoleSeen).toEqual([]);
});

test("the cans' meter reads the hub's live output and follows the sliders", async ({ page }) => {
  const consoleSeen = watchConsole(page);
  // The hub's `prog` leg gets a real VBAN tone (all three channels: program L/R + click).
  const stop = startTone({ port: env.VBAN_PORT, stream: "prog", channels: 3 });
  try {
    await page.goto("/");
    await expect(page.locator('[data-role="strip"]')).toHaveCount(2);
    const meterDb = async () => Number(await page.locator('[data-role="meter"]').getAttribute("data-db"));
    await expect.poll(meterDb, { timeout: 15000 }).toBeGreaterThan(-40);
    await expect(page.locator('[data-role="meter-value"]')).not.toHaveText("ticho");

    // Both sliders muted from the page: the cans go silent.
    for (const name of ["program", "click"]) {
      const mute = strip(page, name).locator('[data-role="mute"]');
      if ((await mute.getAttribute("aria-pressed")) !== "true") await mute.click();
      await expect(mute).toHaveAttribute("aria-pressed", "true");
    }
    await expect.poll(meterDb, { timeout: 10000 }).toBe(-120);
    await expect(page.locator('[data-role="meter-value"]')).toHaveText("ticho");

    // Click alone back on: the meter rises again.
    await strip(page, "click").locator('[data-role="mute"]').click();
    await expect.poll(meterDb, { timeout: 10000 }).toBeGreaterThan(-40);
  } finally {
    stop();
  }
  expect(consoleSeen).toEqual([]);
});
