//! Show model: cue list, cue types, transport commands. No I/O beyond file load.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Show {
    pub name: String,
    #[serde(default)]
    pub settings: ShowSettings,
    #[serde(rename = "cue", default)]
    pub cues: Vec<Cue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShowSettings {
    /// UDP bind address for the OSC control server.
    pub osc_listen: String,
    /// Substring match against output device name. None = system default.
    pub audio_device: Option<String>,
    /// Fade time (s) applied by PANIC.
    pub panic_fade: f32,
    /// Incoming timecode that fires cues with a `timecode` trigger.
    pub timecode: TimecodeSource,
    /// MTC: substring match against the MIDI input port name. None = first
    /// port that is not a "Through" port.
    pub mtc_port: Option<String>,
    /// Art-Net: UDP bind address for ArtTimeCode packets.
    pub artnet_listen: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimecodeSource {
    #[default]
    Off,
    /// MIDI Time Code (quarter-frame + full-frame SysEx) on a MIDI input.
    Mtc,
    /// Art-Net ArtTimeCode (OpCode 0x9700) over UDP.
    Artnet,
}

impl TimecodeSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimecodeSource::Off => "off",
            TimecodeSource::Mtc => "mtc",
            TimecodeSource::Artnet => "artnet",
        }
    }
}

/// SMPTE frame rate, as carried by MTC and Art-Net (same 0..3 encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcRate {
    Fps24,
    Fps25,
    /// 29.97 drop-frame.
    Df2997,
    Fps30,
}

impl TcRate {
    pub fn from_code(code: u8) -> Self {
        match code & 3 {
            0 => TcRate::Fps24,
            1 => TcRate::Fps25,
            2 => TcRate::Df2997,
            _ => TcRate::Fps30,
        }
    }

    /// Frames per timecode second (frame labels run 0..nominal).
    pub fn nominal(self) -> u32 {
        match self {
            TcRate::Fps24 => 24,
            TcRate::Fps25 => 25,
            TcRate::Df2997 | TcRate::Fps30 => 30,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TcRate::Fps24 => "24",
            TcRate::Fps25 => "25",
            TcRate::Df2997 => "29.97df",
            TcRate::Fps30 => "30",
        }
    }
}

/// A timecode label HH:MM:SS:FF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
}

impl Timecode {
    /// `HH:MM:SS:FF` (`;` or `.` also accepted before the frames).
    pub fn parse(s: &str) -> Option<Self> {
        let p: Vec<&str> = s.trim().split([':', ';', '.']).collect();
        let [h, m, sec, f] = p.as_slice() else {
            return None;
        };
        let n = |x: &str, max: u8| x.parse::<u8>().ok().filter(|v| *v < max);
        Some(Self {
            hours: n(h, 24)?,
            minutes: n(m, 60)?,
            seconds: n(sec, 60)?,
            frames: n(f, 30)?,
        })
    }

    /// Frame count since 00:00:00:00, counting labels at the nominal rate
    /// (drop-frame labels are compared as labels, which is what triggers want).
    pub fn to_frames(self, rate: TcRate) -> i64 {
        let secs = (self.hours as i64 * 60 + self.minutes as i64) * 60 + self.seconds as i64;
        secs * rate.nominal() as i64 + self.frames as i64
    }

    pub fn from_frames(frames: i64, rate: TcRate) -> Self {
        let n = rate.nominal() as i64;
        let f = frames.rem_euclid(24 * 3600 * n);
        let secs = f / n;
        Self {
            hours: (secs / 3600) as u8,
            minutes: (secs / 60 % 60) as u8,
            seconds: (secs % 60) as u8,
            frames: (f % n) as u8,
        }
    }
}

impl std::fmt::Display for Timecode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}:{:02}",
            self.hours, self.minutes, self.seconds, self.frames
        )
    }
}

/// One timecode reading from a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcFrame {
    pub tc: Timecode,
    pub rate: TcRate,
}

