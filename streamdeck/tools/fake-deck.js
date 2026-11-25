#!/usr/bin/env node
// Development-only harness: a fake Stream Deck that speaks just enough of the
// plugin protocol to test bin/plugin.js without a device.
//
//   node streamdeck/tools/fake-deck.js
//
// It starts a WebSocket server, launches the plugin against it, sends
// willAppear / keyDown the way the Stream Deck app does, and checks that:
//   * the plugin registers,
//   * a strip key renders the mixer's strip name and mute state,
//   * pressing it mutes the mixer strip (checked through the mixer's own API),
//   * Mute All mutes every strip.
//
// The mixer must be running with the control API on 127.0.0.1:8765 and the
// token in %APPDATA%\Audio Mixer Pro\control.token (i.e. start the mixer as the
// same user, or point APPDATA at the temp folder the mixer wrote to).
"use strict";

const crypto = require("node:crypto");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const PLUGIN = path.join(__dirname, "..", "com.jaygee.audiomixerpro.sdPlugin", "bin", "plugin.js");
const BASE = "http://127.0.0.1:8765";
const ACTION_MUTE = "com.jaygee.audiomixerpro.mute";
const ACTION_MUTE_ALL = "com.jaygee.audiomixerpro.mute-all";

function token() {
  const base = process.env.APPDATA || path.join(os.homedir(), "AppData", "Roaming");
  return fs.readFileSync(path.join(base, "Audio Mixer Pro", "control.token"), "utf8").trim();
}

async function mixerState() {
  const response = await fetch(`${BASE}/state`, {
    headers: { "X-AudioMixer-Token": token() },
    signal: AbortSignal.timeout(2500),
  });
  if (!response.ok) {
    throw new Error(`/state returned ${response.status}`);
  }
  return response.json();
}

/** The mixer can still be enumerating devices when the harness starts. */
async function waitForMixer(attempts = 20) {
  let lastError = null;
  for (let attempt = 0; attempt < attempts; attempt++) {
    try {
      return await mixerState();
    } catch (err) {
      lastError = err;
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  }
  throw lastError || new Error("mixer did not answer");
}

// --- minimal WebSocket server (text frames only, server -> client unmasked) --

const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

function encodeText(text) {
  const payload = Buffer.from(text, "utf8");
  const length = payload.length;
  let header;
  if (length < 126) {
    header = Buffer.from([0x81, length]);
  } else if (length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x81;
    header[1] = 126;
    header.writeUInt16BE(length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x81;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(length), 2);
  }
  return Buffer.concat([header, payload]);
}

/** Extract complete client frames from a buffer; returns [messages, rest]. */
function readFrames(buffer) {
  const messages = [];
  let rest = buffer;
  while (rest.length >= 2) {
    const opcode = rest[0] & 0x0f;
    const masked = (rest[1] & 0x80) !== 0;
    let length = rest[1] & 0x7f;
    let offset = 2;
    if (length === 126) {
      if (rest.length < 4) break;
      length = rest.readUInt16BE(2);
      offset = 4;
    } else if (length === 127) {
      if (rest.length < 10) break;
      length = Number(rest.readBigUInt64BE(2));
      offset = 10;
    }
    const maskOffset = offset;
    if (masked) offset += 4;
    if (rest.length < offset + length) break;
    const payload = Buffer.from(rest.subarray(offset, offset + length));
    if (masked) {
      const mask = rest.subarray(maskOffset, maskOffset + 4);
      for (let i = 0; i < payload.length; i++) {
        payload[i] ^= mask[i % 4];
      }
    }
    rest = rest.subarray(offset + length);
    if (opcode === 1) {
      messages.push(payload.toString("utf8"));
    } else if (opcode === 8) {
      messages.push(null); // close
    }
  }
  return [messages, rest];
}

function startServer() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      resolve({ server, port: server.address().port });
    });
  });
}

function handleClient(socket, onMessage) {
  let buffer = Buffer.alloc(0);
  let upgraded = false;
  socket.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    if (!upgraded) {
      const end = buffer.indexOf("\r\n\r\n");
      if (end < 0) return;
      const head = buffer.subarray(0, end).toString("latin1");
      buffer = buffer.subarray(end + 4);
      const key = /sec-websocket-key:\s*(.+)/i.exec(head);
      if (!key) {
        socket.destroy();
        return;
      }
      const accept = crypto
        .createHash("sha1")
        .update(key[1].trim() + WS_GUID)
        .digest("base64");
      socket.write(
        "HTTP/1.1 101 Switching Protocols\r\n" +
          "Upgrade: websocket\r\n" +
          "Connection: Upgrade\r\n" +
          `Sec-WebSocket-Accept: ${accept}\r\n\r\n`
      );
      upgraded = true;
      socket.sendJson = (object) => socket.write(encodeText(JSON.stringify(object)));
    }
    const [messages, rest] = readFrames(buffer);
    buffer = rest;
    for (const message of messages) {
      if (message !== null) {
        onMessage(message);
      }
    }
  });
}

// --- the test itself -------------------------------------------------------

const received = [];
const waiters = [];

