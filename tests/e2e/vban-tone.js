// A VBAN sine sender for the level-sliders E2E (intercom#34): it feeds the real hub's `prog` leg
// so the cans' output meter has something to read. 48 kHz PCM16, 128 frames a packet, paced on the
// wall clock (each tick sends every packet that is due), the header byte for byte as
// intercom-vban's VbanHeader::encode writes it.
"use strict";
const dgram = require("dgram");

const RATE = 48000;
const FRAMES = 128;
const SR_INDEX_48K = 3;
const PCM16 = 0x01;

function header(stream, channels, counter) {
  const h = Buffer.alloc(28);
  h.write("VBAN", 0, "ascii");
  h[4] = SR_INDEX_48K;
  h[5] = FRAMES - 1;
  h[6] = channels - 1;
  h[7] = PCM16;
  h.write(stream.slice(0, 16), 8, "ascii");
  h.writeUInt32LE(counter >>> 0, 24);
  return h;
}

/** Start sending; returns a function that stops it. */
function startTone({ port, stream, channels, amplitude = 10000, hz = 997 }) {
  const sock = dgram.createSocket("udp4");
  const start = process.hrtime.bigint();
  let sent = 0;
  let phase = 0;
  const tick = () => {
    const elapsedNs = Number(process.hrtime.bigint() - start);
    const due = Math.floor((elapsedNs / 1e9) * (RATE / FRAMES));
    while (sent < due) {
      const body = Buffer.alloc(FRAMES * channels * 2);
      for (let f = 0; f < FRAMES; f++) {
        const s = Math.round(amplitude * Math.sin(phase));
        phase += (2 * Math.PI * hz) / RATE;
        for (let c = 0; c < channels; c++) body.writeInt16LE(s, (f * channels + c) * 2);
      }
      sock.send(Buffer.concat([header(stream, channels, sent), body]), port, "127.0.0.1");
      sent += 1;
    }
  };
  const timer = setInterval(tick, 2);
  return () => {
    clearInterval(timer);
    sock.close();
  };
}

module.exports = { startTone };
