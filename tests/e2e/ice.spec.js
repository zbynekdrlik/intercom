// @ts-check
// The phone page fetches the hub's TURN servers before it opens the Janus session (issue 1453): a
// phone on mobile data needs a media relay, and Janus has only LAN host candidates. Same harness as
// phone.spec.js: the REAL intercom/web client + the REAL vendored janus.js served by stub_hub.py,
// only the Janus SERVER faked (fake-janus-server.js). The stub's `/api/ice` mode is switched per
// test (`/__test/ice/<mode>`). A wrapper around RTCPeerConnection records the configuration janus.js
// builds, so the test sees exactly which ICE servers the page's PeerConnection got.
const path = require("path");
const { test, expect } = require("@playwright/test");

// janus.js's own default when it is given no iceServers (janus.js `gatewayCallbacks.iceServers ||`).
const JANUS_DEFAULT_ICE = [{ urls: "stun:stun.l.google.com:19302" }];

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    if (navigator.serviceWorker) {
      Object.defineProperty(navigator.serviceWorker, "register", {
        value: () => new Promise(() => {}),
        configurable: true,
      });
    }
    try {
      localStorage.setItem("interkom.display", "Kamera 1");
    } catch (e) {
      // private mode -- the name sheet would then show and the join still happens.
    }
  });
  await page.addInitScript({ path: path.join(__dirname, "fake-janus-server.js") });
  // After the fake: record every PeerConnection configuration that carries iceServers (the
  // page's, built by janus.js; the fake's own answering PeerConnection passes none).
  await page.addInitScript(() => {
    const Real = window.RTCPeerConnection;
    window.__pcIce = [];
    window.RTCPeerConnection = class extends Real {
      constructor(cfg, ...rest) {
        if (cfg && cfg.iceServers) window.__pcIce.push(JSON.parse(JSON.stringify(cfg.iceServers)));
        super(cfg, ...rest);
      }
    };
  });
});

test.afterEach(async ({ request }) => {
  await request.get("/__test/ice/off");
});

function watchConsole(page) {
  const seen = [];
  page.on("console", (msg) => seen.push(`${msg.type()}: ${msg.text()}`));
  page.on("pageerror", (err) => seen.push(`pageerror: ${err.message}`));
  return seen;
}

async function expectConnected(page) {
  const conn = page.locator('[data-role="conn"]');
  await expect(conn).toHaveAttribute("data-state", "connected", { timeout: 15000 });
}

async function pageIce(page) {
  await expect.poll(() => page.evaluate(() => window.__pcIce.length), { timeout: 15000 }).toBeGreaterThan(0);
  return page.evaluate(() => window.__pcIce[0]);
}

test("the hub's TURN servers reach the page's PeerConnection before the join", async ({ page, request }) => {
  await request.get("/__test/ice/on");
  const seen = watchConsole(page);
  await page.goto("/");
  await expectConnected(page);

  const ice = await pageIce(page);
  expect(ice).toEqual([
    {
      urls: ["stun:127.0.0.1:3479", "turn:127.0.0.1:3479?transport=udp"],
      username: "stub-user",
      credential: "stub-credential",
    },
  ]);
  const hits = await (await request.get("/__test/ice-hits")).json();
  expect(hits, "one /api/ice fetch per session").toBe(1);
  const f = await page.evaluate(() => window.__fakeJanus.joins.length);
  expect(f).toBe(1);
  expect(seen, `console must be clean:\n${seen.join("\n")}`).toEqual([]);
});

test("a box without TURN keeps janus.js's default ICE server", async ({ page, request }) => {
  await request.get("/__test/ice/off");
  const seen = watchConsole(page);
  await page.goto("/");
  await expectConnected(page);
  expect(await pageIce(page), "turn=off never passes an empty list").toEqual(JANUS_DEFAULT_ICE);
  expect(seen, `console must be clean:\n${seen.join("\n")}`).toEqual([]);
});

test("a hub that answers garbage still joins with the default ICE server", async ({ page, request }) => {
  await request.get("/__test/ice/garbage");
  const seen = watchConsole(page);
  await page.goto("/");
  await expectConnected(page);
  expect(await pageIce(page)).toEqual(JANUS_DEFAULT_ICE);
  expect(seen, `console must be clean:\n${seen.join("\n")}`).toEqual([]);
});

test("a hub that never answers /api/ice does not block the join", async ({ page, request }) => {
  await request.get("/__test/ice/hang");
  const seen = watchConsole(page);
  const t0 = Date.now();
  await page.goto("/");
  await expectConnected(page);
  const took = Date.now() - t0;
  // The stub holds the answer 8 s; the page gives up after 3 s and joins with the default.
  expect(took, `joined after ${took} ms`).toBeLessThan(7000);
  expect(await pageIce(page)).toEqual(JANUS_DEFAULT_ICE);
  expect(seen, `console must be clean:\n${seen.join("\n")}`).toEqual([]);
});
