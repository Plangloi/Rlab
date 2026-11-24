//! Cue editing on the show file itself.
//!
//! Edits arrive as JSON (`/lsp/edit`), are applied to the TOML with
//! `toml_edit` so comments and formatting survive, validated by parsing the
//! result as a `Show`, and written atomically. The controller then reloads
//! the file through the normal hot-reload path.

use anyhow::{anyhow, bail, Context, Result};
use lsp_core::Show;
use serde::Deserialize;
use serde_json::{Map, Value as Json};
use std::collections::HashSet;
use std::path::Path;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

#[derive(Debug, Deserialize)]
pub struct Request {
    /// Show revision the client was looking at; stale edits are refused.
    pub rev: u64,
    #[serde(flatten)]
    pub edit: Edit,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Edit {
    /// Insert a new cue after `after` (None = at the top).
    Add {
        #[serde(default)]
        after: Option<usize>,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        fields: Map<String, Json>,
    },
    /// Set fields on a cue; `null` removes a field (back to its default).
    Update { idx: usize, fields: Map<String, Json> },
    Delete { idx: usize },
    Duplicate { idx: usize },
    /// Move cue `idx` so it ends up at position `to`.
    Move { idx: usize, to: usize },
}

impl Edit {
    pub fn name(&self) -> &'static str {
        match self {
            Edit::Add { .. } => "add",
            Edit::Update { .. } => "update",
            Edit::Delete { .. } => "delete",
            Edit::Duplicate { .. } => "duplicate",
            Edit::Move { .. } => "move",
        }
    }
}

pub struct Outcome {
    pub text: String,
    /// Index of the cue the edit produced or changed (None for delete).
    pub idx: Option<usize>,
    pub number: Option<String>,
    /// (old, new) cue number, so a running cue survives being renumbered.
    pub rename: Option<(String, String)>,
    pub summary: String,
}

const COMMON: &[&str] = &[
    "number",
    "name",
    "type",
    "pre_wait",
    "post_wait",
    "next",
    "second_trigger",
    "timecode",
];

fn type_keys(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "audio" => &[
            "file", "gain_db", "fade_in", "fade_out", "start", "end", "loop", "curve",
        ],
        "fade" => &["target", "duration", "gain_db", "stop", "curve"],
        "stop" => &["target", "fade"],
        "osc" => &["host", "path", "args"],
        "midi" => &["port", "bytes"],
        "wait" => &["duration"],
        _ => return None,
    })
}

/// Fields a freshly created / retyped cue needs to parse.
fn type_defaults(kind: &str) -> Vec<(&'static str, Value)> {
    match kind {
        "audio" => vec![("file", "".into())],
        "fade" => vec![("target", "".into()), ("duration", 3.into())],
        "osc" => vec![("host", "127.0.0.1:8000".into()), ("path", "/".into())],
        "midi" => vec![("port", "".into()), ("bytes", Value::Array(Array::new()))],
        "wait" => vec![("duration", 1.into())],
        _ => vec![],
    }
}

fn json_to_toml(key: &str, v: &Json) -> Result<Value> {
    Ok(match v {
        Json::Bool(b) => (*b).into(),
        Json::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64().ok_or_else(|| anyhow!("{key}: bad number"))?.into(),
        },
        Json::String(s) => s.as_str().into(),
        Json::Array(a) => {
            let mut arr = Array::new();
            for x in a {
                arr.push(json_to_toml(key, x)?);
            }
            Value::Array(arr)
        }
        Json::Null | Json::Object(_) => bail!("{key}: unsupported value {v}"),
    })
}

