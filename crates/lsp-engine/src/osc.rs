//! OSC control surface (UDP) + OSC cue output.
//!
//! In:  /lsp/go  /lsp/stop [cue]  /lsp/hardstop [cue]  /lsp/pause [cue]  /lsp/resume [cue]
//!      /lsp/panic  /lsp/goto <cue>  /lsp/next  /lsp/prev  /lsp/status
//!      /lsp/subscribe [reply_port]  /lsp/unsubscribe [reply_port]  /lsp/cuelist [reply_port]
//!      /lsp/load <path>  /lsp/reload
//! Feedback (see `feedback`) is sent from the same socket.
//! Bundles are flattened; timetags ignored (execute on receipt).

use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use lsp_core::{Command, OscArg};
use rosc::{OscMessage, OscPacket, OscType};
use std::net::{SocketAddr, UdpSocket};

/// Binds the control socket and spawns the receive thread. Returns a clone of
/// the socket for sending feedback.
pub fn spawn_server(addr: &str, tx: Sender<Command>) -> Result<UdpSocket> {
    let sock = UdpSocket::bind(addr).with_context(|| format!("OSC bind {addr}"))?;
    log::info!("OSC listening on udp://{}", sock.local_addr()?);
    let out = sock.try_clone()?;
    std::thread::Builder::new()
        .name("lsp-osc".into())
        .spawn(move || {
            let mut buf = [0u8; rosc::decoder::MTU];
            loop {
                match sock.recv_from(&mut buf) {
                    Ok((n, src)) => match rosc::decoder::decode_udp(&buf[..n]) {
                        Ok((_, pkt)) => dispatch(pkt, src, &tx),
                        Err(e) => log::warn!("OSC decode from {src}: {e:?}"),
                    },
                    Err(e) => log::warn!("OSC recv: {e}"),
                }
            }
        })?;
    Ok(out)
}

fn dispatch(pkt: OscPacket, src: SocketAddr, tx: &Sender<Command>) {
    match pkt {
        OscPacket::Message(m) => {
            if let Some(cmd) = to_command(&m, src) {
                log::debug!("OSC {} -> {cmd:?}", m.addr);
                let _ = tx.send(cmd);
            } else {
                log::warn!("OSC unhandled {} {:?}", m.addr, m.args);
            }
        }
        OscPacket::Bundle(b) => b.content.into_iter().for_each(|p| dispatch(p, src, tx)),
    }
}

fn arg_str(a: &OscType) -> Option<String> {
    match a {
        OscType::String(s) => Some(s.clone()),
        OscType::Int(i) => Some(i.to_string()),
        OscType::Float(f) => Some(f.to_string()),
        OscType::Double(f) => Some(f.to_string()),
        _ => None,
    }
}

fn arg_port(a: &OscType) -> Option<u16> {
    match a {
        OscType::Int(i) => u16::try_from(*i).ok(),
        OscType::Float(f) => Some(*f as u16),
        OscType::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// `src` is the packet's sender; an optional first int arg overrides the reply port.
pub fn to_command(m: &OscMessage, src: SocketAddr) -> Option<Command> {
    let a0 = m.args.first().and_then(arg_str);
    let reply = || {
        let mut r = src;
        if let Some(p) = m.args.first().and_then(arg_port).filter(|p| *p != 0) {
            r.set_port(p);
        }
        r
    };
    Some(match m.addr.as_str() {
        "/lsp/go" => Command::Go,
        "/lsp/stop" => Command::Stop(a0),
        "/lsp/hardstop" => Command::HardStop(a0),
        "/lsp/pause" => Command::Pause(a0),
        "/lsp/resume" => Command::Resume(a0),
        "/lsp/panic" => Command::Panic,
        "/lsp/goto" => Command::Goto(a0?),
        "/lsp/next" => Command::Next,
        "/lsp/prev" => Command::Prev,
        "/lsp/status" => Command::Status,
        "/lsp/subscribe" => Command::Subscribe(reply()),
        "/lsp/unsubscribe" => Command::Unsubscribe(reply()),
        "/lsp/cuelist" => Command::CueList(reply()),
        "/lsp/load" => match m.args.first() {
            Some(OscType::String(p)) if !p.is_empty() => Command::Load(p.into()),
            _ => return None,
        },
        "/lsp/reload" => Command::Reload,
        _ => return None,
    })
}

pub fn encode(path: &str, args: &[OscArg]) -> Result<Vec<u8>> {
    let args = args
        .iter()
        .map(|a| match a {
            OscArg::Bool(b) => OscType::Bool(*b),
            OscArg::Int(i) => OscType::Int(*i),
            OscArg::Float(f) => OscType::Float(*f),
            OscArg::Str(s) => OscType::String(s.clone()),
        })
        .collect();
    Ok(rosc::encoder::encode(&OscPacket::Message(OscMessage {
        addr: path.into(),
        args,
    }))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: SocketAddr = SocketAddr::V4(std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::new(10, 0, 0, 5),
        40000,
    ));

    #[test]
    fn subscribe_reply_port() {
        let m = OscMessage {
            addr: "/lsp/subscribe".into(),
            args: vec![],
        };
        assert_eq!(to_command(&m, SRC), Some(Command::Subscribe(SRC)));
        let m = OscMessage {
            addr: "/lsp/subscribe".into(),
            args: vec![OscType::Int(9000)],
        };
        assert_eq!(
            to_command(&m, SRC),
            Some(Command::Subscribe("10.0.0.5:9000".parse().unwrap()))
        );
    }

    #[test]
    fn map_messages() {
        let m = OscMessage {
            addr: "/lsp/goto".into(),
            args: vec![OscType::Float(1.5)],
        };
        assert_eq!(to_command(&m, SRC), Some(Command::Goto("1.5".into())));
        let m = OscMessage {
            addr: "/lsp/stop".into(),
            args: vec![],
        };
        assert_eq!(to_command(&m, SRC), Some(Command::Stop(None)));
        let m = OscMessage {
            addr: "/lsp/goto".into(),
            args: vec![],
        };
        assert_eq!(to_command(&m, SRC), None);
    }

    #[test]
    fn roundtrip_encode() {
        let bytes = encode("/x", &[OscArg::Int(3), OscArg::Str("a".into())]).unwrap();
        let (_, pkt) = rosc::decoder::decode_udp(&bytes).unwrap();
        match pkt {
            OscPacket::Message(m) => {
                assert_eq!(m.args, vec![OscType::Int(3), OscType::String("a".into())])
            }
            _ => panic!(),
        }
    }
}
