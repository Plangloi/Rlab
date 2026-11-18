//! Audio output. The cpal Stream is !Send on macOS, so it lives on a dedicated
//! thread; the control side talks to the mixer only through SPSC ring buffers.

pub mod decode;
pub mod mixer;

pub use decode::load_clip;
pub use mixer::{AudioCmd, AudioEvent, Clip, Curve, Mixer, VoiceId};

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Output metering, written by the audio callback and read (and reset) by the
/// control thread. Lock-free: plain atomics, no allocation in the callback.
#[derive(Default)]
pub struct Meters {
    /// Max |sample| per channel since the last `take`, as f32 bits. For
    /// non-negative floats the bit patterns order like the values, so
    /// `fetch_max` on the bits is a max on the floats.
    peak: [AtomicU32; 2],
    /// Samples at or over full scale since the last `take`.
    clips: AtomicU32,
    /// Worst callback time / buffer period since the last `take`, per mille.
    load: AtomicU32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MeterReading {
    /// Linear peak per channel (1.0 = 0 dBFS).
    pub peak: [f32; 2],
    pub clips: u32,
    /// 0..1+ fraction of the buffer period spent in the callback.
    pub load: f32,
}

impl Meters {
    /// Called from the audio callback with the mixed (pre-clamp) block.
    #[inline]
    pub fn record(&self, buf: &[f32], channels: usize, busy: Duration, period: Duration) {
        let ch = channels.max(1);
        let (mut l, mut r, mut clips) = (0f32, 0f32, 0u32);
        for frame in buf.chunks_exact(ch) {
            let a = frame[0].abs();
            let b = frame[if ch > 1 { 1 } else { 0 }].abs();
            l = l.max(a);
            r = r.max(b);
            clips += (a >= 1.0) as u32 + (ch > 1 && b >= 1.0) as u32;
        }
        self.peak[0].fetch_max(l.to_bits(), Ordering::Relaxed);
        self.peak[1].fetch_max(r.to_bits(), Ordering::Relaxed);
        if clips > 0 {
            self.clips.fetch_add(clips, Ordering::Relaxed);
        }
        if !period.is_zero() {
            let pm = (busy.as_secs_f32() / period.as_secs_f32() * 1000.0) as u32;
            self.load.fetch_max(pm, Ordering::Relaxed);
        }
    }

