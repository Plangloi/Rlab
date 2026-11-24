//! Show Player operator UI. The Rust side is a thin OSC client of the `lsp`
//! engine: it forwards transport commands and relays feedback
//! (`/lsp/show`, `/lsp/cue`, `/lsp/state`) to the webview as events.
//!
//! Engine address: `--engine host:port`, `LSP_ENGINE`, or 127.0.0.1:53000;
//! changeable at runtime from the UI.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use rosc::{OscMessage, OscPacket, OscType};
use serde::Serialize;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

/// Re-subscribe interval; must be well under the engine's 10 s TTL.
const HEARTBEAT: Duration = Duration::from_secs(2);

struct Client {
    sock: UdpSocket,
    engine: Mutex<SocketAddr>,
}

impl Client {
    fn send(&self, addr: &str, args: Vec<OscType>) -> Result<(), String> {
        let pkt = OscPacket::Message(OscMessage {
            addr: addr.into(),
            args,
        });
        let bytes = rosc::encoder::encode(&pkt).map_err(|e| format!("{e:?}"))?;
        let to = *self.engine.lock().unwrap();
        self.sock
            .send_to(&bytes, to)
            .map(|_| ())
            .map_err(|e| format!("send to {to}: {e}"))
    }
}

type Shared = Arc<Client>;

#[derive(Serialize, Clone)]
struct ShowInfo {
    name: String,
    count: i32,
    /// Show file path on the engine host ("" = none loaded).
    path: String,
    /// Show revision; edits must quote it.
    rev: i32,
}

#[derive(Serialize, Clone)]
struct LoadInfo {
    /// loading | ok | error
    status: String,
    path: String,
    message: String,
}

#[derive(Serialize, Clone)]
struct CueInfo {
    idx: i32,
    number: String,
    name: String,
    kind: String,
    duration: f32,
    next: String,
    pre_wait: f32,
    post_wait: f32,
    /// Why the cue would misbehave if fired ("" = fine).
    problem: String,
    second_trigger: String,
    /// Full cue as stored in the show file (for the inspector).
    data: serde_json::Value,
}

#[derive(Serialize, Clone)]
struct Running {
    idx: i32,
    phase: String,
    elapsed: f32,
    duration: f32,
}

#[derive(Serialize, Clone)]
struct MeterInfo {
    /// Linear peaks since the previous meter message (1.0 = 0 dBFS).
    peak: [f32; 2],
    clips: i32,
    /// Audio callback load, 0..1.
    load: f32,
    /// Feedback clients subscribed to the engine.
    subs: i32,
}

#[derive(Serialize, Clone)]
struct EngineInfo {
    device: String,
    rate: i32,
    channels: i32,
    max_voices: i32,
    watch: bool,
}

#[derive(Serialize, Clone)]
struct TimecodeInfo {
    /// off | mtc | artnet
    source: String,
    /// off | waiting | rolling | stopped | error
    status: String,
    /// Last reading HH:MM:SS:FF ("" = none yet).
    tc: String,
    rate: String,
    /// What the engine listens to, or why it can't.
    message: String,
}

#[derive(Serialize, Clone)]
struct StateInfo {
    standby: i32,
    running: Vec<Running>,
}

fn resolve(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("{addr}: {e}"))?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| format!("{addr}: no IPv4 address"))
}

/// Transport command: `go`, `stop [cue]`, `hardstop [cue]`, `pause [cue]`,
/// `resume [cue]`, `panic`, `goto cue`, `next`, `prev`, `reload`.
#[tauri::command]
fn command(client: State<Shared>, cmd: String, arg: Option<String>) -> Result<(), String> {
    let path = match cmd.as_str() {
        "go" | "stop" | "hardstop" | "pause" | "resume" | "panic" | "goto" | "next" | "prev"
        | "reload" => format!("/lsp/{cmd}"),
        _ => return Err(format!("unknown command {cmd}")),
    };
    let args = arg.into_iter().map(OscType::String).collect();
    client.send(&path, args)
}

#[tauri::command]
fn get_engine(client: State<Shared>) -> String {
    client.engine.lock().unwrap().to_string()
}

/// Point the UI at another engine and subscribe immediately.
#[tauri::command]
fn set_engine(client: State<Shared>, addr: String) -> Result<String, String> {
    let new = resolve(addr.trim())?;
    let _ = client.send("/lsp/unsubscribe", vec![]);
    *client.engine.lock().unwrap() = new;
    client.send("/lsp/subscribe", vec![])?;
    Ok(new.to_string())
}