fn cues_mut(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables> {
    if !doc.contains_key("cue") {
        doc.insert("cue", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    doc["cue"]
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow!("`cue` in the show file is not a [[cue]] list"))
}

fn str_field(t: &Table, k: &str) -> Option<String> {
    t.get(k).and_then(Item::as_str).map(str::to_string)
}

/// Store media paths relative to the show when they live under it.
fn relativize(file: &str, base: &Path) -> String {
    let p = Path::new(file);
    match p.strip_prefix(base) {
        Ok(rel) if p.is_absolute() => rel.to_string_lossy().into_owned(),
        _ => file.to_string(),
    }
}

/// Apply `fields` to a cue table (type changes drop fields the new type
/// doesn't use and add the ones it needs).
fn set_fields(t: &mut Table, fields: &Map<String, Json>, base: &Path) -> Result<()> {
    if let Some(new_kind) = fields.get("type") {
        let new_kind = new_kind
            .as_str()
            .ok_or_else(|| anyhow!("type must be a string"))?;
        let keys = type_keys(new_kind).ok_or_else(|| anyhow!("unknown cue type {new_kind}"))?;
        if str_field(t, "type").as_deref() != Some(new_kind) {
            let stale: Vec<String> = t
                .iter()
                .map(|(k, _)| k.to_string())
                .filter(|k| !COMMON.contains(&k.as_str()) && !keys.contains(&k.as_str()))
                .collect();
            for k in stale {
                t.remove(&k);
            }
            t["type"] = toml_edit::value(new_kind);
            for (k, v) in type_defaults(new_kind) {
                if !t.contains_key(k) {
                    t[k] = Item::Value(v);
                }
            }
        }
    }
    let kind = str_field(t, "type").unwrap_or_default();
    let keys = type_keys(&kind).unwrap_or(&[]);
    for (k, v) in fields {
        if k == "type" {
            continue;
        }
        if !COMMON.contains(&k.as_str()) && !keys.contains(&k.as_str()) {
            bail!("{kind} cues have no field `{k}`");
        }
        if v.is_null() {
            if k == "number" {
                bail!("a cue needs a number");
            }
            // Required fields fall back to their default rather than vanish.
            match type_defaults(&kind).into_iter().find(|(d, _)| d == k) {
                Some((_, dv)) => t[k.as_str()] = Item::Value(dv),
                None => {
                    t.remove(k);
                }
            }
            continue;
        }
        let mut val = json_to_toml(k, v)?;
        if k == "number" {
            let n = v.as_str().map(str::trim).unwrap_or("");
            if n.is_empty() || n.contains(char::is_whitespace) {
                bail!("cue number must be non-empty without spaces");
            }
            val = n.into();
        }
        if k == "file" {
            if let Some(f) = v.as_str() {
                val = relativize(f, base).into();
            }
        }
        // Keep the existing key's decoration (comments, spacing) if present.
        match t.get_mut(k).and_then(Item::as_value_mut) {
            Some(old) => {
                let decor = old.decor().clone();
                *old = val;
                *old.decor_mut() = decor;
            }
            None => t[k.as_str()] = Item::Value(val),
        }
    }
    Ok(())
}

/// A number for a cue inserted after `after` (None = at the top), avoiding
/// `used`. Prefers the next integer, then the midpoint with few decimals.
pub fn auto_number(numbers: &[String], after: Option<usize>) -> String {
    let used: HashSet<&str> = numbers.iter().map(String::as_str).collect();
    let num = |i: usize| numbers.get(i).and_then(|n| n.parse::<f64>().ok());
    let lo = after.and_then(num);
    let hi = num(after.map(|a| a + 1).unwrap_or(0));
    let fmt = |x: f64, d: usize| {
        let s = format!("{x:.d$}");
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    let free = |s: &str| !used.contains(s);
    let lo_v = lo.unwrap_or(0.0);
    if after.is_none() || lo.is_some() {
        let next_int = lo_v.floor() + 1.0;
        if hi.is_none_or(|h| next_int < h) {
            let s = fmt(next_int, 0);
            if free(&s) {
                return s;
            }
        }
        let hi_v = hi.unwrap_or(lo_v + 1.0);
        for d in 1..=4 {
            let m = (lo_v + hi_v) / 2.0;
            let r: f64 = fmt(m, d).parse().unwrap_or(m);
            let s = fmt(m, d);
            if r > lo_v && r < hi_v && free(&s) {
                return s;
            }
        }
    }
    // Non-numeric neighbours: suffix the previous number.
    let stem = after.and_then(|a| numbers.get(a)).cloned().unwrap_or_default();
    (1..)
        .map(|i| {
            if stem.is_empty() {
                i.to_string()
            } else {
                format!("{stem}.{i}")
            }
        })
        .find(|s| free(s))
        .expect("unbounded")
}

/// Rebuild the [[cue]] list in a new order while keeping it in the same
/// place in the document (toml_edit renders tables by stored position).
fn replace_cues(doc: &mut DocumentMut, tables: Vec<Table>) -> Result<()> {
    let n = tables.len();
    let old_positions: Vec<usize> = cues_mut(doc)?
        .iter()
        .filter_map(Table::position)
        .collect();
    let others_max = doc
        .as_table()
        .iter()
        .filter_map(|(k, it)| (k != "cue").then_some(it).and_then(Item::as_table))
        .filter_map(Table::position)
        .max();
    let base = old_positions
        .iter()
        .min()
        .copied()
        .unwrap_or_else(|| others_max.map(|m| m + 1).unwrap_or(1));
    // Push tables that sat after the cue list further down to make room.
    for (k, it) in doc.as_table_mut().iter_mut() {
        if k == "cue" {
            continue;
        }
        if let Some(t) = it.as_table_mut() {
            if let Some(p) = t.position().filter(|p| *p >= base) {
                t.set_position(p + n + 1);
            }
        }
    }
    let aot = cues_mut(doc)?;
    aot.clear();
    for (i, mut t) in tables.into_iter().enumerate() {
        t.set_position(base + i);
        aot.push(t);
    }
    Ok(())
}

fn check(i: usize, len: usize) -> Result<()> {
    if i >= len {
        bail!("no cue at index {i} (show has {len})");
    }
    Ok(())
}

pub fn apply(text: &str, base: &Path, edit: &Edit) -> Result<Outcome> {
    let mut doc: DocumentMut = text.parse().context("show file is not valid TOML")?;
    let mut tables: Vec<Table> = cues_mut(&mut doc)?.iter().cloned().collect();
    let numbers: Vec<String> = tables
        .iter()
        .map(|t| str_field(t, "number").unwrap_or_default())
        .collect();
    let len = tables.len();

    let (idx, rename, summary) = match edit {
        Edit::Add {
            after,
            kind,
            fields,
        } => {
            if let Some(a) = after {
                check(*a, len)?;
            }
            type_keys(kind).ok_or_else(|| anyhow!("unknown cue type {kind}"))?;
            let number = match fields.get("number").and_then(Json::as_str) {
                Some(n) => n.to_string(),
                None => auto_number(&numbers, *after),
            };
            let mut t = Table::new();
            t["number"] = toml_edit::value(number.as_str());
            t["name"] = toml_edit::value("");
            t["type"] = toml_edit::value(kind.as_str());
            for (k, v) in type_defaults(kind) {
                t[k] = Item::Value(v);
            }
            let mut f = fields.clone();
            f.remove("number");
            f.remove("type");
            set_fields(&mut t, &f, base)?;
            let at = after.map(|a| a + 1).unwrap_or(0);
            tables.insert(at, t);
            (Some(at), None, format!("added {kind} cue {number}"))
        }
        Edit::Update { idx, fields } => {
            check(*idx, len)?;
            let old = numbers[*idx].clone();
            set_fields(&mut tables[*idx], fields, base)?;
            let new = str_field(&tables[*idx], "number").unwrap_or_default();
            let rename = (new != old).then(|| (old.clone(), new.clone()));
            let what: Vec<&str> = fields.keys().map(String::as_str).collect();
            let mut summary = format!("cue {new}: set {}", what.join(", "));
            if rename.is_some() {
                // Keep fade / stop cues pointing at the renumbered cue.
                let mut n = 0;
                for (j, t) in tables.iter_mut().enumerate() {
                    if j == *idx || str_field(t, "target").as_deref() != Some(old.as_str()) {
                        continue;
                    }
                    if let Some(v) = t.get_mut("target").and_then(Item::as_value_mut) {
                        let decor = v.decor().clone();
                        *v = new.as_str().into();
                        *v.decor_mut() = decor;
                        n += 1;
                    }
                }
                if n > 0 {
                    summary.push_str(&format!(" (retargeted {n} cue{})", if n == 1 { "" } else { "s" }));
                }
            }
            (Some(*idx), rename, summary)
        }
        Edit::Delete { idx } => {
            check(*idx, len)?;
            tables.remove(*idx);
            (None, None, format!("deleted cue {}", numbers[*idx]))
        }
        Edit::Duplicate { idx } => {
            check(*idx, len)?;
            let mut t = tables[*idx].clone();
            let number = auto_number(&numbers, Some(*idx));
            t["number"] = toml_edit::value(number.as_str());
            t.decor_mut().clear();
            tables.insert(idx + 1, t);
            (
                Some(idx + 1),
                None,
                format!("duplicated cue {} as {number}", numbers[*idx]),
            )
        }
        Edit::Move { idx, to } => {
            check(*idx, len)?;
            check(*to, len)?;
            let t = tables.remove(*idx);
            tables.insert(*to, t);
            (Some(*to), None, format!("moved cue {}", numbers[*idx]))
        }
    };

    let number = idx.and_then(|i| str_field(&tables[i], "number"));
    if let (Some(i), Some(n)) = (idx, number.as_deref()) {
        let clash = tables
            .iter()
            .enumerate()
            .any(|(j, t)| j != i && str_field(t, "number").as_deref() == Some(n));
        if clash {
            bail!("cue number {n} is already used");
        }
    }
    replace_cues(&mut doc, tables)?;
    let text = doc.to_string();
    toml::from_str::<Show>(&text).map_err(|e| anyhow!("edit would make the show invalid: {e}"))?;
    Ok(Outcome {
        text,
        idx,
        number,
        rename,
        summary,
    })
}

/// Write via a temp file + rename so a crash never leaves a half-written show.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub fn new_show_text(name: &str) -> String {
    let mut doc = DocumentMut::new();
    doc["name"] = toml_edit::value(name);
    let mut settings = Table::new();
    settings["panic_fade"] = toml_edit::value(1.0);
    doc["settings"] = Item::Table(settings);
    format!(
        "# Show file — edited by the Show Player UI; hand edits are kept.\n{}",
        doc
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SHOW: &str = r#"# My show
name = "T"

[settings]
panic_fade = 2.0 # seconds

# Opening
[[cue]]
number = "1"
name = "Loop"
type = "audio"
file = "media/a.wav"
loop = true # keep going

[[cue]]
number = "2"
name = "Fade"
type = "fade"
target = "1"
duration = 3

# Last one
[[cue]]
number = "3"
type = "wait"
duration = 1
"#;

    fn run(edit: serde_json::Value) -> Result<Outcome> {
        let e: Edit = serde_json::from_value(edit).unwrap();
        apply(SHOW, Path::new("/shows"), &e)
    }

    fn numbers(text: &str) -> Vec<String> {
        toml::from_str::<Show>(text)
            .unwrap()
            .cues
            .into_iter()
            .map(|c| c.number)
            .collect()
    }

    #[test]
    fn update_keeps_comments_and_reports_rename() {
        let o = run(json!({"op":"update","idx":0,"fields":{"number":"10","gain_db":-6}})).unwrap();
        assert!(o.text.contains("# My show"));
        assert!(o.text.contains("loop = true # keep going"));
        assert!(o.text.contains("panic_fade = 2.0 # seconds"));
        assert!(o.text.contains("gain_db = -6"));
        assert_eq!(o.rename, Some(("1".into(), "10".into())));
        assert_eq!(numbers(&o.text), ["10", "2", "3"]);
        // the fade that targeted cue 1 follows the renumber
        let s: Show = toml::from_str(&o.text).unwrap();
        assert!(matches!(&s.cues[1].kind, lsp_core::CueKind::Fade { target, .. } if target == "10"));
        assert!(o.summary.contains("retargeted 1 cue"), "{}", o.summary);
    }

    #[test]
    fn add_auto_numbers_between() {
        let o = run(json!({"op":"add","after":0,"type":"wait"})).unwrap();
        assert_eq!(numbers(&o.text), ["1", "1.5", "2", "3"]);
        assert_eq!(o.idx, Some(1));
        let o = run(json!({"op":"add","after":2,"type":"audio","fields":{"file":"/shows/media/b.wav"}})).unwrap();
        assert_eq!(numbers(&o.text), ["1", "2", "3", "4"]);
        assert!(o.text.contains("file = \"media/b.wav\""), "{}", o.text);
        let o = run(json!({"op":"add","type":"stop"})).unwrap();
        assert_eq!(numbers(&o.text), ["0.5", "1", "2", "3"]);
    }

    #[test]
    fn move_delete_duplicate_keep_document_order() {
        let o = run(json!({"op":"move","idx":2,"to":0})).unwrap();
        assert_eq!(numbers(&o.text), ["3", "1", "2"]);
        // comment travels with its cue; settings stay above the cues
        let last_one = o.text.find("# Last one").unwrap();
        assert!(last_one < o.text.find("# Opening").unwrap());
        assert!(o.text.find("[settings]").unwrap() < last_one);

        let o = run(json!({"op":"delete","idx":1})).unwrap();
        assert_eq!(numbers(&o.text), ["1", "3"]);
        assert_eq!(o.idx, None);

        let o = run(json!({"op":"duplicate","idx":0})).unwrap();
        assert_eq!(numbers(&o.text), ["1", "1.5", "2", "3"]);
    }

    #[test]
    fn retype_drops_foreign_fields() {
        let o = run(json!({"op":"update","idx":1,"fields":{"type":"wait"}})).unwrap();
        let s: Show = toml::from_str(&o.text).unwrap();
        assert!(matches!(s.cues[1].kind, lsp_core::CueKind::Wait { .. }));
        let cue2 = &o.text[o.text.find("number = \"2\"").unwrap()..];
        assert!(!cue2[..cue2.find("[[cue]]").unwrap()].contains("target"));
    }

    #[test]
    fn rejects_bad_edits() {
        let err = |v| run(v).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err(json!({"op":"update","idx":0,"fields":{"number":"2"}})).contains("already used"));
        assert!(err(json!({"op":"update","idx":0,"fields":{"duration":2}})).contains("no field"));
        assert!(err(json!({"op":"update","idx":2,"fields":{"duration":"x"}})).contains("invalid"));
        assert!(err(json!({"op":"delete","idx":9})).contains("no cue at index"));
        assert!(err(json!({"op":"add","type":"laser"})).contains("unknown cue type"));
    }

    #[test]
    fn null_resets_field() {
        let o = run(json!({"op":"update","idx":0,"fields":{"loop":null}})).unwrap();
        assert!(!o.text.contains("loop ="));
        // required field falls back to its default instead of disappearing
        let o = run(json!({"op":"update","idx":2,"fields":{"duration":null}})).unwrap();
        assert!(toml::from_str::<Show>(&o.text).is_ok());
    }

    #[test]
    fn auto_number_cases() {
        let n = |v: &[&str], a| auto_number(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>(), a);
        assert_eq!(n(&[], None), "1");
        assert_eq!(n(&["1", "2"], Some(1)), "3");
        assert_eq!(n(&["1", "2"], Some(0)), "1.5");
        assert_eq!(n(&["1", "1.5"], Some(0)), "1.2");
        assert_eq!(n(&["A", "B"], Some(0)), "A.1");
        assert_eq!(n(&["1"], None), "0.5");
    }

    #[test]
    fn new_show_parses() {
        let s: Show = toml::from_str(&new_show_text("Tour")).unwrap();
        assert_eq!(s.name, "Tour");
        assert!(s.cues.is_empty());
    }
}