    /// Read and reset.
    pub fn take(&self) -> MeterReading {
        MeterReading {
            peak: [
                f32::from_bits(self.peak[0].swap(0, Ordering::Relaxed)),
                f32::from_bits(self.peak[1].swap(0, Ordering::Relaxed)),
            ],
            clips: self.clips.swap(0, Ordering::Relaxed),
            load: self.load.swap(0, Ordering::Relaxed) as f32 / 1000.0,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct AudioConfig {
    /// cpal host name (e.g. "ALSA", "JACK", "CoreAudio"). None = default.
    pub host: Option<String>,
    /// Substring of output device name. None = default device.
    pub device: Option<String>,
}

pub struct AudioHandle {
    pub cmd: Producer<AudioCmd>,
    pub events: Consumer<AudioEvent>,
    pub sample_rate: u32,
    pub channels: u16,
    pub device_name: String,
    pub meters: Arc<Meters>,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

struct Ready {
    sample_rate: u32,
    channels: u16,
    device_name: String,
}

fn pick_host(name: Option<&str>) -> Result<cpal::Host> {
    let Some(name) = name else {
        return Ok(cpal::default_host());
    };
    let id = cpal::available_hosts()
        .into_iter()
        .find(|h| h.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            anyhow!(
                "audio host '{name}' unavailable; have {:?}",
                cpal::available_hosts()
            )
        })?;
    Ok(cpal::host_from_id(id)?)
}

fn pick_device(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device> {
    match name {
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device")),
        Some(n) => host
            .output_devices()?
            .find(|d| d.name().map(|x| x.contains(n)).unwrap_or(false))
            .ok_or_else(|| anyhow!("output device matching '{n}' not found")),
    }
}

pub fn list_devices() -> Result<()> {
    for id in cpal::available_hosts() {
        println!("host: {}", id.name());
        let host = cpal::host_from_id(id)?;
        let def = host.default_output_device().and_then(|d| d.name().ok());
        for d in host.output_devices()? {
            let name = d.name().unwrap_or_else(|_| "?".into());
            let cfg = d.default_output_config().map(|c| {
                format!(
                    "{}ch {}Hz {:?}",
                    c.channels(),
                    c.sample_rate().0,
                    c.sample_format()
                )
            });
            let mark = if Some(&name) == def.as_ref() {
                "*"
            } else {
                " "
            };
            println!(
                "  {mark} {name}  [{}]",
                cfg.unwrap_or_else(|e| e.to_string())
            );
        }
    }
    Ok(())
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut mixer: Mixer,
    meters: Arc<Meters>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = config.channels as usize;
    let rate = config.sample_rate.0 as f64;
    let mut scratch = vec![0f32; 1 << 15];
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            if scratch.len() < data.len() {
                scratch.resize(data.len(), 0.0); // only if driver exceeds 32k samples/period
            }
            let t0 = Instant::now();
            let s = &mut scratch[..data.len()];
            mixer.process(s, ch);
            for (o, &v) in data.iter_mut().zip(s.iter()) {
                *o = T::from_sample(v.clamp(-1.0, 1.0));
            }
            let period = Duration::from_secs_f64((data.len() / ch.max(1)) as f64 / rate);
            meters.record(s, ch, t0.elapsed(), period);
        },
        |e| log::error!("audio stream error: {e}"),
        None,
    )?;
    Ok(stream)
}

impl AudioHandle {
    pub fn start(cfg: AudioConfig) -> Result<Self> {
        let (cmd_tx, cmd_rx) = RingBuffer::<AudioCmd>::new(1024);
        let (ev_tx, ev_rx) = RingBuffer::<AudioEvent>::new(1024);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Ready>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let meters = Arc::new(Meters::default());
        let meters_cb = meters.clone();

        let thread = std::thread::Builder::new()
            .name("lsp-audio".into())
            .spawn(move || {
                let run = || -> Result<(cpal::Stream, Ready)> {
                    let host = pick_host(cfg.host.as_deref())?;
                    let device = pick_device(&host, cfg.device.as_deref())?;
                    let supported = device.default_output_config()?;
                    let fmt = supported.sample_format();
                    let config: cpal::StreamConfig = supported.into();
                    let mixer = Mixer::new(cmd_rx, ev_tx);
                    let m = meters_cb;
                    let stream = match fmt {
                        SampleFormat::F32 => build::<f32>(&device, &config, mixer, m)?,
                        SampleFormat::I16 => build::<i16>(&device, &config, mixer, m)?,
                        SampleFormat::I32 => build::<i32>(&device, &config, mixer, m)?,
                        SampleFormat::U16 => build::<u16>(&device, &config, mixer, m)?,
                        f => bail!("unsupported sample format {f:?}"),
                    };
                    stream.play()?;
                    let ready = Ready {
                        sample_rate: config.sample_rate.0,
                        channels: config.channels,
                        device_name: device.name().unwrap_or_default(),
                    };
                    Ok((stream, ready))
                };
                match run() {
                    Ok((stream, ready)) => {
                        let _ = ready_tx.send(Ok(ready));
                        let _ = stop_rx.recv(); // park until shutdown / handle drop
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;

        let ready = ready_rx
            .recv()
            .map_err(|_| anyhow!("audio thread died"))??;
        log::info!(
            "audio: {} @ {} Hz, {} ch",
            ready.device_name,
            ready.sample_rate,
            ready.channels
        );
        Ok(Self {
            cmd: cmd_tx,
            events: ev_rx,
            sample_rate: ready.sample_rate,
            channels: ready.channels,
            device_name: ready.device_name,
            meters,
            shutdown: Some(stop_tx),
            thread: Some(thread),
        })
    }

    pub fn send(&mut self, cmd: AudioCmd) {
        if self.cmd.push(cmd).is_err() {
            log::error!("audio command queue full; command dropped");
        }
    }

    pub fn secs_to_frames(&self, s: f32) -> u32 {
        (s.max(0.0) * self.sample_rate as f32) as u32
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        drop(self.shutdown.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod meter_tests {
    use super::*;

    #[test]
    fn peaks_clips_load_and_reset() {
        let m = Meters::default();
        // stereo: L peaks 0.5, R peaks 1.2 (one clip)
        m.record(&[0.1, -0.3, -0.5, 1.2], 2, Duration::from_millis(1), Duration::from_millis(10));
        m.record(&[0.2, 0.1], 2, Duration::from_millis(3), Duration::from_millis(10));
        let r = m.take();
        assert_eq!(r.peak, [0.5, 1.2]);
        assert_eq!(r.clips, 1);
        assert!((r.load - 0.3).abs() < 1e-3);
        assert_eq!(m.take(), MeterReading::default());
        // mono feeds both sides
        m.record(&[-0.25], 1, Duration::ZERO, Duration::from_millis(10));
        assert_eq!(m.take().peak, [0.25, 0.25]);
    }
}
