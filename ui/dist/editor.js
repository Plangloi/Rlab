// Edit mode: select cues, change them in the inspector, add / duplicate /
// move / delete. Every change is an edit request to the engine, which writes
// the show file and sends the updated cue list back; nothing is edited
// locally, so all connected UIs stay in sync.
"use strict";

const editor = { on: false, selected: null /* cue number */ };
try {
  editor.on = localStorage.getItem("lsp.editMode") === "1";
} catch {
  /* storage unavailable: default to show mode */
}

const OPTS = {
  type: [
    ["audio", "Audio"],
    ["fade", "Fade"],
    ["stop", "Stop"],
    ["wait", "Wait"],
    ["osc", "OSC"],
    ["midi", "MIDI"],
  ],
  next: [
    ["do_nothing", "Do not continue"],
    ["auto_continue", "Auto-continue"],
    ["auto_follow", "Auto-follow"],
  ],
  second_trigger: [
    ["ignore", "Ignore"],
    ["stop", "Stop (fade out)"],
    ["hard_stop", "Hard stop"],
    ["restart", "Restart"],
  ],
  curve: [
    ["s_curve", "S-curve"],
    ["equal_power", "Equal power"],
    ["linear", "Linear"],
  ],
};

// [key, label, kind, options]. `def`: value that means "default" — sent as
// null so the key is dropped from the file. `optional`: blank = null.
const COMMON_FIELDS = [
  ["number", "Number", "text"],
  ["name", "Name", "text"],
  ["type", "Type", "select"],
  ["pre_wait", "Pre-wait (s)", "num", { def: 0, min: 0 }],
  ["next", "Then", "select", { def: "do_nothing" }],
  ["post_wait", "Post-wait (s)", "num", { def: 0, min: 0, when: (d) => d.next === "auto_continue" }],
  ["second_trigger", "If running", "select", { def: "ignore" }],
];

const TYPE_FIELDS = {
  audio: [
    ["file", "File", "file"],
    ["gain_db", "Level (dB)", "num", { def: 0 }],
    ["fade_in", "Fade in (s)", "num", { def: 0, min: 0 }],
    ["fade_out", "Fade out (s)", "num", { def: 0, min: 0 }],
    ["start", "Start at (s)", "num", { def: 0, min: 0 }],
    ["end", "End at (s)", "num", { optional: true, min: 0, placeholder: "end of file" }],
    ["loop", "Loop", "bool"],
    ["curve", "Fade curve", "select", { def: "s_curve" }],
  ],
  fade: [
    ["target", "Target cue", "target", { audioOnly: true }],
    ["duration", "Duration (s)", "num", { min: 0 }],
    ["gain_db", "To level (dB)", "num", { optional: true, placeholder: "silence" }],
    ["stop", "Stop when done", "bool"],
    ["curve", "Curve", "select", { def: "s_curve" }],
  ],
  stop: [
    ["target", "Target cue", "target", { allowAll: true }],
    ["fade", "Fade (s)", "num", { def: 0, min: 0 }],
  ],
  wait: [["duration", "Duration (s)", "num", { min: 0 }]],
  osc: [
    ["host", "Host:port", "text"],
    ["path", "Address", "text"],
    ["args", "Arguments", "args", { placeholder: '1 2.5 "two words" true' }],
  ],
  midi: [
    ["port", "Port (contains)", "text"],
    ["bytes", "Bytes (hex)", "bytes", { placeholder: "90 3C 7F" }],
  ],
};

// ── Mode & selection ──────────────────────────────────────

function setEditMode(on) {
  editor.on = on;
  try {
    localStorage.setItem("lsp.editMode", on ? "1" : "0");
  } catch {
    /* ignore */
  }
  document.body.classList.toggle("editing", on);
  $("edit-toggle").classList.toggle("on", on);
  $("edit-bar").hidden = !on;
  renderEmpty();
  editorAfterRender();
}

function onRowClick(c) {
  if (editor.on) select(c.number);
  else send("goto", c.number);
}

