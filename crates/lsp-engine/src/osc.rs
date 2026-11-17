//! OSC control surface (UDP) + OSC cue output.
//!
//! In:  /lsp/go  /lsp/stop [cue]  /lsp/panic  /lsp/goto <cue>  /lsp/next  /lsp/prev  /lsp/status
//! Bundles are flattened; timetags ignored (execute on receipt).

use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use lsp_core::{Command, OscArg};
use rosc::{OscMessage, OscPacket, OscType};
use std::net::UdpSocket;

pub fn spawn_server(addr: &str, tx: Sender<Command>) -> Result<()> {
    let sock = UdpSocket::bind(addr).with_context(|| format!("OSC bind {addr}"))?;
    log::info!("OSC listening on udp://{}", sock.local_addr()?);
    std::thread::Builder::new()
        .name("lsp-osc".into())
        .spawn(move || {
            let mut buf = [0u8; rosc::decoder::MTU];
            loop {
                match sock.recv_from(&mut buf) {
                    Ok((n, src)) => match rosc::decoder::decode_udp(&buf[..n]) {
                        Ok((_, pkt)) => dispatch(pkt, &tx),
                        Err(e) => log::warn!("OSC decode from {src}: {e:?}"),
                    },
                    Err(e) => log::warn!("OSC recv: {e}"),
                }
            }
        })?;
    Ok(())
}

fn dispatch(pkt: OscPacket, tx: &Sender<Command>) {
    match pkt {
        OscPacket::Message(m) => {
            if let Some(cmd) = to_command(&m) {
                log::debug!("OSC {} -> {cmd:?}", m.addr);
                let _ = tx.send(cmd);
            } else {
                log::warn!("OSC unhandled {} {:?}", m.addr, m.args);
            }
        }
        OscPacket::Bundle(b) => b.content.into_iter().for_each(|p| dispatch(p, tx)),
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

pub fn to_command(m: &OscMessage) -> Option<Command> {
    let a0 = m.args.first().and_then(arg_str);
    Some(match m.addr.as_str() {
        "/lsp/go" => Command::Go,
        "/lsp/stop" => Command::Stop(a0),
        "/lsp/panic" => Command::Panic,
        "/lsp/goto" => Command::Goto(a0?),
        "/lsp/next" => Command::Next,
        "/lsp/prev" => Command::Prev,
        "/lsp/status" => Command::Status,
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

    #[test]
    fn map_messages() {
        let m = OscMessage {
            addr: "/lsp/goto".into(),
            args: vec![OscType::Float(1.5)],
        };
        assert_eq!(to_command(&m), Some(Command::Goto("1.5".into())));
        let m = OscMessage {
            addr: "/lsp/stop".into(),
            args: vec![],
        };
        assert_eq!(to_command(&m), Some(Command::Stop(None)));
        let m = OscMessage {
            addr: "/lsp/goto".into(),
            args: vec![],
        };
        assert_eq!(to_command(&m), None);
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