impl Default for ShowSettings {
    fn default() -> Self {
        Self {
            osc_listen: "0.0.0.0:53000".into(),
            audio_device: None,
            panic_fade: 1.0,
            timecode: TimecodeSource::Off,
            mtc_port: None,
            artnet_listen: "0.0.0.0:6454".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cue {
    pub number: String,
    #[serde(default)]
    pub name: String,
    /// Delay (s) between trigger and action.
    #[serde(default)]
    pub pre_wait: f32,
    /// For AutoContinue: delay (s) after action start before triggering the next cue.
    #[serde(default)]
    pub post_wait: f32,
    #[serde(default)]
    pub next: NextAction,
    /// What a trigger does while this cue is already running.
    #[serde(default)]
    pub second_trigger: SecondTrigger,
    /// Fire this cue when incoming timecode (see `settings.timecode`) rolls
    /// past this time, `HH:MM:SS:FF`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timecode: Option<String>,
    #[serde(flatten)]
    pub kind: CueKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecondTrigger {
    /// Leave the running instance alone (standby still advances).
    #[default]
    Ignore,
    /// Stop with the cue's own fade-out.
    Stop,
    /// Stop immediately.
    HardStop,
    /// Hard-stop the running instance and start again.
    Restart,
}

impl SecondTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            SecondTrigger::Ignore => "ignore",
            SecondTrigger::Stop => "stop",
            SecondTrigger::HardStop => "hard_stop",
            SecondTrigger::Restart => "restart",
        }
    }
}

/// Gain trajectory for fades.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadeCurve {
    Linear,
    /// Gentle start and end.
    #[default]
    SCurve,
    /// Constant loudness through crossfades.
    EqualPower,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    #[default]
    DoNothing,
    /// Trigger next cue `post_wait` seconds after this cue's action starts.
    AutoContinue,
    /// Trigger next cue when this cue completes naturally.
    AutoFollow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CueKind {
    Audio(AudioCue),
    Fade {
        target: String,
        duration: f32,
        /// Target level in dB. Omit for -inf (fade out).
        #[serde(default)]
        gain_db: Option<f32>,
        /// Stop target when the fade reaches its level.
        #[serde(default)]
        stop: bool,
        #[serde(default)]
        curve: FadeCurve,
    },
    Stop {
        /// Cue number to stop. None = stop all.
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        fade: f32,
    },
    Osc {
        /// "host:port"
        host: String,
        path: String,
        #[serde(default)]
        args: Vec<OscArg>,
    },
    Midi {
        /// Substring match against MIDI output port name.
        port: String,
        bytes: Vec<u8>,
    },
    Wait {
        duration: f32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioCue {
    pub file: PathBuf,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub fade_in: f32,
    /// Fade applied when this cue is stopped without an explicit fade.
    #[serde(default)]
    pub fade_out: f32,
    #[serde(default)]
    pub start: f32,
    #[serde(default)]
    pub end: Option<f32>,
    #[serde(default, rename = "loop")]
    pub looping: bool,
    /// Curve for this cue's own fade_in / fade_out (and stops that use them).
    #[serde(default)]
    pub curve: FadeCurve,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OscArg {
    Bool(bool),
    Int(i32),
    Float(f32),
    Str(String),
}

/// Transport / control commands from any surface (OSC, stdin, UI).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Go,
    /// None = stop all (each cue's own fade-out).
    Stop(Option<String>),
    /// Stop immediately (declick only). None = all.
    HardStop(Option<String>),
    /// None = all.
    Pause(Option<String>),
    /// None = all.
    Resume(Option<String>),
    /// Fade everything out over `panic_fade`; a second panic during the fade hard-stops.
    Panic,
    Goto(String),
    Next,
    Prev,
    Status,
    /// Register a feedback client (renew every few seconds; expires otherwise).
    Subscribe(SocketAddr),
    Unsubscribe(SocketAddr),
    /// Re-send the full cue list to one client.
    CueList(SocketAddr),
    /// Load a show file (path on the engine host).
    Load(PathBuf),
    /// Re-read the current show file.
    Reload,
    /// Edit the show file: JSON request (see the engine's `edit` module).
    /// Replies go to `reply`.
    Edit {
        json: String,
        reply: Option<SocketAddr>,
    },
    /// Create a new, empty show file and load it (refuses to overwrite).
    NewShow {
        path: PathBuf,
        name: String,
        reply: Option<SocketAddr>,
    },
    /// A reading from the timecode listener.
    Timecode(TcFrame),
    Quit,
}

impl Command {
    /// Parse a text command: `go`, `stop [n]`, `panic`, `goto n`, `next`, `prev`,
    /// `status`, `load <path>`, `reload`, `quit`.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        let mut it = line.split_whitespace();
        let head = it.next().unwrap_or("go").to_ascii_lowercase();
        // `load` takes the rest of the line so paths may contain spaces.
        let rest = line.get(head.len()..).unwrap_or("").trim();
        let arg = it.next().map(str::to_string);
        Some(match head.as_str() {
            "go" | "g" => Command::Go,
            "stop" | "s" => Command::Stop(arg),
            "hardstop" | "hs" => Command::HardStop(arg),
            "pause" => Command::Pause(arg),
            "resume" => Command::Resume(arg),
            "panic" | "p" => Command::Panic,
            "goto" => Command::Goto(arg?),
            "next" | "n" => Command::Next,
            "prev" => Command::Prev,
            "status" | "?" => Command::Status,
            "load" if !rest.is_empty() => Command::Load(PathBuf::from(rest)),
            "reload" => Command::Reload,
            "quit" | "q" | "exit" => Command::Quit,
            _ => return None,
        })
    }
}

impl CueKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            CueKind::Audio(_) => "audio",
            CueKind::Fade { .. } => "fade",
            CueKind::Stop { .. } => "stop",
            CueKind::Osc { .. } => "osc",
            CueKind::Midi { .. } => "midi",
            CueKind::Wait { .. } => "wait",
        }
    }
}

