#!/usr/bin/env node
// Audio Mixer PRO - Stream Deck plugin.
//
// No npm dependencies: the Stream Deck app runs this with its own Node 24
// runtime, which has a global WebSocket, fetch and AbortSignal. That keeps the
// packaged plugin tiny and means an end user installs nothing.
//
// Two halves, as Stream Deck requires:
//   * this file talks to the Stream Deck app over the WebSocket it is launched
//     with (`-port`, `-pluginUUID`, `-registerEvent`);
//   * it talks to the mixer over the loopback control API in `control.rs`,
//     authenticating with the token the mixer writes next to its scene.
"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const BASE_URL = "http://127.0.0.1:8765";
const ACTION_MUTE = "com.jaygee.audiomixerpro.mute";
const ACTION_MUTE_ALL = "com.jaygee.audiomixerpro.mute-all";
const POLL_MS = 250;
const OFFLINE_POLL_MS = 1000;

// ---------------------------------------------------------------------------
// Stream Deck connection
// ---------------------------------------------------------------------------

function arg(name) {
  const at = process.argv.indexOf(name);
  return at >= 0 ? process.argv[at + 1] : null;
}

const port = arg("-port");
const pluginUUID = arg("-pluginUUID");
const registerEvent = arg("-registerEvent");

if (!port || !pluginUUID || !registerEvent) {
  console.error("not launched by Stream Deck (missing -port/-pluginUUID/-registerEvent)");
  process.exit(1);
}

let socket = null;
/** context -> { action, settings } */
const actions = new Map();
/** Action contexts whose property inspector is currently open. */
const inspectors = new Set();
/** Last /state snapshot: { strips: [...] } */
let strips = [];
/** null = unknown, true/false once a poll has succeeded/failed. */
let online = null;

function send(message) {
  if (socket && socket.readyState === 1) {
    socket.send(JSON.stringify(message));
  }
}

function connect() {
  socket = new WebSocket(`ws://127.0.0.1:${port}`);
  socket.onopen = () => {
    send({ event: registerEvent, uuid: pluginUUID });
    log("registered with Stream Deck");
    poll();
  };
  socket.onmessage = (raw) => {
    let message;
    try {
      message = JSON.parse(raw.data);
    } catch {
      return;
    }
    handle(message);
  };
  socket.onclose = () => {
    log("Stream Deck connection closed");
    process.exit(0);
  };
  socket.onerror = (err) => log(`stream deck socket error: ${err && err.message}`);
}

function log(message) {
  console.log(`[audiomixerpro] ${message}`);
}

// ---------------------------------------------------------------------------
// Mixer control API
// ---------------------------------------------------------------------------

function tokenPath() {
  const base = process.env.APPDATA || path.join(os.homedir(), "AppData", "Roaming");
  return path.join(base, "Audio Mixer Pro", "control.token");
}

function readToken() {
  try {
    return fs.readFileSync(tokenPath(), "utf8").trim();
  } catch {
    return null;
  }
}

