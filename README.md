# showplayer-rs

Cue-based show playback engine for macOS + Linux, modelled on [Linux Show Player](https://www.linux-show-player.org/).
Headless engine first — every UI (Tauri, egui, TouchOSC, Q-SYS, Stream Deck) is just an OSC client.

## Layout

```
crates/
  lsp-core/    show model (TOML), cue types, Command enum — no I/O
  lsp-audio/   cpal output thread, lock-free mixer (rtrb SPSC), symphonia decode + SRC
  lsp-engine/  `lsp` binary: sequencer (timer heap), OSC server/client + feedback, MIDI out, stdin console
ui/            Tauri operator UI (separate workspace) — an OSC client of the engine
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
| `/lsp/subscribe` | `[reply_port]` | receive feedback (renew every <10 s) |
| `/lsp/unsubscribe` | `[reply_port]` | stop feedback |
| `/lsp/cuelist` | `[reply_port]` | re-send the cue list to the caller |

```bash
oscsend localhost 53000 /lsp/go          # liblo-tools
```

### Feedback

Subscribers get packets from the engine's OSC port, sent to the subscriber's source address (or `reply_port` on the same host, handy for TouchOSC). The cue list is sent on first subscribe; state is pushed at 20 Hz while anything runs and 1 Hz when idle (doubles as a heartbeat).

| Address | Args |
|---|---|
| `/lsp/show` | `name:s count:i` |
| `/lsp/cue` | `idx:i number:s name:s type:s duration:f next:s pre_wait:f post_wait:f` |
| `/lsp/state` | `standby:i` then `[idx:i phase:s elapsed:f duration:f]` per active cue |

`standby` = -1 at end of list. `duration` = -1 for looping audio. `phase` = `pre_wait` · `running` · `stopping`.

## Operator UI (Tauri)

```bash
./target/release/lsp shows/demo.toml               # engine, in one terminal
cd ui/src-tauri && cargo run --release              # UI; --engine host:port or LSP_ENGINE to target a remote engine
```

Cue list with live progress, standby, GO, stop all / panic, per-cue stop, click to set standby. Keys: Space GO · Esc stop all · ↑↓ move standby. Front end is static HTML/JS in `ui/dist` (no npm). For a `.app` bundle: `cargo install tauri-cli --version '^2' && cd ui/src-tauri && cargo tauri build`.

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

- [x] OSC feedback (`/lsp/state` push to subscribers) for remote UIs
- [ ] Multichannel routing (cue → output bus matrix), per-cue patch
- [ ] LTC / MTC chase + timecode cues
- [ ] Hot reload of show file, file watcher
- [x] Tauri UI: cue list, GO, progress, stop/panic over OSC
- [ ] UI: level meters, cue editing, show file open
- [ ] Group cues, playlist mode, cue edit over OSC
- [ ] Streaming decode for long files (> RAM budget)
