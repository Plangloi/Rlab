//! MIDI out for MIDI / MSC cues. Ports opened lazily, matched by substring, cached.

use anyhow::{anyhow, Result};
use midir::{MidiOutput, MidiOutputConnection};
use std::collections::HashMap;

#[derive(Default)]
pub struct MidiOut {
    conns: HashMap<String, MidiOutputConnection>,
}

impl MidiOut {
    pub fn send(&mut self, port: &str, bytes: &[u8]) -> Result<()> {
        if !self.conns.contains_key(port) {
            let out = MidiOutput::new("lsp-rs")?;
            let ports = out.ports();
            let p = ports
                .iter()
                .find(|p| out.port_name(p).map(|n| n.contains(port)).unwrap_or(false))
                .cloned()
                .ok_or_else(|| anyhow!("MIDI port matching '{port}' not found"))?;
            let conn = out
                .connect(&p, "lsp-rs-out")
                .map_err(|e| anyhow!("MIDI connect: {e}"))?;
            self.conns.insert(port.to_string(), conn);
        }
        self.conns.get_mut(port).expect("inserted").send(bytes)?;
        Ok(())
    }
}

pub fn list_ports() -> Result<()> {
    let out = MidiOutput::new("lsp-rs-list")?;
    println!("MIDI out:");
    for p in out.ports() {
        println!("  {}", out.port_name(&p).unwrap_or_else(|_| "?".into()));
    }
    Ok(())
}