function select(number) {
  editor.selected = number;
  disarmDelete();
  editorAfterRender();
}

function selectedIdx() {
  return editor.selected == null ? -1 : model.cues.findIndex((c) => c?.number === editor.selected);
}

/** Called by app.js after the cue list is (re)rendered. */
function editorAfterRender() {
  const sel = editor.on ? selectedIdx() : -1;
  rows.forEach((tr, i) => tr?.classList.toggle("selected", i === sel));
  const has = sel >= 0;
  const canEdit = editor.on && !!model.show?.path;
  $("add-type").disabled = !canEdit;
  $("dup").disabled = !canEdit || !has;
  $("move-up").disabled = !canEdit || !has || sel === 0;
  $("move-down").disabled = !canEdit || !has || sel >= model.cues.length - 1;
  $("delete").disabled = !canEdit || !has;
  $("edit-toggle").disabled = !model.show?.path && !editor.on;
  renderInspector();
}

// ── Sending edits ─────────────────────────────────────────

function sendEdit(req) {
  if (!model.show?.path) return showBanner("error", "Open or create a show first");
  req.rev = model.show.rev;
  invoke("edit", { req }).catch((e) => showBanner("error", "Edit failed", String(e)));
}

function update(idx, fields) {
  sendEdit({ op: "update", idx, fields });
}

listen("lsp:edit", ({ payload: r }) => {
  if (!r) return;
  if (r.status === "ok") {
    if (r.number) editor.selected = r.number;
    if (r.op === "new") setEditMode(true);
    editorAfterRender();
  } else {
    showBanner("error", "Edit not applied", r.message);
    renderInspector(true); // put the fields back to what the engine has
  }
});

$("edit-toggle").addEventListener("click", () => setEditMode(!editor.on));
$("new-show").addEventListener("click", newShow);

async function newShow() {
  try {
    await invoke("new_show", { current: model.show?.path || null });
  } catch (e) {
    showBanner("error", "Could not create show", String(e));
  }
}

$("add-type").addEventListener("change", (e) => {
  const type = e.target.value;
  e.target.value = "";
  e.target.blur();
  if (!type) return;
  const sel = selectedIdx();
  const after = sel >= 0 ? sel : model.cues.length ? model.cues.length - 1 : null;
  const fields = {};
  // A new fade / stop targets the selected audio cue.
  const selCue = model.cues[sel];
  if ((type === "fade" || type === "stop") && selCue?.kind === "audio") fields.target = selCue.number;
  sendEdit({ op: "add", after, type, fields });
});

$("dup").addEventListener("click", () => {
  const i = selectedIdx();
  if (i >= 0) sendEdit({ op: "duplicate", idx: i });
});

function move(delta) {
  const i = selectedIdx();
  const to = i + delta;
  if (i >= 0 && to >= 0 && to < model.cues.length) sendEdit({ op: "move", idx: i, to });
}
$("move-up").addEventListener("click", () => move(-1));
$("move-down").addEventListener("click", () => move(1));

// Delete is two-step: first press arms, second (within 3 s) deletes.
let deleteTimer = null;

function disarmDelete() {
  clearTimeout(deleteTimer);
  deleteTimer = null;
  const b = $("delete");
  b.classList.remove("confirm");
  b.textContent = "Delete";
}

function deleteSelected() {
  const i = selectedIdx();
  if (i < 0) return;
  const b = $("delete");
  if (!deleteTimer) {
    b.classList.add("confirm");
    b.textContent = `Delete cue ${model.cues[i].number}?`;
    deleteTimer = setTimeout(disarmDelete, 3000);
    return;
  }
  disarmDelete();
  const neighbour = model.cues[i + 1] ?? model.cues[i - 1];
  sendEdit({ op: "delete", idx: i });
  editor.selected = neighbour?.number ?? null;
}
$("delete").addEventListener("click", deleteSelected);

