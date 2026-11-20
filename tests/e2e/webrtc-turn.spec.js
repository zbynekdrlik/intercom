// @ts-check
// The hub-side TURN relay end to end (intercom#42 phase 3): two Chromium phones with
// iceTransportPolicy "relay" (the page's test-only `?relay=1`) against the REAL hub with
// `[webrtc] turn = "cloudflare"`, whose key API is a fake minting a REAL coturn's credentials
// (webrtc-turn-env.js). A relay-only phone has no host or srflx candidate, and coturn refuses the
// hub's own address as a peer, so the ONLY working path is phone relay -> hub relay: what venue
// CGNAT leaves a phone on mobile data. The cutter's tone reaches both phones, A's mic reaches B
// (never A), the hub's /api/state reads pair "relay" for both, the page's own selected pair is
// relay <-> relay, and the console stays empty.
const fs = require("fs");
const path = require("path");
const { test, expect } = require("@playwright/test");
const env = require("./webrtc-turn-env");
const { startTone } = require("./vban-tone");

let relayEnv = null;

test.beforeAll(async () => {
  relayEnv = await env.startRelayEnv();
});

test.afterAll(async () => {
  if (relayEnv) await relayEnv.stop();
});

async function preparePage(page, name) {
  const still = fs.readFileSync(path.join(__dirname, "fixtures", "picture-0.jpg"));
  await page.route("**/interkom.mjpeg*", (route) =>
    route.fulfill({ status: 200, contentType: "image/jpeg", body: still })
  );
  await page.addInitScript((n) => {
    if (navigator.serviceWorker) {
      Object.defineProperty(navigator.serviceWorker, "register", {
        value: () => new Promise(() => {}),
        configurable: true,
      });
    }
    try {
      localStorage.setItem("interkom.display", n);
    } catch (e) {
      // private mode: the name sheet would show and the test fail loudly.
    }
  }, name);
}

function watchConsole(page) {
  const seen = [];
  page.on("console", (msg) => seen.push(`${msg.type()}: ${msg.text()}`));
  page.on("pageerror", (err) => seen.push(`pageerror: ${err.message}`));
  return seen;
}

async function hubState() {
  const r = await fetch(`${env.hub.BASE_URL}/api/state`);
  return r.json();
}

async function meterMax(page, role, ms) {
  const loc = page.locator(`[data-role="${role}"]`);
  let max = -120;
  const end = Date.now() + ms;
  while (Date.now() < end) {
    max = Math.max(max, Number(await loc.getAttribute("data-db")));
    await page.waitForTimeout(100);
  }
  return max;
}

/** The page's own selected candidate pair: [local type, remote type]. */
async function selectedPair(page) {
  return page.evaluate(async () => {
    // `nativePc` is the classic app.js's top-level binding for the live RTCPeerConnection.
    // eslint-disable-next-line no-undef
    const stats = await nativePc.getStats();
    let pair = null;
    stats.forEach((s) => {
      if (s.type === "transport" && s.selectedCandidatePairId) pair = stats.get(s.selectedCandidatePairId);
    });
    if (!pair) return null;
    return [stats.get(pair.localCandidateId).candidateType, stats.get(pair.remoteCandidateId).candidateType];
  });
}

test("relay-only phones reach the hub through its own TURN relay: tone both ways, pair relay", async ({ browser }) => {
  test.setTimeout(90000);
  // The hub allocated its relay from the minted credential before any phone comes.
  await expect
    .poll(async () => {
      const st = await hubState();
      return st.webrtc && st.webrtc.turn && st.webrtc.turn.state;
    }, { timeout: 15000 })
    .toBe("allocated");
  const turn = (await hubState()).webrtc.turn;
  expect(turn.relayed.startsWith(`${env.TURN_IP}:`), JSON.stringify(turn)).toBe(true);
  expect(turn.server).toBe(`${env.TURN_IP}:${env.TURN_PORT}`);

  const ctxA = await browser.newContext({ baseURL: env.hub.BASE_URL });
  const ctxB = await browser.newContext({ baseURL: env.hub.BASE_URL });
  const a = await ctxA.newPage();
  const b = await ctxB.newPage();
  const seenA = watchConsole(a);
  const seenB = watchConsole(b);
  await preparePage(a, "Relay A");
  await preparePage(b, "Relay B");
  await a.goto("/?relay=1");
  await b.goto("/?relay=1");
  for (const p of [a, b]) {
    await expect(p.locator('[data-role="conn"]')).toHaveAttribute("data-state", "connected", { timeout: 20000 });
  }

  // Both phones are on the relay, seen from the hub and from the page itself.
  await expect
    .poll(async () => {
      const st = await hubState();
      return st.webrtc.phones
        .filter((p) => p.connected)
        .map((p) => `${p.display}:${p.pair}`)
        .sort();
    })
    .toEqual(["Relay A:relay", "Relay B:relay"]);
  expect(await selectedPair(a)).toEqual(["relay", "relay"]);
  expect(await selectedPair(b)).toEqual(["relay", "relay"]);

  // The cutter's tone reaches both phones through the relay.
  const stopTone = startTone({ port: env.hub.VBAN_PORT, stream: "prog", channels: 1, hz: 997 });
  try {
    await expect(a.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "true", { timeout: 10000 });
    await expect(b.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "true", { timeout: 10000 });
  } finally {
    stopTone();
  }
  await expect(a.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "false", { timeout: 10000 });
  await expect(b.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "false", { timeout: 10000 });

  // A's mic goes through A's relay -> the hub's relay -> the room -> back out to B, never to A.
  const micA = a.locator('[data-role="mic-toggle"]');
  await micA.click();
  await expect(micA).toHaveAttribute("data-muted", "false");
  expect(await meterMax(b, "meter-in", 4000), "B hears A's mic through the relay").toBeGreaterThan(-50);
  expect(await meterMax(a, "meter-in", 3000), "A never hears its own mic").toBeLessThan(-70);
  await expect
    .poll(async () => {
      const st = await hubState();
      const pa = st.webrtc.phones.find((p) => p.display === "Relay A");
      return pa && pa.mic_active;
    })
    .toBe(true);
  const after = (await hubState()).webrtc.turn;
  expect(after.state).toBe("allocated");
  expect(after.channels, JSON.stringify(after)).toBeGreaterThanOrEqual(2);
  expect(after.rx_packets).toBeGreaterThan(100);
  expect(after.tx_packets).toBeGreaterThan(100);

  expect(seenA, "browser console must stay completely clean").toEqual([]);
  expect(seenB, "browser console must stay completely clean").toEqual([]);
  await ctxA.close();
  await ctxB.close();
});
