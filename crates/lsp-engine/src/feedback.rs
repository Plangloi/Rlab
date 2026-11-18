//! OSC feedback to subscribed clients (remote UIs, TouchOSC, Q-SYS, ...).
//!
//! A client sends `/lsp/subscribe [reply_port]` and renews it at least every
//! `TTL` seconds; replies go to the sender's address (or `reply_port` on the
//! sender's host). Packets are sent from the OSC server socket.
//!
//! Out:
//!   /lsp/show   name:s count:i path:s rev:i
//!   /lsp/cue    idx:i number:s name:s type:s duration:f next:s pre_wait:f post_wait:f
//!               problem:s second_trigger:s data:s (problem "" = cue is fine;
//!               data = the cue as JSON, media paths relative to the show)
//!   /lsp/edit/reply  json:s   (to the editing client only)
//!   /lsp/state  standby:i  [idx:i phase:s elapsed:f duration:f]*
//!   /lsp/load   status:s path:s message:s     (status: loading | ok | error)
//!   /lsp/meter  peak_l:f peak_r:f clips:i load:f subs:i   (linear peaks since last push)
//!   /lsp/engine device:s rate:i channels:i max_voices:i watch:i   (on subscribe)
//!
//! `standby` is the playhead index (-1 = end of list). `duration` -1 = unbounded
//! (looping audio). `phase` is `pre_wait` | `running` | `paused` | `stopping`.

use lsp_core::{CueKind, Show};
use rosc::{OscMessage, OscPacket, OscType};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

pub const TTL: Duration = Duration::from_secs(10);

pub struct Subscribers {
    sock: UdpSocket,
    clients: HashMap<SocketAddr, Instant>,
}

impl Subscribers {
    pub fn new(sock: UdpSocket) -> Self {
        Self {
            sock,
            clients: HashMap::new(),
        }
    }

    /// Returns true if `addr` was not subscribed before.
    pub fn subscribe(&mut self, addr: SocketAddr) -> bool {
        self.clients.insert(addr, Instant::now()).is_none()
    }

    pub fn unsubscribe(&mut self, addr: SocketAddr) -> bool {
        self.clients.remove(&addr).is_some()
    }

    pub fn expire(&mut self) {
        self.clients.retain(|addr, seen| {
            let keep = seen.elapsed() < TTL;
            if !keep {
                log::info!("feedback: {addr} expired");
            }
            keep
        });
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn send_to(&self, addr: SocketAddr, msg: OscMessage) {
        match rosc::encoder::encode(&OscPacket::Message(msg)) {
            Ok(b) => {
                if let Err(e) = self.sock.send_to(&b, addr) {
                    log::debug!("feedback -> {addr}: {e}");
                }
            }
            Err(e) => log::error!("feedback encode: {e:?}"),
        }
    }

    pub fn broadcast_many(&self, msgs: Vec<OscMessage>) {
        for m in msgs {
            self.broadcast(m);
        }
    }

    pub fn broadcast(&self, msg: OscMessage) {
        let Ok(b) = rosc::encoder::encode(&OscPacket::Message(msg)) else {
            return;
        };
        for addr in self.clients.keys() {
            if let Err(e) = self.sock.send_to(&b, addr) {
                log::debug!("feedback -> {addr}: {e}");
            }
        }
    }
}

fn msg(addr: &str, args: Vec<OscType>) -> OscMessage {
    OscMessage {
        addr: addr.into(),
        args,
    }
}

/// `/lsp/show` followed by one `/lsp/cue` per cue.
pub struct ListMeta<'a> {
    pub path: &'a str,
    pub rev: u64,
    pub problems: &'a [Option<String>],
    /// Per-cue JSON (`data` arg); missing entries are sent as "".
    pub data: &'a [String],
}

pub fn cue_list(show: &Show, durations: &[f32], meta: &ListMeta) -> Vec<OscMessage> {
    let mut out = Vec::with_capacity(show.cues.len() + 1);
    out.push(msg(
        "/lsp/show",
        vec![
            OscType::String(show.name.clone()),
            OscType::Int(show.cues.len() as i32),
            OscType::String(meta.path.into()),
            OscType::Int(meta.rev as i32),
        ],
    ));
    for (i, c) in show.cues.iter().enumerate() {
        out.push(msg(
            "/lsp/cue",
            vec![
                OscType::Int(i as i32),
                OscType::String(c.number.clone()),
                OscType::String(c.name.clone()),
                OscType::String(c.kind.type_name().into()),
                OscType::Float(durations.get(i).copied().unwrap_or(0.0)),
                OscType::String(c.next.as_str().into()),
                OscType::Float(c.pre_wait),
                OscType::Float(c.post_wait),
                OscType::String(meta.problems.get(i).cloned().flatten().unwrap_or_default()),
                OscType::String(c.second_trigger.as_str().into()),
                OscType::String(meta.data.get(i).cloned().unwrap_or_default()),
            ],
        ));
    }
    out
}

