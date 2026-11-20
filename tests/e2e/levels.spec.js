// @ts-check
// intercom#34 + #38: the cutter's mixer page, end to end on the REAL hub binary (start-levels-hub.js
// runs it on levels.toml, shaped like strih-snv: FOH, LV1, Click, Kamera 1..7, Telefóny and the
// Master). The browser drags a fader, uses the keyboard on it and toggles mutes; each result is
// read back from the hub's /api/levels AND from the state file it wrote (the backend effect), one
// camera's fader moves only that camera, the master trims the whole output, the page shows the
// hub's version, the cans' meter is live, a reload keeps the values, the mixer fits a 1366-wide
// notebook screen (and a narrower window scrolls the faders, never the page), and the console stays
// completely empty.
const fs = require("fs");
const path = require("path");
const { test, expect } = require("@playwright/test");
const env = require("./levels-env");
const { startTone } = require("./vban-tone");

const STATE_FILE = path.join(env.RUN_DIR, "state.toml");
const WORKSPACE_VERSION = /\[workspace\.package\]\s*\nversion = "([^"]+)"/.exec(
  fs.readFileSync(path.resolve(__dirname, "../../Cargo.toml"), "utf8"),
)[1];

const NAMES = ["foh", "lv1", "click", "cam1", "cam2", "cam3", "cam4", "cam5", "cam6", "cam7", "telefony", "master"];
const LABELS = ["FOH", "LV1", "Click", "Kamera 1", "Kamera 2", "Kamera 3", "Kamera 4", "Kamera 5", "Kamera 6", "Kamera 7", "Telefóny", "Master"];

function watchConsole(page) {
  const seen = [];
  page.on("console", (msg) => seen.push(`${msg.type()}: ${msg.text()}`));
  page.on("pageerror", (err) => seen.push(`pageerror: ${err.message}`));
  return seen;
}

async function apiLevels(request) {
  const r = await request.get("/api/levels");
  expect(r.ok()).toBe(true);
  return (await r.json()).levels;
}

async function apiLevel(request, name) {
  return (await apiLevels(request)).find((l) => l.name === name);
}

/** {name: [db, muted]} of every slider but the ones named. */
async function othersThan(request, names) {
  const out = {};
  for (const l of await apiLevels(request)) {
    if (!names.includes(l.name)) out[l.name] = [l.db, l.muted];
  }
  return out;
}

/** The state file's entry for one slider ({db, muted}), or null before the first save. */
function storedLevel(name) {
  if (!fs.existsSync(STATE_FILE)) return null;
  const text = fs.readFileSync(STATE_FILE, "utf8");
  const m = new RegExp(`\\[levels\\.${name}\\]\\s*\\ndb = ([-0-9.e]+)\\s*\\nmuted = (true|false)`).exec(text);
  return m ? { db: Number(m[1]), muted: m[2] === "true" } : null;
}

const strip = (page, name) => page.locator(`[data-role="strip"][data-name="${name}"]`);

// One hub serves every test: each starts from the shipped 0 dB, unmuted, on every slider.
test.beforeEach(async ({ request }) => {
  for (const name of NAMES) {
    const r = await request.put(`/api/levels/${name}`, { data: { db: 0, muted: false } });
    expect(r.ok()).toBe(true);
  }
});

