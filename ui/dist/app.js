// Show Player operator UI. All state comes from engine feedback relayed by the
// Tauri backend (lsp:show / lsp:cue / lsp:state / lsp:load); this file only
// renders it and sends transport commands. Editing lives in editor.js.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

/** Engine pushes /lsp/state at ≥1 Hz; silence longer than this = offline. */
const STALE_MS = 2500;
/** Ignore a second GO this soon after the first (key bounce / double click). */
const GO_GUARD_MS = 250;

const model = {
  show: null, // { name, count, path, rev }
  cues: [], // CueInfo by idx (may have holes until all /lsp/cue arrive)
  standby: -1,
  running: new Map(), // idx -> { phase, elapsed, duration, at }
  lastState: 0,
};

let rows = []; // <tr> by idx
const activeItems = new Map(); // idx -> drawer <li>
let lastGo = 0;

// ── Commands ──────────────────────────────────────────────

function send(cmd, arg) {
  logSend(cmd, arg);
  invoke("command", { cmd, arg: arg ?? null }).catch((e) => {
    console.error(cmd, e);
    logTraffic(`!! /lsp/${cmd} failed: ${e}`, "l-err");
  });
}

// ── Theme ─────────────────────────────────────────────────
// "standard" (Run/Edit design) or "underground" (v2 terminal design). Labels
// that differ per theme carry data-ug; their standard text is kept in data-std.

const THEMES = { standard: "Standard", underground: "Underground", pro: "Pro" };
let theme = "standard";
try {
  const saved = localStorage.getItem("lsp.theme");
  if (saved in THEMES) theme = saved;
} catch {
  /* storage unavailable */
}
const ug = () => theme === "underground";
const pro = () => theme === "pro";
/** Underground and Pro put the cue number before its type, as their designs do. */
const numFirst = () => theme !== "standard";

const KIND_LABEL = { audio: "Audio", fade: "Fade", stop: "Stop", osc: "OSC", midi: "MIDI", wait: "Wait" };

function orderCells(tr) {
  const num = tr.querySelector(".c-num");
  const type = tr.querySelector(".c-type");
  if (!num || !type) return;
  const first = numFirst() ? num : type;
  const second = numFirst() ? type : num;
  if (first.nextElementSibling !== second) tr.insertBefore(first, second);
}

function applyTheme(name) {
  theme = name;
  try {
    localStorage.setItem("lsp.theme", name);
  } catch {
    /* ignore */
  }
  if (name === "standard") delete document.body.dataset.theme;
  else document.body.dataset.theme = name;
  document.querySelectorAll("[data-ug], [data-pro]").forEach((el) => {
    if (el.dataset.std === undefined) el.dataset.std = el.textContent;
    const alt = ug() ? el.dataset.ug : pro() ? el.dataset.pro : undefined;
    el.textContent = alt ?? el.dataset.std;
  });
  $("theme-toggle").textContent = `Theme: ${THEMES[name]}`;
  orderCells(document.querySelector("thead tr"));
  rows.forEach((tr) => tr && orderCells(tr));
  if (rows.length) applyState();
  renderChrome();
  renderProStatus();
}

$("theme-toggle").addEventListener("click", () => {
  const names = Object.keys(THEMES);
  applyTheme(names[(names.indexOf(theme) + 1) % names.length]);
});

// ── Traffic log (underground theme's OSC_TRAFFIC panel) ───────────────

const TRAFFIC_MAX = 200;

