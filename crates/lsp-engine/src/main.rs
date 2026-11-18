mod controller;
mod feedback;
mod loader;
mod midi;
mod osc;

use anyhow::Result;
use clap::Parser;
use lsp_audio::{AudioConfig, AudioHandle};
use lsp_core::{Command, Show};
use std::io::BufRead;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "lsp",
    version,
    about = "Headless cue engine (Linux Show Player-style), OSC-controlled"
)]
struct Args {
    /// Show file (.toml). Optional: without it the engine waits for /lsp/load.
    show: Option<PathBuf>,
    /// List audio hosts/devices and MIDI ports, then exit
    #[arg(long)]
    list_devices: bool,
    /// Output device (substring match); overrides show setting
    #[arg(long)]
    device: Option<String>,
    /// cpal host: ALSA | JACK | CoreAudio
    #[arg(long)]
    host: Option<String>,
    /// OSC bind address; overrides show setting
    #[arg(long)]
    osc: Option<String>,
    /// Disable stdin console (for daemon / systemd use)
    #[arg(long)]
    no_stdin: bool,
    /// Don't reload the show when its file changes on disk
    #[arg(long)]
    no_watch: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    let args = Args::parse();

    if args.list_devices {
        lsp_audio::list_devices()?;
        midi::list_ports()?;
        return Ok(());
    }

    // Settings (OSC port, audio device) come from the initial show; the full
    // load (with decoding) needs the device sample rate, so it happens after.
    let settings = match &args.show {
        Some(p) => Show::load(p)?.settings,
        None => Show::empty().settings,
    };

    let audio = AudioHandle::start(AudioConfig {
        host: args.host,
        device: args.device.or_else(|| settings.audio_device.clone()),
    })?;

    let (tx, rx) = crossbeam_channel::unbounded::<Command>();
    let feedback_sock = osc::spawn_server(
        args.osc.as_deref().unwrap_or(&settings.osc_listen),
        tx.clone(),
    )?;

    {
        let tx = tx.clone();
        ctrlc::set_handler(move || {
            let _ = tx.send(Command::Quit);
        })?;
    }

    if !args.no_stdin {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("lsp-stdin".into())
            .spawn(move || {
                eprintln!(
                    "console: <enter>=GO | stop [n] | panic | goto n | next | prev | status | load <path> | reload | quit"
                );
                for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                    match Command::parse(&line) {
                        Some(c) => {
                            if tx.send(c).is_err() {
                                break;
                            }
                        }
                        None => eprintln!("? {line}"),
                    }
                }
            })?;
    }
    drop(tx);

    // Initial load is synchronous so a broken show file fails at startup.
    let loaded = match &args.show {
        Some(p) => Some(loader::load(p, audio.sample_rate, &Default::default())?),
        None => None,
    };
    let mut ctl = controller::Controller::new(loaded, audio, feedback_sock, !args.no_watch)?;
    ctl.run(rx);
    Ok(())
}
