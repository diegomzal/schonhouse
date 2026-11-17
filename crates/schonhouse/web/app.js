// Schonhouse web UI: renders device cards from the SSE stream and sends commands
// through the REST API. No build step, no dependencies.

const grid = document.getElementById("devices");
const emptyNote = document.getElementById("empty");
const connection = document.getElementById("connection");
const toastEl = document.getElementById("toast");
const template = document.getElementById("card");
const logList = document.getElementById("log");

const cards = new Map(); // id -> { el, snapshot }
const dragging = new Set(); // "id/field" while the user holds a slider
const pendingTimers = new Map(); // "id/field" -> throttle timer id
const THROTTLE_MS = 180;
const LOG_LIMIT = 300;

const KIND_LABELS = { "tapo-l535": "Tapo L535" };
const SWATCHES = [
  ["Red", 0], ["Orange", 30], ["Yellow", 60], ["Green", 120],
  ["Cyan", 180], ["Blue", 240], ["Purple", 280], ["Pink", 330],
];

const stats = { events: 0, lastEvent: null, loadedAt: Date.now(), sse: "connecting" };
let serverInfo = null;

// ---------------------------------------------------------------- helpers

function showToast(message, ok = false) {
  toastEl.textContent = message;
  toastEl.className = ok ? "toast ok" : "toast";
  toastEl.hidden = false;
  clearTimeout(showToast.timer);
  showToast.timer = setTimeout(() => (toastEl.hidden = true), 3500);
}

async function api(path, options = {}) {
  const res = await fetch(path, options);
  if (!res.ok) {
    const payload = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(payload.error || res.statusText);
  }
  return res.status === 202 ? null : res.json();
}

async function sendCommand(id, body) {
  try {
    render(await api(`/api/devices/${encodeURIComponent(id)}/state`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }));
  } catch (err) {
    showToast(`${nameOf(id)}: ${err.message}`);
  }
}

function nameOf(id) {
  return cards.get(id)?.snapshot?.name || id;
}

// Sliders fire many events while dragging: send at most one command per THROTTLE_MS,
// and always send the final value on "change".
function throttled(key, fn) {
  if (pendingTimers.has(key)) return;
  pendingTimers.set(key, setTimeout(() => {
    pendingTimers.delete(key);
    fn();
  }, THROTTLE_MS));
}

function clamp(input) {
  const v = Number(input.value);
  if (Number.isNaN(v)) return Number(input.min);
  return Math.min(Number(input.max), Math.max(Number(input.min), Math.round(v)));
}

function formatValue(field, value) {
  switch (field) {
    case "color_temp": return `${value} K`;
    case "hue": return `${value}°`;
    default: return `${value}%`;
  }
}

function formatDuration(secs) {
  if (secs == null) return "–";
  const d = Math.floor(secs / 86400), h = Math.floor((secs % 86400) / 3600), m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d}d ${h}h ${m}m`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m ${secs % 60}s`;
}

function formatTime(unixSecs) {
  if (!unixSecs) return "–";
  return new Date(unixSecs * 1000).toLocaleString();
}

function clock(unixSecs) {
  return new Date(unixSecs * 1000).toLocaleTimeString([], { hour12: false });
}

function hsl(h, s = 100, l = 50) {
  return `hsl(${h} ${s}% ${l}%)`;
}

function kelvinToCss(k) {
  // Rough visual approximation, enough for a preview dot.
  const t = (Math.min(6500, Math.max(2500, k)) - 2500) / 4000;
  const r = 255, g = Math.round(170 + 70 * t), b = Math.round(90 + 165 * t);
  return `rgb(${r} ${g} ${b})`;
}

function setFacts(dl, rows) {
  dl.replaceChildren();
  for (const [label, value, cls] of rows) {
    const dt = document.createElement("dt");
    dt.textContent = label;
    const dd = document.createElement("dd");
    if (value instanceof Node) dd.appendChild(value); else dd.textContent = value ?? "–";
    if (cls) dd.className = cls;
    dl.append(dt, dd);
  }
}

function signalBars(level) {
  const span = document.createElement("span");
  span.className = "signal";
  for (let i = 1; i <= 3; i++) {
    const bar = document.createElement("i");
    if (i <= level) bar.classList.add("lit");
    span.appendChild(bar);
  }
  return span;
}

// ---------------------------------------------------------------- device cards