function stamp() {
  const d = new Date();
  const p = (n, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

function logTraffic(text, cls = "l-rx") {
  const log = $("traffic-log");
  const row = document.createElement("div");
  const t = document.createElement("span");
  t.className = "t";
  t.textContent = stamp();
  const m = document.createElement("span");
  m.className = `m ${cls}`;
  m.textContent = text;
  m.title = text;
  row.append(t, m);
  log.append(row);
  while (log.childElementCount > TRAFFIC_MAX) log.firstElementChild.remove();
  log.scrollTop = log.scrollHeight;
}

function logSend(cmd, arg) {
  let text = `/lsp/${cmd}${arg ? ` ${arg}` : ""}`;
  let cls = "l-tx";
  if (cmd === "go") {
    const c = model.cues[model.standby];
    text = c ? `/lsp/go  -> ${c.number} ${(c.name || c.kind).toUpperCase()}` : "/lsp/go  -> (end of list)";
    cls = "l-go";
  } else if (cmd === "panic" || cmd === "hardstop") {
    cls = "l-err";
  } else if (cmd === "stop" || cmd === "pause") {
    cls = "l-warn";
  }
  logTraffic(text, cls);
}

function go() {
  const now = performance.now();
  if (now - lastGo < GO_GUARD_MS) return;
  lastGo = now;
  if (showRunStart == null && model.cues[model.standby]) showRunStart = Date.now();
  send("go");
  const b = $("go");
  b.classList.add("flash");
  setTimeout(() => b.classList.remove("flash"), 90);
}

/**
 * The engine hard-stops on a second panic during its panic fade. Mirror that
 * so the button can say so: armed from a panic until no cue is stopping any
 * more (after a short grace for the engine's state to arrive).
 */
let panicAt = 0;
let panicArmed = false;

function panic() {
  send("panic");
  panicArmed = !panicArmed;
  panicAt = performance.now();
}

function anyPhase(phase) {
  for (const r of model.running.values()) if (r.phase === phase) return true;
  return false;
}

const isLive = (r) => r.phase === "running" || r.phase === "pre_wait";

function togglePauseAll() {
  const live = [...model.running.values()].some(isLive);
  send(!live && anyPhase("paused") ? "resume" : "pause");
}

async function openShow() {
  try {
    await invoke("open_show", { current: model.show?.path || null });
  } catch (e) {
    showBanner("error", "Could not open show", String(e));
  }
}

$("go").addEventListener("click", go);
$("prev").addEventListener("click", () => send("prev"));
$("next").addEventListener("click", () => send("next"));
$("stop-all").addEventListener("click", () => send("stop"));
$("panic").addEventListener("click", panic);
$("pause-all").addEventListener("click", togglePauseAll);
$("open").addEventListener("click", openShow);
$("reload").addEventListener("click", () => send("reload"));
$("banner-close").addEventListener("click", () => ($("banner").hidden = true));
$("running-toggle").addEventListener("click", () => ($("drawer").hidden = !$("drawer").hidden));
$("drawer-close").addEventListener("click", () => ($("drawer").hidden = true));

document.addEventListener("keydown", (e) => {
  if (e.metaKey || e.ctrlKey) {
    const k = e.key.toLowerCase();
    if (k === "o") {
      e.preventDefault();
      openShow();
    } else if (k === "r") {
      e.preventDefault(); // also stops the webview from reloading itself
      send("reload");
    }
    return;
  }
  // Typing in a field (inspector, engine address) must never fire GO / panic.
  if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement) return;
  switch (e.key) {
    case " ":
      e.preventDefault();
      if (!e.repeat) go();
      break;
    case "Escape":
      e.preventDefault();
      if (!e.repeat) panic();
      break;
    case "p":
    case "P":
      e.preventDefault();
      if (!e.repeat) togglePauseAll();
      break;
    case "ArrowUp":
      e.preventDefault();
      send("prev");
      break;
    case "ArrowDown":
      e.preventDefault();
      send("next");
      break;
  }
});

// Buttons must not keep focus, or Space would "click" them instead of GO.
document.addEventListener("mouseup", () => {
  if (document.activeElement instanceof HTMLButtonElement) document.activeElement.blur();
});

$("engine-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const input = $("engine");
  try {
    input.value = await invoke("set_engine", { addr: input.value });
    resetShow();
    input.blur();
  } catch (err) {
    input.setCustomValidity(String(err));
    input.reportValidity();
    setTimeout(() => input.setCustomValidity(""), 2000);
  }
});

// ── Formatting ────────────────────────────────────────────

/** Static times: "0.0" for waits, "3.00" for actions, mm:ss.s past a minute. */
function fmtSecs(s, decimals) {
  if (s >= 60) {
    const m = Math.floor(s / 60);
    return `${String(m).padStart(2, "0")}:${(s - m * 60).toFixed(1).padStart(4, "0")}`;
  }
  return s.toFixed(decimals);
}

function fmtAction(c) {
  if (c.kind === "stop") return c.data?.fade ? fmtSecs(c.data.fade, 2) : "—";
  if (c.duration < 0) return "loop";
  if (!c.duration) return "—";
  return fmtSecs(c.duration, 2);
}

/** Running elapsed: "1.2" under a minute, "02:14.6" after. */
const fmtElapsed = (s) => fmtSecs(Math.max(0, s), 1);

function fmtClock(s) {
  s = Math.max(0, s);
  const m = Math.floor(s / 60);
  return `${m}:${(s - m * 60).toFixed(1).padStart(4, "0")}`;
}