impl NextAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            NextAction::DoNothing => "do_nothing",
            NextAction::AutoContinue => "auto_continue",
            NextAction::AutoFollow => "auto_follow",
        }
    }
}

impl Show {
    /// Placeholder used when the engine starts without a show file.
    pub fn empty() -> Self {
        Self {
            name: String::new(),
            settings: ShowSettings::default(),
            cues: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let txt =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&txt).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn find(&self, number: &str) -> Option<usize> {
        self.cues.iter().position(|c| c.number == number)
    }

    /// Per-cue problems that would make the cue misbehave when fired
    /// (None = fine). Media decode failures are added by the loader.
    pub fn cue_problems(&self) -> Vec<Option<String>> {
        let mut first = std::collections::HashMap::new();
        self.cues
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut p = Vec::new();
                if let Some(j) = first.insert(c.number.as_str(), i) {
                    first.insert(c.number.as_str(), j);
                    p.push(format!("duplicate cue number (also cue #{})", j + 1));
                }
                let target = match &c.kind {
                    CueKind::Fade { target, .. } => Some(target),
                    CueKind::Stop {
                        target: Some(t), ..
                    } => Some(t),
                    _ => None,
                };
                if let Some(t) = target {
                    match self.find(t) {
                        None => p.push(format!("target cue {t} does not exist")),
                        Some(j) if !matches!(self.cues[j].kind, CueKind::Audio(_)) => {
                            if matches!(c.kind, CueKind::Fade { .. }) {
                                p.push(format!("target cue {t} is not an audio cue"));
                            }
                        }
                        _ => {}
                    }
                }
                if let CueKind::Osc { host, path, .. } = &c.kind {
                    let ok = host
                        .rsplit_once(':')
                        .is_some_and(|(h, port)| !h.is_empty() && port.parse::<u16>().is_ok());
                    if !ok {
                        p.push(format!("OSC host '{host}' is not host:port"));
                    }
                    if !path.starts_with('/') {
                        p.push(format!("OSC path '{path}' must start with /"));
                    }
                }
                if let Some(t) = &c.timecode {
                    if Timecode::parse(t).is_none() {
                        p.push(format!("timecode '{t}' is not HH:MM:SS:FF"));
                    }
                }
                (!p.is_empty()).then(|| p.join("; "))
            })
            .collect()
    }

    /// Non-fatal consistency problems (duplicate numbers, dangling targets).
    pub fn validate(&self) -> Vec<String> {
        let mut warn = Vec::new();
        let mut seen = HashSet::new();
        for c in &self.cues {
            if !seen.insert(c.number.as_str()) {
                warn.push(format!("duplicate cue number {}", c.number));
            }
            let target = match &c.kind {
                CueKind::Fade { target, .. } => Some(target),
                CueKind::Stop {
                    target: Some(t), ..
                } => Some(t),
                _ => None,
            };
            if let Some(t) = target {
                if self.find(t).is_none() {
                    warn.push(format!("cue {} targets missing cue {}", c.number, t));
                }
            }
        }
        warn
    }
}

