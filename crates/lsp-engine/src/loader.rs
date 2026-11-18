//! Show loading: parse the TOML, resolve media paths, decode clips.
//!
//! Decoding can take seconds for long media, so reloads run on a worker
//! thread and hand the result back to the controller. Clips are cached by
//! (path, mtime): a reload only decodes files that changed.

use anyhow::Result;
use crossbeam_channel::Sender;
use lsp_audio::{load_clip, Clip};
use lsp_core::{CueKind, Show};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone)]
pub struct CachedClip {
    pub mtime: Option<SystemTime>,
    pub clip: Clip,
}

pub type ClipCache = HashMap<PathBuf, CachedClip>;

pub struct Loaded {
    /// Canonical path of the show file.
    pub path: PathBuf,
    /// Show file mtime at load (for the watcher).
    pub mtime: Option<SystemTime>,
    /// Media paths already resolved to absolute.
    pub show: Show,
    pub clips: ClipCache,
    /// Non-fatal problems (validation warnings, undecodable media).
    pub warnings: Vec<String>,
    /// Per cue: what would make it misbehave if fired (None = fine).
    pub problems: Vec<Option<String>>,
    pub decoded: usize,
    pub reused: usize,
    pub elapsed: Duration,
}

pub fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Synchronous load. `cache` is consulted for unchanged clips.
pub fn load(path: &Path, sample_rate: u32, cache: &ClipCache) -> Result<Loaded> {
    let t0 = Instant::now();
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let file_mtime = mtime(&path);
    let mut show = Show::load(&path)?;
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut warnings = show.validate();
    let mut problems = show.cue_problems();

    let mut clips = ClipCache::new();
    let (mut decoded, mut reused) = (0, 0);
    for (i, cue) in show.cues.iter_mut().enumerate() {
        let CueKind::Audio(a) = &mut cue.kind else {
            continue;
        };
        if a.file.as_os_str().is_empty() {
            problems[i] = Some("no media file chosen".into());
            continue;
        }
        if a.file.is_relative() {
            a.file = base.join(&a.file);
        }
        if clips.contains_key(&a.file) {
            continue;
        }
        let m = mtime(&a.file);
        if let Some(c) = cache.get(&a.file).filter(|c| m.is_some() && c.mtime == m) {
            clips.insert(a.file.clone(), c.clone());
            reused += 1;
            continue;
        }
        match load_clip(&a.file, sample_rate) {
            Ok(clip) => {
                clips.insert(a.file.clone(), CachedClip { mtime: m, clip });
                decoded += 1;
            }
            Err(e) => {
                warnings.push(format!("cue {}: {e:#}", cue.number));
                let msg = format!("media: {e:#}");
                problems[i] = Some(match problems[i].take() {
                    Some(p) => format!("{p}; {msg}"),
                    None => msg,
                });
            }
        }
    }

    Ok(Loaded {
        path,
        mtime: file_mtime,
        show,
        clips,
        warnings,
        problems,
        decoded,
        reused,
        elapsed: t0.elapsed(),
    })
}

pub type LoadResult = std::result::Result<Loaded, (PathBuf, String)>;

/// Load on a worker thread; the result arrives on `tx`.
pub fn spawn(path: PathBuf, sample_rate: u32, cache: ClipCache, tx: Sender<LoadResult>) {
    let r = std::thread::Builder::new()
        .name("lsp-load".into())
        .spawn(move || {
            let res = load(&path, sample_rate, &cache).map_err(|e| (path, format!("{e:#}")));
            // Drop the old cache here, not on the control thread.
            drop(cache);
            let _ = tx.send(res);
        });
    if let Err(e) = r {
        log::error!("spawn loader: {e}");
    }
}

/// For each cue index in `old`, its index in `new` (matched by cue number,
/// following a known `rename` (old, new) from an edit).
pub fn remap(old: &Show, new: &Show, rename: Option<&(String, String)>) -> Vec<Option<usize>> {
    old.cues
        .iter()
        .map(|c| match rename {
            Some((from, to)) if *from == c.number => new.find(to),
            _ => new.find(&c.number),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn demo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../shows/demo.toml")
    }

    #[test]
    fn load_demo_and_reuse_cache() {
        let a = load(&demo(), 48_000, &ClipCache::new()).unwrap();
        assert_eq!(a.show.cues.len(), 6);
        assert_eq!((a.decoded, a.reused), (2, 0));
        assert!(a.warnings.is_empty(), "{:?}", a.warnings);
        assert!(a.problems.iter().all(Option::is_none));
        match &a.show.cues[0].kind {
            CueKind::Audio(c) => assert!(c.file.is_absolute()),
            k => panic!("{k:?}"),
        }

        let b = load(&demo(), 48_000, &a.clips).unwrap();
        assert_eq!((b.decoded, b.reused), (0, 2));
        let (k, v) = a.clips.iter().next().unwrap();
        assert!(Arc::ptr_eq(&v.clip, &b.clips[k].clip));
    }

    #[test]
    fn parse_error_is_err() {
        let dir = std::env::temp_dir().join(format!("lsp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.toml");
        std::fs::write(&p, "name = \"x\"\n[[cue]]\nnumber = 1\n").unwrap();
        assert!(load(&p, 48_000, &ClipCache::new()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remap_by_number() {
        let parse = |s: &str| -> Show { toml::from_str(s).unwrap() };
        let old = parse(
            "name='a'\n[[cue]]\nnumber='1'\ntype='wait'\nduration=1\n[[cue]]\nnumber='2'\ntype='wait'\nduration=1\n",
        );
        let new = parse(
            "name='a'\n[[cue]]\nnumber='0'\ntype='wait'\nduration=1\n[[cue]]\nnumber='1'\ntype='wait'\nduration=1\n",
        );
        assert_eq!(remap(&old, &new, None), vec![Some(1), None]);
        let r = ("2".to_string(), "0".to_string());
        assert_eq!(remap(&old, &new, Some(&r)), vec![Some(1), Some(0)]);
    }
}