function commandFor(card, field) {
  const value = clamp(card.querySelector(`input[type="range"][name="${field}"]`));
  if (field === "hue" || field === "saturation") {
    return {
      hue: clamp(card.querySelector('input[type="range"][name="hue"]')),
      saturation: clamp(card.querySelector('input[type="range"][name="saturation"]')),
    };
  }
  return { [field]: value };
}

function createCard(snapshot) {
  const el = template.content.firstElementChild.cloneNode(true);
  const id = snapshot.id;
  el.dataset.id = id;

  el.querySelector(".power").addEventListener("click", () => {
    const on = cards.get(id)?.snapshot?.state?.on ?? false;
    sendCommand(id, { on: !on });
  });

  // Only show the controls this device supports.
  for (const control of el.querySelectorAll(".control")) {
    control.hidden = !snapshot.capabilities.includes(control.dataset.cap);
  }

  for (const range of el.querySelectorAll('input[type="range"]')) {
    const field = range.name;
    const key = `${id}/${field}`;
    const number = el.querySelector(`input[type="number"][name="${field}"]`);
    const start = () => dragging.add(key);
    const stop = () => dragging.delete(key);
    range.addEventListener("pointerdown", start);
    range.addEventListener("keydown", start);
    range.addEventListener("pointerup", stop);
    range.addEventListener("pointercancel", stop);
    range.addEventListener("blur", stop);
    range.addEventListener("input", () => {
      number.value = range.value;
      if (field === "hue") updateSatTrack(el, range.value);
      throttled(key, () => sendCommand(id, commandFor(el, field)));
    });
    range.addEventListener("change", () => {
      stop();
      clearTimeout(pendingTimers.get(key));
      pendingTimers.delete(key);
      sendCommand(id, commandFor(el, field));
    });
    // Typed values: apply on Enter or when leaving the field.
    number.addEventListener("keydown", (e) => { if (e.key === "Enter") number.blur(); });
    number.addEventListener("change", () => {
      number.value = clamp(number);
      range.value = number.value;
      sendCommand(id, commandFor(el, field));
    });
  }

  for (const chip of el.querySelectorAll(".chips button")) {
    chip.addEventListener("click", () => sendCommand(id, { color_temp: Number(chip.dataset.value) }));
  }

  const swatches = el.querySelector(".swatches");
  for (const [label, hue] of SWATCHES) {
    const b = document.createElement("button");
    b.type = "button";
    b.title = `${label} (${hue}°)`;
    b.style.background = hsl(hue);
    b.addEventListener("click", () => sendCommand(id, { hue, saturation: 100 }));
    swatches.appendChild(b);
  }

  const output = el.querySelector(".output");
  for (const button of el.querySelectorAll(".actions button")) {
    button.addEventListener("click", () => deviceAction(id, button.dataset.action, output, button));
  }

  grid.appendChild(el);
  return el;
}

function updateSatTrack(card, hue) {
  card.querySelector('input[type="range"][name="saturation"]').style.setProperty("--sat-color", hsl(hue));
}

async function deviceAction(id, action, output, button) {
  const base = `/api/devices/${encodeURIComponent(id)}`;
  button.disabled = true;
  try {
    if (action === "refresh") {
      render(await api(`${base}/refresh`, { method: "POST" }));
      showToast(`${nameOf(id)}: refreshed`, true);
    } else if (action === "usage" || action === "raw") {
      const data = await api(`${base}/${action}`);
      output.textContent = JSON.stringify(data, null, 2);
      output.hidden = false;
    } else if (action === "reboot") {
      if (!confirm(`Reboot "${nameOf(id)}"? It will be unavailable for a few seconds.`)) return;
      await api(`${base}/reboot`, { method: "POST" });
      showToast(`${nameOf(id)}: reboot requested`, true);
    }
  } catch (err) {
    showToast(`${nameOf(id)}: ${err.message}`);
  } finally {
    button.disabled = false;
  }
}

function setValue(card, id, field, value) {
  if (value === null || value === undefined) return;
  const range = card.querySelector(`input[type="range"][name="${field}"]`);
  const number = card.querySelector(`input[type="number"][name="${field}"]`);
  if (dragging.has(`${id}/${field}`) || document.activeElement === number) return;
  range.value = value;
  number.value = value;
  if (field === "hue") updateSatTrack(card, value);
}

