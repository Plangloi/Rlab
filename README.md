# showplayer-rs

Cue-based show playback engine for macOS + Linux, modelled on [Linux Show Player](https://www.linux-show-player.org/).
Headless engine first — every UI (Tauri, egui, TouchOSC, Q-SYS, Stream Deck) is just an OSC client.

## Layout

```
crates/
  lsp-core/    show model (TOML), cue types, Command enum — no I/O
  lsp-audio/   cpal output thread, lock-free mixer (rtrb SPSC), symphonia decode + SRC
  lsp-engine/  `lsp` binary: sequencer (timer heap), OSC server/client, MIDI out, stdin console
shows/         demo.toml + generated test tones
```

## Build / run

```bash
# Linux deps
sudo apt install libasound2-dev pkg-config        # + libjack-jackd2-dev for --features jack

cargo build --release
./target/release/lsp --list-devices
./target/release/lsp shows/demo.toml               # <enter> = GO
./target/release/lsp shows/demo.toml --host JACK --device system   # needs --features jack
```

## OSC API (default udp/53000)

| Address | Args | Action |
|---|---|---|
| `/lsp/go` | – | fire standby cue, advance playhead |
| `/lsp/stop` | `[cue]` | stop cue (uses its `fade_out`) or all |
| `/lsp/panic` | – | clear timers, fade all (`panic_fade`) |
| `/lsp/goto` | `cue` | set standby |
| `/lsp/next` / `/lsp/prev` | – | move playhead |
| `/lsp/status` | – | log standby + running |

```bash
oscsend localhost 53000 /lsp/go          # liblo-tools
```

## Cue types

`audio` · `fade` · `stop` · `osc` · `midi` (raw bytes → MSC/notes/PC) · `wait`
Chaining: `next = "auto_continue"` (fires next after `post_wait` from start) | `"auto_follow"` (fires on natural end). `pre_wait` on any cue.
See `shows/demo.toml`.

## RT design notes

- Audio callback: no locks, no alloc. Voices preallocated (128). Finished clips are returned through the event ring so the final `Arc` drop happens on the control thread.
- All audio is decoded + resampled to device rate at show load (preload), so GO latency is one callback period.
- cpal `Stream` is `!Send` on macOS → owned by a dedicated thread, parked until shutdown.
- Linear SRC: adequate for cues; swap in `rubato` for higher quality.

## Roadmap

- [ ] OSC feedback (`/lsp/state` push to subscribers) for remote UIs
- [ ] Multichannel routing (cue → output bus matrix), per-cue patch
- [ ] LTC / MTC chase + timecode cues
- [ ] Hot reload of show file, file watcher
- [ ] Tauri UI (cue list, GO, meters) talking OSC/WebSocket to engine
- [ ] Group cues, playlist mode, cue edit over OSC
- [ ] Streaming decode for long files (> RAM budget)