pub fn meter(m: &lsp_audio::MeterReading, subs: usize) -> OscMessage {
    msg(
        "/lsp/meter",
        vec![
            OscType::Float(m.peak[0]),
            OscType::Float(m.peak[1]),
            OscType::Int(m.clips as i32),
            OscType::Float(m.load),
            OscType::Int(subs as i32),
        ],
    )
}

pub fn engine_info(device: &str, rate: u32, channels: u16, max_voices: usize, watch: bool) -> OscMessage {
    msg(
        "/lsp/engine",
        vec![
            OscType::String(device.into()),
            OscType::Int(rate as i32),
            OscType::Int(channels as i32),
            OscType::Int(max_voices as i32),
            OscType::Int(watch as i32),
        ],
    )
}

pub fn edit_reply(json: String) -> OscMessage {
    msg("/lsp/edit/reply", vec![OscType::String(json)])
}

pub fn load_status(status: &str, path: &str, message: &str) -> OscMessage {
    msg(
        "/lsp/load",
        vec![
            OscType::String(status.into()),
            OscType::String(path.into()),
            OscType::String(message.into()),
        ],
    )
}

pub struct RunInfo {
    pub idx: usize,
    pub phase: &'static str,
    pub elapsed: f32,
    pub duration: f32,
}

pub fn state(standby: Option<usize>, running: &[RunInfo]) -> OscMessage {
    let mut args = Vec::with_capacity(1 + running.len() * 4);
    args.push(OscType::Int(standby.map(|i| i as i32).unwrap_or(-1)));
    for r in running {
        args.push(OscType::Int(r.idx as i32));
        args.push(OscType::String(r.phase.into()));
        args.push(OscType::Float(r.elapsed));
        args.push(OscType::Float(r.duration));
    }
    msg("/lsp/state", args)
}

/// Nominal duration (s) of each cue for display: audio region length
/// (-1 if looping), fade / wait duration, 0 for instant cues.
pub fn durations(show: &Show, clip_secs: impl Fn(&std::path::Path) -> Option<f32>) -> Vec<f32> {
    show.cues
        .iter()
        .map(|c| match &c.kind {
            CueKind::Audio(a) if a.looping => -1.0,
            CueKind::Audio(a) => {
                let len = clip_secs(&a.file).unwrap_or(0.0);
                let end = a.end.map(|e| e.min(len)).unwrap_or(len);
                (end - a.start.max(0.0)).max(0.0)
            }
            CueKind::Fade { duration, .. } | CueKind::Wait { duration } => *duration,
            _ => 0.0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_layout() {
        let m = state(
            Some(2),
            &[RunInfo {
                idx: 0,
                phase: "running",
                elapsed: 1.5,
                duration: -1.0,
            }],
        );
        assert_eq!(m.addr, "/lsp/state");
        assert_eq!(
            m.args,
            vec![
                OscType::Int(2),
                OscType::Int(0),
                OscType::String("running".into()),
                OscType::Float(1.5),
                OscType::Float(-1.0)
            ]
        );
        assert_eq!(state(None, &[]).args, vec![OscType::Int(-1)]);
    }

    #[test]
    fn durations_and_list() {
        let show: Show = toml::from_str(
            r#"
name = "t"
[[cue]]
number = "1"
type = "audio"
file = "a.wav"
start = 1
[[cue]]
number = "2"
type = "audio"
file = "a.wav"
loop = true
[[cue]]
number = "3"
type = "wait"
duration = 2.5
"#,
        )
        .unwrap();
        let d = durations(&show, |_| Some(10.0));
        assert_eq!(d, vec![9.0, -1.0, 2.5]);
        let meta = ListMeta {
            path: "/x/show.toml",
            rev: 7,
            problems: &[None, Some("bad".into())],
            data: &[],
        };
        let l = cue_list(&show, &d, &meta);
        assert_eq!(l[0].args[3], OscType::Int(7));
        assert_eq!(l[2].args[8], OscType::String("bad".into()));
        assert_eq!(l[3].args[8], OscType::String("".into()));
        assert_eq!(l[0].args[2], OscType::String("/x/show.toml".into()));
        assert_eq!(l.len(), 4);
        assert_eq!(l[3].args[3], OscType::String("wait".into()));
    }
}
