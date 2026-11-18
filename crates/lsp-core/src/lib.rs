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
}

impl Default for ShowSettings {
    fn default() -> Self {
        Self {
            osc_listen: "0.0.0.0:53000".into(),
            audio_device: None,
            panic_fade: 1.0,
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
    #[serde(flatten)]
    pub kind: CueKind,
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
    /// None = stop all.
    Stop(Option<String>),
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
    Quit,
}

impl Command {
    /// Parse a text command: `go`, `stop [n]`, `panic`, `goto n`, `next`, `prev`, `status`, `quit`.
    pub fn parse(line: &str) -> Option<Self> {
        let mut it = line.split_whitespace();
        let head = it.next().unwrap_or("go").to_ascii_lowercase();
        let arg = it.next().map(str::to_string);
        Some(match head.as_str() {
            "go" | "g" => Command::Go,
            "stop" | "s" => Command::Stop(arg),
            "panic" | "p" => Command::Panic,
            "goto" => Command::Goto(arg?),
            "next" | "n" => Command::Next,
            "prev" => Command::Prev,
            "status" | "?" => Command::Status,
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
    pub fn load(path: &Path) -> Result<Self> {
        let txt =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&txt).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn find(&self, number: &str) -> Option<usize> {
        self.cues.iter().position(|c| c.number == number)
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
    }

    #[test]
    fn db() {
        assert!((db_to_lin(-6.0) - 0.501).abs() < 1e-3);
        assert_eq!(db_to_lin(-200.0), 0.0);
    }
}