test("the cutter drags, steps and mutes the faders; the hub applies and stores them", async ({ page, request }) => {
  const consoleSeen = watchConsole(page);
  await page.goto("/");

  // The hub's own version, from the binary under test.
  await expect(page.locator('[data-role="version"]')).toHaveText(`v${WORKSPACE_VERSION}`);
  const ver = await (await request.get("/api/version")).json();
  expect(ver.version).toBe(WORKSPACE_VERSION);

  await expect(page.locator('[data-role="status"]')).toHaveAttribute("data-state", "good");
  await expect(page.locator('[data-role="strip"]')).toHaveCount(NAMES.length);
  await expect(page.locator('[data-role="strip"] [data-role="label"]')).toHaveText(LABELS);
  await expect(strip(page, "foh").locator('[data-role="db"]')).toHaveText("0.0 dB");

  // Drag the FOH fader (vertical, -60..+12 dB) from its bottom to near its top.
  const slider = strip(page, "foh").locator('[data-role="slider"]');
  const box = await slider.boundingBox();
  if (!box) throw new Error("the foh fader has no box");
  const x = box.x + box.width / 2;
  await page.mouse.move(x, box.y + box.height - 2);
  await page.mouse.down();
  await page.mouse.move(x, box.y + box.height * 0.5, { steps: 8 });
  await page.mouse.move(x, box.y + 4, { steps: 8 });
  await page.mouse.up();

  const shown = Number(await strip(page, "foh").getAttribute("data-db"));
  expect(shown).toBeGreaterThan(6);
  expect(shown).toBeLessThanOrEqual(12);
  await expect.poll(async () => (await apiLevel(request, "foh")).db).toBe(shown);
  await expect.poll(() => storedLevel("foh")).toEqual({ db: shown, muted: false });
  await expect(strip(page, "foh")).toHaveAttribute("data-saved", "true");

  // The keyboard steps it by 0.5 dB.
  await slider.focus();
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  const stepped = shown - 1;
  await expect(strip(page, "foh")).toHaveAttribute("data-db", String(stepped));
  await expect.poll(async () => (await apiLevel(request, "foh")).db).toBe(stepped);
  await expect.poll(() => storedLevel("foh")).toEqual({ db: stepped, muted: false });

  // Mute Click: the button shows it, the hub and the file hold it, the dB is kept.
  const mute = strip(page, "click").locator('[data-role="mute"]');
  await mute.click();
  await expect(mute).toHaveAttribute("aria-pressed", "true");
  await expect(strip(page, "click").locator('[data-role="db"]')).toHaveText("stlmené");
  await expect.poll(async () => (await apiLevel(request, "click")).muted).toBe(true);
  await expect.poll(() => storedLevel("click")).toEqual({ db: 0, muted: true });
  expect((await apiLevel(request, "foh")).db).toBe(stepped);

  // The cans' meter is the hub's live output reading (no input here: silence).
  await expect(page.locator('[data-role="meter-value"]')).toHaveText("ticho", { timeout: 5000 });
  await expect(page.locator('[data-role="meter"]')).toHaveAttribute("data-db", "-120");

  // A reload shows what the hub holds.
  await page.reload();
  await expect(strip(page, "foh")).toHaveAttribute("data-db", String(stepped));
  await expect(strip(page, "click")).toHaveAttribute("data-muted", "true");

  // Unmute again: back to 0 dB for Click.
  await strip(page, "click").locator('[data-role="mute"]').click();
  await expect.poll(() => storedLevel("click")).toEqual({ db: 0, muted: false });

  expect(consoleSeen).toEqual([]);
});

// intercom#38/#39 live 20.11.2026: the tray's window is WebKitGTK, which ignored the range's
// `writing-mode: vertical-lr` and drew each fader as a tiny horizontal slider in the strip. A cutter
// clicks where the fader is: the top of the strip's fader is the maximum, the bottom the minimum,
// in every engine (the `levels-webkit` project runs this test in WebKit).
test("a click at the top or bottom of a fader sets its max or min (every engine)", async ({ page, request }) => {
  const seen = watchConsole(page);
  await page.goto("/");
  await expect(page.locator('[data-role="status"]')).toHaveAttribute("data-state", "good");
  const slider = strip(page, "cam3").locator('[data-role="slider"]');
  const box = await slider.boundingBox();
  if (!box) throw new Error("the cam3 fader has no box");
  expect(box.height, "the fader stands upright").toBeGreaterThan(box.width * 3);
  const x = box.x + box.width / 2;
  await page.mouse.click(x, box.y + 3);
  await expect.poll(async () => (await apiLevel(request, "cam3")).db).toBeGreaterThanOrEqual(10);
  await page.mouse.click(x, box.y + box.height - 3);
  await expect.poll(async () => (await apiLevel(request, "cam3")).db).toBeLessThanOrEqual(-55);
  await page.mouse.click(x, box.y + box.height / 2);
  const mid = (await apiLevel(request, "cam3")).db;
  expect(mid, "the middle of the fader is the middle of -60..+12").toBeGreaterThan(-36);
  expect(mid).toBeLessThan(-12);
  expect(seen, "browser console must stay completely clean").toEqual([]);
});