/// Pick a show file and tell the engine to load it. The path is local to this
/// machine, so it only resolves if the engine runs here (or sees the same path).
/// Returns the chosen path, or None if the dialog was cancelled.
#[tauri::command]
async fn open_show(
    app: AppHandle,
    client: State<'_, Shared>,
    current: Option<String>,
) -> Result<Option<String>, String> {
    let mut dlg = app
        .dialog()
        .file()
        .set_title("Open show")
        .add_filter("Show file", &["toml"]);
    if let Some(dir) = current
        .as_deref()
        .filter(|p| !p.is_empty())
        .and_then(|p| std::path::Path::new(p).parent())
    {
        dlg = dlg.set_directory(dir);
    }
    let Some(picked) = dlg.blocking_pick_file() else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    let path = path.to_string_lossy().into_owned();
    client.send("/lsp/load", vec![OscType::String(path.clone())])?;
    Ok(Some(path))
}

/// Send an edit request (`{op, rev, ...}`, see the engine's edit module).
/// The engine answers with `lsp:edit`.
#[tauri::command]
fn edit(client: State<Shared>, req: serde_json::Value) -> Result<(), String> {
    client.send("/lsp/edit", vec![OscType::String(req.to_string())])
}

/// Ask where to create a new show file, then have the engine create and load it.
#[tauri::command]
async fn new_show(
    app: AppHandle,
    client: State<'_, Shared>,
    current: Option<String>,
) -> Result<Option<String>, String> {
    let mut dlg = app
        .dialog()
        .file()
        .set_title("New show")
        .set_file_name("show.toml")
        .add_filter("Show file", &["toml"]);
    if let Some(dir) = current
        .as_deref()
        .filter(|p| !p.is_empty())
        .and_then(|p| std::path::Path::new(p).parent())
    {
        dlg = dlg.set_directory(dir);
    }
    let Some(picked) = dlg.blocking_save_file() else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    let path = path.to_string_lossy().into_owned();
    // The engine refuses to overwrite an existing file (reported via lsp:edit),
    // so a show is never replaced from here.
    client.send("/lsp/new", vec![OscType::String(path.clone())])?;
    Ok(Some(path))
}

