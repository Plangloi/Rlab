//! RT-safe mixer. Runs inside the audio callback: no locks, no allocation
//! (voice Vec is preallocated; finished clips are shipped back to the control
//! thread so the last Arc drop never happens here).

use rtrb::{Consumer, Producer};
use std::sync::Arc;

pub type VoiceId = u64;
/// Stereo-interleaved f32 at engine sample rate.
pub type Clip = Arc<[f32]>;

pub const MAX_VOICES: usize = 128;
/// Pause / resume declick ramp length (frames).
const DECLICK: f32 = 256.0;

/// Gain trajectory of a fade, from the voice's current gain to `target`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Curve {
    Linear,
    /// Smoothstep: gentle start and end. Default.
    #[default]
    SCurve,
    /// Linear in power (sqrt in amplitude): constant loudness through crossfades.
    EqualPower,
}

impl Curve {
    /// Gain at `t` ∈ [0, 1] going from `from` to `to`. Never overshoots.
    #[inline]
    pub fn at(self, from: f32, to: f32, t: f32) -> f32 {
        match self {
            Curve::Linear => from + (to - from) * t,
            Curve::SCurve => from + (to - from) * (t * t * (3.0 - 2.0 * t)),
            Curve::EqualPower => (from * from * (1.0 - t) + to * to * t).sqrt(),
        }
    }
}

pub enum AudioCmd {
    Play {
        id: VoiceId,
        clip: Clip,
        gain: f32,
        fade_in: u32,
        curve: Curve,
        start: usize,
        end: usize,
        looping: bool,
    },
    /// A fade on a paused voice with `stop` ends it immediately (it is silent).
    Fade {
        id: VoiceId,
        target: f32,
        frames: u32,
        curve: Curve,
        stop: bool,
    },
    Stop {
        id: VoiceId,
    },
    /// Pause (true) or resume (false), with a short declick ramp.
    Pause {
        id: VoiceId,
        paused: bool,
    },
    /// Fade out and stop every voice; paused voices end immediately.
    FadeAll {
        frames: u32,
        curve: Curve,
    },
}

pub enum AudioEvent {
    /// `natural` = reached end of region (vs. stopped / faded out / dropped).
    Finished {
        id: VoiceId,
        natural: bool,
        clip: Clip,
    },
}

struct Voice {
    id: VoiceId,
    clip: Clip,
    pos: usize,
    start: usize,
    end: usize,
    looping: bool,
    gain: f32,
    ramp: Option<Ramp>,
    /// Pause declick multiplier, moves toward 0 (paused) or 1.
    pause_gain: f32,
    paused: bool,
    done: bool,
    natural: bool,
}

#[derive(Clone, Copy)]
struct Ramp {
    from: f32,
    to: f32,
    len: u32,
    pos: u32,
    curve: Curve,
    stop: bool,
}

impl Voice {
    fn ramp(&mut self, target: f32, frames: u32, curve: Curve, stop: bool) {
        if stop && self.is_frozen() {
            self.done = true;
            return;
        }
        if frames == 0 {
            self.gain = target;
            self.ramp = None;
            if stop {
                self.done = true;
            }
        } else {
            self.ramp = Some(Ramp {
                from: self.gain,
                to: target,
                len: frames,
                pos: 0,
                curve,
                stop,
            });
        }
    }

    /// Paused (or pausing): silent, so stopping it needs no fade.
    fn is_frozen(&self) -> bool {
        self.paused
    }
}

pub struct Mixer {
    voices: Vec<Voice>,
    cmds: Consumer<AudioCmd>,
    events: Producer<AudioEvent>,
}

impl Mixer {
    pub fn new(cmds: Consumer<AudioCmd>, events: Producer<AudioEvent>) -> Self {
        Self {
            voices: Vec::with_capacity(MAX_VOICES),
            cmds,
            events,
        }
    }

    fn emit(&mut self, ev: AudioEvent) {
        // Ring full: drop here (would only dealloc if cache no longer holds clip).
        let _ = self.events.push(ev);
    }

