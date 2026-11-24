//! Incoming timecode for cue triggers: MIDI Time Code on a MIDI input, or
//! Art-Net ArtTimeCode over UDP. Each reading is sent to the controller as
//! `Command::Timecode`; the controller decides what fires.

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Sender;
use lsp_core::{Command, TcFrame, TcRate, Timecode, TimecodeSource};
use midir::{Ignore, MidiInput, MidiInputConnection};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// A running listener; dropping it stops listening and frees the port.
pub enum Listener {
    Mtc(#[allow(dead_code)] MidiInputConnection<()>),
    Artnet {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    },
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Listener::Artnet { stop, thread } = self {
            stop.store(true, Ordering::Relaxed);
            if let Some(t) = thread.take() {
                let _ = t.join(); // ≤ one read timeout
            }
        }
    }
}

/// Start listening. Returns `Ok(None)` for `TimecodeSource::Off`, and a
/// human-readable description of what is being listened to.
pub fn start(
    source: TimecodeSource,
    mtc_port: Option<&str>,
    artnet_listen: &str,
    tx: Sender<Command>,
) -> Result<Option<(Listener, String)>> {
    match source {
        TimecodeSource::Off => Ok(None),
        TimecodeSource::Mtc => start_mtc(mtc_port, tx).map(Some),
        TimecodeSource::Artnet => start_artnet(artnet_listen, tx).map(Some),
    }
}

fn start_mtc(port: Option<&str>, tx: Sender<Command>) -> Result<(Listener, String)> {
    let mut input = MidiInput::new("lsp-rs-mtc")?;
    // Default ignores SysEx and timing messages, which is all of MTC.
    input.ignore(Ignore::ActiveSense);
    let ports = input.ports();
    let named: Vec<_> = ports
        .iter()
        .map(|p| (p, input.port_name(p).unwrap_or_default()))
        .collect();
    let (p, name) = match port {
        Some(want) => named.iter().find(|(_, n)| n.contains(want)),
        None => named.iter().find(|(_, n)| !n.contains("Through")),
    }
    .cloned()
    .ok_or_else(|| match port {
        Some(want) => anyhow!("MIDI input matching '{want}' not found"),
        None => anyhow!("no MIDI input port found"),
    })?;
    let p = p.clone();
    let mut dec = MtcDecoder::default();
    let conn = input
        .connect(
            &p,
            "lsp-rs-mtc-in",
            move |_, bytes, _| {
                if let Some(f) = dec.feed(bytes) {
                    let _ = tx.send(Command::Timecode(f));
                }
            },
            (),
        )
        .map_err(|e| anyhow!("MIDI input connect: {e}"))?;
    Ok((Listener::Mtc(conn), format!("MTC on '{name}'")))
}

