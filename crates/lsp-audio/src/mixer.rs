//! RT-safe mixer. Runs inside the audio callback: no locks, no allocation
//! (voice Vec is preallocated; finished clips are shipped back to the control
//! thread so the last Arc drop never happens here).

use rtrb::{Consumer, Producer};
use std::sync::Arc;

pub type VoiceId = u64;
/// Stereo-interleaved f32 at engine sample rate.
pub type Clip = Arc<[f32]>;

pub const MAX_VOICES: usize = 128;

pub enum AudioCmd {
    Play {
        id: VoiceId,
        clip: Clip,
        gain: f32,
        fade_in: u32,
        start: usize,
        end: usize,
        looping: bool,
    },
    Fade {
        id: VoiceId,
        target: f32,
        frames: u32,
        stop: bool,
    },
    Stop {
        id: VoiceId,
    },
    FadeAll {
        frames: u32,
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
    target: f32,
    step: f32,
    remaining: u32,
    stop_at_target: bool,
    done: bool,
    natural: bool,
}

impl Voice {
    fn ramp(&mut self, target: f32, frames: u32, stop: bool) {
        if frames == 0 {
            self.gain = target;
            self.remaining = 0;
            if stop {
                self.done = true;
            }
        } else {
            self.target = target;
            self.step = (target - self.gain) / frames as f32;
            self.remaining = frames;
            self.stop_at_target = stop;
        }
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
                    target: gain,
                    step: 0.0,
                    remaining: 0,
                    stop_at_target: false,
                    done: false,
                    natural: false,
                };
                if fade_in > 0 {
                    v.ramp(gain, fade_in, false);
                }
                self.voices.push(v);
            }
            AudioCmd::Fade {
                id,
                target,
                frames,
                stop,
            } => {
                if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
                    v.ramp(target, frames, stop);
                }
            }
            AudioCmd::Stop { id } => {
                if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
                    v.done = true;
                }
            }
            AudioCmd::FadeAll { frames } => {
                for v in &mut self.voices {
                    v.ramp(0.0, frames, true);
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
                if v.remaining > 0 {
                    v.gain += v.step;
                    v.remaining -= 1;
                    if v.remaining == 0 {
                        v.gain = v.target;
                        if v.stop_at_target {
                            v.done = true;
                        }
                    }
                }
                let i = v.pos * 2;
                let (l, r) = (v.clip[i] * v.gain, v.clip[i + 1] * v.gain);
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
            start: 0,
            end: 2,
            looping: true,
        })
        .ok();
        let mut out = vec![0.0; 5];
        m.process(&mut out, 1);
        assert_eq!(out, vec![0.5; 5]);
    }
}