#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    if db <= -120.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOW: &str = r#"
name = "t"
[[cue]]
number = "1"
type = "audio"
file = "a.wav"
gain_db = -6
next = "auto_continue"
post_wait = 2
[[cue]]
number = "2"
type = "fade"
target = "1"
duration = 3
stop = true
[[cue]]
number = "3"
type = "osc"
host = "127.0.0.1:9000"
path = "/x"
args = [1, 2.5, "s", true]
[[cue]]
number = "4"
type = "stop"
target = "99"
"#;

    #[test]
    fn parse_show() {
        let s: Show = toml::from_str(SHOW).unwrap();
        assert_eq!(s.cues.len(), 4);
        assert_eq!(s.cues[0].next, NextAction::AutoContinue);
        match &s.cues[0].kind {
            CueKind::Audio(a) => assert_eq!(a.gain_db, -6.0),
            k => panic!("{k:?}"),
        }
        match &s.cues[2].kind {
            CueKind::Osc { args, .. } => assert_eq!(
                args,
                &vec![
                    OscArg::Int(1),
                    OscArg::Float(2.5),
                    OscArg::Str("s".into()),
                    OscArg::Bool(true)
                ]
            ),
            k => panic!("{k:?}"),
        }
        let w = s.validate();
        assert_eq!(w.len(), 1, "{w:?}");
    }

    #[test]
    fn parse_cmd() {
        assert_eq!(Command::parse(""), Some(Command::Go));
        assert_eq!(
            Command::parse("stop 3"),
            Some(Command::Stop(Some("3".into())))
        );
        assert_eq!(Command::parse("goto"), None);
        assert_eq!(
            Command::parse("load /tmp/My Show.toml"),
            Some(Command::Load("/tmp/My Show.toml".into()))
        );
        assert_eq!(Command::parse("load"), None);
        assert_eq!(Command::parse("reload"), Some(Command::Reload));
    }

    #[test]
    fn problems_per_cue() {
        let s: Show = toml::from_str(
            r#"
name = "t"
[[cue]]
number = "1"
type = "wait"
duration = 1
[[cue]]
number = "1"
type = "fade"
target = "1"
duration = 1
[[cue]]
number = "3"
type = "osc"
host = "nohost"
path = "x"
[[cue]]
number = "4"
type = "stop"
"#,
        )
        .unwrap();
        let p = s.cue_problems();
        assert_eq!(p[0], None);
        let p1 = p[1].as_deref().unwrap();
        assert!(p1.contains("duplicate") && p1.contains("not an audio cue"), "{p1}");
        assert!(p[2].as_deref().unwrap().contains("host:port"));
        assert_eq!(p[3], None);
        assert_eq!(s.cues[0].second_trigger, SecondTrigger::Ignore);
    }

    #[test]
    fn transport_cmds() {
        assert_eq!(Command::parse("hs"), Some(Command::HardStop(None)));
        assert_eq!(Command::parse("pause 2"), Some(Command::Pause(Some("2".into()))));
        assert_eq!(Command::parse("resume"), Some(Command::Resume(None)));
    }

    #[test]
    fn timecode_labels() {
        let t = Timecode::parse("01:02:03:04").unwrap();
        assert_eq!(t.to_string(), "01:02:03:04");
        assert_eq!(t.to_frames(TcRate::Fps25), (3600 + 120 + 3) * 25 + 4);
        assert_eq!(Timecode::from_frames(t.to_frames(TcRate::Fps25), TcRate::Fps25), t);
        assert_eq!(Timecode::parse("00:00:10;29").unwrap().frames, 29);
        assert!(Timecode::parse("00:61:00:00").is_none());
        assert!(Timecode::parse("00:00:00").is_none());
        let s: Show = toml::from_str(
            r#"
name = "t"
[settings]
timecode = "artnet"
[[cue]]
number = "1"
type = "wait"
duration = 1
timecode = "00:00:05:00"
[[cue]]
number = "2"
type = "wait"
duration = 1
timecode = "bad"
"#,
        )
        .unwrap();
        assert_eq!(s.settings.timecode, TimecodeSource::Artnet);
        assert_eq!(s.settings.artnet_listen, "0.0.0.0:6454");
        let p = s.cue_problems();
        assert_eq!(p[0], None);
        assert!(p[1].as_deref().unwrap().contains("timecode"));
    }

    #[test]
    fn db() {
        assert!((db_to_lin(-6.0) - 0.501).abs() < 1e-3);
        assert_eq!(db_to_lin(-200.0), 0.0);
    }
}
