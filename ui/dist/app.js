// Show Player operator UI. All state comes from engine feedback relayed by the
// Tauri backend (lsp:show / lsp:cue / lsp:state); this file only renders it and
// sends transport commands.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

/** Engine pushes /lsp/state at ≥1 Hz; silence longer than this = offline. */
const STALE_MS = 2500;
/** Ignore a second GO this soon after the first (key bounce / double click). */
const GO_GUARD_MS = 250;

const model = {
  show: null, // { name, count }
  cues: [], // CueInfo by idx (may have holes until all /lsp/cue arrive)
  standby: -1,
  running: new Map(), // idx -> { phase, elapsed, duration, at }
  lastState: 0,
};

let rows = []; // <tr> by idx
let activeItems = new Map(); // idx -> <li>
let lastGo = 0;

// ── Commands ──────────────────────────────────────────────

function send(cmd, arg) {
  invoke("command", { cmd, arg: arg ?? null }).catch((e) => console.error(cmd, e));
}

function go() {
  const now = performance.now();
  if (now - lastGo < GO_GUARD_MS) return;
  lastGo = now;
  send("go");
  const b = $("go");
  b.classList.add("flash");
  setTimeout(() => b.classList.remove("flash"), 90);
}

$("go").addEventListener("click", go);
$("prev").addEventListener("click", () => send("prev"));
$("next").addEventListener("click", () => send("next"));
$("stop-all").addEventListener("click", () => send("stop"));
$("panic").addEventListener("click", panic);
$("pause-all").addEventListener("click", togglePauseAll);

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

function togglePauseAll() {
  const live = [...model.running.values()].some((r) => r.phase === "running" || r.phase === "pre_wait");
  send(!live && anyPhase("paused") ? "resume" : "pause");
}
$("open").addEventListener("click", openShow);
$("reload").addEventListener("click", () => send("reload"));
$("banner-close").addEventListener("click", () => ($("banner").hidden = true));

async function openShow() {
  try {
    await invoke("open_show", { current: model.show?.path || null });
  } catch (e) {
    showBanner("error", "Could not open show", String(e));
  }
}

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

function fmt(s) {
  if (s < 0) return "∞";
  if (!s) return "";
  const m = Math.floor(s / 60);
  const sec = s - m * 60;
  return m ? `${m}:${sec.toFixed(1).padStart(4, "0")}` : `${sec.toFixed(1)}`;
}

function fmtClock(s) {
  s = Math.max(0, s);
  const m = Math.floor(s / 60);
  return `${m}:${(s - m * 60).toFixed(1).padStart(4, "0")}`;
}

const NEXT_ICON = { auto_continue: "↪", auto_follow: "⤵", do_nothing: "" };
const NEXT_TEXT = {
  auto_continue: "auto-continue",
  auto_follow: "auto-follow",
  do_nothing: "",
};

function baseName(p) {
  return p ? p.split(/[\\/]/).pop() : "";
}

// ── Load status banner ────────────────────────────────────

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

function renderEmpty() {
  const el = $("empty");
  el.hidden = model.cues.length > 0;
  if (el.hidden) return;
  el.textContent = "";
  if (!model.show) {
    el.textContent = "Waiting for the engine…";
  } else {
    if (model.show.path) {
      el.append(editor.on ? "No cues yet — use “+ Add cue…” above." : "This show has no cues.");
      if (!editor.on) {
        const b = document.createElement("button");
        b.textContent = "Edit show";
        b.addEventListener("click", () => setEditMode(true));
        el.append(document.createElement("br"), b);
      }
    } else {
      el.append("No show loaded.");
      const b = document.createElement("button");
      b.textContent = "Open a show…";
      b.addEventListener("click", openShow);
      const n = document.createElement("button");
      n.textContent = "New show…";
      n.addEventListener("click", newShow);
      el.append(document.createElement("br"), b, " ", n);
    }
  }
}

