// Edit mode: select cues, change them in the bottom inspector, add /
// duplicate / move / delete. Every change is an edit request to the engine,
// which writes the show file and sends the updated cue list back; nothing is
// edited locally, so all connected UIs stay in sync.
"use strict";

const editor = { on: false, selected: null /* cue number */, tab: "basics" };
try {
  editor.on = localStorage.getItem("lsp.editMode") === "1";
} catch {
  /* storage unavailable: default to run mode */
}

const TYPE_LABEL = { audio: "Audio", fade: "Fade", stop: "Stop", osc: "OSC", midi: "MIDI", wait: "Wait" };

const OPTS = {
  type: Object.entries(TYPE_LABEL),
  next: [
    ["do_nothing", "Do not continue"],
    ["auto_continue", "Auto-continue"],
    ["auto_follow", "Auto-follow"],
  ],
  second_trigger: [
    ["ignore", "Ignore"],
    ["stop", "Stop"],
    ["hard_stop", "Hard stop"],
    ["restart", "Restart"],
  ],
  curve: [
    ["s_curve", "S-curve"],
    ["equal_power", "Equal power"],
    ["linear", "Linear"],
  ],
};

// Field: [key, label, kind, options]. kind: text | num | seg | select |
// target | bool | file | args | bytes. Options: span (grid columns), def
// (value meaning "default", sent as null so the key leaves the file),
// optional (blank = null), min, placeholder.
const F = {
  number: ["number", "Number", "text"],
  name: ["name", "Name", "text", { span: 3 }],
  pre_wait: ["pre_wait", "Pre-wait", "num", { def: 0, min: 0 }],
  post_wait: ["post_wait", "Post-wait", "num", { def: 0, min: 0 }],
  next: ["next", "Continue", "seg", { span: 3, def: "do_nothing" }],
  type: ["type", "Type", "select", { span: 2 }],
  second_trigger: ["second_trigger", "If GO hits this cue while it is running", "seg", { span: 4, def: "ignore" }],
  timecode: ["timecode", "Timecode (MTC / Art-Net)", "text", { span: 2, optional: true, placeholder: "HH:MM:SS:FF" }],
  curve: ["curve", "Fade curve", "seg", { span: 3, def: "s_curve" }],
};

/** Basics: the design's first row (number, name, main field) and timing row. */
function basicsFields(type) {
  const main = {
    audio: ["file", "File", "file", { span: 2 }],
    fade: ["target", "Target cue", "target", { span: 2, audioOnly: true }],
    stop: ["target", "Target cue", "target", { span: 2, allowAll: true }],
    osc: ["host", "Host:port", "text", { span: 2 }],
    midi: ["port", "Port (name contains)", "text", { span: 2 }],
    wait: ["duration", "Duration (s)", "num", { span: 2, min: 0 }],
  }[type];
  const third = {
    audio: ["gain_db", "Gain dB", "num", { def: 0 }],
    fade: ["duration", "Duration (s)", "num", { min: 0 }],
    stop: ["fade", "Fade (s)", "num", { def: 0, min: 0 }],
    osc: ["path", "Address", "text"],
  }[type];
  return [F.number, F.name, main, F.pre_wait, F.post_wait, third ?? null, F.next].filter((f) => f !== undefined);
}

/** The type tab: everything else that type has. */
function typeFields(type) {
  const rest = {
    audio: [
      ["fade_in", "Fade in (s)", "num", { def: 0, min: 0 }],
      ["fade_out", "Fade out (s)", "num", { def: 0, min: 0 }],
      ["start", "Start at (s)", "num", { def: 0, min: 0 }],
      ["end", "End at (s)", "num", { optional: true, min: 0, placeholder: "end of file" }],
      ["loop", "Loop", "bool"],
      F.curve,
    ],
    fade: [
      ["gain_db", "To level (dB)", "num", { optional: true, placeholder: "silence" }],
      ["stop", "Stop target when done", "bool", { span: 2 }],
      F.curve,
    ],
    stop: [],
    osc: [["args", "Arguments", "args", { span: 4, placeholder: '1 2.5 "two words" true' }]],
    midi: [["bytes", "Bytes (hex)", "bytes", { span: 4, placeholder: "90 3C 7F" }]],
    wait: [],
  }[type];
  return [F.type, ...(rest ?? [])];
}