document.addEventListener("keydown", (e) => {
  if (!editor.on) {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "e" && model.show?.path) {
      e.preventDefault();
      setEditMode(true);
    }
    return;
  }
  const typing = e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement;
  if (e.metaKey || e.ctrlKey) {
    const k = e.key.toLowerCase();
    if (k === "e") {
      e.preventDefault();
      setEditMode(false);
    } else if (k === "d" && !typing) {
      e.preventDefault();
      $("dup").click();
    }
    return;
  }
  if (!typing && (e.key === "Backspace" || e.key === "Delete")) {
    e.preventDefault();
    deleteSelected();
  }
});

// ── Inspector ─────────────────────────────────────────────

const fmtNum = (v) => (v == null ? "" : String(Math.round(v * 1000) / 1000));

function parseArgs(text) {
  const out = [];
  for (const m of text.matchAll(/"([^"]*)"|(\S+)/g)) {
    if (m[1] !== undefined) out.push(m[1]);
    else if (m[2] === "true" || m[2] === "false") out.push(m[2] === "true");
    else if (/^-?\d+$/.test(m[2])) out.push(parseInt(m[2], 10));
    else if (/^-?\d*\.\d+$|^-?\d+\.\d*$/.test(m[2])) out.push(parseFloat(m[2]));
    else out.push(m[2]);
  }
  return out;
}

const fmtArgs = (a) =>
  (a || []).map((x) => (typeof x === "string" && (/\s/.test(x) || x === "" || /^[-\d.]+$|^(true|false)$/.test(x)) ? `"${x}"` : String(x))).join(" ");

function parseBytes(text) {
  const out = [];
  for (const t of text.split(/[\s,]+/).filter(Boolean)) {
    const v = parseInt(t.replace(/^0x/i, ""), 16);
    if (!/^(0x)?[0-9a-f]{1,2}$/i.test(t) || v > 255) throw new Error(`“${t}” is not a byte in hex (00–FF)`);
    out.push(v);
  }
  return out;
}

const fmtBytes = (b) => (b || []).map((x) => x.toString(16).toUpperCase().padStart(2, "0")).join(" ");