    fn apply(&mut self, cmd: AudioCmd) {
        match cmd {
            AudioCmd::Play {
                id,
                clip,
                gain,
                fade_in,
                curve,
                start,
                end,
                looping,
            } => {
                let frames = clip.len() / 2;
                let end = end.min(frames);
                if self.voices.len() >= MAX_VOICES || start >= end {
                    self.emit(AudioEvent::Finished {
                        id,
                        natural: start >= end,
                        clip,
                    });
                    return;
                }
                let mut v = Voice {
                    id,
                    clip,
                    pos: start,
                    start,
                    end,
                    looping,
                    gain: if fade_in > 0 { 0.0 } else { gain },
                    ramp: None,
                    pause_gain: 1.0,
                    paused: false,
                    done: false,
                    natural: false,
                };
                if fade_in > 0 {
                    v.ramp(gain, fade_in, curve, false);
                }
                self.voices.push(v);
            }
            AudioCmd::Fade {
                id,
                target,
                frames,
                curve,
                stop,
            } => {
                if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
                    v.ramp(target, frames, curve, stop);
                }
            }
            AudioCmd::Pause { id, paused } => {
                if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
                    v.paused = paused;
                }
            }
            AudioCmd::Stop { id } => {
                if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
                    v.done = true;
                }
            }
            AudioCmd::FadeAll { frames, curve } => {
                for v in &mut self.voices {
                    v.ramp(0.0, frames, curve, true);
                }
            }
        }
    }

    /// Fill `out` (interleaved, `channels` wide). Channels beyond 2 are left silent.
    pub fn process(&mut self, out: &mut [f32], channels: usize) {
        out.fill(0.0);
        while let Ok(cmd) = self.cmds.pop() {
            self.apply(cmd);
        }
        let channels = channels.max(1);
        let frames = out.len() / channels;

        for v in &mut self.voices {
            for f in 0..frames {
                if v.done {
                    break;
                }
                // Pause declick; once fully paused, the voice is frozen
                // (position and fade ramp both hold).
                if v.paused {
                    v.pause_gain = (v.pause_gain - 1.0 / DECLICK).max(0.0);
                    if v.pause_gain == 0.0 {
                        break;
                    }
                } else if v.pause_gain < 1.0 {
                    v.pause_gain = (v.pause_gain + 1.0 / DECLICK).min(1.0);
                }
                if let Some(r) = &mut v.ramp {
                    r.pos += 1;
                    let t = r.pos as f32 / r.len as f32;
                    v.gain = r.curve.at(r.from, r.to, t.min(1.0));
                    if r.pos >= r.len {
                        v.gain = r.to;
                        if r.stop {
                            v.done = true;
                        }
                        v.ramp = None;
                    }
                }
                let g = v.gain * v.pause_gain;
                let i = v.pos * 2;
                let (l, r) = (v.clip[i] * g, v.clip[i + 1] * g);
                let o = &mut out[f * channels..(f + 1) * channels];
                if channels == 1 {
                    o[0] += 0.5 * (l + r);
                } else {
                    o[0] += l;
                    o[1] += r;
                }
                v.pos += 1;
                if v.pos >= v.end {
                    if v.looping {
                        v.pos = v.start;
                    } else {
                        v.done = true;
                        v.natural = true;
                    }
                }
            }
        }

        let mut i = 0;
        while i < self.voices.len() {
            if self.voices[i].done {
                let v = self.voices.swap_remove(i);
                self.emit(AudioEvent::Finished {
                    id: v.id,
                    natural: v.natural,
                    clip: v.clip,
                });
            } else {
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    fn rig() -> (Mixer, Producer<AudioCmd>, Consumer<AudioEvent>) {
        let (ctx, crx) = RingBuffer::new(64);
        let (etx, erx) = RingBuffer::new(64);
        (Mixer::new(crx, etx), ctx, erx)
    }

    fn ones(frames: usize) -> Clip {
        vec![1.0f32; frames * 2].into()
    }

    #[test]
    fn gain_and_natural_end() {
        let (mut m, mut tx, mut ev) = rig();
        tx.push(AudioCmd::Play {
            id: 1,
            clip: ones(4),
            gain: 0.5,
            fade_in: 0,
            curve: Curve::Linear,
            start: 0,
            end: usize::MAX,
            looping: false,
        })
        .ok();
        let mut out = vec![0.0; 16]; // 8 frames stereo
        m.process(&mut out, 2);
        assert_eq!(&out[..8], &[0.5; 8]);
        assert_eq!(&out[8..], &[0.0; 8]);
        match ev.pop().unwrap() {
            AudioEvent::Finished { id, natural, .. } => assert!(id == 1 && natural),
        }
    }

    #[test]
    fn fade_out_stops_unnaturally() {
        let (mut m, mut tx, mut ev) = rig();
        tx.push(AudioCmd::Play {
            id: 7,
            clip: ones(1000),
            gain: 1.0,
            fade_in: 0,
            curve: Curve::Linear,
            start: 0,
            end: usize::MAX,
            looping: true,
        })
        .ok();
        let mut out = vec![0.0; 20];
        m.process(&mut out, 2);
        tx.push(AudioCmd::Fade {
            id: 7,
            target: 0.0,
            frames: 4,
            curve: Curve::Linear,
            stop: true,
        })
        .ok();
        m.process(&mut out, 2);
        assert!((out[0] - 0.75).abs() < 1e-6 && out[6] == 0.0 && out[8] == 0.0);
        match ev.pop().unwrap() {
            AudioEvent::Finished { id, natural, .. } => assert!(id == 7 && !natural),
        }
    }

    #[test]
    fn mono_downmix_and_loop() {
        let (mut m, mut tx, _ev) = rig();
        let clip: Clip = vec![1.0, 0.0, 0.0, 1.0].into(); // 2 frames
        tx.push(AudioCmd::Play {
            id: 1,
            clip,
            gain: 1.0,
            fade_in: 0,
            curve: Curve::Linear,
            start: 0,
            end: 2,
            looping: true,
        })
        .ok();
        let mut out = vec![0.0; 5];
        m.process(&mut out, 1);
        assert_eq!(out, vec![0.5; 5]);
    }

    fn play(tx: &mut Producer<AudioCmd>, id: VoiceId, frames: usize) {
        tx.push(AudioCmd::Play {
            id,
            clip: ones(frames),
            gain: 1.0,
            fade_in: 0,
            curve: Curve::Linear,
            start: 0,
            end: usize::MAX,
            looping: true,
        })
        .ok();
    }

    #[test]
    fn curves_hit_endpoints_without_overshoot() {
        for c in [Curve::Linear, Curve::SCurve, Curve::EqualPower] {
            assert!((c.at(1.0, 0.0, 0.0) - 1.0).abs() < 1e-6, "{c:?}");
            assert!(c.at(1.0, 0.0, 1.0).abs() < 1e-6, "{c:?}");
            assert!((c.at(0.2, 0.8, 1.0) - 0.8).abs() < 1e-6, "{c:?}");
            for i in 0..=100 {
                let g = c.at(0.5, 1.0, i as f32 / 100.0);
                assert!((0.5..=1.0 + 1e-6).contains(&g), "{c:?} {g}");
            }
        }
        // equal power keeps more level mid-fade than linear
        assert!(Curve::EqualPower.at(1.0, 0.0, 0.5) > 0.7);
        assert!(Curve::SCurve.at(1.0, 0.0, 0.25) > Curve::Linear.at(1.0, 0.0, 0.25));
    }

    #[test]
    fn pause_freezes_position_and_resume_continues() {
        let (mut m, mut tx, _ev) = rig();
        // ramp 0..N so output reveals position
        let clip: Clip = (0..2000).flat_map(|i| [i as f32, i as f32]).collect::<Vec<_>>().into();
        tx.push(AudioCmd::Play {
            id: 1,
            clip,
            gain: 1.0,
            fade_in: 0,
            curve: Curve::Linear,
            start: 0,
            end: usize::MAX,
            looping: false,
        })
        .ok();
        let mut out = vec![0.0; 20]; // 10 frames
        m.process(&mut out, 2);
        tx.push(AudioCmd::Pause { id: 1, paused: true }).ok();
        let mut big = vec![0.0; 2 * 600];
        m.process(&mut big, 2); // declick (256) then silent
        assert_eq!(big[2 * 300], 0.0);
        let frozen = m.voices[0].pos;
        m.process(&mut big, 2);
        assert_eq!(m.voices[0].pos, frozen);
        assert!(big.iter().all(|x| *x == 0.0));
        tx.push(AudioCmd::Pause { id: 1, paused: false }).ok();
        m.process(&mut out, 2);
        assert_eq!(m.voices[0].pos, frozen + 10);
        assert!(out[2] > 0.0);
    }

    #[test]
    fn stopping_paused_voice_ends_immediately() {
        let (mut m, mut tx, mut ev) = rig();
        play(&mut tx, 1, 1000);
        play(&mut tx, 2, 1000);
        let mut big = vec![0.0; 2 * 300];
        m.process(&mut big, 2);
        tx.push(AudioCmd::Pause { id: 1, paused: true }).ok();
        m.process(&mut big, 2);
        tx.push(AudioCmd::FadeAll {
            frames: 48_000,
            curve: Curve::SCurve,
        })
        .ok();
        m.process(&mut big, 2);
        // paused voice 1 ends at once; voice 2 is still fading
        match ev.pop().unwrap() {
            AudioEvent::Finished { id, .. } => assert_eq!(id, 1),
        }
        assert!(ev.pop().is_err());
        assert_eq!(m.voices.len(), 1);
    }
}