test("one camera's fader moves only that camera, the master its own value", async ({ page, request }) => {
  const consoleSeen = watchConsole(page);
  await page.goto("/");
  await expect(page.locator('[data-role="strip"]')).toHaveCount(NAMES.length);

  // Kamera 3 down 3 dB by the keyboard: only cam3 changes in the hub and in the file.
  const before = await othersThan(request, ["cam3"]);
  const cam3 = strip(page, "cam3").locator('[data-role="slider"]');
  await cam3.focus();
  for (let i = 0; i < 6; i++) await page.keyboard.press("ArrowDown");
  await expect(strip(page, "cam3")).toHaveAttribute("data-db", "-3");
  await expect.poll(async () => (await apiLevel(request, "cam3")).db).toBe(-3);
  await expect.poll(() => storedLevel("cam3")).toEqual({ db: -3, muted: false });
  expect(await othersThan(request, ["cam3"])).toEqual(before);
  expect(storedLevel("cam4")).toEqual({ db: 0, muted: false });

  // The master is its own strip, set apart, last; it moves and mutes like the others.
  const master = strip(page, "master");
  await expect(master).toHaveAttribute("data-master", "true");
  await expect(strip(page, "cam3")).toHaveAttribute("data-master", "false");
  await expect(page.locator('[data-role="strip"]').last()).toHaveAttribute("data-name", "master");
  await master.locator('[data-role="slider"]').focus();
  await page.keyboard.press("ArrowUp");
  await page.keyboard.press("ArrowUp");
  await expect.poll(async () => (await apiLevel(request, "master")).db).toBe(1);
  await expect.poll(() => storedLevel("master")).toEqual({ db: 1, muted: false });
  expect((await apiLevel(request, "cam3")).db).toBe(-3);
  await master.locator('[data-role="mute"]').click();
  await expect.poll(async () => (await apiLevel(request, "master")).muted).toBe(true);
  await expect(master.locator('[data-role="db"]')).toHaveText("stlmené");
  expect((await apiLevel(request, "master")).db).toBe(1);
  expect((await apiLevel(request, "cam3")).muted).toBe(false);

  expect(consoleSeen).toEqual([]);
});

test("the mixer fits a 1366-wide notebook screen; a narrower window scrolls the faders, not the page", async ({ page }) => {
  const consoleSeen = watchConsole(page);
  await page.setViewportSize({ width: 1366, height: 768 });
  await page.goto("/");
  await expect(page.locator('[data-role="strip"]')).toHaveCount(NAMES.length);

  // Side by side: one row, left to right, every fader and its mute on screen without scrolling.
  const boxes = [];
  for (const name of NAMES) {
    const b = await strip(page, name).boundingBox();
    if (!b) throw new Error(`${name} has no box`);
    boxes.push(b);
  }
  for (let i = 1; i < boxes.length; i++) {
    expect(Math.abs(boxes[i].y - boxes[0].y)).toBeLessThan(2);
    expect(boxes[i].x).toBeGreaterThan(boxes[i - 1].x + boxes[i - 1].width - 1);
  }
  for (const name of NAMES) {
    for (const role of ["slider", "mute"]) {
      const b = await strip(page, name).locator(`[data-role="${role}"]`).boundingBox();
      if (!b) throw new Error(`${name} ${role} has no box`);
      expect(b.x + b.width).toBeLessThanOrEqual(1366);
      expect(b.y + b.height).toBeLessThanOrEqual(768);
      // A fader tall enough to set by hand.
      if (role === "slider") expect(b.height).toBeGreaterThanOrEqual(180);
    }
  }
  const meter = await page.locator('[data-role="meter"]').boundingBox();
  if (!meter) throw new Error("no meter");
  expect(meter.x + meter.width).toBeLessThanOrEqual(1366);
  const pageScroll = () =>
    page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(await pageScroll()).toBeLessThanOrEqual(0);

  // A narrow window: the page never scrolls sideways; the fader row does, and its last strip is
  // reachable and usable.
  await page.setViewportSize({ width: 700, height: 768 });
  expect(await pageScroll()).toBeLessThanOrEqual(0);
  const row = page.locator('[data-role="strips"]');
  const overflow = await row.evaluate((el) => el.scrollWidth - el.clientWidth);
  expect(overflow).toBeGreaterThan(0);
  await strip(page, "master").locator('[data-role="mute"]').scrollIntoViewIfNeeded();
  await strip(page, "master").locator('[data-role="mute"]').click();
  await expect(strip(page, "master")).toHaveAttribute("data-muted", "true");
  expect(await pageScroll()).toBeLessThanOrEqual(0);

  expect(consoleSeen).toEqual([]);
});