function control(f, d, idx) {
  const [key, , kind, o = {}] = f;
  const v = d[key];
  let el;
  const commit = (value) => update(idx, { [key]: value });

  if (kind === "select") {
    el = document.createElement("select");
    for (const [val, text] of OPTS[key]) el.add(new Option(text, val));
    el.value = v ?? o.def ?? "";
    el.addEventListener("change", () => commit(o.def !== undefined && el.value === o.def ? null : el.value));
  } else if (kind === "target") {
    el = document.createElement("select");
    el.add(new Option(o.allowAll ? "(all cues)" : "(choose a cue)", ""));
    model.cues.forEach((c) => {
      if (!c || c.number === d.number || (o.audioOnly && c.kind !== "audio")) return;
      el.add(new Option(`${c.number} ${c.name || ""}`.trim(), c.number));
    });
    if (v && ![...el.options].some((op) => op.value === v)) el.add(new Option(`${v} (missing)`, v));
    el.value = v ?? "";
    el.addEventListener("change", () => commit(el.value === "" ? (o.allowAll ? null : "") : el.value));
  } else if (kind === "bool") {
    el = document.createElement("input");
    el.type = "checkbox";
    el.checked = !!v;
    el.addEventListener("change", () => commit(el.checked ? true : null));
  } else if (kind === "num") {
    el = document.createElement("input");
    el.type = "number";
    el.step = "0.1";
    if (o.min !== undefined) el.min = o.min;
    el.placeholder = o.placeholder ?? "";
    el.value = fmtNum(v);
    el.addEventListener("change", () => {
      if (el.value.trim() === "") {
        if (o.optional || o.def !== undefined) return commit(null);
        el.value = fmtNum(v);
        return;
      }
      const n = parseFloat(el.value);
      if (!Number.isFinite(n)) return void (el.value = fmtNum(v));
      commit(o.def !== undefined && n === o.def ? null : n);
    });
  } else if (kind === "args" || kind === "bytes") {
    el = document.createElement("input");
    el.placeholder = o.placeholder ?? "";
    el.value = kind === "args" ? fmtArgs(v) : fmtBytes(v);
    el.addEventListener("change", () => {
      try {
        commit(kind === "args" ? parseArgs(el.value) : parseBytes(el.value));
      } catch (e) {
        showBanner("error", "Not applied", e.message);
        el.value = kind === "args" ? fmtArgs(v) : fmtBytes(v);
      }
    });
  } else {
    // text / file
    el = document.createElement("input");
    el.spellcheck = false;
    el.value = v ?? "";
    el.addEventListener("change", () => {
      if (key === "number" && el.value.trim() === "") return void (el.value = v ?? "");
      commit(key === "number" ? el.value.trim() : el.value);
    });
    if (kind === "file") {
      const wrap = document.createElement("div");
      wrap.className = "with-btn";
      el.placeholder = "media/file.wav";
      const b = document.createElement("button");
      b.textContent = "Browse…";
      b.addEventListener("click", async () => {
        const path = model.show?.path || "";
        const dir = path.slice(0, Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\")));
        try {
          const picked = await invoke("pick_media", { dir: dir || null });
          if (picked) commit(picked);
        } catch (e) {
          showBanner("error", "Could not choose file", String(e));
        }
      });
      el.dataset.key = key;
      wrap.append(el, b);
      return wrap;
    }
  }
  el.dataset.key = key;
  // Checkboxes and dropdowns let go of focus once committed, so the next
  // Space fires GO instead of toggling / opening the control.
  if (el.tagName === "SELECT" || el.type === "checkbox") el.addEventListener("change", () => el.blur());
  // Enter commits text-ish inputs (change fires on blur otherwise).
  if (el.tagName === "INPUT" && el.type !== "checkbox") {
    el.addEventListener("keydown", (e) => {
      if (e.key === "Enter") el.blur();
      if (e.key === "Escape") {
        e.stopPropagation();
        renderInspector(true);
      }
    });
  }
  return el;
}

let inspectorKey = "";

/**
 * Rebuild the form when the selected cue's data changed (or `force`).
 * Keeps keyboard focus on the same field across rebuilds.
 */
function renderInspector(force = false) {
  const box = $("inspector");
  box.hidden = !editor.on;
  if (!editor.on) return;
  const idx = selectedIdx();
  const c = model.cues[idx];
  const key = c ? `${model.show?.rev}:${idx}:${JSON.stringify(c.data)}:${c.problem}` : "none";
  if (!force && key === inspectorKey) return;
  inspectorKey = key;

  const body = $("insp-body");
  const focused = body.contains(document.activeElement) ? document.activeElement.dataset.key : null;
  body.textContent = "";
  if (!c) {
    $("insp-title").textContent = "Inspector";
    const p = document.createElement("p");
    p.className = "muted";
    p.textContent = model.cues.length ? "Select a cue to edit it." : "Add a cue to get started.";
    body.append(p);
    return;
  }
  const d = c.data || {};
  $("insp-title").textContent = `Cue ${c.number} · ${d.type ?? c.kind}`;
  if (c.problem) {
    const p = document.createElement("div");
    p.className = "problem";
    p.textContent = `⚠ ${c.problem}`;
    body.append(p);
  }
  const fields = [...COMMON_FIELDS, null, ...(TYPE_FIELDS[d.type] || [])];
  for (const f of fields) {
    if (f === null) {
      const s = document.createElement("div");
      s.className = "sep";
      body.append(s);
      continue;
    }
    if (f[3]?.when && !f[3].when(d)) continue;
    const row = document.createElement("div");
    row.className = "f";
    const label = document.createElement("label");
    label.textContent = f[1];
    const ctl = control(f, d, idx);
    const input = ctl.matches?.("input,select") ? ctl : ctl.querySelector("input,select");
    if (input) {
      input.id = `insp-${f[0]}`;
      label.htmlFor = input.id;
    }
    row.append(label, ctl);
    body.append(row);
  }
  if (focused) body.querySelector(`[data-key="${focused}"]`)?.focus();
}

setEditMode(editor.on);
