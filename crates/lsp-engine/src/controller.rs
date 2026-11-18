//! Cue sequencer. Single-threaded event loop: control commands in, audio
//! events in, timer heap for pre-wait / post-wait / durations.

use anyhow::Result;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lsp_audio::{AudioCmd, AudioEvent, AudioHandle, Curve, VoiceId};
use lsp_core::{
    db_to_lin, AudioCue, Command, CueKind, FadeCurve, NextAction, SecondTrigger, Show,
};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::feedback::{self, RunInfo, Subscribers};
use crate::loader::{self, ClipCache, LoadResult, Loaded};
use crate::{midi::MidiOut, osc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Action {
    /// Pre-wait elapsed → run action.
    Fire(usize),
    /// Post-wait elapsed (auto-continue) → trigger idx+1.
    Continue(usize),
    /// Timed cue (fade / wait) completed.
    Done(usize),
}

impl Action {
    fn cue(self) -> usize {
        match self {
            Action::Fire(i) | Action::Continue(i) | Action::Done(i) => i,
        }
    }

    fn map(self, f: impl Fn(usize) -> Option<usize>) -> Option<Self> {
        Some(match self {
            Action::Fire(i) => Action::Fire(f(i)?),
            Action::Continue(i) => Action::Continue(f(i)?),
            Action::Done(i) => Action::Done(f(i)?),
        })
    }
}

/// A timer frozen by pause, with the time it had left.
#[derive(Debug, Clone, Copy)]
struct PausedTimer {
    remaining: Duration,
    action: Action,
}

fn curve(c: FadeCurve) -> Curve {
    match c {
        FadeCurve::Linear => Curve::Linear,
        FadeCurve::SCurve => Curve::SCurve,
        FadeCurve::EqualPower => Curve::EqualPower,
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Timer {
    at: Instant,
    seq: u64,
    action: Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    PreWait,
    Running,
    Stopping,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::PreWait => "pre_wait",
            Phase::Running => "running",
            Phase::Stopping => "stopping",
        }
    }
}

/// What a cue is doing right now (for feedback only; the sequencer itself
/// runs off `timers` and `cue_voices`).
#[derive(Debug, Clone, Copy)]
struct RunState {
    phase: Phase,
    /// Shifted forward on resume, so `elapsed` excludes paused time.
    started: Instant,
    paused_at: Option<Instant>,
    /// Seconds; -1 = unbounded.
    duration: f32,
}

impl RunState {
    fn new(phase: Phase, duration: f32) -> Self {
        Self {
            phase,
            started: Instant::now(),
            paused_at: None,
            duration,
        }
    }

    fn elapsed(&self) -> f32 {
        self.paused_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started)
            .as_secs_f32()
    }

    fn phase_str(&self) -> &'static str {
        if self.paused_at.is_some() && self.phase != Phase::Stopping {
            "paused"
        } else {
            self.phase.as_str()
        }
    }
}

pub struct Controller {
    show: Show,
    /// File the current show came from (None = started empty).
    show_path: Option<PathBuf>,
    show_mtime: Option<SystemTime>,
    playhead: usize,
    audio: AudioHandle,
    clips: ClipCache,
    voice_cue: HashMap<VoiceId, usize>,
    cue_voices: HashMap<usize, Vec<VoiceId>>,
    next_voice: VoiceId,
    timers: BinaryHeap<Reverse<Timer>>,
    paused_timers: Vec<PausedTimer>,
    seq: u64,
    /// End of the current panic fade; a panic before then hard-stops.
    panic_until: Option<Instant>,
    /// Per-cue problems (missing media, bad targets, ...), sent to UIs.
    problems: Vec<Option<String>>,
    osc_out: UdpSocket,
    midi: MidiOut,
    subs: Subscribers,
    states: HashMap<usize, RunState>,
    durations: Vec<f32>,
    dirty: bool,
    last_push: Instant,
    /// Poll the show file's mtime and reload on change.
    watch: bool,
    last_watch: Instant,
    loading: bool,
    /// Load requested while another was in flight; runs when it finishes.
    queued_load: Option<PathBuf>,
    load_tx: Sender<LoadResult>,
    load_rx: Receiver<LoadResult>,
}