// ── Mode & selection ──────────────────────────────────────

function setEditMode(on) {
  editor.on = on;
  try {
    localStorage.setItem("lsp.editMode", on ? "1" : "0");
  } catch {
    /* ignore */
  }
  document.body.classList.toggle("editing", on);
  $("edit-badge").hidden = !on;
  $("edit-bar").hidden = !on;
  $("edit-toggle").textContent = on ? "Done" : "Edit";
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
  document.querySelectorAll("#edit-bar .add").forEach((b) => (b.disabled = !canEdit));
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
  const what = req.fields ? ` ${Object.keys(req.fields).join(",")}` : req.type ? ` ${req.type}` : "";
  logTraffic(`/lsp/edit ${req.op}${req.idx != null ? ` #${req.idx}` : ""}${what}`, "l-edit");
  invoke("edit", { req }).catch((e) => showBanner("error", "Edit failed", String(e)));
}

function update(idx, fields) {
  sendEdit({ op: "update", idx, fields });
}

listen("lsp:edit", ({ payload: r }) => {
  if (!r) return;
  logTraffic(`<- /lsp/edit/reply ${r.status} ${r.message ?? r.path ?? ""}`, r.status === "ok" ? "l-edit" : "l-err");
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

function addCue(type) {
  const sel = selectedIdx();
  const after = sel >= 0 ? sel : model.cues.length ? model.cues.length - 1 : null;
  const fields = {};
  // A new fade / stop targets the selected audio cue.
  const selCue = model.cues[sel];
  if ((type === "fade" || type === "stop") && selCue?.kind === "audio") fields.target = selCue.number;
  editor.tab = "basics";
  sendEdit({ op: "add", after, type, fields });
}

document.querySelectorAll("#edit-bar .add").forEach((b) => b.addEventListener("click", () => addCue(b.dataset.type)));

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
  const k = e.key.toLowerCase();
  if (!editor.on) {
    if ((e.metaKey || e.ctrlKey) && k === "e" && model.show?.path) {
      e.preventDefault();
      setEditMode(true);
    }
    return;
  }
  const typing = e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement;
  if (e.metaKey || e.ctrlKey) {
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

document.querySelectorAll(".tabs [role=tab]").forEach((t) =>
  t.addEventListener("click", () => {
    editor.tab = t.dataset.tab;
    renderInspector(true);
  }),
);

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
  (a || [])
    .map((x) => (typeof x === "string" && (/\s/.test(x) || x === "" || /^[-\d.]+$|^(true|false)$/.test(x)) ? `"${x}"` : String(x)))
    .join(" ");

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

/** Build one field (label + control). Returns the wrapper element. */
function field(f, d, idx) {
  const [key, labelText, kind, o = {}] = f;
  const v = d[key];
  const commit = (value) => update(idx, { [key]: value });
  const wrap = document.createElement("div");
  wrap.className = `f${o.span ? ` s${o.span}` : ""}`;
  const id = `insp-${key}`;
  let el;

  if (kind === "seg") {
    const lbl = document.createElement("span");
    lbl.className = "lbl";
    lbl.id = `${id}-l`;
    lbl.textContent = labelText;
    const group = document.createElement("div");
    group.className = "seg";
    group.setAttribute("role", "group");
    group.setAttribute("aria-labelledby", lbl.id);
    const cur = v ?? o.def;
    for (const [val, text] of OPTS[key]) {
      const b = document.createElement("button");
      b.textContent = text;
      b.setAttribute("aria-pressed", String(val === cur));
      b.addEventListener("click", () => val !== cur && commit(o.def !== undefined && val === o.def ? null : val));
      group.append(b);
    }
    wrap.append(lbl, group);
    return wrap;
  }

  if (kind === "bool") {
    wrap.classList.add("check");
    el = document.createElement("input");
    el.type = "checkbox";
    el.id = id;
    el.checked = !!v;
    el.addEventListener("change", () => {
      commit(el.checked ? true : null);
      el.blur(); // so the next Space is GO, not a toggle
    });
    const label = document.createElement("label");
    label.htmlFor = id;
    label.textContent = labelText;
    wrap.append(el, label);
    return wrap;
  }

  if (kind === "select" || kind === "target") {
    el = document.createElement("select");
    if (kind === "select") {
      for (const [val, text] of OPTS[key]) el.add(new Option(text, val));
      el.value = v ?? "";
      el.addEventListener("change", () => commit(el.value));
    } else {
      el.add(new Option(o.allowAll ? "(all cues)" : "(choose a cue)", ""));
      model.cues.forEach((c) => {
        if (!c || c.number === d.number || (o.audioOnly && c.kind !== "audio")) return;
        el.add(new Option(`${c.number} ${c.name || ""}`.trim(), c.number));
      });
      if (v && ![...el.options].some((op) => op.value === v)) el.add(new Option(`${v} (missing)`, v));
      el.value = v ?? "";
      el.addEventListener("change", () => commit(el.value === "" ? (o.allowAll ? null : "") : el.value));
    }
    el.addEventListener("change", () => el.blur());
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
    el.placeholder = o.placeholder ?? "";
    el.addEventListener("change", () => {
      if (key === "number" && el.value.trim() === "") return void (el.value = v ?? "");
      if (o.optional && el.value.trim() === "") return commit(null);
      if (o.optional) return commit(el.value.trim());
      commit(key === "number" ? el.value.trim() : el.value);
    });
  }

  el.id = id;
  el.dataset.key = key;
  if (el.tagName === "INPUT") {
    // Enter commits (change fires on blur otherwise); Esc reverts.
    el.addEventListener("keydown", (e) => {
      if (e.key === "Enter") el.blur();
      if (e.key === "Escape") {
        e.stopPropagation();
        renderInspector(true);
      }
    });
  }
  const label = document.createElement("label");
  label.htmlFor = id;
  label.textContent = labelText;
  if (kind === "file") {
    el.placeholder = "media/file.wav";
    const row = document.createElement("div");
    row.className = "with-btn";
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
    row.append(el, b);
    wrap.append(label, row);
  } else {
    wrap.append(label, el);
  }
  return wrap;
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
  const d = c?.data || {};
  const type = d.type ?? c?.kind;
  const key = c ? `${model.show?.rev}:${idx}:${editor.tab}:${JSON.stringify(d)}:${c.problem}` : `none:${model.cues.length}`;
  if (!force && key === inspectorKey) return;
  inspectorKey = key;

  $("tab-type").textContent = TYPE_LABEL[type] ?? "Type";
  document.querySelectorAll(".tabs [role=tab]").forEach((t) => t.setAttribute("aria-selected", String(t.dataset.tab === editor.tab)));
  const status = $("insp-status");
  status.textContent = c?.problem ? `⚠ ${c.problem}` : "Saved to show file on Enter";
  status.classList.toggle("warn", !!c?.problem);

  const body = $("insp-body");
  const focused = body.contains(document.activeElement) ? document.activeElement.dataset.key : null;
  body.textContent = "";
  if (!c) {
    const p = document.createElement("p");
    p.className = "hint";
    p.textContent = model.cues.length ? "Select a cue to edit it." : "Add a cue with the buttons above.";
    body.append(p);
    return;
  }
  const fields =
    editor.tab === "basics" ? basicsFields(type) : editor.tab === "type" ? typeFields(type) : [F.second_trigger, F.timecode];
  for (const f of fields) {
    if (f === null) {
      body.append(document.createElement("div")); // keeps the design's grid rhythm
      continue;
    }
    body.append(field(f, d, idx));
  }
  if (editor.tab === "type" && fields.length === 1) {
    const p = document.createElement("p");
    p.className = "hint";
    p.textContent = `Nothing else to set for ${TYPE_LABEL[type]} cues.`;
    body.append(p);
  }
  if (focused) body.querySelector(`[data-key="${focused}"]`)?.focus();
}

setEditMode(editor.on);