function render(snapshot) {
  let entry = cards.get(snapshot.id);
  if (!entry) {
    entry = { el: createCard(snapshot), snapshot };
    cards.set(snapshot.id, entry);
  }
  entry.snapshot = snapshot;
  const card = entry.el;
  const { state, info } = snapshot;

  card.classList.toggle("offline", !snapshot.online);
  card.classList.toggle("on", Boolean(state?.on));
  card.querySelector(".name").textContent = snapshot.name;
  card.querySelector(".model").textContent = info?.model || KIND_LABELS[snapshot.kind] || snapshot.kind;
  card.querySelector(".ip").textContent = snapshot.ip;

  const dot = card.querySelector(".dot");
  dot.className = `dot ${snapshot.online ? "online" : snapshot.error ? "offline" : "connecting"}`;
  dot.title = snapshot.online ? "Online" : snapshot.error ? "Offline" : "Connecting";

  const summary = card.querySelector(".summary");
  let preview = summary.querySelector(".preview");
  if (!preview) {
    preview = document.createElement("span");
    preview.className = "preview";
    summary.appendChild(preview);
  }
  let text;
  let color = null;
  if (!snapshot.online) {
    text = snapshot.error ? (snapshot.error === "rebooting" ? "rebooting…" : "offline") : "connecting…";
  } else if (!state) {
    text = "no state yet";
  } else if (!state.on) {
    text = "off";
  } else if (state.color_temp > 0) {
    text = `on · white ${state.color_temp} K · ${state.brightness}%`;
    color = kelvinToCss(state.color_temp);
  } else {
    text = `on · color ${state.hue ?? 0}° / ${state.saturation ?? 0}% · ${state.brightness}%`;
    color = hsl(state.hue ?? 0, state.saturation ?? 100, 50);
  }
  summary.firstChild?.nodeType === Node.TEXT_NODE ? (summary.firstChild.textContent = text) : summary.prepend(text);
  preview.classList.toggle("visible", Boolean(color));
  if (color) preview.style.background = color;

  if (state) {
    setValue(card, snapshot.id, "brightness", state.brightness);
    if (state.color_temp > 0) setValue(card, snapshot.id, "color_temp", state.color_temp);
    setValue(card, snapshot.id, "hue", state.hue);
    setValue(card, snapshot.id, "saturation", state.saturation);
  }
  for (const input of card.querySelectorAll(".controls input, .controls button")) input.disabled = !snapshot.online;
  card.querySelector(".power").disabled = !snapshot.online;

  const error = card.querySelector(".error");
  const errText = snapshot.error && snapshot.error !== "rebooting" ? snapshot.error : "";
  error.hidden = !errText;
  error.textContent = errText.length > 160 ? `${errText.slice(0, 160)}…` : errText;
  error.title = errText;

  const rows = [
    ["Status", snapshot.online ? "online" : "offline"],
    ["Name", info?.nickname || "–"],
    ["Model", info ? `${info.model} (${info.device_type})` : KIND_LABELS[snapshot.kind] || snapshot.kind],
    ["Firmware", info?.fw_ver],
    ["Hardware", info?.hw_ver],
    ["IP", snapshot.ip, "mono"],
    ["MAC", info?.mac, "mono"],
    ["Wi-Fi", info?.ssid],
  ];
  if (info) {
    const wrap = document.createElement("span");
    wrap.append(signalBars(info.signal_level), `${info.rssi} dBm`);
    rows.push(["Signal", wrap]);
    rows.push(["On for", state?.on ? formatDuration(info.on_time) : "–"]);
    rows.push(["Overheated", info.overheated ? "yes" : "no"]);
    if (info.region) rows.push(["Region", info.region]);
    rows.push(["Device id", info.device_id, "mono"]);
  }
  rows.push(["Last seen", formatTime(snapshot.last_seen)]);
  rows.push(["Updated", formatTime(snapshot.updated_at)]);
  setFacts(card.querySelector(".details .facts"), rows);

  emptyNote.hidden = cards.size > 0;
}

// ---------------------------------------------------------------- system tab