const TICK: Duration = Duration::from_millis(5);
/// Feedback rate while cues are running / while idle (heartbeat).
const PUSH_ACTIVE: Duration = Duration::from_millis(50);
const PUSH_IDLE: Duration = Duration::from_secs(1);
const WATCH_EVERY: Duration = Duration::from_millis(500);
/// Hard stops still ramp this long to avoid a click.
const HARD_STOP_FADE: f32 = 0.01;

impl Controller {
    /// `loaded` = initial show (None → start empty and wait for `/lsp/load`).
    pub fn new(
        loaded: Option<Loaded>,
        audio: AudioHandle,
        feedback_sock: UdpSocket,
        watch: bool,
    ) -> Result<Self> {
        let (load_tx, load_rx) = crossbeam_channel::unbounded();
        let mut ctl = Self {
            show: Show::empty(),
            show_path: None,
            show_mtime: None,
            playhead: 0,
            audio,
            clips: ClipCache::new(),
            voice_cue: HashMap::new(),
            cue_voices: HashMap::new(),
            next_voice: 1,
            timers: BinaryHeap::new(),
            paused_timers: Vec::new(),
            seq: 0,
            panic_until: None,
            problems: Vec::new(),
            osc_out: UdpSocket::bind("0.0.0.0:0")?,
            midi: MidiOut::default(),
            subs: Subscribers::new(feedback_sock),
            states: HashMap::new(),
            durations: Vec::new(),
            dirty: true,
            last_push: Instant::now(),
            watch,
            last_watch: Instant::now(),
            loading: false,
            queued_load: None,
            load_tx,
            load_rx,
        };
        match loaded {
            Some(l) => ctl.apply_show(l),
            None => log::info!("no show loaded — waiting for /lsp/load"),
        }
        Ok(ctl)
    }

