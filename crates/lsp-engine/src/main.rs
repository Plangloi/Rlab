mod controller;
mod feedback;
mod midi;
mod osc;

use anyhow::{Context, Result};
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
    /// Show file (.toml)
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

    let path = args.show.context("show file required (see --help)")?;
    let show = Show::load(&path)?;
    let base = path.parent().map(PathBuf::from).unwrap_or_default();
    log::info!("show '{}' — {} cues", show.name, show.cues.len());

    let audio = AudioHandle::start(AudioConfig {
        host: args.host,
        device: args.device.or_else(|| show.settings.audio_device.clone()),
    })?;

    let (tx, rx) = crossbeam_channel::unbounded::<Command>();
    let feedback_sock = osc::spawn_server(
        args.osc.as_deref().unwrap_or(&show.settings.osc_listen),
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
                    "console: <enter>=GO | stop [n] | panic | goto n | next | prev | status | quit"
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

    let mut ctl = controller::Controller::new(show, &base, audio, feedback_sock)?;
    ctl.run(rx);
    Ok(())
}