fn start_artnet(addr: &str, tx: Sender<Command>) -> Result<(Listener, String)> {
    let sock = UdpSocket::bind(addr).with_context(|| format!("Art-Net bind {addr}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let desc = format!("Art-Net timecode on udp://{}", sock.local_addr()?);
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("lsp-artnet".into())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                while !stop.load(Ordering::Relaxed) {
                    match sock.recv_from(&mut buf) {
                        Ok((n, _)) => {
                            if let Some(f) = parse_artnet(&buf[..n]) {
                                let _ = tx.send(Command::Timecode(f));
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(e) => {
                            log::warn!("Art-Net recv: {e}");
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            })?
    };
    Ok((
        Listener::Artnet {
            stop,
            thread: Some(thread),
        },
        desc,
    ))
}

/// ArtTimeCode: "Art-Net\0", OpCode 0x9700 (LE), ProtVer (BE), Filler1,
/// StreamId, Frames, Seconds, Minutes, Hours, Type. Other packets → None.
pub fn parse_artnet(b: &[u8]) -> Option<TcFrame> {
    if b.len() < 19 || &b[..8] != b"Art-Net\0" || b[8] != 0x00 || b[9] != 0x97 {
        return None;
    }
    let tc = Timecode {
        frames: b[14],
        seconds: b[15],
        minutes: b[16],
        hours: b[17],
    };
    let rate = TcRate::from_code(b[18]);
    valid(tc, rate).then_some(TcFrame { tc, rate })
}

fn valid(tc: Timecode, rate: TcRate) -> bool {
    tc.hours < 24 && tc.minutes < 60 && tc.seconds < 60 && (tc.frames as u32) < rate.nominal()
}

/// MTC decoder: quarter-frame messages (F1 0xnv) and full-frame SysEx
/// (F0 7F dev 01 01 hr mn sc fr F7).
#[derive(Default)]
pub struct MtcDecoder {
    pieces: [u8; 8],
    /// Bit n set = piece n received in this cycle.
    have: u8,
    /// Last time reported from quarter frames (for the mid-cycle update).
    last: Option<TcFrame>,
}

impl MtcDecoder {
    pub fn feed(&mut self, msg: &[u8]) -> Option<TcFrame> {
        match msg {
            [0xF1, d] => self.quarter(*d),
            [0xF0, 0x7F, _, 0x01, 0x01, hr, mn, sc, fr, 0xF7, ..] => {
                self.have = 0;
                self.last = None;
                let rate = TcRate::from_code(hr >> 5);
                let tc = Timecode {
                    hours: hr & 0x1F,
                    minutes: *mn,
                    seconds: *sc,
                    frames: *fr,
                };
                valid(tc, rate).then_some(TcFrame { tc, rate })
            }
            _ => None,
        }
    }

    fn quarter(&mut self, d: u8) -> Option<TcFrame> {
        let piece = (d >> 4) as usize & 7;
        if piece == 0 {
            self.have = 0;
        }
        self.pieces[piece] = d & 0x0F;
        self.have |= 1 << piece;
        match piece {
            // The 8 pieces take two frames to arrive, so the time they encode
            // is two frames old by the last one.
            7 if self.have == 0xFF => {
                let p = &self.pieces;
                let rate = TcRate::from_code(p[7] >> 1);
                let tc = Timecode {
                    frames: p[0] | (p[1] & 1) << 4,
                    seconds: p[2] | (p[3] & 3) << 4,
                    minutes: p[4] | (p[5] & 3) << 4,
                    hours: p[6] | (p[7] & 1) << 4,
                };
                if !valid(tc, rate) {
                    return None;
                }
                let f = TcFrame {
                    tc: Timecode::from_frames(tc.to_frames(rate) + 2, rate),
                    rate,
                };
                self.last = Some(f);
                Some(f)
            }
            // Halfway through the next cycle: one frame later.
            3 => self.last.map(|l| TcFrame {
                tc: Timecode::from_frames(l.tc.to_frames(l.rate) + 1, l.rate),
                rate: l.rate,
            }),
            _ => None,
        }
    }
}

pub fn list_ports() -> Result<()> {
    let input = MidiInput::new("lsp-rs-list")?;
    println!("MIDI in (timecode):");
    for p in input.ports() {
        println!("  {}", input.port_name(&p).unwrap_or_else(|_| "?".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 8 quarter-frame messages encoding `tc` at `rate_code`.
    fn qf(tc: Timecode, rate_code: u8) -> Vec<[u8; 2]> {
        let v = [
            tc.frames & 0xF,
            tc.frames >> 4,
            tc.seconds & 0xF,
            tc.seconds >> 4,
            tc.minutes & 0xF,
            tc.minutes >> 4,
            tc.hours & 0xF,
            tc.hours >> 4 | rate_code << 1,
        ];
        (0..8).map(|i| [0xF1, (i as u8) << 4 | v[i]]).collect()
    }

    #[test]
    fn mtc_quarter_frames() {
        let mut d = MtcDecoder::default();
        let tc = Timecode::parse("01:00:59:23").unwrap();
        let mut out = vec![];
        for m in qf(tc, 1) {
            out.extend(d.feed(&m));
        }
        // 25 fps: 23 + 2 frames rolls into the next minute.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rate, TcRate::Fps25);
        assert_eq!(out[0].tc.to_string(), "01:01:00:00");
        // Next cycle: piece 3 reports one frame on.
        let next = Timecode::parse("01:01:00:00").unwrap();
        let mut mid = vec![];
        for m in &qf(next, 1)[..4] {
            mid.extend(d.feed(m));
        }
        assert_eq!(mid.len(), 1);
        assert_eq!(mid[0].tc.to_string(), "01:01:00:01");
    }

    #[test]
    fn mtc_partial_cycle_is_ignored() {
        let mut d = MtcDecoder::default();
        let msgs = qf(Timecode::parse("00:00:10:00").unwrap(), 3);
        for m in &msgs[2..] {
            assert_eq!(d.feed(m), None); // joined mid-cycle
        }
    }

    #[test]
    fn mtc_full_frame() {
        let mut d = MtcDecoder::default();
        // 30 fps (code 3) in bits 5-6 of hours, hour 2.
        let f = d
            .feed(&[0xF0, 0x7F, 0x7F, 0x01, 0x01, 3 << 5 | 2, 10, 20, 29, 0xF7])
            .unwrap();
        assert_eq!(f.rate, TcRate::Fps30);
        assert_eq!(f.tc.to_string(), "02:10:20:29");
    }

    #[test]
    fn artnet_timecode() {
        let mut p = b"Art-Net\0".to_vec();
        p.extend([0x00, 0x97, 0, 14, 0, 0, 12, 34, 56, 1, 1]);
        let f = parse_artnet(&p).unwrap();
        assert_eq!(f.tc.to_string(), "01:56:34:12");
        assert_eq!(f.rate, TcRate::Fps25);
        p[9] = 0x50; // ArtDmx
        assert_eq!(parse_artnet(&p), None);
        assert_eq!(parse_artnet(b"Art-Net\0"), None);
    }
}
