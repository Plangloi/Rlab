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
$("panic").addEventListener("click", () => send("panic"));

document.addEventListener("keydown", (e) => {
  if (e.target instanceof HTMLInputElement) return;
  switch (e.key) {
    case " ":
      e.preventDefault();
      if (!e.repeat) go();
      break;
    case "Escape":
      e.preventDefault();
      send("stop");
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

function cueLabel(c) {
  return c ? `${c.number} ${c.name}` : "";
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

function renderList() {
  const tbody = $("cues");
  tbody.textContent = "";
  rows = [];
  const cues = model.cues;
  $("show-name").textContent = model.show?.name ?? "—";
  $("cue-count").textContent = model.show ? `${model.show.count} cues` : "";
  $("empty").hidden = cues.length > 0;

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
    tr.addEventListener("click", () => send("goto", c.number));
    tbody.appendChild(tr);
    rows[idx] = tr;
  });
  applyState();
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
    $("sb-meta").textContent = bits.join(" · ");
  } else {
    $("sb-number").textContent = "—";
    $("sb-name").textContent = model.show ? "End of cue list" : "No show loaded";
    $("sb-meta").textContent = "";
  }
  $("go").disabled = !c;
}

function applyState() {
  rows.forEach((tr, idx) => {
    if (!tr) return;
    tr.classList.toggle("standby", idx === model.standby);
    const r = model.running.get(idx);
    tr.classList.toggle("live", !!r);
    tr.classList.remove("running", "pre_wait", "stopping");
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
        <div class="row"><span class="t"><b></b><span></span></span><button title="Stop this cue">Stop</button></div>
        <div class="bar"><i></i></div>
        <div class="times"><span class="el"></span><span class="ph"></span><span class="rem"></span></div>`;
      li.querySelector("b").textContent = c?.number ?? idx;
      li.querySelector(".t span").textContent = c?.name ?? "";
      li.querySelector("button").addEventListener("click", () => c && send("stop", c.number));
      activeItems.set(idx, li);
    }
    li.className = r.phase;
    li.querySelector(".ph").textContent = r.phase.replace("_", "-");
    li.querySelector(".bar").classList.toggle("inf", r.duration < 0);
    ul.appendChild(li); // keeps order sorted
  }
  $("active-empty").hidden = model.running.size > 0;
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

  for (const [idx, r] of model.running) {
    const el = online ? r.elapsed + (now - r.at) / 1000 : r.elapsed;
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

invoke("get_engine").then((a) => ($("engine").value = a));
invoke("refresh").catch(() => {});
renderStandby();
requestAnimationFrame(frame);