    pub fn run(&mut self, rx: Receiver<Command>) {
        self.print_status();
        loop {
            let wait = self
                .timers
                .peek()
                .map(|Reverse(t)| t.at.saturating_duration_since(Instant::now()))
                .unwrap_or(TICK)
                .min(TICK);
            match rx.recv_timeout(wait) {
                Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => {
                    self.handle(cmd);
                    self.dirty = true;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.poll_audio();
            self.run_timers();
            self.poll_loads();
            self.watch_file();
            self.push_feedback();
        }
        self.audio.send(AudioCmd::FadeAll {
            frames: 0,
            curve: Curve::Linear,
        });
        std::thread::sleep(Duration::from_millis(50));
        log::info!("bye");
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Go => self.go(),
            Command::Stop(None) => self.stop_all(None),
            Command::Stop(Some(n)) => {
                if let Some(i) = self.find("stop", &n) {
                    self.stop_cue(i, None)
                }
            }
            Command::HardStop(None) => self.hard_stop_all(),
            Command::HardStop(Some(n)) => {
                if let Some(i) = self.find("hardstop", &n) {
                    self.hard_stop_cue(i)
                }
            }
            Command::Pause(None) => {
                let all = self.active_cues();
                log::info!("pause all ({} cues)", all.len());
                all.into_iter().for_each(|i| self.pause_cue(i));
            }
            Command::Pause(Some(n)) => {
                if let Some(i) = self.find("pause", &n) {
                    self.pause_cue(i)
                }
            }
            Command::Resume(None) => {
                let mut all: Vec<usize> = self
                    .states
                    .iter()
                    .filter(|(_, s)| s.paused_at.is_some())
                    .map(|(&i, _)| i)
                    .chain(self.paused_timers.iter().map(|t| t.action.cue()))
                    .collect();
                all.sort_unstable();
                all.dedup();
                log::info!("resume all ({} cues)", all.len());
                all.into_iter().for_each(|i| self.resume_cue(i));
            }
            Command::Resume(Some(n)) => {
                if let Some(i) = self.find("resume", &n) {
                    self.resume_cue(i)
                }
            }
            Command::Panic => self.panic(),
            Command::Goto(n) => match self.show.find(&n) {
                Some(i) => {
                    self.playhead = i;
                    self.print_status();
                }
                None => log::warn!("goto: no cue {n}"),
            },
            Command::Next => {
                self.playhead = (self.playhead + 1).min(self.show.cues.len());
                self.print_status();
            }
            Command::Prev => {
                self.playhead = self.playhead.saturating_sub(1);
                self.print_status();
            }
            Command::Status => self.print_status(),
            Command::Subscribe(addr) => {
                if self.subs.subscribe(addr) {
                    log::info!("feedback: {addr} subscribed");
                    self.send_cue_list(addr);
                }
            }
            Command::Unsubscribe(addr) => {
                if self.subs.unsubscribe(addr) {
                    log::info!("feedback: {addr} unsubscribed");
                }
            }
            Command::CueList(addr) => self.send_cue_list(addr),
            Command::Load(path) => self.start_load(path),
            Command::Reload => match self.show_path.clone() {
                Some(p) => self.start_load(p),
                None => {
                    log::warn!("reload: no show file loaded");
                    self.subs
                        .broadcast(feedback::load_status("error", "", "no show file loaded"));
                }
            },
            Command::Quit => {}
        }
    }

    fn find(&self, what: &str, number: &str) -> Option<usize> {
        let i = self.show.find(number);
        if i.is_none() {
            log::warn!("{what}: no cue {number}");
        }
        i
    }

    fn schedule(&mut self, after: f32, action: Action) {
        self.schedule_in(Duration::from_secs_f32(after.max(0.0)), action);
    }

    fn schedule_in(&mut self, after: Duration, action: Action) {
        self.seq += 1;
        let at = Instant::now() + after;
        self.timers.push(Reverse(Timer {
            at,
            seq: self.seq,
            action,
        }));
    }

    fn run_timers(&mut self) {
        let now = Instant::now();
        while let Some(Reverse(t)) = self.timers.peek() {
            if t.at > now {
                break;
            }
            let Reverse(t) = self.timers.pop().expect("peeked");
            self.dirty = true;
            match t.action {
                Action::Fire(i) => self.execute(i),
                Action::Continue(i) => self.trigger(i + 1),
                Action::Done(i) => self.cue_done(i),
            }
        }
    }

    fn poll_audio(&mut self) {
        while let Ok(ev) = self.audio.events.pop() {
            let AudioEvent::Finished { id, natural, clip } = ev;
            self.dirty = true;
            drop(clip); // dealloc (if last ref) happens here, off the RT thread
            let Some(idx) = self.voice_cue.remove(&id) else {
                continue;
            };
            if let Some(list) = self.cue_voices.get_mut(&idx) {
                list.retain(|v| *v != id);
                if list.is_empty() {
                    self.cue_voices.remove(&idx);
                    log::info!("  end  {}", self.label(idx));
                    if natural {
                        self.cue_done(idx);
                    }
                }
            }
        }
    }

    fn go(&mut self) {
        let idx = self.playhead;
        if idx >= self.show.cues.len() {
            log::info!("end of cue list");
            return;
        }
        self.trigger(idx);
        self.print_status();
    }

    /// Entry point for GO and chained triggers: honours pre-wait and the
    /// cue's second-trigger policy.
    fn trigger(&mut self, idx: usize) {
        let Some(cue) = self.show.cues.get(idx) else {
            return;
        };
        self.playhead = self.playhead.max(idx + 1);
        let active = self
            .states
            .get(&idx)
            .is_some_and(|s| s.phase != Phase::Stopping);
        if active {
            match cue.second_trigger {
                SecondTrigger::Ignore => {
                    log::info!("     {} already running — ignored", self.label(idx));
                    return;
                }
                SecondTrigger::Stop => {
                    log::info!("     {} second trigger → stop", self.label(idx));
                    return self.stop_cue(idx, None);
                }
                SecondTrigger::HardStop => {
                    log::info!("     {} second trigger → hard stop", self.label(idx));
                    return self.hard_stop_cue(idx);
                }
                SecondTrigger::Restart => {
                    log::info!("     {} second trigger → restart", self.label(idx));
                    self.hard_stop_cue(idx);
                }
            }
        }
        let cue = &self.show.cues[idx];
        if cue.pre_wait > 0.0 {
            let pw = cue.pre_wait;
            self.states.insert(idx, RunState::new(Phase::PreWait, pw));
            self.schedule(pw, Action::Fire(idx));
        } else {
            self.execute(idx);
        }
    }

    fn execute(&mut self, idx: usize) {
        let cue = self.show.cues[idx].clone();
        log::info!("GO   {}", self.label(idx));
        let finished_now = match &cue.kind {
            CueKind::Audio(a) => !self.play_audio(idx, a),
            CueKind::Fade {
                target,
                duration,
                gain_db,
                stop,
                curve: c,
            } => {
                match self.show.find(target) {
                    Some(t) => {
                        let lvl = gain_db.map(db_to_lin).unwrap_or(0.0);
                        let frames = self.audio.secs_to_frames(*duration);
                        for &id in self.cue_voices.get(&t).cloned().unwrap_or_default().iter() {
                            self.audio.send(AudioCmd::Fade {
                                id,
                                target: lvl,
                                frames,
                                curve: curve(*c),
                                stop: *stop,
                            });
                        }
                    }
                    None => log::warn!("fade: no cue {target}"),
                }
                self.schedule(*duration, Action::Done(idx));
                false
            }
            CueKind::Stop { target, fade } => {
                match target {
                    Some(t) => match self.show.find(t) {
                        Some(i) => self.stop_cue(i, Some(*fade)),
                        None => log::warn!("stop: no cue {t}"),
                    },
                    None => self.stop_all(Some(*fade)),
                }
                true
            }
            CueKind::Osc { host, path, args } => {
                match osc::encode(path, args)
                    .and_then(|b| Ok(self.osc_out.send_to(&b, host.as_str())?))
                {
                    Ok(_) => log::debug!("OSC -> {host} {path}"),
                    Err(e) => log::error!("OSC cue {}: {e:#}", cue.number),
                }
                true
            }
            CueKind::Midi { port, bytes } => {
                if let Err(e) = self.midi.send(port, bytes) {
                    log::error!("MIDI cue {}: {e:#}", cue.number);
                }
                true
            }
            CueKind::Wait { duration } => {
                self.schedule(*duration, Action::Done(idx));
                false
            }
        };
        if finished_now {
            self.states.remove(&idx);
        } else {
            self.states
                .insert(idx, RunState::new(Phase::Running, self.durations[idx]));
        }
        if cue.next == NextAction::AutoContinue {
            self.schedule(cue.post_wait, Action::Continue(idx));
        }
        if finished_now {
            self.cue_done(idx);
        }
    }

    /// Returns true if a voice was started.
    fn play_audio(&mut self, idx: usize, a: &AudioCue) -> bool {
        let Some(clip) = self.clips.get(&a.file).map(|c| c.clip.clone()) else {
            log::error!(
                "cue {}: clip not loaded ({})",
                self.show.cues[idx].number,
                a.file.display()
            );
            return false;
        };
        let sr = self.audio.sample_rate as f32;
        let id = self.next_voice;
        self.next_voice += 1;
        self.audio.send(AudioCmd::Play {
            id,
            clip,
            gain: db_to_lin(a.gain_db),
            fade_in: self.audio.secs_to_frames(a.fade_in),
            curve: curve(a.curve),
            start: (a.start.max(0.0) * sr) as usize,
            end: a.end.map(|e| (e * sr) as usize).unwrap_or(usize::MAX),
            looping: a.looping,
        });
        self.voice_cue.insert(id, idx);
        self.cue_voices.entry(idx).or_default().push(id);
        true
    }

    fn cue_done(&mut self, idx: usize) {
        if self.show.cues[idx].next == NextAction::AutoFollow {
            self.trigger(idx + 1);
        }
    }

    /// fade=None → use the audio cue's own fade_out (and curve).
    fn stop_cue(&mut self, idx: usize, fade: Option<f32>) {
        let (own_fade, c) = match &self.show.cues[idx].kind {
            CueKind::Audio(a) => (a.fade_out, curve(a.curve)),
            _ => (0.0, Curve::SCurve),
        };
        self.stop_cue_with(idx, fade.unwrap_or(own_fade), c);
    }

    fn hard_stop_cue(&mut self, idx: usize) {
        self.stop_cue_with(idx, HARD_STOP_FADE, Curve::Linear);
    }

    fn stop_cue_with(&mut self, idx: usize, fade: f32, c: Curve) {
        self.timers.retain(|Reverse(t)| t.action.cue() != idx);
        self.paused_timers.retain(|t| t.action.cue() != idx);
        let frames = self.audio.secs_to_frames(fade);
        if let Some(s) = self.states.get_mut(&idx) {
            s.phase = Phase::Stopping;
        }
        for id in self.cue_voices.get(&idx).cloned().unwrap_or_default() {
            self.audio.send(AudioCmd::Fade {
                id,
                target: 0.0,
                frames,
                curve: c,
                stop: true,
            });
        }
    }

    fn stop_all(&mut self, fade: Option<f32>) {
        self.timers.clear();
        self.paused_timers.clear();
        let running: Vec<usize> = self.cue_voices.keys().copied().collect();
        for idx in running {
            self.stop_cue(idx, fade);
        }
    }

    fn hard_stop_all(&mut self) {
        self.timers.clear();
        self.paused_timers.clear();
        for s in self.states.values_mut() {
            s.phase = Phase::Stopping;
        }
        self.audio.send(AudioCmd::FadeAll {
            frames: self.audio.secs_to_frames(HARD_STOP_FADE),
            curve: Curve::Linear,
        });
        log::warn!("HARD STOP all");
    }

    /// First panic: fade everything over `panic_fade`. A second panic while
    /// that fade is running: hard stop.
    fn panic(&mut self) {
        let now = Instant::now();
        if self.panic_until.is_some_and(|t| now < t) {
            self.panic_until = None;
            log::warn!("PANIC ×2");
            return self.hard_stop_all();
        }
        self.timers.clear();
        self.paused_timers.clear();
        for s in self.states.values_mut() {
            s.phase = Phase::Stopping;
        }
        let fade = self.show.settings.panic_fade;
        self.audio.send(AudioCmd::FadeAll {
            frames: self.audio.secs_to_frames(fade),
            curve: Curve::SCurve,
        });
        self.panic_until = Some(now + Duration::from_secs_f32(fade.max(0.0)));
        log::warn!("PANIC (again within {fade}s = hard stop)");
    }

    /// Cues with something in flight: run state, voices or pending timers.
    fn active_cues(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .states
            .keys()
            .chain(self.cue_voices.keys())
            .copied()
            .chain(self.timers.iter().map(|Reverse(t)| t.action.cue()))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Freeze a cue: its voices (declicked), and its pre-wait / duration /
    /// post-wait timers with their remaining time.
    fn pause_cue(&mut self, idx: usize) {
        let now = Instant::now();
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.timers)
            .into_vec()
            .into_iter()
            .partition(|Reverse(t)| t.action.cue() == idx);
        self.timers = rest.into();
        for Reverse(t) in mine {
            self.paused_timers.push(PausedTimer {
                remaining: t.at.saturating_duration_since(now),
                action: t.action,
            });
        }
        for id in self.cue_voices.get(&idx).cloned().unwrap_or_default() {
            self.audio.send(AudioCmd::Pause { id, paused: true });
        }
        if let Some(s) = self.states.get_mut(&idx) {
            s.paused_at.get_or_insert(now);
        }
        log::info!("PAUSE {}", self.label(idx));
    }

    fn resume_cue(&mut self, idx: usize) {
        let now = Instant::now();
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.paused_timers)
            .into_iter()
            .partition(|t| t.action.cue() == idx);
        self.paused_timers = rest;
        for t in mine {
            self.schedule_in(t.remaining, t.action);
        }
        for id in self.cue_voices.get(&idx).cloned().unwrap_or_default() {
            self.audio.send(AudioCmd::Pause { id, paused: false });
        }
        if let Some(s) = self.states.get_mut(&idx) {
            if let Some(p) = s.paused_at.take() {
                s.started += now.saturating_duration_since(p);
            }
        }
        log::info!("RESUME {}", self.label(idx));
    }

