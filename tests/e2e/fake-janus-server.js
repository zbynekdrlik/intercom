"use strict";
// Test double for the external Janus SERVER (issue 1345 phone UX E2E). The page runs the REAL
// vendored janus.js; only the network peer is faked. This init script replaces window.WebSocket for
// `.../janus` URLs with an in-page socket that speaks the Janus WebSocket API (create / attach /
// message / trickle / keepalive / hangup / detach / destroy) and plays the audiobridge plugin:
// `join` answers `joined`, `configure` with a jsep offer is answered by a REAL RTCPeerConnection
// that sends a "room audio" track (the Chromium fake audio device) and receives the phone's mic.
// So the SDP directions, the renegotiation and the audio really flow through WebRTC in the browser.
//
// Test controls + records live on window.__fakeJanus:
//   joins / configures / offers (with the offer's audio direction) / sessions,
//   failConnects (the next N sockets fail to open), dropConnection() (the server drops the live
//   socket), rejectNextConfigures (the next N configure-with-offer get an error event and no
//   answer), inboundAudioBytes() (what the "server" received from the phone's mic).
// It never writes to the console.
(function () {
  const NativeWebSocket = window.WebSocket;
  const state = {
    sessions: 0,
    failConnects: 0,
    rejectNextConfigures: 0,
    joins: [],
    configures: [],
    offers: [],
    sockets: [],
    handles: [],
  };
  window.__fakeJanus = state;
  let nextId = 1000;

  let roomTrackPromise = null;
  function roomTrack() {
    // The spec keeps the unwrapped getUserMedia as window.__realGUM so this "room audio" is never
    // counted as a permission request made by the page.
    if (!roomTrackPromise) {
      const gum = window.__realGUM || navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
      roomTrackPromise = gum({ audio: true }).then((s) => s.getAudioTracks()[0]);
    }
    return roomTrackPromise;
  }

  function audioDirection(sdp) {
    const at = sdp.indexOf("m=audio");
    if (at === -1) return "none";
    const m = /a=(sendrecv|sendonly|recvonly|inactive)/.exec(sdp.slice(at));
    return m ? m[1] : "sendrecv";
  }

  class FakeJanusSocket extends EventTarget {
    constructor(url, protocol) {
      super();
      this.url = url;
      this.protocol = protocol || "";
      this.readyState = 0;
      this.sessionId = null;
      this.handles = new Map();
      state.sockets.push(this);
      setTimeout(() => {
        if (this.readyState !== 0) return;
        if (state.failConnects > 0) {
          state.failConnects -= 1;
          this.readyState = 3;
          this.dispatchEvent(new Event("error"));
          this.dispatchEvent(new CloseEvent("close", { code: 1006 }));
          return;
        }
        this.readyState = 1;
        this.dispatchEvent(new Event("open"));
      }, 20);
    }

    send(text) {
      if (this.readyState !== 1) return;
      const msg = JSON.parse(text);
      Promise.resolve().then(() => this.onRequest(msg));
    }

    close() {
      if (this.readyState >= 2) return;
      this.readyState = 3;
      this.shutdown();
      setTimeout(() => this.dispatchEvent(new CloseEvent("close", { code: 1000 })), 0);
    }

    // Server -> client, in order.
    push(obj) {
      const data = JSON.stringify(obj);
      setTimeout(() => {
        if (this.readyState === 1) this.dispatchEvent(new MessageEvent("message", { data }));
      }, 5);
    }

    shutdown() {
      for (const h of this.handles.values()) {
        if (h.pc) h.pc.close();
      }
    }

    onRequest(msg) {
      const tx = msg.transaction;
      switch (msg.janus) {
        case "create":
          this.sessionId = ++nextId;
          state.sessions += 1;
          this.push({ janus: "success", transaction: tx, data: { id: this.sessionId } });
          break;
        case "attach": {
          const id = ++nextId;
          const h = { id, pc: null, chain: Promise.resolve(), up: false, inbound: null };
          this.handles.set(id, h);
          state.handles.push(h);
          this.push({ janus: "success", transaction: tx, session_id: this.sessionId, data: { id } });
          break;
        }
        case "keepalive":
          this.push({ janus: "ack", transaction: tx, session_id: this.sessionId });
          break;
        case "trickle": {
          this.push({ janus: "ack", transaction: tx, session_id: this.sessionId });
          const h = this.handles.get(msg.handle_id);
          const c = msg.candidate;
          if (h && c && !c.completed) {
            h.chain = h.chain.then(() => (h.pc ? h.pc.addIceCandidate(c).catch(() => {}) : null));
          }
          break;
        }
        case "message":
          this.push({ janus: "ack", transaction: tx, session_id: this.sessionId });
          this.onPluginMessage(this.handles.get(msg.handle_id), msg.body || {}, msg.jsep, tx);
          break;
        case "hangup":
        case "detach": {
          const h = this.handles.get(msg.handle_id);
          if (h && h.pc) h.pc.close();
          this.push({ janus: "success", transaction: tx, session_id: this.sessionId });
          break;
        }
        case "destroy":
          this.shutdown();
          this.push({ janus: "success", transaction: tx, session_id: this.sessionId });
          break;
        default:
          this.push({ janus: "error", transaction: tx, error: { code: 457, reason: "unknown request" } });
      }
    }

    event(h, tx, data, jsep) {
      const ev = {
        janus: "event",
        session_id: this.sessionId,
        sender: h.id,
        transaction: tx,
        plugindata: { plugin: "janus.plugin.audiobridge", data },
      };
      if (jsep) ev.jsep = jsep;
      this.push(ev);
    }

    onPluginMessage(h, body, jsep, tx) {
      if (!h) return;
      if (body.request === "join") {
        state.joins.push(body);
        this.event(h, tx, { audiobridge: "joined", room: body.room, id: 7, participants: [] });
        return;
      }
      if (body.request === "configure") {
        state.configures.push({ message: body, jsep: !!jsep });
        if (!jsep) {
          this.event(h, tx, { audiobridge: "event", room: 1000, result: "ok" });
          return;
        }
        state.offers.push({ direction: audioDirection(jsep.sdp) });
        if (state.rejectNextConfigures > 0) {
          state.rejectNextConfigures -= 1;
          this.event(h, tx, { audiobridge: "event", room: 1000, error_code: 499, error: "rejected by the test" });
          return;
        }
        h.chain = h.chain
          .then(() => this.answer(h, jsep))
          .then((answer) => this.event(h, tx, { audiobridge: "event", room: 1000, result: "ok" }, answer))
          .catch(() => this.event(h, tx, { audiobridge: "event", error_code: 499, error: "fake answer failed" }));
      }
    }

    async answer(h, offer) {
      if (!h.pc) {
        const pc = new RTCPeerConnection();
        h.pc = pc;
        pc.onicecandidate = (e) => {
          const c = e.candidate
            ? { candidate: e.candidate.candidate, sdpMid: e.candidate.sdpMid, sdpMLineIndex: e.candidate.sdpMLineIndex }
            : { completed: true };
          this.push({ janus: "trickle", session_id: this.sessionId, sender: h.id, candidate: c });
        };
        pc.onconnectionstatechange = () => {
          if (pc.connectionState === "connected" && !h.up) {
            h.up = true;
            this.push({ janus: "webrtcup", session_id: this.sessionId, sender: h.id });
          }
        };
        pc.ontrack = (e) => {
          h.inbound = e.track;
        };
      }
      await h.pc.setRemoteDescription({ type: "offer", sdp: offer.sdp });
      const tr = h.pc.getTransceivers().find((t) => t.receiver.track.kind === "audio");
      if (tr && !tr.sender.track) {
        await tr.sender.replaceTrack(await roomTrack());
      }
      if (tr) tr.direction = "sendrecv"; // the answer narrows it to sendonly for a recvonly offer
      const answer = await h.pc.createAnswer();
      await h.pc.setLocalDescription(answer);
      return { type: "answer", sdp: answer.sdp };
    }
  }

  // Only the Janus endpoint is faked; any other WebSocket stays real.
  function PatchedWebSocket(url, protocols) {
    if (/\/janus$/.test(String(url))) return new FakeJanusSocket(url, protocols);
    return new NativeWebSocket(url, protocols);
  }
  PatchedWebSocket.CONNECTING = 0;
  PatchedWebSocket.OPEN = 1;
  PatchedWebSocket.CLOSING = 2;
  PatchedWebSocket.CLOSED = 3;
  window.WebSocket = PatchedWebSocket;

  state.dropConnection = function () {
    const live = state.sockets.filter((s) => s.readyState === 1);
    const s = live[live.length - 1];
    if (!s) return false;
    s.readyState = 3;
    s.shutdown();
    s.dispatchEvent(new CloseEvent("close", { code: 1006 }));
    return true;
  };

  // Bytes of the phone's mic audio the "server" has received on its newest PeerConnection.
  state.inboundAudioBytes = async function () {
    const h = state.handles.filter((x) => x.pc && x.pc.connectionState !== "closed").pop();
    if (!h) return 0;
    const report = await h.pc.getStats();
    let bytes = 0;
    report.forEach((r) => {
      if (r.type === "inbound-rtp" && r.kind === "audio") bytes += r.bytesReceived || 0;
    });
    return bytes;
  };
})();