/// Pick a media file for an audio cue. Returns the path (not sent anywhere).
#[tauri::command]
async fn pick_media(app: AppHandle, dir: Option<String>) -> Result<Option<String>, String> {
    let mut dlg = app.dialog().file().set_title("Choose audio file").add_filter(
        "Audio",
        &["wav", "aif", "aiff", "flac", "mp3", "m4a", "aac", "ogg", "caf"],
    );
    if let Some(d) = dir.filter(|d| !d.is_empty()) {
        dlg = dlg.set_directory(d);
    }
    let Some(picked) = dlg.blocking_pick_file() else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// Ask the engine to re-send the cue list (e.g. after a missed packet).
#[tauri::command]
fn refresh(client: State<Shared>) -> Result<(), String> {
    client.send("/lsp/cuelist", vec![])
}

fn arg_i(a: Option<&OscType>) -> i32 {
    match a {
        Some(OscType::Int(i)) => *i,
        Some(OscType::Float(f)) => *f as i32,
        _ => 0,
    }
}

fn arg_f(a: Option<&OscType>) -> f32 {
    match a {
        Some(OscType::Float(f)) => *f,
        Some(OscType::Double(f)) => *f as f32,
        Some(OscType::Int(i)) => *i as f32,
        _ => 0.0,
    }
}

fn arg_s(a: Option<&OscType>) -> String {
    match a {
        Some(OscType::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn relay(app: &AppHandle, pkt: OscPacket) {
    let m = match pkt {
        OscPacket::Message(m) => m,
        OscPacket::Bundle(b) => {
            b.content.into_iter().for_each(|p| relay(app, p));
            return;
        }
    };
    let a = &m.args;
    let r = match m.addr.as_str() {
        "/lsp/show" => app.emit(
            "lsp:show",
            ShowInfo {
                name: arg_s(a.first()),
                count: arg_i(a.get(1)),
                path: arg_s(a.get(2)),
                rev: arg_i(a.get(3)),
            },
        ),
        "/lsp/cue" => app.emit(
            "lsp:cue",
            CueInfo {
                idx: arg_i(a.first()),
                number: arg_s(a.get(1)),
                name: arg_s(a.get(2)),
                kind: arg_s(a.get(3)),
                duration: arg_f(a.get(4)),
                next: arg_s(a.get(5)),
                pre_wait: arg_f(a.get(6)),
                post_wait: arg_f(a.get(7)),
                problem: arg_s(a.get(8)),
                second_trigger: arg_s(a.get(9)),
                data: serde_json::from_str(&arg_s(a.get(10))).unwrap_or_default(),
            },
        ),
        "/lsp/meter" => app.emit(
            "lsp:meter",
            MeterInfo {
                peak: [arg_f(a.first()), arg_f(a.get(1))],
                clips: arg_i(a.get(2)),
                load: arg_f(a.get(3)),
                subs: arg_i(a.get(4)),
            },
        ),
        "/lsp/timecode" => app.emit(
            "lsp:timecode",
            TimecodeInfo {
                source: arg_s(a.first()),
                status: arg_s(a.get(1)),
                tc: arg_s(a.get(2)),
                rate: arg_s(a.get(3)),
                message: arg_s(a.get(4)),
            },
        ),
        "/lsp/engine" => app.emit(
            "lsp:engine",
            EngineInfo {
                device: arg_s(a.first()),
                rate: arg_i(a.get(1)),
                channels: arg_i(a.get(2)),
                max_voices: arg_i(a.get(3)),
                watch: arg_i(a.get(4)) != 0,
            },
        ),
        "/lsp/edit/reply" => app.emit(
            "lsp:edit",
            serde_json::from_str::<serde_json::Value>(&arg_s(a.first())).unwrap_or_default(),
        ),
        "/lsp/load" => app.emit(
            "lsp:load",
            LoadInfo {
                status: arg_s(a.first()),
                path: arg_s(a.get(1)),
                message: arg_s(a.get(2)),
            },
        ),
        "/lsp/state" => app.emit(
            "lsp:state",
            StateInfo {
                standby: arg_i(a.first()),
                running: a[1.min(a.len())..]
                    .chunks_exact(4)
                    .map(|c| Running {
                        idx: arg_i(c.first()),
                        phase: arg_s(c.get(1)),
                        elapsed: arg_f(c.get(2)),
                        duration: arg_f(c.get(3)),
                    })
                    .collect(),
            },
        ),
        other => {
            log::debug!("ignored {other}");
            Ok(())
        }
    };
    if let Err(e) = r {
        log::warn!("emit: {e}");
    }
}

fn engine_arg() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--engine" {
            if let Some(v) = args.next() {
                return v;
            }
        } else if let Some(v) = a.strip_prefix("--engine=") {
            return v.to_string();
        }
    }
    std::env::var("LSP_ENGINE").unwrap_or_else(|_| "127.0.0.1:53000".into())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let engine = resolve(&engine_arg()).unwrap_or_else(|e| {
        log::error!("engine address {e}; using 127.0.0.1:53000");
        "127.0.0.1:53000".parse().unwrap()
    });
    let sock = UdpSocket::bind("0.0.0.0:0").expect("bind UDP socket");
    log::info!(
        "engine {engine}, feedback on udp/{}",
        sock.local_addr().map(|a| a.port()).unwrap_or(0)
    );
    let client: Shared = Arc::new(Client {
        sock,
        engine: Mutex::new(engine),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(client.clone())
        .invoke_handler(tauri::generate_handler![
            command, get_engine, set_engine, refresh, open_show, edit, new_show, pick_media
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let rx = client.sock.try_clone()?;
            std::thread::Builder::new()
                .name("osc-rx".into())
                .spawn(move || {
                    // Cue data can exceed one Ethernet MTU.
                    let mut buf = vec![0u8; 65_535];
                    loop {
                        match rx.recv_from(&mut buf) {
                            Ok((n, _)) => match rosc::decoder::decode_udp(&buf[..n]) {
                                Ok((_, pkt)) => relay(&handle, pkt),
                                Err(e) => log::warn!("decode: {e:?}"),
                            },
                            // ICMP port-unreachable surfaces here on some OSes while
                            // the engine is down; keep listening.
                            Err(e) => {
                                log::debug!("recv: {e}");
                                std::thread::sleep(Duration::from_millis(100));
                            }
                        }
                    }
                })?;

            let hb = app.state::<Shared>().inner().clone();
            std::thread::Builder::new()
                .name("osc-heartbeat".into())
                .spawn(move || loop {
                    if let Err(e) = hb.send("/lsp/subscribe", vec![]) {
                        log::debug!("subscribe: {e}");
                    }
                    std::thread::sleep(HEARTBEAT);
                })?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