    /// Drop run states with nothing left behind them (no voices, no pending
    /// pre-wait / duration timer).
    fn reconcile_states(&mut self) {
        let pending: HashSet<usize> = self
            .timers
            .iter()
            .map(|Reverse(t)| t.action)
            .chain(self.paused_timers.iter().map(|t| t.action))
            .filter_map(|a| match a {
                Action::Fire(i) | Action::Done(i) => Some(i),
                Action::Continue(_) => None,
            })
            .collect();
        let voices = &self.cue_voices;
        self.states
            .retain(|i, _| voices.contains_key(i) || pending.contains(i));
    }

    fn cue_list(&self) -> Vec<rosc::OscMessage> {
        let path = self
            .show_path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        feedback::cue_list(&self.show, &self.durations, &self.problems, &path)
    }

    fn send_cue_list(&self, addr: std::net::SocketAddr) {
        for m in self.cue_list() {
            self.subs.send_to(addr, m);
        }
    }

    // ── Show loading ──────────────────────────────────────

    fn start_load(&mut self, path: PathBuf) {
        if self.loading {
            self.queued_load = Some(path);
            return;
        }
        log::info!("loading {}", path.display());
        self.loading = true;
        self.subs.broadcast(feedback::load_status(
            "loading",
            &path.display().to_string(),
            "",
        ));
        loader::spawn(
            path,
            self.audio.sample_rate,
            self.clips.clone(),
            self.load_tx.clone(),
        );
    }

