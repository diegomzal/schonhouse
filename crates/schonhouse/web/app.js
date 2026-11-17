// Schonhouse web UI: renders devices, rooms, groups, scenes, presets and usage from the
// REST API and the SSE stream, and sends commands back. No build step, no dependencies.

const roomsContainer = document.getElementById("rooms-container");
const groupsGrid = document.getElementById("groups");
const scenesBar = document.getElementById("scenes");
const emptyNote = document.getElementById("empty");
const connection = document.getElementById("connection");
const toastEl = document.getElementById("toast");
const template = document.getElementById("card");
const groupTemplate = document.getElementById("group-card");
const entryTemplate = document.getElementById("scene-entry");
const logList = document.getElementById("log");

const cards = new Map(); // device id -> { el, snapshot }
const dragging = new Set(); // "key/field" while the user holds a slider
const pendingTimers = new Map(); // "key/field" -> throttle timer id
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
let presets = [];
let layout = { rooms: [], devices: [] };

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

// Batch endpoints answer 200 with per-device results; surface the failures.
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

function formatMinutes(mins) {
  if (mins == null) return "–";
  const h = Math.floor(mins / 60), m = mins % 60;
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

function formatWh(wh) {
  if (wh == null) return "–";
  return wh >= 1000 ? `${(wh / 1000).toFixed(2)} kWh` : `${wh} Wh`;
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

// Color of a command (preset or scene entry) for a swatch dot.
function commandColor(command) {
  if (!command) return null;
  if (command.on === false) return "#444";
  if (command.hue != null) return hsl(command.hue, command.saturation ?? 100, 50);
  if (command.color_temp) return kelvinToCss(command.color_temp);
  return null;
}

function describeCommand(c) {
  if (!c) return "";
  if (c.on === false) return "off";
  const parts = [];
  if (c.color_temp) parts.push(`${c.color_temp} K`);
  if (c.hue != null) parts.push(`${c.hue}° / ${c.saturation ?? 100}%`);
  if (c.brightness != null) parts.push(`${c.brightness}%`);
  if (c.on === true && !parts.length) parts.push("on");
  return parts.join(" · ");
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

function swatchDot(color) {
  const dot = el("span", { className: "swatch" });
  dot.style.background = color || "transparent";
  return dot;
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

// Shared setup of the color controls on a device or group card.
function bindColorControls(card, apply) {
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
  for (const chip of card.querySelectorAll('.chips[data-field="color_temp"] button')) {
    chip.addEventListener("click", () => apply({ color_temp: Number(chip.dataset.value) }));
  }
  const swatches = card.querySelector(".swatches");
  for (const [label, hue] of SWATCHES) {
    const b = el("button", { type: "button", title: `${label} (${hue}°)` });
    b.style.background = hsl(hue);
    b.addEventListener("click", () => apply({ hue, saturation: 100 }));
    swatches.appendChild(b);
  }
  card.querySelector(".presets-row").dataset.bound = "1";
  card.applyCommand = apply;
}

function updateSatTrack(card, hue) {
  card.querySelector('input[type="range"][name="saturation"]').style.setProperty("--sat-color", hsl(hue));
}

// Re-renders the user preset chips on every device and group card.
function renderPresetChips() {
  for (const card of document.querySelectorAll(".card .presets-row")) {
    const container = card.querySelector(".presets");
    container.replaceChildren();
    const owner = card.closest(".card");
    const caps = owner.dataset.caps ? owner.dataset.caps.split(",") : [];
    const usable = presets.filter((p) => {
      if (p.command.hue != null && !caps.includes("color")) return false;
      if (p.command.color_temp && !caps.includes("color-temperature")) return false;
      if (p.command.brightness != null && !caps.includes("brightness") && p.command.hue == null && !p.command.color_temp) return false;
      return true;
    });
    card.hidden = usable.length === 0;
    for (const preset of usable) {
      const b = el("button", { type: "button", title: describeCommand(preset.command) }, swatchDot(commandColor(preset.command)), preset.name);
      b.addEventListener("click", () => owner.applyCommand?.(preset.command));
      container.appendChild(b);
    }
  }
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
  card.dataset.caps = snapshot.capabilities.join(",");

  card.querySelector(".power").addEventListener("click", () => {
    const on = cards.get(id)?.snapshot?.state?.on ?? false;
    sendCommand(id, { on: !on });
  });

  // Only show the controls this device supports.
  for (const control of card.querySelectorAll(".control[data-cap]")) {
    control.hidden = !snapshot.capabilities.includes(control.dataset.cap);
  }
  bindColorControls(card, (body) => sendCommand(id, body));

  const output = card.querySelector(".output");
  for (const button of card.querySelectorAll(".actions button, .rename button, .layout-row button")) {
    button.addEventListener("click", () => deviceAction(id, button.dataset.action, card, output, button));
  }
  card.querySelector(".rename-input").addEventListener("keydown", (e) => {
    if (e.key === "Enter") deviceAction(id, "rename", card, output, e.target);
  });
  card.querySelector(".room-select").addEventListener("change", (e) => moveToRoom(id, e.target.value ? Number(e.target.value) : null));

  return card;
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
    } else if (action === "preset") {
      await savePresetFromDevice(id, null);
    } else if (action === "move-up" || action === "move-down") {
      await moveDevice(id, action === "move-up" ? -1 : 1);
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
  renderLayout();
}

function render(snapshot) {
  let entry = cards.get(snapshot.id);
  let created = false;
  if (!entry) {
    entry = { el: createCard(snapshot), snapshot };
    cards.set(snapshot.id, entry);
    created = true;
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
  if (created) {
    renderLayout();
    renderPresetChips();
  }
  if (nameChanged) renderGroups();
}

// ---------------------------------------------------------------- rooms and layout

async function loadLayout() {
  try {
    layout = await api("/api/layout");
  } catch (err) {
    showToast(`layout: ${err.message}`);
  }
  renderLayout();
}

function layoutOf(id) {
  return layout.devices.find((d) => d.id === id) || { id, room_id: null, position: Number.MAX_SAFE_INTEGER };
}

// Places every device card under its room section, in stored order.
function renderLayout() {
  const sections = [...layout.rooms.map((r) => ({ id: r.id, name: r.name })), { id: null, name: layout.rooms.length ? "Other" : "" }];
  const existing = new Map([...roomsContainer.querySelectorAll(".room")].map((s) => [s.dataset.room, s]));
  const keep = new Set();
  for (const section of sections) {
    const key = String(section.id ?? "none");
    keep.add(key);
    let node = existing.get(key);
    if (!node) {
      node = el("section", { className: "room" }, el("h3"), el("div", { className: "grid" }));
      node.dataset.room = key;
    }
    node.querySelector("h3").textContent = section.name;
    node.querySelector("h3").hidden = !section.name;
    roomsContainer.appendChild(node); // appending re-orders in place
    const grid = node.querySelector(".grid");
    const members = [...cards.values()]
      .filter((e) => (layoutOf(e.snapshot.id).room_id ?? null) === section.id)
      .sort((a, b) => layoutOf(a.snapshot.id).position - layoutOf(b.snapshot.id).position || a.snapshot.name.localeCompare(b.snapshot.name));
    for (const member of members) grid.appendChild(member.el);
    node.hidden = members.length === 0 && section.id === null;
  }
  for (const [key, node] of existing) if (!keep.has(key)) node.remove();

  // Room selects in each card's details.
  for (const [id, entry] of cards) {
    const select = entry.el.querySelector(".room-select");
    const current = layoutOf(id).room_id ?? "";
    select.replaceChildren(el("option", { value: "", textContent: "No room" }), ...layout.rooms.map((r) => el("option", { value: String(r.id), textContent: r.name })));
    select.value = String(current);
  }
}

async function saveLayout(devices) {
  await api("/api/layout", jsonBody("PUT", { devices }));
  await loadLayout();
}

function roomMembers(roomId) {
  return [...cards.keys()]
    .filter((id) => (layoutOf(id).room_id ?? null) === roomId)
    .sort((a, b) => layoutOf(a).position - layoutOf(b).position || nameOf(a).localeCompare(nameOf(b)));
}

async function moveDevice(id, delta) {
  const roomId = layoutOf(id).room_id ?? null;
  const ids = roomMembers(roomId);
  const index = ids.indexOf(id);
  const target = index + delta;
  if (index < 0 || target < 0 || target >= ids.length) return;
  [ids[index], ids[target]] = [ids[target], ids[index]];
  await saveLayout(ids.map((d, position) => ({ id: d, room_id: roomId, position })));
}

async function moveToRoom(id, roomId) {
  try {
    const position = roomMembers(roomId).length;
    await saveLayout([{ id, room_id: roomId, position }]);
  } catch (err) {
    showToast(`${nameOf(id)}: ${err.message}`);
    renderLayout();
  }
}

const roomsDialog = document.getElementById("dlg-rooms");
const roomsForm = roomsDialog.querySelector("form");

function renderRoomsDialog() {
  const list = document.getElementById("rooms-list");
  list.replaceChildren();
  for (const room of layout.rooms) {
    const count = layout.devices.filter((d) => d.room_id === room.id).length;
    const rename = el("button", { type: "button", className: "ghost small" }, "Rename");
    rename.addEventListener("click", async () => {
      const name = prompt("Room name:", room.name);
      if (!name || !name.trim()) return;
      try {
        await api(`/api/rooms/${room.id}`, jsonBody("PUT", { name: name.trim() }));
        await loadLayout();
        renderRoomsDialog();
      } catch (err) {
        showToast(`room: ${err.message}`);
      }
    });
    const del = el("button", { type: "button", className: "ghost danger small" }, "Delete");
    del.addEventListener("click", async () => {
      if (!confirm(`Delete room "${room.name}"? Its devices stay, unassigned.`)) return;
      try {
        await api(`/api/rooms/${room.id}`, { method: "DELETE" });
        await loadLayout();
        renderRoomsDialog();
      } catch (err) {
        showToast(`room: ${err.message}`);
      }
    });
    list.appendChild(el("li", {}, el("div", { className: "what" }, el("b", {}, room.name), el("span", { className: "muted" }, `${count} device(s)`)), el("div", { className: "row" }, rename, del)));
  }
  if (!layout.rooms.length) list.appendChild(el("li", { className: "muted" }, "No rooms yet."));
}

document.getElementById("manage-rooms").addEventListener("click", () => {
  roomsForm.reset();
  renderRoomsDialog();
  roomsDialog.showModal();
});

roomsForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const name = roomsForm.elements.name.value.trim();
  if (!name) return;
  try {
    await api("/api/rooms", jsonBody("POST", { name }));
    roomsForm.reset();
    await loadLayout();
    renderRoomsDialog();
  } catch (err) {
    showToast(`room: ${err.message}`);
  }
});

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
    const caps = group.capabilities || [];
    card.dataset.caps = caps.join(",");
    card.querySelector(".name").textContent = group.name;
    card.querySelector(".members").textContent = group.devices.map(nameOf).join(", ") || "no members";
    const apply = async (body) => {
      try {
        reportBatch(group.name, await api(`/api/groups/${group.id}/state`, jsonBody("POST", body)));
      } catch (err) {
        showToast(`${group.name}: ${err.message}`);
      }
    };
    const power = caps.includes("power");
    card.querySelector('[data-action="on"]').hidden = !power;
    card.querySelector('[data-action="off"]').hidden = !power;
    card.querySelector('[data-action="on"]').addEventListener("click", () => apply({ on: true }));
    card.querySelector('[data-action="off"]').addEventListener("click", () => apply({ on: false }));

    // Show only the controls every member supports (intersection computed by the server).
    let shown = 0;
    for (const control of card.querySelectorAll(".control[data-cap]")) {
      control.hidden = !caps.includes(control.dataset.cap);
      if (!control.hidden) shown++;
    }
    card.querySelector(".no-common").hidden = shown > 0 || power;
    bindColorControls(card, apply);

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
  renderPresetChips();
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
    const button = el("button", { type: "button", title: `${scene.entries.length} device(s)` }, scene.name, el("span", { className: "x" }, "✎"));
    button.addEventListener("click", async () => {
      if (scenesBar.classList.contains("manage")) {
        openSceneEditor(scene);
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

// Scene editor -----------------------------------------------------------

const sceneDialog = document.getElementById("dlg-scene");
const sceneForm = sceneDialog.querySelector("form");
const sceneEntries = document.getElementById("scene-entries");
let editingScene = null;

function modeOf(command) {
  if (command.on === false) return "off";
  if (command.hue != null) return "color";
  if (command.color_temp) return "white";
  return "on";
}

function addEntryRow(deviceId, command) {
  const row = entryTemplate.content.firstElementChild.cloneNode(true);
  row.dataset.device = deviceId;
  row.querySelector(".dev-name").textContent = nameOf(deviceId);
  const f = row.elements ? row.elements : null;
  const q = (name) => row.querySelector(`[name="${name}"]`);
  q("mode").value = modeOf(command);
  q("brightness").value = command.brightness ?? "";
  q("color_temp").value = command.color_temp ?? "";
  q("hue").value = command.hue ?? "";
  q("saturation").value = command.saturation ?? "";
  const refresh = () => {
    const mode = q("mode").value;
    q("brightness").parentElement.hidden = mode === "off";
    q("color_temp").parentElement.hidden = mode !== "white";
    q("hue").parentElement.hidden = mode !== "color";
    q("saturation").parentElement.hidden = mode !== "color";
  };
  q("mode").addEventListener("change", refresh);
  refresh();
  row.querySelector('[data-action="remove"]').addEventListener("click", () => {
    row.remove();
    refreshSceneAddSelect();
  });
  sceneEntries.appendChild(row);
  void f;
}

function refreshSceneAddSelect() {
  const select = sceneForm.elements["add-device"];
  const used = new Set([...sceneEntries.querySelectorAll(".entry")].map((r) => r.dataset.device));
  select.replaceChildren(...[...cards.keys()].filter((id) => !used.has(id)).map((id) => el("option", { value: id, textContent: nameOf(id) })));
  select.disabled = select.options.length === 0;
  document.getElementById("scene-add-device").disabled = select.disabled;
}

function openSceneEditor(scene) {
  editingScene = scene;
  sceneForm.elements.name.value = scene.name;
  sceneEntries.replaceChildren();
  for (const entry of scene.entries) addEntryRow(entry.device_id, entry.command);
  refreshSceneAddSelect();
  sceneDialog.showModal();
}

document.getElementById("scene-add-device").addEventListener("click", () => {
  const id = sceneForm.elements["add-device"].value;
  if (!id) return;
  const state = cards.get(id)?.snapshot?.state;
  const command = state
    ? (state.on ? (state.color_temp > 0 ? { on: true, brightness: state.brightness, color_temp: state.color_temp } : { on: true, brightness: state.brightness, hue: state.hue ?? 0, saturation: Math.min(100, state.saturation ?? 100) }) : { on: false })
    : { on: true, brightness: 50, color_temp: 2700 };
  addEntryRow(id, command);
  refreshSceneAddSelect();
});

function entriesFromEditor() {
  const entries = [];
  for (const row of sceneEntries.querySelectorAll(".entry")) {
    const q = (name) => row.querySelector(`[name="${name}"]`);
    const mode = q("mode").value;
    const command = {};
    if (mode === "off") {
      command.on = false;
    } else {
      command.on = true;
      if (q("brightness").value !== "") command.brightness = clamp(q("brightness"));
      if (mode === "white") command.color_temp = clamp(q("color_temp"));
      if (mode === "color") {
        command.hue = q("hue").value === "" ? 0 : clamp(q("hue"));
        command.saturation = q("saturation").value === "" ? 100 : clamp(q("saturation"));
      }
    }
    entries.push({ device_id: row.dataset.device, command });
  }
  return entries;
}

sceneForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const body = { name: sceneForm.elements.name.value.trim(), entries: entriesFromEditor() };
  if (!body.entries.length) {
    showToast("A scene needs at least one device");
    return;
  }
  try {
    await api(`/api/scenes/${editingScene.id}`, jsonBody("PUT", body));
    sceneDialog.close();
    await loadScenes();
    showToast(`Scene "${body.name}" saved`, true);
  } catch (err) {
    showToast(`scene: ${err.message}`);
  }
});

document.getElementById("scene-delete").addEventListener("click", async () => {
  if (!editingScene || !confirm(`Delete scene "${editingScene.name}"?`)) return;
  try {
    await api(`/api/scenes/${editingScene.id}`, { method: "DELETE" });
    sceneDialog.close();
    await loadScenes();
  } catch (err) {
    showToast(`scene: ${err.message}`);
  }
});

// ---------------------------------------------------------------- presets

async function loadPresets() {
  try {
    presets = await api("/api/presets");
  } catch (err) {
    showToast(`presets: ${err.message}`);
    presets = [];
  }
  renderPresetChips();
}

// Builds a preset command (no on/off) from a device's current state.
function presetCommandFromState(state) {
  if (!state) return null;
  const command = { brightness: state.brightness };
  if (state.color_temp > 0) command.color_temp = state.color_temp;
  else {
    command.hue = state.hue ?? 0;
    command.saturation = Math.min(100, state.saturation ?? 100);
  }
  return command;
}

async function savePresetFromDevice(id, name) {
  const command = presetCommandFromState(cards.get(id)?.snapshot?.state);
  if (!command) {
    showToast(`${nameOf(id)}: no state to save`);
    return;
  }
  name = name || prompt(`Preset name for ${describeCommand(command)}:`);
  if (!name || !name.trim()) return;
  await api("/api/presets", jsonBody("POST", { name: name.trim(), command }));
  await loadPresets();
  showToast(`Preset "${name.trim()}" saved`, true);
}

const presetsDialog = document.getElementById("dlg-presets");
const presetsForm = presetsDialog.querySelector("form");

function renderPresetsDialog() {
  const list = document.getElementById("presets-list");
  list.replaceChildren();
  for (const preset of presets) {
    const rename = el("button", { type: "button", className: "ghost small" }, "Rename");
    rename.addEventListener("click", async () => {
      const name = prompt("Preset name:", preset.name);
      if (!name || !name.trim()) return;
      try {
        await api(`/api/presets/${preset.id}`, jsonBody("PUT", { name: name.trim(), command: preset.command }));
        await loadPresets();
        renderPresetsDialog();
      } catch (err) {
        showToast(`preset: ${err.message}`);
      }
    });
    const del = el("button", { type: "button", className: "ghost danger small" }, "Delete");
    del.addEventListener("click", async () => {
      if (!confirm(`Delete preset "${preset.name}"?`)) return;
      try {
        await api(`/api/presets/${preset.id}`, { method: "DELETE" });
        await loadPresets();
        renderPresetsDialog();
      } catch (err) {
        showToast(`preset: ${err.message}`);
      }
    });
    list.appendChild(el("li", {},
      el("div", { className: "what" }, el("b", {}, swatchDot(commandColor(preset.command)), preset.name), el("span", { className: "muted" }, describeCommand(preset.command))),
      el("div", { className: "row" }, rename, del)));
  }
  if (!presets.length) list.appendChild(el("li", { className: "muted" }, "No presets yet."));
  const select = presetsForm.elements.device;
  select.replaceChildren(...[...cards.values()].map((e) => el("option", { value: e.snapshot.id, textContent: e.snapshot.name })));
}

document.getElementById("manage-presets").addEventListener("click", () => {
  presetsForm.reset();
  renderPresetsDialog();
  presetsDialog.showModal();
});

presetsForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const id = presetsForm.elements.device.value;
  const name = presetsForm.elements.name.value.trim();
  if (!id || !name) {
    showToast("Pick a device and a name");
    return;
  }
  try {
    await savePresetFromDevice(id, name);
    presetsForm.elements.name.value = "";
    renderPresetsDialog();
  } catch (err) {
    showToast(`preset: ${err.message}`);
  }
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

// ---------------------------------------------------------------- energy tab

const usageDeviceSelect = document.getElementById("usage-device");
let usageSamples = [];

function dayKey(unixSecs) {
  const d = new Date(unixSecs * 1000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

async function loadUsage() {
  try {
    const [latest, samples] = await Promise.all([
      api("/api/usage/latest"),
      api(`/api/usage?since=${Math.floor(Date.now() / 1000) - 31 * 86400}`),
    ]);
    usageSamples = samples;
    renderUsageTable(latest);
    renderUsageChart();
  } catch (err) {
    showToast(`usage: ${err.message}`);
  }
}

function renderUsageTable(latest) {
  const tbody = document.querySelector("#usage-table tbody");
  tbody.replaceChildren();
  document.getElementById("usage-empty").hidden = latest.length > 0;
  document.getElementById("usage-table").hidden = latest.length === 0;
  const totals = { time_today: 0, time_past7: 0, time_past30: 0, power_today: 0, power_past7: 0, power_past30: 0 };
  const cell = (time, power) => el("td", {}, `${formatWh(power)}`, el("small", {}, `on ${formatMinutes(time)}`));
  for (const s of latest) {
    for (const k of Object.keys(totals)) totals[k] += s[k] ?? 0;
    tbody.appendChild(el("tr", {},
      el("td", {}, nameOf(s.device_id)),
      cell(s.time_today, s.power_today),
      cell(s.time_past7, s.power_past7),
      cell(s.time_past30, s.power_past30),
      el("td", { className: "muted" }, formatTime(s.ts))));
  }
  if (latest.length > 1) {
    tbody.appendChild(el("tr", { className: "total" },
      el("td", {}, "Total"),
      cell(totals.time_today, totals.power_today),
      cell(totals.time_past7, totals.power_past7),
      cell(totals.time_past30, totals.power_past30),
      el("td")));
  }
  const current = usageDeviceSelect.value;
  usageDeviceSelect.replaceChildren(el("option", { value: "", textContent: "All devices" }), ...latest.map((s) => el("option", { value: s.device_id, textContent: nameOf(s.device_id) })));
  usageDeviceSelect.value = [...usageDeviceSelect.options].some((o) => o.value === current) ? current : "";
  if (serverInfo) {
    document.getElementById("usage-note").textContent = serverInfo.usage_sample_interval_mins > 0
      ? `Counters reported by the devices: time on and energy for today, the last 7 and the last 30 days. Sampled every ${serverInfo.usage_sample_interval_mins} min.`
      : "Automatic sampling is disabled in the configuration; use “Sample now”.";
  }
}

// Energy per calendar day: the highest "today" reading seen that day, per device.
function dailyEnergy(deviceId) {
  const perDay = new Map(); // day -> Map(device -> wh)
  for (const s of usageSamples) {
    if (deviceId && s.device_id !== deviceId) continue;
    if (s.power_today == null) continue;
    const day = dayKey(s.ts);
    if (!perDay.has(day)) perDay.set(day, new Map());
    const m = perDay.get(day);
    m.set(s.device_id, Math.max(m.get(s.device_id) ?? 0, s.power_today));
  }
  const days = [];
  for (let i = 29; i >= 0; i--) {
    const d = new Date();
    d.setDate(d.getDate() - i);
    const key = dayKey(Math.floor(d.getTime() / 1000));
    const m = perDay.get(key);
    days.push({ key, label: `${d.getDate()}/${d.getMonth() + 1}`, wh: m ? [...m.values()].reduce((a, b) => a + b, 0) : null });
  }
  return days;
}

function renderUsageChart() {
  const svg = document.getElementById("usage-chart");
  const NS = "http://www.w3.org/2000/svg";
  svg.replaceChildren();
  const days = dailyEnergy(usageDeviceSelect.value || null);
  const max = Math.max(1, ...days.map((d) => d.wh ?? 0));
  const W = 640, H = 220, left = 40, bottom = 24, top = 10;
  const plotW = W - left - 8, plotH = H - top - bottom;
  const slot = plotW / days.length;
  const axis = document.createElementNS(NS, "line");
  axis.setAttribute("x1", left); axis.setAttribute("x2", W - 8); axis.setAttribute("y1", H - bottom); axis.setAttribute("y2", H - bottom);
  axis.setAttribute("class", "axis");
  svg.appendChild(axis);
  for (const frac of [0.5, 1]) {
    const y = H - bottom - plotH * frac;
    const g = document.createElementNS(NS, "line");
    g.setAttribute("x1", left); g.setAttribute("x2", W - 8); g.setAttribute("y1", y); g.setAttribute("y2", y);
    g.setAttribute("class", "axis");
    svg.appendChild(g);
    const t = document.createElementNS(NS, "text");
    t.setAttribute("x", left - 4); t.setAttribute("y", y + 4); t.setAttribute("text-anchor", "end"); t.setAttribute("class", "label");
    t.textContent = formatWh(Math.round(max * frac));
    svg.appendChild(t);
  }
  days.forEach((day, i) => {
    if (day.wh == null) return;
    const h = (day.wh / max) * plotH;
    const rect = document.createElementNS(NS, "rect");
    rect.setAttribute("x", left + i * slot + slot * 0.15);
    rect.setAttribute("y", H - bottom - h);
    rect.setAttribute("width", slot * 0.7);
    rect.setAttribute("height", Math.max(h, day.wh > 0 ? 1 : 0));
    rect.setAttribute("rx", 2);
    rect.setAttribute("class", "bar");
    const title = document.createElementNS(NS, "title");
    title.textContent = `${day.label}: ${formatWh(day.wh)}`;
    rect.appendChild(title);
    svg.appendChild(rect);
  });
  days.forEach((day, i) => {
    if (i % 5 !== 0 && i !== days.length - 1) return;
    const t = document.createElementNS(NS, "text");
    t.setAttribute("x", left + i * slot + slot / 2); t.setAttribute("y", H - 8); t.setAttribute("text-anchor", "middle"); t.setAttribute("class", "label");
    t.textContent = day.label;
    svg.appendChild(t);
  });
  const total = days.reduce((a, d) => a + (d.wh ?? 0), 0);
  document.getElementById("chart-caption").textContent = `last 30 days · ${formatWh(total)}`;
}

usageDeviceSelect.addEventListener("change", renderUsageChart);
document.getElementById("sample-usage").addEventListener("click", async (e) => {
  e.currentTarget.disabled = true;
  try {
    const r = await api("/api/usage/sample", { method: "POST" });
    showToast(`Sampled ${r.sampled} device(s)`, r.sampled > 0);
    await loadUsage();
  } catch (err) {
    showToast(`usage: ${err.message}`);
  } finally {
    e.currentTarget.disabled = false;
  }
});

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
    ["Usage sampling", serverInfo.usage_sample_interval_mins > 0 ? `every ${serverInfo.usage_sample_interval_mins} min` : "off"],
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
    ["Installed app", window.matchMedia("(display-mode: standalone)").matches ? "yes" : "no (open in browser)"],
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

function activeTab() {
  return document.querySelector(".tabs button.active")?.dataset.tab;
}

for (const tab of document.querySelectorAll(".tabs button")) {
  tab.addEventListener("click", () => {
    for (const t of document.querySelectorAll(".tabs button")) t.classList.toggle("active", t === tab);
    for (const s of document.querySelectorAll(".tab")) s.classList.toggle("active", s.id === `tab-${tab.dataset.tab}`);
    if (tab.dataset.tab === "system") loadRawAll();
    if (tab.dataset.tab === "energy") loadUsage();
    try { localStorage.setItem("schonhouse.tab", tab.dataset.tab); } catch {}
  });
}
try {
  const saved = localStorage.getItem("schonhouse.tab");
  if (saved && saved !== "devices") document.querySelector(`.tabs button[data-tab="${saved}"]`)?.click();
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
    if (what === "layout") loadLayout();
    if (what === "presets") loadPresets();
    if (what === "usage" && activeTab() === "energy") loadUsage();
  });
  source.addEventListener("log", (event) => { seen(); addLogEntry(JSON.parse(event.data)); });
  source.addEventListener("error", () => {
    // EventSource reconnects on its own; just reflect it in the header.
    stats.sse = "reconnecting";
    connection.textContent = "reconnecting…";
    connection.className = "pill bad";
  });
}

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

Promise.all([
  api("/api/devices").then((list) => { list.forEach(render); emptyNote.hidden = list.length > 0; }).catch(() => {}),
  loadServerInfo(),
  loadLog(),
]).then(() => Promise.all([loadLayout(), loadGroups(), loadScenes(), loadPresets()])).finally(() => {
  renderConnection();
  if (activeTab() === "energy") loadUsage();
  connect();
});