const fmtDb = (db) => `${db < 0 ? "−" : db > 0 ? "+" : ""}${Math.abs(db)} dB`;
const trimNum = (n) => String(Math.round(n * 100) / 100);

const CONTINUE_TEXT = { auto_continue: "Auto-continue", auto_follow: "Auto-follow", do_nothing: "" };

function baseName(p) {
  return p ? p.split(/[\\/]/).pop() : "";
}

/** One-line description of what a cue does, for the Next panel. */
function describe(c, idx) {
  const d = c.data || {};
  const bits = [];
  switch (c.kind) {
    case "audio":
      bits.push(d.file ? baseName(d.file) : "no file");
      if (d.gain_db) bits.push(fmtDb(d.gain_db));
      if (d.fade_in) bits.push(`fade in ${trimNum(d.fade_in)} s`);
      if (d.loop) bits.push("loop");
      break;
    case "fade":
      bits.push(`Cue ${d.target || "?"} to ${d.gain_db == null ? "silence" : fmtDb(d.gain_db)} over ${trimNum(d.duration ?? 0)} s${d.stop ? ", then stop" : ""}`);
      break;
    case "stop":
      bits.push(`${d.target ? `Stop cue ${d.target}` : "Stop all"}${d.fade ? `, fade ${trimNum(d.fade)} s` : ""}`);
      break;
    case "osc":
      bits.push(`${d.path ?? ""} ${(d.args || []).map((a) => (typeof a === "string" ? `"${a}"` : a)).join(" ")} → ${d.host ?? ""}`.trim());
      break;
    case "midi":
      bits.push(`${(d.bytes || []).map((b) => b.toString(16).toUpperCase().padStart(2, "0")).join(" ")} → ${d.port || "?"}`);
      break;
    case "wait":
      bits.push(`Wait ${trimNum(d.duration ?? 0)} s`);
      break;
  }
  if (c.pre_wait) bits.push(`pre-wait ${trimNum(c.pre_wait)} s`);
  const nxt = model.cues[idx + 1];
  if (c.next === "auto_continue" && nxt) bits.push(`fires ${nxt.number} with it`);
  if (c.next === "auto_follow" && nxt) bits.push(`then ${nxt.number}`);
  return bits.join(" · ");
}

/** 20-cell progress bar, e.g. [████████░░░░░░░░░░░░]. */
function asciiBar(pct) {
  const n = Math.max(0, Math.min(20, Math.round(pct / 5)));
  return `[${"█".repeat(n)}${"░".repeat(20 - n)}]`;
}

function stateText(idx, r) {
  if (r) {
    const c = model.cues[idx];
    if (r.phase === "pre_wait") return "WAIT";
    if (r.phase === "paused") return "PAUSE";
    if (r.phase === "stopping") return "STOP";
    if (r.duration < 0) return "LOOP";
    return c?.kind === "fade" ? "FADE" : "RUN";
  }
  return idx === model.standby ? "ARMED" : "IDLE";
}

let lastOnline = null;

/** Underground chrome: the shell prompt and the uplink line. */
function renderChrome() {
  const path = model.show?.path ?? "";
  const dir = path ? path.split(/[\\/]/).slice(-2, -1)[0] || "" : "";
  const p = $("prompt");
  p.textContent = "";
  const span = (cls, text) => {
    const s = document.createElement("span");
    if (cls) s.className = cls;
    s.textContent = text;
    return s;
  };
  const cmd = path ? `lsp ${baseName(path)} --osc ${$("engine").value}` : `lsp --osc ${$("engine").value}`;
  const cur = span("cur", "█");
  p.append(span("u", "root@booth"), span("s", ":"), span("d", `~/${dir}`), span("s", "# "), cmd, cur);
  const online = performance.now() - model.lastState < STALE_MS;
  const u = $("uplink");
  u.className = `uplink ug-only${online ? "" : " down"}`;
  const b = document.createElement("b");
  b.textContent = online ? "UPLINK OK" : "NO CARRIER";
  u.textContent = "[";
  u.append(b, `] ${online ? "20Hz" : "--"} · act ${String(model.running.size).padStart(3, "0")} · rev 0x${String(model.show?.rev ?? 0).padStart(2, "0")}`);
}

// ── Pro console: active cards, master meter, clocks ───────

const consoleCards = new Map(); // idx -> card element
let engineInfo = null;
let showRunStart = null; // ms epoch of the first GO of this show