    fn poll_loads(&mut self) {
        while let Ok(res) = self.load_rx.try_recv() {
            self.loading = false;
            match res {
                Ok(l) => self.apply_show(l),
                Err((path, e)) => {
                    log::error!("load {}: {e} (keeping current show)", path.display());
                    self.subs.broadcast(feedback::load_status(
                        "error",
                        &path.display().to_string(),
                        &e,
                    ));
                }
            }
            self.dirty = true;
            if let Some(p) = self.queued_load.take() {
                self.start_load(p);
            }
        }
    }

    fn watch_file(&mut self) {
        if !self.watch || self.loading || self.last_watch.elapsed() < WATCH_EVERY {
            return;
        }
        self.last_watch = Instant::now();
        let Some(path) = self.show_path.clone() else {
            return;
        };
        let m = loader::mtime(&path);
        if m.is_some() && m != self.show_mtime {
            // Record it now so a file that fails to parse isn't retried until it changes again.
            self.show_mtime = m;
            log::info!("show file changed");
            self.start_load(path);
        }
    }

    /// Swap in a freshly loaded show. Reloading the same file keeps running
    /// cues (matched by cue number); cues that disappeared, or everything when
    /// a different file is loaded, fade out with their own `fade_out`.
    fn apply_show(&mut self, l: Loaded) {
        let same_file = self.show_path.as_deref() == Some(l.path.as_path());
        let remap: Vec<Option<usize>> = if same_file {
            loader::remap(&self.show, &l.show)
        } else {
            vec![None; self.show.cues.len()]
        };
        let m = |i: usize| remap.get(i).copied().flatten();

        // Voices: re-point or fade out.
        let mut orphans = 0;
        for (voice, idx) in std::mem::take(&mut self.voice_cue) {
            match m(idx) {
                Some(n) => {
                    self.voice_cue.insert(voice, n);
                }
                None => {
                    let (fade, c) = match &self.show.cues[idx].kind {
                        CueKind::Audio(a) => (a.fade_out, curve(a.curve)),
                        _ => (0.0, Curve::SCurve),
                    };
                    let frames = self.audio.secs_to_frames(fade);
                    self.audio.send(AudioCmd::Fade {
                        id: voice,
                        target: 0.0,
                        frames,
                        curve: c,
                        stop: true,
                    });
                    orphans += 1;
                }
            }
        }
        self.cue_voices.clear();
        for (&v, &i) in &self.voice_cue {
            self.cue_voices.entry(i).or_default().push(v);
        }
        self.states = std::mem::take(&mut self.states)
            .into_iter()
            .filter_map(|(i, s)| m(i).map(|n| (n, s)))
            .collect();
        self.timers = std::mem::take(&mut self.timers)
            .into_vec()
            .into_iter()
            .filter_map(|Reverse(mut t)| {
                t.action = t.action.map(m)?;
                Some(Reverse(t))
            })
            .collect();
        self.paused_timers = std::mem::take(&mut self.paused_timers)
            .into_iter()
            .filter_map(|mut t| {
                t.action = t.action.map(m)?;
                Some(t)
            })
            .collect();

        self.playhead = if same_file {
            match self
                .show
                .cues
                .get(self.playhead)
                .and_then(|c| l.show.find(&c.number))
            {
                Some(i) => i,
                None => self.playhead.min(l.show.cues.len()),
            }
        } else {
            0
        };

        if self.show_path.is_some() {
            let (a, b) = (&self.show.settings, &l.show.settings);
            if a.osc_listen != b.osc_listen || a.audio_device != b.audio_device {
                log::warn!("osc_listen / audio_device changed: restart the engine to apply");
            }
        }
        for w in &l.warnings {
            log::warn!("show: {w}");
        }

        let sr = self.audio.sample_rate as f32;
        self.durations = feedback::durations(&l.show, |p| {
            l.clips.get(p).map(|c| (c.clip.len() / 2) as f32 / sr)
        });
        let mb: usize = l.clips.values().map(|c| c.clip.len() * 4).sum::<usize>() >> 20;
        let summary = format!(
            "{} cues · {} clips ({mb} MiB, {} decoded, {} cached) in {:.0?}",
            l.show.cues.len(),
            l.clips.len(),
            l.decoded,
            l.reused,
            l.elapsed
        );
        log::info!(
            "{} '{}' — {summary}{}",
            if same_file { "reloaded" } else { "loaded" },
            l.show.name,
            if orphans > 0 {
                format!(", faded out {orphans} voice(s)")
            } else {
                String::new()
            }
        );
        let message = match l.warnings.as_slice() {
            [] => summary,
            [w] => format!("{summary} · warning: {w}"),
            [w, rest @ ..] => format!("{summary} · {} warnings, first: {w}", rest.len() + 1),
        };

        let broken = l.problems.iter().flatten().count();
        if broken > 0 {
            log::warn!("{broken} cue(s) have problems");
        }
        self.show = l.show;
        self.problems = l.problems;
        self.clips = l.clips;
        self.show_path = Some(l.path);
        self.show_mtime = l.mtime;
        self.dirty = true;

        let path = self.show_path.as_deref().unwrap().display().to_string();
        self.subs.broadcast_many(self.cue_list());
        self.subs
            .broadcast(feedback::load_status("ok", &path, &message));
        self.print_status();
    }