function waitFor(predicate, label, timeoutMs = 4000) {
  const existing = received.find(predicate);
  if (existing) return Promise.resolve(existing);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`timed out waiting for ${label}`));
    }, timeoutMs);
    waiters.push({ predicate, label, resolve, timer });
  });
}

function deliver(message) {
  received.push(message);
  for (let i = waiters.length - 1; i >= 0; i--) {
    if (waiters[i].predicate(message)) {
      clearTimeout(waiters[i].timer);
      waiters[i].resolve(message);
      waiters.splice(i, 1);
    }
  }
}

async function main() {
  const check = [];
  const report = (ok, label) => {
    check.push({ ok, label });
    console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  };

  const before = await waitForMixer();
  const target = before.strips[1] || before.strips[0];
  console.log(`mixer: ${before.strips.length} strips, test target #${target.number} ${target.name}`);

  const { server, port } = await startServer();
  let socket = null;
  server.on("connection", (connection) => {
    socket = connection;
    handleClient(connection, (raw) => {
      let message;
      try {
        message = JSON.parse(raw);
      } catch {
        return;
      }
      deliver(message);
    });
  });

  const child = spawn(process.execPath, [PLUGIN, "-port", String(port), "-pluginUUID", "TESTUUID", "-registerEvent", "registerPlugin"], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", (data) => process.stdout.write(`  plugin: ${data}`));
  child.stderr.on("data", (data) => process.stdout.write(`  plugin!: ${data}`));

  try {
    const register = await waitFor((m) => m.event === "registerPlugin", "registration");
    report(register.uuid === "TESTUUID", "plugin registers with the uuid it was given");

    socket.sendJson({
      event: "willAppear",
      action: ACTION_MUTE,
      context: "key-strip",
      device: "TEST",
      payload: { settings: { strip: target.number }, coordinates: { column: 0, row: 0 }, controller: "Keypad", isInMultiAction: false },
    });
    const title = await waitFor(
      (m) => m.event === "setTitle" && m.context === "key-strip" && m.payload.title === target.name,
      "strip title"
    );
    report(title.payload.title === target.name, `key title shows "${title.payload.title}" (mixer name)`);
    const initial = await waitFor((m) => m.event === "setState" && m.context === "key-strip", "initial state");
    report(initial.payload.state === 0, "unmuted strip renders in state 0");

    socket.sendJson({
      event: "keyDown",
      action: ACTION_MUTE,
      context: "key-strip",
      device: "TEST",
      payload: { settings: { strip: target.number }, coordinates: { column: 0, row: 0 }, controller: "Keypad", isInMultiAction: false },
    });
    await waitFor((m) => m.event === "setState" && m.context === "key-strip" && m.payload.state === 1, "optimistic light");
    report(true, "press lights the key immediately (optimistic state 1)");

    await new Promise((resolve) => setTimeout(resolve, 400));
    const muted = await mixerState();
    const after = muted.strips.find((strip) => strip.number === target.number);
    report(after.mute === true, `mixer strip #${target.number} is muted through the API`);

    socket.sendJson({
      event: "willAppear",
      action: ACTION_MUTE_ALL,
      context: "key-all",
      device: "TEST",
      payload: { settings: {}, coordinates: { column: 1, row: 0 }, controller: "Keypad", isInMultiAction: false },
    });
    // One strip is muted at this point, so the first press releases all of
    // them - that is the button's documented behaviour.
    socket.sendJson({
      event: "keyDown",
      action: ACTION_MUTE_ALL,
      context: "key-all",
      device: "TEST",
      payload: { settings: {}, coordinates: { column: 1, row: 0 }, controller: "Keypad", isInMultiAction: false },
    });
    await new Promise((resolve) => setTimeout(resolve, 500));
    const released = await mixerState();
    report(
      released.strips.every((strip) => strip.mute === false),
      "Mute All releases everything when a strip is already muted"
    );

    // Pressing it again (nothing muted now) mutes everything.
    socket.sendJson({
      event: "keyDown",
      action: ACTION_MUTE_ALL,
      context: "key-all",
      device: "TEST",
      payload: { settings: {}, coordinates: { column: 1, row: 0 }, controller: "Keypad", isInMultiAction: false },
    });
    await new Promise((resolve) => setTimeout(resolve, 500));
    const all = await mixerState();
    report(all.strips.every((strip) => strip.mute), "Mute All mutes every strip");

    // Leave the mixer as we found it.
    await fetch(`${BASE}/command`, {
      method: "POST",
      headers: { "X-AudioMixer-Token": token(), "Content-Type": "application/json" },
      body: JSON.stringify({ cmd: "mute.all", value: false }),
      signal: AbortSignal.timeout(2500),
    });
    await new Promise((resolve) => setTimeout(resolve, 300));
    const restored = await mixerState();
    report(restored.strips.every((strip) => strip.mute === false), "unmute-all restores the scene");
  } finally {
    child.kill();
    server.close();
  }

  const failed = check.filter((entry) => !entry.ok).length;
  console.log(failed === 0 ? "\nall checks passed" : `\n${failed} check(s) failed`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((err) => {
  const cause = err && err.cause ? ` (${err.cause.code || err.cause.message || err.cause})` : "";
  console.error(`harness error: ${err && err.message}${cause}`);
  process.exit(1);
});
