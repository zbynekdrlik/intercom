"use strict";
// Test double for the vendored janus.js (issue 1345 phone UX E2E). The real Janus is an external
// network service a CI browser cannot reach, so the stub hub serves THIS file at /janus.js. It
// implements only the janus.js 1.x surface app.js uses and records every request, so the spec can
// assert what the page asked Janus for. It never writes to the console.
//
// Behaviour (mirrors the audiobridge contract app.js relies on):
// - new Janus(...) succeeds after a short delay (or fails while `failConnects` > 0);
// - `join` answers `joined`;
// - `configure` WITH a jsep answers with a fake answer jsep; the first one brings the "media" up
//   (webrtcState(true)) and delivers a remote audio track from the Chromium fake audio device;
// - createOffer resolves `capture` (a MediaStreamTrack or getUserMedia constraints) and reports
//   what was offered.
// Test controls live on window.__fakeJanus (dropConnection, failConnects, the recorded requests).
(function () {
  const state = {
    sessions: 0,
    failConnects: 0,
    joins: [],
    offers: [],
    configures: [],
    replaces: [],
    gateways: [],
  };
  window.__fakeJanus = state;

  let remoteTrackPromise = null;
  function remoteTrack() {
    if (!remoteTrackPromise) {
      // The spec stores the unwrapped getUserMedia as window.__realGUM so this fake "room audio"
      // is not counted as a permission request made by the page.
      const gum = window.__realGUM || navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
      remoteTrackPromise = gum({ audio: true }).then((s) => s.getAudioTracks()[0]);
    }
    return remoteTrackPromise;
  }

  function later(ms, fn) {
    setTimeout(fn, ms);
  }

  function Handle(gw, cb) {
    this.gw = gw;
    this.cb = cb;
    this.mediaUp = false;
    this.webrtcStuff = { pc: null };
  }
  Handle.prototype.send = function (req) {
    const gw = this.gw;
    const cb = this.cb;
    const msg = req.message || {};
    if (gw.destroyed) return;
    if (msg.request === "join") {
      state.joins.push(msg);
      later(20, () => {
        if (!gw.destroyed) cb.onmessage({ audiobridge: "joined", room: msg.room, id: 7, participants: [] });
      });
    } else if (msg.request === "configure") {
      state.configures.push({ message: msg, jsep: !!req.jsep });
      if (req.jsep) {
        later(20, async () => {
          if (gw.destroyed) return;
          cb.onmessage({ audiobridge: "event", room: 1000, result: "ok" }, { type: "answer", sdp: "fake" });
          if (!this.mediaUp) {
            this.mediaUp = true;
            if (cb.webrtcState) cb.webrtcState(true);
            const t = await remoteTrack();
            if (!gw.destroyed && cb.onremotetrack) cb.onremotetrack(t, "0", true);
          }
        });
      } else {
        later(10, () => {
          if (!gw.destroyed) cb.onmessage({ audiobridge: "event", room: 1000, result: "ok" });
        });
      }
    }
  };
  Handle.prototype.createOffer = function (opts) {
    const t = (opts.tracks || [])[0] || {};
    const rec = { capture: !!t.capture, recv: !!t.recv, replace: !!t.replace };
    state.offers.push(rec);
    (async () => {
      try {
        if (t.capture) {
          const track =
            t.capture instanceof MediaStreamTrack
              ? t.capture
              : (await navigator.mediaDevices.getUserMedia({ audio: t.capture === true ? true : t.capture })).getAudioTracks()[0];
          rec.trackId = track.id;
          if (this.cb.onlocaltrack) this.cb.onlocaltrack(track, true);
        }
        if (opts.success) opts.success({ type: "offer", sdp: "fake" });
      } catch (e) {
        if (opts.error) opts.error(e);
      }
    })();
  };
  Handle.prototype.replaceTracks = function (opts) {
    const t = (opts.tracks || [])[0] || {};
    state.replaces.push({ capture: !!t.capture, trackId: t.capture && t.capture.id });
    if (opts.success) later(5, opts.success);
  };
  Handle.prototype.handleRemoteJsep = function () {};
  Handle.prototype.hangup = function () {};

  function Janus(opts) {
    state.sessions += 1;
    this.opts = opts;
    this.destroyed = false;
    this.handles = [];
    state.gateways.push(this);
    later(30, () => {
      if (this.destroyed) return;
      if (state.failConnects > 0) {
        state.failConnects -= 1;
        this.destroyed = true;
        opts.error("Lost connection to the server (is it down?)");
        return;
      }
      opts.success();
    });
  }
  Janus.prototype.attach = function (cb) {
    const h = new Handle(this, cb);
    this.handles.push(h);
    later(10, () => {
      if (!this.destroyed) cb.success(h);
    });
  };
  Janus.prototype.destroy = function (o) {
    this.destroyed = true;
    if (o && o.success) o.success();
  };
  Janus.init = function (o) {
    later(0, () => o.callback());
  };
  Janus.useDefaultDependencies = function (d) {
    return d || {};
  };
  Janus.noop = function () {};

  // Simulate the server dropping the live session (a WebSocket close).
  state.dropConnection = function () {
    const live = state.gateways.filter((g) => !g.destroyed);
    const g = live[live.length - 1];
    if (!g) return false;
    g.destroyed = true;
    g.opts.error("Lost connection to the server (is it down?)");
    return true;
  };

  window.Janus = Janus;
})();