    fn push_feedback(&mut self) {
        if self.dirty {
            self.reconcile_states();
        }
        self.subs.expire();
        if self.subs.is_empty() {
            self.dirty = false;
            return;
        }
        let every = if self.states.is_empty() {
            PUSH_IDLE
        } else {
            PUSH_ACTIVE
        };
        if !self.dirty && self.last_push.elapsed() < every {
            return;
        }
        let mut running: Vec<RunInfo> = self
            .states
            .iter()
            .map(|(&idx, s)| RunInfo {
                idx,
                phase: s.phase_str(),
                elapsed: s.elapsed(),
                duration: s.duration,
            })
            .collect();
        running.sort_by_key(|r| r.idx);
        let standby = (self.playhead < self.show.cues.len()).then_some(self.playhead);
        self.subs.broadcast(feedback::state(standby, &running));
        self.last_push = Instant::now();
        self.dirty = false;
    }

    fn label(&self, idx: usize) -> String {
        let c = &self.show.cues[idx];
        format!("[{}] {}", c.number, c.name)
    }

    fn print_status(&self) {
        let next = self
            .show
            .cues
            .get(self.playhead)
            .map(|_| self.label(self.playhead))
            .unwrap_or_else(|| "— end —".into());
        let mut running: Vec<String> = self.cue_voices.keys().map(|&i| self.label(i)).collect();
        running.sort();
        log::info!(
            "standby: {next}   running: {}",
            if running.is_empty() {
                "-".into()
            } else {
                running.join(", ")
            }
        );
    }
}
