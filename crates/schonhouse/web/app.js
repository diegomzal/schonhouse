// Schonhouse web UI: renders devices, groups and scenes from the REST API and the SSE
// stream, and sends commands back. No build step, no dependencies.

const grid = document.getElementById("devices");
const groupsGrid = document.getElementById("groups");
const scenesBar = document.getElementById("scenes");
const emptyNote = document.getElementById("empty");
const connection = document.getElementById("connection");
const toastEl = document.getElementById("toast");
const template = document.getElementById("card");
const groupTemplate = document.getElementById("group-card");
const logList = document.getElementById("log");

const cards = new Map(); // device id -> { el, snapshot }
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
let groups = [];
let scenes = [];

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
  return res.status === 202 || res.status === 204 ? null : res.json();
}

function jsonBody(method, body) {
  return { method, headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) };
}

async function sendCommand(id, body) {
  try {
    render(await api(`/api/devices/${encodeURIComponent(id)}/state`, jsonBody("POST", body)));
  } catch (err) {
    showToast(`${nameOf(id)}: ${err.message}`);
  }
}

/// Batch endpoints answer 200 with per-device results; surface the failures.
function reportBatch(label, response) {
  const failed = (response?.results || []).filter((r) => !r.ok);
  for (const r of response?.results || []) if (r.ok && r.snapshot) render(r.snapshot);
  if (failed.length) {
    showToast(`${label}: ${failed.map((r) => `${nameOf(r.id)} (${r.error})`).join(", ")}`);
  } else {
    showToast(`${label}: done`, true);
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

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  Object.assign(node, props);
  node.append(...children);
  return node;
}

// Links a range + number pair: typing or dragging keeps both in sync and calls `send`.
function bindPair(container, field, send) {
  const range = container.querySelector(`input[type="range"][name="${field}"]`);
  const number = container.querySelector(`input[type="number"][name="${field}"]`);
  const key = `${container.dataset.key}/${field}`;
  const start = () => dragging.add(key);
  const stop = () => dragging.delete(key);
  range.addEventListener("pointerdown", start);
  range.addEventListener("keydown", start);
  range.addEventListener("pointerup", stop);
  range.addEventListener("pointercancel", stop);
  range.addEventListener("blur", stop);
  range.addEventListener("input", () => {
    number.value = range.value;
    throttled(key, () => send(Number(range.value)));
  });
  range.addEventListener("change", () => {
    stop();
    clearTimeout(pendingTimers.get(key));
    pendingTimers.delete(key);
    send(Number(range.value));
  });
  number.addEventListener("keydown", (e) => { if (e.key === "Enter") number.blur(); });
  number.addEventListener("change", () => {
    number.value = clamp(number);
    range.value = number.value;
    send(Number(number.value));
  });
}

// ---------------------------------------------------------------- device cards

function commandFor(card, field) {
  if (field === "hue" || field === "saturation") {
    return {
      hue: clamp(card.querySelector('input[type="range"][name="hue"]')),
      saturation: clamp(card.querySelector('input[type="range"][name="saturation"]')),
    };
  }
  return { [field]: clamp(card.querySelector(`input[type="range"][name="${field}"]`)) };
}

function createCard(snapshot) {
  const card = template.content.firstElementChild.cloneNode(true);
  const id = snapshot.id;
  card.dataset.id = id;
  card.dataset.key = id;

  card.querySelector(".power").addEventListener("click", () => {
    const on = cards.get(id)?.snapshot?.state?.on ?? false;
    sendCommand(id, { on: !on });
  });

  // Only show the controls this device supports.
  for (const control of card.querySelectorAll(".control")) {
    control.hidden = !snapshot.capabilities.includes(control.dataset.cap);
  }

  for (const field of ["brightness", "color_temp", "hue", "saturation"]) {
    bindPair(card, field, () => {
      if (field === "hue") updateSatTrack(card, card.querySelector('input[type="range"][name="hue"]').value);
      sendCommand(id, commandFor(card, field));
    });
  }
  card.querySelector('input[type="range"][name="hue"]').addEventListener("input", (e) => updateSatTrack(card, e.target.value));

  for (const chip of card.querySelectorAll(".chips button")) {
    chip.addEventListener("click", () => sendCommand(id, { color_temp: Number(chip.dataset.value) }));
  }

  const swatches = card.querySelector(".swatches");
  for (const [label, hue] of SWATCHES) {
    const b = el("button", { type: "button", title: `${label} (${hue}°)` });
    b.style.background = hsl(hue);
    b.addEventListener("click", () => sendCommand(id, { hue, saturation: 100 }));
    swatches.appendChild(b);
  }

  const output = card.querySelector(".output");
  for (const button of card.querySelectorAll(".actions button, .rename button")) {
    button.addEventListener("click", () => deviceAction(id, button.dataset.action, card, output, button));
  }
  card.querySelector(".rename-input").addEventListener("keydown", (e) => {
    if (e.key === "Enter") deviceAction(id, "rename", card, output, e.target);
  });

  grid.appendChild(card);
  return card;
}

function updateSatTrack(card, hue) {
  card.querySelector('input[type="range"][name="saturation"]').style.setProperty("--sat-color", hsl(hue));
}

async function deviceAction(id, action, card, output, button) {
  const base = `/api/devices/${encodeURIComponent(id)}`;
  button.disabled = true;
  try {
    if (action === "refresh") {
      render(await api(`${base}/refresh`, { method: "POST" }));
      showToast(`${nameOf(id)}: refreshed`, true);
    } else if (action === "usage" || action === "raw") {
      output.textContent = JSON.stringify(await api(`${base}/${action}`), null, 2);
      output.hidden = false;
    } else if (action === "reboot") {
      if (!confirm(`Reboot "${nameOf(id)}"? It will be unavailable for a few seconds.`)) return;
      await api(`${base}/reboot`, { method: "POST" });
      showToast(`${nameOf(id)}: reboot requested`, true);
    } else if (action === "rename") {
      const name = card.querySelector(".rename-input").value.trim();
      render(await api(base, jsonBody("PATCH", { name: name || null })));
      showToast(name ? `Renamed to "${name}"` : "Using the device's own name", true);
    } else if (action === "remove") {
      if (!confirm(`Remove "${nameOf(id)}" from Schonhouse? The bulb itself is not changed.`)) return;
      await api(base, { method: "DELETE" });
      removeCard(id);
      showToast(`${id}: removed`, true);
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

function removeCard(id) {
  const entry = cards.get(id);
  if (entry) {
    entry.el.remove();
    cards.delete(id);
  }
  emptyNote.hidden = cards.size > 0;
  renderGroups();
}

function render(snapshot) {
  let entry = cards.get(snapshot.id);
  if (!entry) {
    entry = { el: createCard(snapshot), snapshot };
    cards.set(snapshot.id, entry);
  }
  const nameChanged = entry.snapshot.name !== snapshot.name;
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
    preview = el("span", { className: "preview" });
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
  if (summary.firstChild?.nodeType === Node.TEXT_NODE) summary.firstChild.textContent = text;
  else summary.prepend(text);
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

  const renameInput = card.querySelector(".rename-input");
  if (document.activeElement !== renameInput) renameInput.value = snapshot.custom_name || "";

  const error = card.querySelector(".error");
  const errText = snapshot.error && snapshot.error !== "rebooting" ? snapshot.error : "";
  error.hidden = !errText;
  error.textContent = errText.length > 160 ? `${errText.slice(0, 160)}…` : errText;
  error.title = errText;

  const rows = [
    ["Status", snapshot.online ? "online" : "offline"],
    ["Id", snapshot.id, "mono"],
    ["Device name", info?.nickname || "–"],
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
  }
  rows.push(["Last seen", formatTime(snapshot.last_seen)]);
  rows.push(["Updated", formatTime(snapshot.updated_at)]);
  setFacts(card.querySelector(".details .facts"), rows);

  emptyNote.hidden = cards.size > 0;
  if (nameChanged) renderGroups();
}

// ---------------------------------------------------------------- groups

async function loadGroups() {
  try {
    groups = await api("/api/groups");
  } catch (err) {
    showToast(`groups: ${err.message}`);
    groups = [];
  }
  renderGroups();
}

function renderGroups() {
  groupsGrid.replaceChildren();
  for (const group of groups) {
    const card = groupTemplate.content.firstElementChild.cloneNode(true);
    card.dataset.key = `group-${group.id}`;
    card.querySelector(".name").textContent = group.name;
    card.querySelector(".members").textContent = group.devices.map(nameOf).join(", ") || "no members";
    const apply = async (body) => {
      try {
        reportBatch(group.name, await api(`/api/groups/${group.id}/state`, jsonBody("POST", body)));
      } catch (err) {
        showToast(`${group.name}: ${err.message}`);
      }
    };
    const caps = group.capabilities || [];
    const power = caps.includes("power");
    card.querySelector('[data-action="on"]').hidden = !power;
    card.querySelector('[data-action="off"]').hidden = !power;
    card.querySelector('[data-action="on"]').addEventListener("click", () => apply({ on: true }));
    card.querySelector('[data-action="off"]').addEventListener("click", () => apply({ on: false }));

    // Show only the controls every member supports (intersection computed by the server).
    let shown = 0;
    for (const control of card.querySelectorAll(".control")) {
      control.hidden = !caps.includes(control.dataset.cap);
      if (!control.hidden) shown++;
    }
    card.querySelector(".no-common").hidden = shown > 0 || power;

    bindPair(card, "brightness", (v) => apply({ brightness: v }));
    bindPair(card, "color_temp", (v) => apply({ color_temp: v }));
    const colorCommand = () => ({
      hue: clamp(card.querySelector('input[type="range"][name="hue"]')),
      saturation: clamp(card.querySelector('input[type="range"][name="saturation"]')),
    });
    bindPair(card, "hue", () => apply(colorCommand()));
    bindPair(card, "saturation", () => apply(colorCommand()));
    const hueRange = card.querySelector('input[type="range"][name="hue"]');
    hueRange.addEventListener("input", () => updateSatTrack(card, hueRange.value));
    updateSatTrack(card, hueRange.value);
    for (const chip of card.querySelectorAll(".chips button")) {
      chip.addEventListener("click", () => apply({ color_temp: Number(chip.dataset.value) }));
    }
    const swatches = card.querySelector(".swatches");
    for (const [label, hue] of SWATCHES) {
      const b = el("button", { type: "button", title: `${label} (${hue}°)` });
      b.style.background = hsl(hue);
      b.addEventListener("click", () => apply({ hue, saturation: 100 }));
      swatches.appendChild(b);
    }
    card.querySelector('[data-action="edit"]').addEventListener("click", () => openGroupDialog(group));
    card.querySelector('[data-action="delete"]').addEventListener("click", async () => {
      if (!confirm(`Delete group "${group.name}"?`)) return;
      try {
        await api(`/api/groups/${group.id}`, { method: "DELETE" });
        await loadGroups();
      } catch (err) {
        showToast(`${group.name}: ${err.message}`);
      }
    });
    groupsGrid.appendChild(card);
  }
}

const groupDialog = document.getElementById("dlg-group");
const groupForm = groupDialog.querySelector("form");
let editingGroup = null;

function openGroupDialog(group) {
  editingGroup = group || null;
  document.getElementById("dlg-group-title").textContent = group ? "Edit group" : "New group";
  groupForm.elements.name.value = group?.name || "";
  const list = document.getElementById("group-members");
  list.replaceChildren();
  for (const [id, entry] of cards) {
    const box = el("input", { type: "checkbox", value: id, checked: Boolean(group?.devices.includes(id)) });
    list.appendChild(el("label", {}, box, entry.snapshot.name, el("span", { className: "muted" }, ` ${entry.snapshot.ip}`)));
  }
  if (cards.size === 0) list.appendChild(el("p", { className: "muted" }, "No devices to choose from."));
  groupDialog.showModal();
}

groupForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const body = {
    name: groupForm.elements.name.value.trim(),
    devices: [...groupForm.querySelectorAll('input[type="checkbox"]:checked')].map((c) => c.value),
  };
  if (!body.devices.length) {
    showToast("Pick at least one device");
    return;
  }
  try {
    if (editingGroup) await api(`/api/groups/${editingGroup.id}`, jsonBody("PUT", body));
    else await api("/api/groups", jsonBody("POST", body));
    groupDialog.close();
    await loadGroups();
  } catch (err) {
    showToast(`group: ${err.message}`);
  }
});

document.getElementById("new-group").addEventListener("click", () => openGroupDialog(null));
document.getElementById("all-on").addEventListener("click", () => applyAll({ on: true }));
document.getElementById("all-off").addEventListener("click", () => applyAll({ on: false }));

async function applyAll(body) {
  try {
    reportBatch("All devices", await api("/api/devices/state", jsonBody("POST", body)));
  } catch (err) {
    showToast(`all: ${err.message}`);
  }
}

// ---------------------------------------------------------------- scenes

async function loadScenes() {
  try {
    scenes = await api("/api/scenes");
  } catch (err) {
    showToast(`scenes: ${err.message}`);
    scenes = [];
  }
  renderScenes();
}

function renderScenes() {
  scenesBar.replaceChildren();
  for (const scene of scenes) {
    const button = el("button", { type: "button", title: `${scene.entries.length} device(s)` }, scene.name, el("span", { className: "x" }, "×"));
    button.addEventListener("click", async () => {
      if (scenesBar.classList.contains("manage")) {
        if (!confirm(`Delete scene "${scene.name}"?`)) return;
        try {
          await api(`/api/scenes/${scene.id}`, { method: "DELETE" });
          await loadScenes();
        } catch (err) {
          showToast(`${scene.name}: ${err.message}`);
        }
        return;
      }
      try {
        reportBatch(scene.name, await api(`/api/scenes/${scene.id}/apply`, { method: "POST" }));
      } catch (err) {
        showToast(`${scene.name}: ${err.message}`);
      }
    });
    scenesBar.appendChild(button);
  }
  document.getElementById("scenes-empty").hidden = scenes.length > 0;
}

document.getElementById("save-scene").addEventListener("click", async () => {
  const name = prompt("Scene name (saves the current state of every online device):");
  if (!name || !name.trim()) return;
  try {
    await api("/api/scenes/capture", jsonBody("POST", { name: name.trim() }));
    await loadScenes();
    showToast(`Scene "${name.trim()}" saved`, true);
  } catch (err) {
    showToast(`scene: ${err.message}`);
  }
});

document.getElementById("manage-scenes").addEventListener("click", (e) => {
  const on = !scenesBar.classList.contains("manage");
  scenesBar.classList.toggle("manage", on);
  e.currentTarget.setAttribute("aria-pressed", String(on));
});

// ---------------------------------------------------------------- add device

const deviceDialog = document.getElementById("dlg-device");
const deviceForm = deviceDialog.querySelector("form");
const foundList = document.getElementById("discovered");
const discoverStatus = document.getElementById("discover-status");

document.getElementById("add-device").addEventListener("click", () => {
  deviceForm.reset();
  foundList.replaceChildren();
  discoverStatus.textContent = "Press Scan to look for Tapo devices (takes about 5 seconds).";
  deviceDialog.showModal();
});

document.getElementById("discover").addEventListener("click", async (e) => {
  const button = e.currentTarget;
  button.disabled = true;
  discoverStatus.textContent = "Scanning…";
  foundList.replaceChildren();
  try {
    const found = await api("/api/discover?timeout=5");
    discoverStatus.textContent = found.length ? `${found.length} device(s) answered.` : "Nothing answered. Make sure the devices are on the same network.";
    for (const d of found) {
      const what = el("div", { className: "what" },
        el("b", {}, d.model || d.device_type || "Unknown device"),
        el("span", { className: "muted" }, `${d.ip}${d.mac ? ` · ${d.mac}` : ""}${d.protocol ? ` · ${d.protocol}` : ""}`));
      let action;
      if (d.known_as) action = el("span", { className: "muted" }, `added as ${nameOf(d.known_as)}`);
      else if (!d.kind) action = el("span", { className: "muted" }, "not supported yet");
      else {
        action = el("button", { type: "button", className: "ghost" }, "Use");
        action.addEventListener("click", () => {
          deviceForm.elements.ip.value = d.ip;
          deviceForm.elements.kind.value = d.kind;
          deviceForm.elements.name.focus();
        });
      }
      foundList.appendChild(el("li", {}, what, action));
    }
  } catch (err) {
    discoverStatus.textContent = `Scan failed: ${err.message}`;
  } finally {
    button.disabled = false;
  }
});

deviceForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = deviceForm.elements;
  const body = { ip: f.ip.value.trim(), kind: f.kind.value };
  if (f.name.value.trim()) body.name = f.name.value.trim();
  if (f.id.value.trim()) body.id = f.id.value.trim();
  try {
    const snapshot = await api("/api/devices", jsonBody("POST", body));
    render(snapshot);
    deviceDialog.close();
    showToast(`Added ${snapshot.name}`, true);
  } catch (err) {
    showToast(`add device: ${err.message}`);
  }
});