async function api(route, options = {}) {
  const token = readToken();
  if (!token) {
    throw new Error("control.token missing - is the mixer running?");
  }
  const headers = { "X-AudioMixer-Token": token };
  if (options.body) {
    headers["Content-Type"] = "application/json";
  }
  const response = await fetch(BASE_URL + route, {
    ...options,
    headers,
    signal: AbortSignal.timeout(1500),
  });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} from ${route}`);
  }
  return response.json();
}

async function postCommand(body) {
  await api("/command", { method: "POST", body: JSON.stringify(body) });
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function stripNumber(settings) {
  const number = Number(settings && settings.strip);
  return Number.isFinite(number) && number >= 1 ? Math.floor(number) : 1;
}

function findStrip(number) {
  return strips.find((strip) => strip.number === number) || null;
}

function describe(context) {
  const entry = actions.get(context);
  if (!entry) {
    return null;
  }
  if (entry.action === ACTION_MUTE_ALL) {
    const muted = strips.some((strip) => strip.mute);
    return { muted, title: "MUTE ALL" };
  }
  const number = stripNumber(entry.settings);
  const strip = findStrip(number);
  return {
    muted: strip ? strip.mute : false,
    title: strip ? strip.name : `STRIP ${number}`,
  };
}

function render(context) {
  const info = describe(context);
  if (!info) {
    return;
  }
  // State 0 = live/grey artwork, state 1 = amber artwork. While the first poll
  // is still in flight, show the strip number rather than a scary OFFLINE: the
  // key is only "offline" once a poll has actually failed.
  const state = online && info.muted ? 1 : 0;
  const title = online === false ? "OFFLINE" : info.title;
  send({ event: "setState", context, payload: { state } });
  send({
    event: "setTitle",
    context,
    payload: { title, target: 0 },
  });
}

function renderAll() {
  for (const context of actions.keys()) {
    render(context);
  }
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

let timer = null;

function schedule(ms) {
  if (timer) {
    clearTimeout(timer);
  }
  timer = setTimeout(poll, ms);
}

async function poll() {
  if (actions.size === 0) {
    // No keys of ours on screen: almost idle, just stay connected for the
    // next time one appears.
    schedule(OFFLINE_POLL_MS);
    return;
  }
  try {
    const state = await api("/state");
    strips = Array.isArray(state.strips) ? state.strips : [];
    if (online !== true) {
      online = true;
      log(`connected to the mixer (${strips.length} strips)`);
    }
    renderAll();
    // An inspector opened before the first poll would otherwise keep its
    // generic "Strip 1..16" list for the whole session.
    for (const context of inspectors) {
      sendStrips(context, actions.get(context)?.action);
    }
    schedule(POLL_MS);
  } catch (err) {
    if (online !== false) {
      online = false;
      log(`mixer offline: ${err.message}`);
    }
    renderAll();
    schedule(OFFLINE_POLL_MS);
  }
}

// ---------------------------------------------------------------------------
// Stream Deck events
// ---------------------------------------------------------------------------

function handle(message) {
  const { event, action, context, payload } = message;
  switch (event) {
    case "willAppear":
      actions.set(context, { action, settings: payload.settings || {} });
      render(context);
      schedule(0);
      break;
    case "willDisappear":
      actions.delete(context);
      break;
    case "didReceiveSettings":
      if (actions.has(context)) {
        actions.get(context).settings = payload.settings || {};
        render(context);
      }
      break;
    case "keyDown":
      onKeyDown(context, action, payload);
      break;
    case "propertyInspectorDidAppear":
      // Hand the inspector the strip names so its picker reads like the
      // console instead of "1..16".
      inspectors.add(context);
      sendStrips(context, action);
      if (strips.length === 0) {
        schedule(0);
      }
      break;
    case "propertyInspectorDidDisappear":
      inspectors.delete(context);
      break;
    default:
      break;
  }
}

async function onKeyDown(context, action, payload) {
  const settings = (payload && payload.settings) || {};
  const entry = actions.get(context) || { action, settings };
  entry.settings = settings;
  actions.set(context, entry);

  if (action === ACTION_MUTE_ALL) {
    const value = !strips.some((strip) => strip.mute);
    // Optimistic: the light must not wait for a round trip.
    strips = strips.map((strip) => ({ ...strip, mute: value }));
    renderAll();
    try {
      await postCommand({ cmd: "mute.all", value });
    } catch (err) {
      log(`mute.all failed: ${err.message}`);
      send({ event: "showAlert", context });
    }
  } else {
    const number = stripNumber(settings);
    const strip = findStrip(number);
    const value = strip ? !strip.mute : true;
    if (strip) {
      strip.mute = value;
      render(context);
    }
    try {
      await postCommand({ cmd: "strip.mute", strip: number, value });
    } catch (err) {
      log(`strip.mute ${number} failed: ${err.message}`);
      send({ event: "showAlert", context });
      online = false;
      render(context);
    }
  }
  schedule(120);
}

function sendStrips(context, action) {
  send({
    event: "sendToPropertyInspector",
    action,
    context,
    payload: {
      strips: strips.map((strip) => ({ number: strip.number, name: strip.name })),
    },
  });
}

connect();
