//! Offline decode → stereo interleaved f32 at the engine rate (full preload).

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::mixer::Clip;

pub fn load_clip(path: &Path, target_rate: u32) -> Result<Clip> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("probe {}", path.display()))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("no audio track"))?;
    let track_id = track.id;
    let src_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| anyhow!("unknown sample rate"))?;
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut stereo: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(e)) => {
                log::warn!("{}: decode error skipped: {e}", path.display());
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let spec = *decoded.spec();
        let ch = spec.channels.count().max(1);
        let mut sb = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        sb.copy_interleaved_ref(decoded);
        stereo.reserve(sb.samples().len() / ch * 2);
        for fr in sb.samples().chunks_exact(ch) {
            let l = fr[0];
            let r = if ch > 1 { fr[1] } else { l };
            stereo.push(l);
            stereo.push(r);
        }
    }
    if stereo.is_empty() {
        return Err(anyhow!("{}: no audio decoded", path.display()));
    }
    Ok(resample_linear(&stereo, src_rate, target_rate).into())
}

/// Linear-interp SRC on stereo-interleaved data. Adequate for cue playback;
/// swap for `rubato` if you need mastering-grade conversion.
pub fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return input.to_vec();
    }
    let in_frames = input.len() / 2;
    let ratio = from as f64 / to as f64;
    let out_frames = ((in_frames as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for n in 0..out_frames {
        let pos = n as f64 * ratio;
        let i = pos as usize;
        let frac = (pos - i as f64) as f32;
        let j = (i + 1).min(in_frames - 1);
        for c in 0..2 {
            let a = input[i * 2 + c];
            let b = input[j * 2 + c];
            out.push(a + (b - a) * frac);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_mono_wav_and_resample() {
        let dir = std::env::temp_dir().join(format!("lsp-dec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&p, spec).unwrap();
        for _ in 0..44100 {
            w.write_sample(i16::MAX / 2).unwrap();
        }
        w.finalize().unwrap();

        let clip = load_clip(&p, 48000).unwrap();
        let frames = clip.len() / 2;
        assert!((frames as i64 - 48000).abs() <= 2, "frames={frames}");
        assert!((clip[1000] - 0.5).abs() < 1e-3 && clip[1000] == clip[1001]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