for (const dialog of document.querySelectorAll("dialog")) {
  for (const b of dialog.querySelectorAll("[data-close]")) b.addEventListener("click", () => dialog.close());
  dialog.addEventListener("click", (e) => { if (e.target === dialog) dialog.close(); });
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
    ["Discovery", serverInfo.discovery_broadcast, "mono"],
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
  const li = el("li", {},
    el("span", { className: "ts" }, clock(entry.ts)),
    el("span", { className: `lvl ${entry.level}` }, entry.level),
    el("span", { className: "dev" }, entry.device ? nameOf(entry.device) : "server"),
    el("span", { className: "msg" }, entry.message));
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
  const seen = () => { stats.events++; stats.lastEvent = Date.now(); };
  source.addEventListener("open", () => {
    stats.sse = "live";
    connection.textContent = "live";
    connection.className = "pill ok";
  });
  source.addEventListener("device", (event) => { seen(); render(JSON.parse(event.data)); });
  source.addEventListener("device-removed", (event) => { seen(); removeCard(JSON.parse(event.data).id); });
  source.addEventListener("changed", (event) => {
    seen();
    const { what } = JSON.parse(event.data);
    if (what === "groups") loadGroups();
    if (what === "scenes") loadScenes();
  });
  source.addEventListener("log", (event) => { seen(); addLogEntry(JSON.parse(event.data)); });
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
]).then(() => Promise.all([loadGroups(), loadScenes()])).finally(() => {
  renderConnection();
  connect();
});