function renderList() {
  const tbody = $("cues");
  tbody.textContent = "";
  rows = [];
  // Indices may have shifted (reload): rebuild active items from scratch.
  for (const li of activeItems.values()) li.remove();
  activeItems.clear();
  const cues = model.cues;
  const path = model.show?.path ?? "";
  $("show-name").textContent = path ? model.show.name || "Untitled" : "—";
  $("cue-count").textContent = path ? `${model.show.count} cues` : "";
  const broken = cues.filter((c) => c?.problem).length;
  if (broken) {
    const b = document.createElement("span");
    b.className = "broken-count";
    b.textContent = ` · ⚠ ${broken} broken`;
    $("cue-count").append(b);
  }
  $("show-file").textContent = baseName(path);
  $("show-file").title = path;
  $("reload").disabled = !path;
  document.title = model.show?.name ? `${model.show.name} — Show Player` : "Show Player";
  renderEmpty();

  cues.forEach((c, idx) => {
    if (!c) return;
    const tr = document.createElement("tr");
    tr.dataset.idx = idx;
    tr.innerHTML = `
      <td class="c-state"></td>
      <td class="c-num"></td>
      <td class="name"></td>
      <td class="c-type"><span class="type ${c.kind}">${c.kind}</span></td>
      <td class="c-time">${c.pre_wait ? fmt(c.pre_wait) : ""}</td>
      <td class="c-time">${fmt(c.duration)}</td>
      <td class="c-next"><span class="chain" title="${NEXT_TEXT[c.next] ?? c.next}">${NEXT_ICON[c.next] ?? ""}</span></td>`;
    tr.children[1].textContent = c.number;
    tr.children[2].textContent = c.name;
    if (c.problem) {
      tr.classList.add("broken");
      tr.children[2].dataset.problem = c.problem;
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

function renderStandby() {
  const c = model.cues[model.standby];
  if (c) {
    $("sb-number").textContent = c.number;
    $("sb-name").textContent = c.name;
    const bits = [c.kind];
    if (c.pre_wait) bits.push(`pre-wait ${fmt(c.pre_wait)}`);
    if (c.duration) bits.push(fmt(c.duration) === "∞" ? "loops" : fmt(c.duration));
    if (NEXT_TEXT[c.next]) bits.push(NEXT_TEXT[c.next]);
    const meta = $("sb-meta");
    meta.textContent = bits.join(" · ");
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
    $("sb-meta").textContent = "";
  }
  $("go").disabled = !c;
}

function addMeta(meta, cls, text) {
  const s = document.createElement("span");
  s.className = cls;
  s.textContent = ` · ${text}`;
  meta.append(s);
}

function applyState() {
  rows.forEach((tr, idx) => {
    if (!tr) return;
    tr.classList.toggle("standby", idx === model.standby);
    const r = model.running.get(idx);
    tr.classList.toggle("live", !!r);
    tr.classList.remove("running", "pre_wait", "stopping", "paused");
    if (r) tr.classList.add(r.phase);
    else tr.style.removeProperty("--p");
  });

  // active panel: add/remove items, progress is updated per frame
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

  const pa = $("pause-all");
  const live = [...model.running.values()].some((r) => r.phase === "running" || r.phase === "pre_wait");
  const resume = !live && anyPhase("paused");
  pa.textContent = resume ? "Resume all" : "Pause all";
  pa.classList.toggle("resume", resume);
  pa.disabled = !live && !resume;
  renderStandby();
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

  if (!model.show) invoke("refresh"); // UI reloaded after subscribing
  if (standbyChanged) {
    renderStandby();
    rows[model.standby]?.scrollIntoView({ block: "nearest" });
  }
  if (standbyChanged || shapeChanged) applyState();
}

// Smooth progress between 20 Hz updates.
function frame() {
  const now = performance.now();
  const online = now - model.lastState < STALE_MS;
  document.body.classList.toggle("offline", !online);
  $("conn").classList.toggle("on", online);
  $("conn").classList.toggle("off", !online);
  $("conn-text").textContent = online ? "connected" : "offline";

  if (panicArmed && now - panicAt > 300 && !anyPhase("stopping")) panicArmed = false;
  const armed = panicArmed;
  const pb = $("panic");
  if (pb.classList.contains("armed") !== armed) {
    pb.classList.toggle("armed", armed);
    pb.textContent = armed ? "Hard stop" : "Panic";
  }

  for (const [idx, r] of model.running) {
    const moving = online && r.phase !== "paused";
    const el = moving ? r.elapsed + (now - r.at) / 1000 : r.elapsed;
    const p = r.duration > 0 ? Math.min(100, (el / r.duration) * 100) : 0;
    rows[idx]?.style.setProperty("--p", `${p}%`);
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
listen("lsp:load", ({ payload }) => onLoad(payload));

invoke("get_engine").then((a) => ($("engine").value = a));
invoke("refresh").catch(() => {});
renderStandby();
requestAnimationFrame(frame);