test("a change the hub cannot store still applies and the page says it was not saved", async ({ page, request }) => {
  const consoleSeen = watchConsole(page);
  await page.goto("/");
  await expect(page.locator('[data-role="strip"]')).toHaveCount(NAMES.length);
  // The state file's path turned into a non-empty directory: the hub's rename onto it fails (as
  // root too, which the CI container runs as), so the write fails like a full or read-only disk.
  const keep = fs.existsSync(STATE_FILE) ? fs.readFileSync(STATE_FILE) : null;
  fs.rmSync(STATE_FILE, { recursive: true, force: true });
  fs.mkdirSync(path.join(STATE_FILE, "blocker"), { recursive: true });
  try {
    const slider = strip(page, "click").locator('[data-role="slider"]');
    await slider.focus();
    await page.keyboard.press("ArrowDown");
    await expect(strip(page, "click")).toHaveAttribute("data-saved", "false");
    await expect(page.locator('[data-role="status"]')).toHaveAttribute("data-state", "warn");
    await expect(page.locator('[data-role="status"]')).toContainText("neuložilo sa");
    // The hub applies it anyway (the mix follows the slider until the next restart).
    await expect.poll(async () => (await apiLevel(request, "click")).db).toBe(-0.5);
  } finally {
    fs.rmSync(STATE_FILE, { recursive: true, force: true });
    if (keep) fs.writeFileSync(STATE_FILE, keep);
  }
  expect(consoleSeen).toEqual([]);
});

test("a value the hub refuses changes nothing the page shows", async ({ page, request }) => {
  const consoleSeen = watchConsole(page);
  await page.goto("/");
  await expect(page.locator('[data-role="strip"]')).toHaveCount(NAMES.length);
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

test("the cans' meter follows one camera's fader and the master", async ({ page }) => {
  const consoleSeen = watchConsole(page);
  // Only cam3's leg gets a real VBAN tone.
  const stop = startTone({ port: env.VBAN_PORT, stream: "cam3", channels: 1 });
  const meterDb = async () => Number(await page.locator('[data-role="meter"]').getAttribute("data-db"));
  const toggle = async (name, on) => {
    const mute = strip(page, name).locator('[data-role="mute"]');
    if ((await mute.getAttribute("aria-pressed")) !== String(on)) await mute.click();
    await expect(mute).toHaveAttribute("aria-pressed", String(on));
  };
  try {
    await page.goto("/");
    await expect(page.locator('[data-role="strip"]')).toHaveCount(NAMES.length);
    await expect.poll(meterDb, { timeout: 15000 }).toBeGreaterThan(-40);
    await expect(page.locator('[data-role="meter-value"]')).not.toHaveText("ticho");

    // Every other source muted: cam3 still plays.
    for (const name of NAMES.filter((n) => n !== "cam3" && n !== "master")) await toggle(name, true);
    await page.waitForTimeout(2500);
    expect(await meterDb()).toBeGreaterThan(-40);

    // Kamera 3 muted: the cans go silent.
    await toggle("cam3", true);
    await expect.poll(meterDb, { timeout: 10000 }).toBe(-120);
    await expect(page.locator('[data-role="meter-value"]')).toHaveText("ticho");

    // Kamera 3 back on, then the master muted: silent again; master on: it plays.
    await toggle("cam3", false);
    await expect.poll(meterDb, { timeout: 10000 }).toBeGreaterThan(-40);
    await toggle("master", true);
    await expect.poll(meterDb, { timeout: 10000 }).toBe(-120);
    await toggle("master", false);
    await expect.poll(meterDb, { timeout: 10000 }).toBeGreaterThan(-40);
  } finally {
    stop();
  }
  expect(consoleSeen).toEqual([]);
});
