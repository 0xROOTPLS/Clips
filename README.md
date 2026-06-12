<div align="center">

# 🔴 clips

**Instant replay for Windows, engineered for zero overhead.**

A single 500 KB exe that buffers your screen to RAM around the clock at ~0.6 % of one CPU core.
Press <kbd>Alt</kbd>+<kbd>C</kbd> and the last 15/30/60 seconds hit disk as an MP4 in ~130 ms.

</div>

---

## Why

- **No game impact.** Frames never leave the GPU: capture, color conversion, and scaling are all GPU passes, and encoding runs on the GPU's dedicated video silicon, not your cores or the 3D engine.
- **Instant saves.** The buffer is already HEVC-compressed, so saving is a remux, not a re-encode. A 30 s clip writes in about 130 ms.
- **No bloat.** One portable exe. No installer, no service, no account, no overlay, no GPU vendor suite.
- **Never dies.** A supervisor restarts capture/audio legs on device loss, monitor changes, audio device swaps, and panics. Built in Rust.

## Use

1. Run `clips.exe`. It sits in the tray.
2. Something cool happens? Press <kbd>Alt</kbd>+<kbd>C</kbd>.
3. The clip lands in `Videos\Clips`, with a chime on success.

Everything else is in the tray menu:

| Option | Choices |
|---|---|
| Clip length | 15 / 30 / 60 s |
| Resolution | Native / 1440p / 1080p / 720p |
| Quality | High / Medium / Low (3.1 / 1.9 / 1.0 MB/s) |
| Microphone | Off / Default / specific device, mixed into the system-audio track |
| Monitor | Primary or any attached display |
| Capture cursor | On / Off |
| Start with Windows | Registers the exe's current location, so keep it somewhere permanent |

## How it works

```
Windows Graphics Capture -> GPU BGRA-to-NV12 -> hardware HEVC -> ring buffer (RAM)
WASAPI loopback + mic    -> AAC              -> ring buffer        | Alt+C
                                                                   v
                                              pass-through mux  -> .mp4
```

Video and audio share the QPC clock, so sync is exact with no resampling or drift correction. The ring holds encoded packets only (~140 MB for 60 s at default quality, hard-capped at 400 MB).

On **Windows 10**, where the capture API's yellow screen border can't be disabled, clips automatically falls back to DXGI Desktop Duplication: no border, same pipeline. Only caveat: the mouse cursor isn't captured there.

## Requirements

- Windows 10 / 11
- GPU with a hardware HEVC encoder (any non-ancient AMD / NVIDIA / Intel)

## Build

```
cargo build --release
```

Statically linked CRT, so the resulting `target/release/clips.exe` runs anywhere as-is.

Config lives at `%APPDATA%\InstantReplay\config.cfg`, logs next to it. Hidden keys there: `fps`, `gop_seconds`, `backend=auto|wgc|dxgi`, custom hotkey.

## Tests

```
cargo test                                   # ring buffer, config, mux unit tests
cargo run --release -- --record-test 8      # full pipeline: record, save, validate the MP4
```
