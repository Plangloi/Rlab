//! Cue sequencer. Single-threaded event loop: control commands in, audio
//! events in, timer heap for pre-wait / post-wait / durations.

use anyhow::Result;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use lsp_audio::{load_clip, AudioCmd, AudioEvent, AudioHandle, Clip, VoiceId};
use lsp_core::{db_to_lin, AudioCue, Command, CueKind, NextAction, Show};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Timer {
    at: Instant,
    seq: u64,
    action: Action,
}

pub struct Controller {
    show: Show,
    playhead: usize,
    audio: AudioHandle,
    clips: HashMap<PathBuf, Clip>,
    voice_cue: HashMap<VoiceId, usize>,
    cue_voices: HashMap<usize, Vec<VoiceId>>,
    next_voice: VoiceId,
    timers: BinaryHeap<Reverse<Timer>>,
    seq: u64,
    osc_out: UdpSocket,
    midi: MidiOut,
}

const TICK: Duration = Duration::from_millis(5);

impl Controller {
    pub fn new(show: Show, base_dir: &Path, audio: AudioHandle) -> Result<Self> {
        for w in show.validate() {
            log::warn!("show: {w}");
        }
        let mut clips = HashMap::new();
        let t0 = Instant::now();
        let mut show = show;
        for cue in &mut show.cues {
            if let CueKind::Audio(a) = &mut cue.kind {
                if a.file.is_relative() {
                    a.file = base_dir.join(&a.file);
                }
                if clips.contains_key(&a.file) {
                    continue;
                }
                match load_clip(&a.file, audio.sample_rate) {
                    Ok(c) => {
                        clips.insert(a.file.clone(), c);
                    }
                    Err(e) => log::error!("cue {}: {e:#}", cue.number),
                }
            }
        }
        let mb: usize = clips.values().map(|c| c.len() * 4).sum::<usize>() >> 20;
        log::info!(
            "preloaded {} clips ({mb} MiB) in {:?}",
            clips.len(),
            t0.elapsed()
        );

        Ok(Self {
            show,
            playhead: 0,
            audio,
            clips,
            voice_cue: HashMap::new(),
            cue_voices: HashMap::new(),
            next_voice: 1,
            timers: BinaryHeap::new(),
            seq: 0,
            osc_out: UdpSocket::bind("0.0.0.0:0")?,
            midi: MidiOut::default(),
        })
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
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.poll_audio();
            self.run_timers();
        }
        self.audio.send(AudioCmd::FadeAll { frames: 0 });
        std::thread::sleep(Duration::from_millis(50));
        log::info!("bye");
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Go => self.go(),
            Command::Stop(None) => self.stop_all(None),
            Command::Stop(Some(n)) => match self.show.find(&n) {
                Some(i) => self.stop_cue(i, None),
                None => log::warn!("stop: no cue {n}"),
            },
            Command::Panic => {
                self.timers.clear();
                let f = self.audio.secs_to_frames(self.show.settings.panic_fade);
                self.audio.send(AudioCmd::FadeAll { frames: f });
                log::warn!("PANIC");
            }
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
            Command::Quit => {}
        }
    }

    fn schedule(&mut self, after: f32, action: Action) {
        self.seq += 1;
        let at = Instant::now() + Duration::from_secs_f32(after.max(0.0));
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

    /// Entry point for GO and chained triggers: honours pre-wait.
    fn trigger(&mut self, idx: usize) {
        let Some(cue) = self.show.cues.get(idx) else {
            return;
        };
        self.playhead = self.playhead.max(idx + 1);
        if cue.pre_wait > 0.0 {
            let pw = cue.pre_wait;
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
        if cue.next == NextAction::AutoContinue {
            self.schedule(cue.post_wait, Action::Continue(idx));
        }
        if finished_now {
            self.cue_done(idx);
        }
    }

    /// Returns true if a voice was started.
    fn play_audio(&mut self, idx: usize, a: &AudioCue) -> bool {
        let Some(clip) = self.clips.get(&a.file).cloned() else {
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

    /// fade=None → use the audio cue's own fade_out.
    fn stop_cue(&mut self, idx: usize, fade: Option<f32>) {
        self.timers.retain(|Reverse(t)| !matches!(t.action, Action::Fire(i) | Action::Continue(i) | Action::Done(i) if i == idx));
        let fade = fade.unwrap_or(match &self.show.cues[idx].kind {
            CueKind::Audio(a) => a.fade_out,
            _ => 0.0,
        });
        let frames = self.audio.secs_to_frames(fade);
        for id in self.cue_voices.get(&idx).cloned().unwrap_or_default() {
            self.audio.send(AudioCmd::Fade {
                id,
                target: 0.0,
                frames,
                stop: true,
            });
        }
    }

    fn stop_all(&mut self, fade: Option<f32>) {
        self.timers.clear();
        let running: Vec<usize> = self.cue_voices.keys().copied().collect();
        for idx in running {
            self.stop_cue(idx, fade);
        }
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