function renderServerInfo() {
  const dl = document.getElementById("server-info");
  if (!serverInfo) {
    setFacts(dl, [["Status", "unavailable"]]);
    return;
  }
  const uptime = serverInfo.uptime_secs + Math.floor((Date.now() - serverInfo.fetchedAt) / 1000);
  setFacts(dl, [
    ["Version", `${serverInfo.version}${serverInfo.debug_build ? " (debug build)" : ""}`],
    ["Uptime", formatDuration(uptime)],
    ["Started", formatTime(serverInfo.started_at)],
    ["Platform", `${serverInfo.os} / ${serverInfo.arch}`],
    ["Listening on", serverInfo.bind, "mono"],
    ["Config file", serverInfo.config_path, "mono"],
    ["Poll interval", `${serverInfo.poll_interval_secs} s`],
    ["Devices", String(serverInfo.device_count)],
  ]);
}

function renderConnection() {
  setFacts(document.getElementById("conn-info"), [
    ["Event stream", stats.sse],
    ["Events received", String(stats.events)],
    ["Last event", stats.lastEvent ? new Date(stats.lastEvent).toLocaleTimeString() : "–"],
    ["Page loaded", new Date(stats.loadedAt).toLocaleTimeString()],
    ["Page URL", location.host, "mono"],
  ]);
}

async function loadServerInfo() {
  try {
    serverInfo = { ...(await api("/api/info")), fetchedAt: Date.now() };
  } catch {
    serverInfo = null;
  }
  renderServerInfo();
}

function addLogEntry(entry, prepend = true) {
  const li = document.createElement("li");
  const ts = document.createElement("span"); ts.className = "ts"; ts.textContent = clock(entry.ts);
  const lvl = document.createElement("span"); lvl.className = `lvl ${entry.level}`; lvl.textContent = entry.level;
  const dev = document.createElement("span"); dev.className = "dev"; dev.textContent = entry.device ? nameOf(entry.device) : "server";
  const msg = document.createElement("span"); msg.className = "msg"; msg.textContent = entry.message;
  li.append(ts, lvl, dev, msg);
  if (prepend) logList.prepend(li); else logList.appendChild(li);
  while (logList.children.length > LOG_LIMIT) logList.lastChild.remove();
}

async function loadLog() {
  try {
    const entries = await api("/api/log");
    logList.replaceChildren();
    for (const entry of entries.slice().reverse()) addLogEntry(entry, false);
  } catch (err) {
    showToast(`log: ${err.message}`);
  }
}

async function loadRawAll() {
  try {
    document.getElementById("raw-all").textContent = JSON.stringify(await api("/api/devices"), null, 2);
  } catch (err) {
    showToast(`snapshots: ${err.message}`);
  }
}

document.getElementById("reload-info").addEventListener("click", loadServerInfo);
document.getElementById("reload-raw").addEventListener("click", loadRawAll);
document.getElementById("clear-log").addEventListener("click", () => logList.replaceChildren());

for (const tab of document.querySelectorAll(".tabs button")) {
  tab.addEventListener("click", () => {
    for (const t of document.querySelectorAll(".tabs button")) t.classList.toggle("active", t === tab);
    for (const s of document.querySelectorAll(".tab")) s.classList.toggle("active", s.id === `tab-${tab.dataset.tab}`);
    if (tab.dataset.tab === "system") loadRawAll();
    try { localStorage.setItem("schonhouse.tab", tab.dataset.tab); } catch {}
  });
}
try {
  const saved = localStorage.getItem("schonhouse.tab");
  if (saved === "system") document.querySelector('.tabs button[data-tab="system"]').click();
} catch {}

setInterval(() => { renderServerInfo(); renderConnection(); }, 1000);

// ---------------------------------------------------------------- live stream

function connect() {
  const source = new EventSource("/api/events");
  source.addEventListener("open", () => {
    stats.sse = "live";
    connection.textContent = "live";
    connection.className = "pill ok";
  });
  source.addEventListener("device", (event) => {
    stats.events++;
    stats.lastEvent = Date.now();
    render(JSON.parse(event.data));
  });
  source.addEventListener("log", (event) => {
    stats.events++;
    stats.lastEvent = Date.now();
    addLogEntry(JSON.parse(event.data));
  });
  source.addEventListener("error", () => {
    // EventSource reconnects on its own; just reflect it in the header.
    stats.sse = "reconnecting";
    connection.textContent = "reconnecting…";
    connection.className = "pill bad";
  });
}

Promise.all([
  api("/api/devices").then((list) => { list.forEach(render); emptyNote.hidden = list.length > 0; }).catch(() => {}),
  loadServerInfo(),
  loadLog(),
]).finally(() => {
  renderConnection();
  connect();
});