function renderProStatus() {
  const s = model.show;
  const parts = [];
  if (s?.path) parts.push(`${s.count} cues`, `rev ${s.rev}`);
  if (engineInfo) parts.push(`hot reload ${engineInfo.watch ? "on" : "off"}`);
  $("pro-status").textContent = parts.join(" · ");
}

const STATE_LABEL = { pre_wait: "PRE-WAIT", paused: "PAUSED", stopping: "STOPPING" };

function renderConsole() {
  for (const [idx, card] of consoleCards) {
    if (!model.running.has(idx)) {
      card.remove();
      consoleCards.delete(idx);
    }
  }
  const box = $("console-active");
  for (const [idx, r] of [...model.running].sort((a, b) => a[0] - b[0])) {
    const c = model.cues[idx];
    let card = consoleCards.get(idx);
    if (!card) {
      card = document.createElement("div");
      card.innerHTML = `
        <div class="h"><span class="n"></span><span class="nm"></span><span class="st"></span></div>
        <div class="b"><span class="tm"></span><span class="dt"></span></div>
        <span class="pb"><i></i></span>
        <div class="acts"><button class="pz"></button><button class="sp" title="Stop (its own fade-out)">Stop</button></div>`;
      card.querySelector(".n").textContent = c?.number ?? idx;
      card.querySelector(".nm").textContent = c?.name || c?.kind || "";
      const d = c?.data || {};
      card.querySelector(".dt").textContent =
        c?.kind === "audio"
          ? `${(d.gain_db ?? 0) < 0 ? "−" : ""}${Math.abs(d.gain_db ?? 0).toFixed(1)} dB`
          : c?.kind === "fade"
            ? d.curve ?? "s_curve"
            : c?.kind ?? "";
      card.querySelector(".sp").addEventListener("click", () => c && send("stop", c.number));
      card.querySelector(".pz").addEventListener("click", () => {
        const cur = model.running.get(idx);
        if (c && cur) send(cur.phase === "paused" ? "resume" : "pause", c.number);
      });
      consoleCards.set(idx, card);
    }
    card.className = `ccard ${r.phase} ${c?.kind === "fade" ? "fade" : ""}`;
    card.querySelector(".st").textContent =
      STATE_LABEL[r.phase] ?? (r.duration < 0 ? "LOOP" : c?.kind === "fade" ? "FADING" : "RUNNING");
    card.querySelector(".pz").textContent = r.phase === "paused" ? "Resume" : "Pause";
    card.querySelector(".pz").hidden = r.phase === "stopping";
    card.querySelector(".pb").hidden = !(r.duration > 0);
    box.appendChild(card); // keep order
  }
  const n = model.running.size;
  $("console-count").textContent = n ? `${n} active` : "";
  $("console-empty").hidden = n > 0;
}

function updateCard(card, r, el, p) {
  card.style.setProperty("--p", `${p}%`);
  card.querySelector(".tm").textContent =
    r.duration > 0 ? `−${Math.max(0, r.duration - el).toFixed(1)} s` : fmtMinSec(el);
}

/** Always mm:ss.s, e.g. 02:14.6 (looping cues in the Active cards). */
function fmtMinSec(s) {
  s = Math.max(0, s);
  const m = Math.floor(s / 60);
  return `${String(m).padStart(2, "0")}:${(s - m * 60).toFixed(1).padStart(4, "0")}`;
}

let lastClock = "";
function tickClocks() {
  const d = new Date();
  const p = (n) => String(n).padStart(2, "0");
  const tod = `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  if (tod === lastClock) return;
  lastClock = tod;
  $("tod").textContent = tod;
  let run = "00:00:00";
  if (showRunStart != null) {
    const s = Math.floor((Date.now() - showRunStart) / 1000);
    run = `${p(Math.floor(s / 3600))}:${p(Math.floor(s / 60) % 60)}:${p(s % 60)}`;
  }
  $("show-run").textContent = run;
  $("show-run-box").classList.toggle("on", showRunStart != null);
}
$("show-run-box").addEventListener("click", () => {
  showRunStart = null;
  lastClock = "";
});

// Meter ballistics: instant attack, 24 dB/s release, peak hold 1.5 s then 12 dB/s.
const METER_FLOOR = -60;
const meter = {
  level: [METER_FLOOR, METER_FLOOR],
  hold: [METER_FLOOR, METER_FLOOR],
  holdAt: [0, 0],
  peak: -Infinity,
  clips: 0,
  load: 0,
  loadAt: 0,
  last: 0,
};
const toDb = (v) => (v > 0 ? Math.max(METER_FLOOR, 20 * Math.log10(v)) : METER_FLOOR);

function onMeter(m) {
  const now = performance.now();
  for (let i = 0; i < 2; i++) {
    const db = toDb(m.peak[i]);
    if (db > meter.level[i]) meter.level[i] = db;
    if (db >= meter.hold[i]) {
      meter.hold[i] = db;
      meter.holdAt[i] = now;
    }
    if (m.peak[i] > 0) meter.peak = Math.max(meter.peak, db);
  }
  meter.clips += m.clips;
  // DSP chip: worst load over the last second
  if (now - meter.loadAt > 1000) {
    meter.load = 0;
    meter.loadAt = now;
  }
  meter.load = Math.max(meter.load, m.load);
  $("m-subs").textContent = m.subs;
}

/** dB -> % height on the 0 / −6 / −12 / −24 / −48 scale (equal steps). */
function dbToPct(db) {
  const pts = [[0, 100], [-6, 75], [-12, 50], [-24, 25], [-48, 0]];
  if (db >= 0) return 100;
  for (let i = 1; i < pts.length; i++) {
    const [d1, p1] = pts[i - 1];
    const [d2, p2] = pts[i];
    if (db >= d2) return p2 + ((db - d2) / (d1 - d2)) * (p1 - p2);
  }
  return 0;
}

const fmtDbfs = (db) => (db <= METER_FLOOR ? "-inf" : `${db < 0 ? "−" : ""}${Math.abs(db).toFixed(1)}`);

function tickMeters(now) {
  const dt = meter.last ? (now - meter.last) / 1000 : 0;
  meter.last = now;
  ["m-l", "m-r"].forEach((id, i) => {
    meter.level[i] = Math.max(METER_FLOOR, meter.level[i] - 24 * dt);
    if (now - meter.holdAt[i] > 1500) meter.hold[i] = Math.max(METER_FLOOR, meter.hold[i] - 12 * dt);
    const el = $(id);
    el.style.setProperty("--mask", `${100 - dbToPct(meter.level[i])}%`);
    el.style.setProperty("--hold", `${100 - dbToPct(meter.hold[i])}%`);
    el.setAttribute("aria-label", `${i ? "Right" : "Left"} channel ${fmtDbfs(meter.level[i])} dBFS`);
  });
  $("m-level").textContent = `${fmtDbfs(Math.max(...meter.level))} dBFS`;
  $("m-peak").textContent = Number.isFinite(meter.peak) ? fmtDbfs(meter.peak) : "-inf";
  $("m-clips").textContent = meter.clips;
  $("m-clips").classList.toggle("bad", meter.clips > 0);
  const pct = meter.load * 100;
  $("dsp-chip").textContent = `DSP ${pct < 1 ? "<1" : Math.round(pct)}%`;
}

$("m-reset").addEventListener("click", () => {
  meter.peak = -Infinity;
  meter.clips = 0;
});

// ── Banner ────────────────────────────────────────────────

let bannerTimer = null;

function showBanner(kind, text, detail) {
  clearTimeout(bannerTimer);
  const b = $("banner");
  b.className = `banner ${kind}`;
  $("banner-text").textContent = text;
  $("banner-detail").textContent = detail || "";
  $("banner-detail").hidden = !detail;
  b.hidden = false;
  if (kind === "ok") bannerTimer = setTimeout(() => (b.hidden = true), 4000);
}

function onLoad({ status, path, message }) {
  const file = baseName(path);
  if (status === "loading") {
    // Only surface slow loads; most reloads finish in a few ms.
    clearTimeout(bannerTimer);
    bannerTimer = setTimeout(() => showBanner("loading", `Loading ${file}…`), 300);
  } else if (status === "edited") {
    // Edits reload the file every time; their result shows in the list itself.
    clearTimeout(bannerTimer);
    if ($("banner").classList.contains("loading")) $("banner").hidden = true;
  } else if (status === "ok") {
    showBanner("ok", `Loaded ${file} · ${message}`);
  } else {
    showBanner("error", file ? `Could not load ${file} — still running the previous show` : message, file ? message : "");
  }
}

// ── Cue list ──────────────────────────────────────────────

function resetShow() {
  model.show = null;
  model.cues = [];
  model.running.clear();
  model.standby = -1;
  renderList();
  renderStandby();
}

function emptyButton(text, onClick) {
  const b = document.createElement("button");
  b.textContent = text;
  b.addEventListener("click", onClick);
  return b;
}

function renderEmpty() {
  const el = $("empty");
  el.hidden = model.cues.length > 0;
  if (el.hidden) return;
  el.textContent = "";
  if (!model.show) {
    el.textContent = "Waiting for the engine…";
  } else if (model.show.path) {
    el.append(editor.on ? "No cues yet — add one with the buttons above." : "This show has no cues.");
    if (!editor.on) el.append(document.createElement("br"), emptyButton("Edit show", () => setEditMode(true)));
  } else {
    el.append("No show loaded.", document.createElement("br"), emptyButton("Open a show…", openShow), emptyButton("New show…", newShow));
  }
}

function renderList() {
  const tbody = $("cues");
  tbody.textContent = "";
  rows = [];
  for (const li of activeItems.values()) li.remove(); // indices may have shifted
  activeItems.clear();
  for (const card of consoleCards.values()) card.remove();
  consoleCards.clear();
  const cues = model.cues;
  const path = model.show?.path ?? "";
  $("show-name").textContent = path ? model.show.name || "Untitled" : "No show";
  $("show-file").textContent = path;
  $("show-file").title = path;
  $("reload").disabled = !path;
  $("cue-count").textContent = path ? `${model.show.count} cue${model.show.count === 1 ? "" : "s"}` : "";
  const broken = cues.filter((c) => c?.problem).length;
  $("broken-count").hidden = !broken;
  $("broken-count").textContent = `⚠ ${broken} broken`;
  document.title = model.show?.name ? `${model.show.name} — Show Player` : "Show Player";
  $("eof").textContent = path
    ? `-- EOF · ${model.show.count} cues · rev 0x${String(model.show.rev).padStart(2, "0")}${broken ? ` · ${broken} broken` : ""} --`
    : "-- no show loaded: open one or create one --";
  renderEmpty();
  renderChrome();
  renderProStatus();

  cues.forEach((c, idx) => {
    if (!c) return;
    const d = c.data || {};
    const tr = document.createElement("tr");
    tr.dataset.idx = idx;
    tr.innerHTML = `
      <td class="c-ph"></td>
      <td class="c-type"><span class="chip"></span></td>
      <td class="c-num"></td>
      <td class="c-name"><span class="nm"></span><span class="pbar"><i></i></span></td>
      <td class="c-target"></td>
      <td class="c-pre t"></td>
      <td class="c-state"></td>
      <td class="c-bar"></td>
      <td class="c-act t"></td>
      <td class="c-post t"></td>
      <td class="c-cont"></td>`;
    const chip = tr.querySelector(".chip");
    chip.classList.add(`chip-${c.kind}`);
    chip.textContent = KIND_LABEL[c.kind] ?? c.kind;
    tr.classList.add(`kind-${c.kind}`);
    tr.querySelector(".c-num").textContent = c.number;
    tr.querySelector(".c-name .nm").textContent = c.name;
    orderCells(tr);
    tr.querySelector(".c-target").textContent = d.target ?? "";
    tr.querySelector(".c-pre").textContent = fmtSecs(c.pre_wait || 0, 1);
    tr.querySelector(".c-act").textContent = fmtAction(c);
    tr.querySelector(".c-post").textContent = fmtSecs(c.post_wait || 0, 1);
    tr.querySelector(".c-cont").textContent = CONTINUE_TEXT[c.next] ?? "";
    if (c.problem) {
      tr.classList.add("broken");
      tr.querySelector(".c-name").dataset.problem = c.problem;
      tr.title = c.problem;
    }
    tr.addEventListener("click", () => onRowClick(c));
    tr.addEventListener("dblclick", () => editor.on && send("goto", c.number));
    tbody.appendChild(tr);
    rows[idx] = tr;
  });
  applyState();
  editorAfterRender();
}

let cueListTimer = null;

function scheduleCueListRender() {
  // /lsp/cue messages arrive in a burst; render once after the burst.
  clearTimeout(cueListTimer);
  cueListTimer = setTimeout(() => {
    renderList();
    renderStandby();
    const have = model.cues.filter(Boolean).length;
    if (model.show && have < model.show.count) invoke("refresh"); // lost a packet
  }, 40);
}

// ── State ─────────────────────────────────────────────────

function addMeta(meta, cls, text) {
  const s = document.createElement("span");
  s.className = cls;
  s.textContent = ` · ${text}`;
  meta.append(s);
}

function renderStandby() {
  const c = model.cues[model.standby];
  const meta = $("sb-meta");
  $("sb-type").textContent = c ? KIND_LABEL[c.kind] ?? c.kind : "";
  if (c) {
    $("sb-number").textContent = c.number;
    $("sb-name").textContent = c.name || c.kind;
    if (pro()) {
      meta.textContent = "";
      const fact = (k, v, mono = true) => {
        const s = document.createElement("span");
        const b = document.createElement("b");
        if (mono) b.className = "mono";
        b.textContent = v;
        s.append(`${k} `, b);
        meta.append(s);
      };
      fact("Pre", fmtSecs(c.pre_wait || 0, 1));
      fact("Action", fmtAction(c));
      fact("Continue", CONTINUE_TEXT[c.next] || "—", false);
      const d = document.createElement("span");
      d.textContent = describe(c, model.standby);
      meta.append(d);
    } else {
      meta.textContent = describe(c, model.standby);
    }
    const r = model.running.get(model.standby);
    if (r && r.phase !== "stopping") {
      const what = {
        ignore: "already running — GO will be ignored",
        stop: "already running — GO will stop it",
        hard_stop: "already running — GO will hard-stop it",
        restart: "already running — GO will restart it",
      }[c.second_trigger];
      if (what) addMeta(meta, "note", what);
    }
    if (c.problem) addMeta(meta, "warn", `⚠ ${c.problem}`);
  } else {
    $("sb-number").textContent = "—";
    $("sb-name").textContent = model.show?.path ? "End of cue list" : "No show loaded";
    meta.textContent = model.show?.path ? "GO does nothing" : "";
  }
  $("go").disabled = !c;
}

function applyState() {
  rows.forEach((tr, idx) => {
    if (!tr) return;
    tr.classList.toggle("standby", idx === model.standby);
    const r = model.running.get(idx);
    tr.classList.toggle("live", !!r);
    tr.classList.remove("running", "pre_wait", "stopping", "paused");
    if (r) {
      tr.classList.add(r.phase);
    } else {
      tr.style.removeProperty("--p");
      const c = model.cues[idx];
      if (c) tr.querySelector(".c-act").textContent = fmtAction(c);
      tr.querySelector(".c-bar").textContent = asciiBar(0);
    }
    tr.querySelector(".c-state").textContent = stateText(idx, r);
  });

  // running drawer: add/remove items, progress is updated per frame
  for (const [idx, li] of activeItems) {
    if (!model.running.has(idx)) {
      li.remove();
      activeItems.delete(idx);
    }
  }
  const ul = $("active");
  for (const [idx, r] of [...model.running].sort((a, b) => a[0] - b[0])) {
    let li = activeItems.get(idx);
    if (!li) {
      const c = model.cues[idx];
      li = document.createElement("li");
      li.innerHTML = `
        <div class="row"><span class="t"><b></b><span></span></span><button class="pz"></button><button class="st" title="Stop this cue (its own fade-out)">Stop</button></div>
        <div class="bar"><i></i></div>
        <div class="times"><span class="el"></span><span class="ph"></span><span class="rem"></span></div>`;
      li.querySelector("b").textContent = c?.number ?? idx;
      li.querySelector(".t span").textContent = c?.name ?? "";
      li.querySelector(".st").addEventListener("click", () => c && send("stop", c.number));
      li.querySelector(".pz").addEventListener("click", () => {
        const cur = model.running.get(idx);
        if (c && cur) send(cur.phase === "paused" ? "resume" : "pause", c.number);
      });
      activeItems.set(idx, li);
    }
    li.className = r.phase;
    li.querySelector(".ph").textContent = r.phase.replace("_", "-");
    const pz = li.querySelector(".pz");
    pz.textContent = r.phase === "paused" ? "Resume" : "Pause";
    pz.hidden = r.phase === "stopping";
    li.querySelector(".bar").classList.toggle("inf", r.duration < 0);
    ul.appendChild(li); // keeps order sorted
  }
  $("active-empty").hidden = model.running.size > 0;

  const n = model.running.size;
  $("running-count").textContent = `${n} running`;
  $("running-toggle").classList.toggle("on", n > 0);

  const pa = $("pause-all");
  const live = [...model.running.values()].some(isLive);
  const resume = !live && anyPhase("paused");
  pa.textContent = ug() ? (resume ? "RESUME_ALL" : "PAUSE_ALL") : resume ? "Resume all" : "Pause all";
  renderConsole();
  pa.classList.toggle("resume", resume);
  pa.disabled = !live && !resume;
  renderStandby();
  if (ug()) renderChrome();
}

function onState(s) {
  const now = performance.now();
  model.lastState = now;
  const standbyChanged = s.standby !== model.standby;
  model.standby = s.standby;

  const seen = new Set();
  let shapeChanged = false;
  for (const r of s.running) {
    seen.add(r.idx);
    const prev = model.running.get(r.idx);
    if (!prev || prev.phase !== r.phase) shapeChanged = true;
    model.running.set(r.idx, { ...r, at: now });
  }
  for (const idx of [...model.running.keys()]) {
    if (!seen.has(idx)) {
      model.running.delete(idx);
      shapeChanged = true;
    }
  }

  if (standbyChanged || shapeChanged) {
    logTraffic(`<- /lsp/state sb=${s.standby} act=${s.running.length}`);
  }
  if (!model.show) invoke("refresh"); // UI reloaded after subscribing
  if (standbyChanged) rows[model.standby]?.scrollIntoView({ block: "nearest" });
  if (standbyChanged || shapeChanged) applyState();
}

// Smooth progress between 20 Hz updates.
function frame() {
  const now = performance.now();
  const online = now - model.lastState < STALE_MS;
  document.body.classList.toggle("offline", !online);
  $("conn").classList.toggle("off", !online);
  if (ug() && online !== lastOnline) renderChrome();
  lastOnline = online;
  if (pro()) {
    tickClocks();
    tickMeters(now);
  }

  if (panicArmed && now - panicAt > 300 && !anyPhase("stopping")) panicArmed = false;
  const pb = $("panic");
  if (pb.classList.contains("armed") !== panicArmed) {
    pb.classList.toggle("armed", panicArmed);
    pb.textContent = panicArmed ? "HARD STOP" : "PANIC";
  }

  for (const [idx, r] of model.running) {
    const moving = online && r.phase !== "paused";
    const el = moving ? r.elapsed + (now - r.at) / 1000 : r.elapsed;
    const p = r.duration > 0 ? Math.min(100, (el / r.duration) * 100) : 100;
    const tr = rows[idx];
    if (tr) {
      tr.style.setProperty("--p", `${p}%`);
      tr.querySelector(".c-act").textContent = fmtElapsed(el);
      if (ug()) tr.querySelector(".c-bar").textContent = asciiBar(p);
    }
    const card = consoleCards.get(idx);
    if (card && pro()) updateCard(card, r, el, p);
    const li = activeItems.get(idx);
    if (li) {
      li.style.setProperty("--p", `${p}%`);
      li.querySelector(".el").textContent = fmtClock(el);
      li.querySelector(".rem").textContent =
        r.duration > 0 ? `−${fmtClock(r.duration - el)}` : r.duration < 0 ? "loop" : "";
    }
  }
  requestAnimationFrame(frame);
}

// ── Wiring ────────────────────────────────────────────────

listen("lsp:show", ({ payload }) => {
  logTraffic(`<- /lsp/show "${payload.name}" x${payload.count} rev=${payload.rev}`);
  if (payload.path !== model.show?.path) showRunStart = null; // a different show
  model.show = payload;
  model.cues = new Array(payload.count);
  scheduleCueListRender();
});

listen("lsp:cue", ({ payload }) => {
  if (!model.show) return;
  model.cues[payload.idx] = payload;
  scheduleCueListRender();
});

listen("lsp:state", ({ payload }) => onState(payload));
listen("lsp:meter", ({ payload }) => onMeter(payload));
listen("lsp:engine", ({ payload }) => {
  engineInfo = payload;
  $("dev-chip").textContent = `${payload.device} · ${trimNum(payload.rate / 1000)} kHz`;
  renderProStatus();
});
listen("lsp:load", ({ payload }) => {
  const cls = payload.status === "error" ? "l-err" : "l-rx";
  logTraffic(`<- /lsp/load ${payload.status} ${baseName(payload.path)}${payload.status === "error" ? ` ${payload.message.split("\n")[0]}` : ""}`, cls);
  onLoad(payload);
});

invoke("get_engine").then((a) => {
  $("engine").value = a;
  renderChrome();
});
invoke("refresh").catch(() => {});
applyTheme(theme);
renderStandby();
logTraffic("/lsp/subscribe", "l-tx");
requestAnimationFrame(frame);
