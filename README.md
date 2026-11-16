# create-sticker-video-tele

A Rust desktop app (egui) that turns any video into a **Telegram video sticker**, following the official [WEBM/VP9 requirements](https://core.telegram.org/stickers/webm-vp9-encoding).

| Telegram requirement | How the app handles it |
| --- | --- |
| WEBM container, VP9 codec | Encodes with `libvpx-vp9` into `.webm` |
| No audio stream | Audio is always dropped (`-an`) |
| Sticker: one side 512 px / emoji: 100×100 | Always square (1:1); 512×512 by default, Emoji 100 mode available |
| Duration ≤ 3 s | Enforced in both the UI and the encoder; frame count = ⌊duration × FPS⌋, so it never exceeds 3 s |
| ≤ 30 FPS | Defaults to the source FPS, capped at 30 |
| File size ≤ 256 KB | Bitrate is derived from duration; if the file is still too large it re-encodes at a lower bitrate (up to 6 attempts) |

Transparent sources (VP8/VP9 WEBM with alpha) keep their alpha channel. The background added around the video can also be transparent.

> 60 FPS is **not** allowed for video stickers. Telegram allows 60 FPS only for vector `.TGS` stickers, which cannot be made from real footage.

---

## 1. Requirements

| Tool | Why | Install (Windows) |
| --- | --- | --- |
| **ffmpeg + ffprobe** with `libvpx-vp9` | decoding, preview, encoding | `winget install Gyan.FFmpeg` |
| **Rust** (stable) | building the app | `winget install Rustlang.Rustup` |
| **MSVC C++ Build Tools** | Rust linker on Windows | Visual Studio Build Tools → "Desktop development with C++" |

`ffmpeg`/`ffprobe` must be on `PATH`, or placed next to the app executable.

Check:

```powershell
ffmpeg -hide_banner -encoders | findstr vp9   # should list libvpx-vp9
cargo --version
```

> **`cargo` not recognized after installing Rust?** The terminal still has the old `PATH`.
> Quit every VS Code / terminal window (check Task Manager for leftover `Code.exe`) and reopen,
> or fix the current terminal only:
>
> ```powershell
> $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
> ```

## 2. Run

From the project folder:

```powershell
# Development (debug build, opens a console window with logs)

cargo run

# Open a video directly

cargo run -- "D:\videos\clip.mp4"

# Release build (faster UI, no console window)

cargo build --release
.\target\release\create-sticker-video-tele.exe
```

Optional auto-rebuild while editing code:

```powershell
cargo install cargo-watch
cargo watch -x run
```

## 3. Using the app

### 3.1 Open a video

Click **📂 Open video…** or drag & drop a file onto the window. The top bar shows resolution, duration, FPS, codec, and whether audio will be dropped.

### 3.2 Watch the original video

- **▶ Play original** or **Space**: plays the whole source from the red playhead at 1× and source FPS (up to 60).
- Click/drag on the timeline while playing to seek; playback continues from the new position.

### 3.3 Choose the time range (timeline)

- Drag the **two blue edges** to set start/end; drag the middle to move the whole range; click the bar to move the red playhead.
- **[ Set start** / **Set end ]** set start/end at the playhead. **⏮ ⏭** step one frame.
- The range is capped at **3 s of source video**.
- **▶ Play selection** loops the range exactly as it will be exported (output FPS and speed).

**Concatenate several clips:** click **+ Add to join list** to store the current range as a segment, then pick the next range. Segments are joined in order (reorder with ◀ ▶, remove with ✕, **Clear all** to reset). The total is still capped at 3 s. If the list is empty, the current range is exported.

### 3.4 Choose the crop area (on the video)

- Drag the blue square to move it, drag a corner to resize, scroll the mouse wheel to zoom, or drag outside the square to draw a new one.
- Or type **X / Y / Size** in the right panel. **Max, centered** = largest centered square; **Center** = center the current square.

**Extend beyond the video (e.g. keep a full 9:16 video):**

- Tick **Extend beyond video (add background)**, or click **Fit whole video** — the square becomes as large as the long side and the empty area is filled with background.
- **Background**: black by default; pick any color, or tick **Transparent** to export with an alpha channel.

### 3.5 Output settings

- **Output size**: **Sticker 512** (required for stickers), **Emoji 100**, or **Custom** (≤ 512).
- **FPS**: 1–30.
- **Speed**: > 1× plays the selected range faster (shorter output); < 1× slows it down (the range shrinks so the output stays ≤ 3 s). **Reset speed 1x** resets it.
- **CRF**: lower = better quality / bigger file. The app lowers bitrate automatically if the file exceeds 256 KB.
- **Output file**: output path (default `<video name>_sticker.webm` next to the source); **Browse…** to change it.

### 3.6 Export

Click **💾 Export sticker**. A progress bar is shown (with the attempt number if a re-encode is needed; **Cancel** stops it). When done, the panel shows size/duration/bitrate and loops a preview of the result; **Open folder** opens the output folder.

### 3.7 Upload to Telegram

1. Open [@Stickers](https://t.me/Stickers) → `/newvideo` (or `/newemojipack` for emoji, exported with **Emoji 100**).
2. Send the `.webm` **as a file** (drag & drop in Telegram Desktop), not as a video, so Telegram does not re-compress it.
3. Pick an emoji for the sticker, repeat for more stickers, then `/publish`.

## 4. Command line

The same encoder is available without the GUI (`-i` switches to CLI mode):

```powershell
create-sticker-video-tele -i in.mp4 -o out.webm --seg 1-2.5 --seg 5-7 --fit --crop 400,100,800
create-sticker-video-tele -i portrait.mov --crop full --bg transparent
```

| Option | Meaning |
| --- | --- |
| `--seg <start>-<end>` | Segment in seconds; repeat to concatenate. Default `0-3` |
| `--crop <x>,<y>,<size>` | Square crop in px; x/y may be negative to extend past the video |
| `--crop full` | Whole video in a square, padded with background |
| `--bg <color>` | `black` (default), `white`, `transparent`, or `RRGGBB` |
| `--size <px>` | Output side, ≤ 512 (emoji: 100). Default 512 |
| `--fps <n>` | ≤ 30. Default: source FPS capped at 30 |
| `--speed <x>` | Speed factor. Default 1 |
| `--fit` | Raise speed so the output fits in 3 s |
| `--crf <n>` | Initial CRF (0–63). Default 30 |

With `cargo`: `cargo run -- -i in.mp4 --crop full`. Run `--help` for the full list.

## 5. Tests

```powershell
cargo test
```

Unit tests cover the Telegram constraints, crop/pad math and timeline logic. Integration tests (`tests/encode.rs`) generate synthetic videos with ffmpeg, encode them for real, and verify the output with ffprobe: VP9, no audio, exact size, ≤ 3 s, ≤ 30 FPS, ≤ 256 KB, frame count, background/alpha pixels. They need `ffmpeg` on `PATH` and take ~40 s.

## 6. Project layout

| Path | Purpose |
| --- | --- |
| `src/sticker.rs` | Parameters, validation, ffmpeg filter/args, encode loop with size retry |
| `src/ffmpeg.rs` | Locating/running ffmpeg, frame grabbing, range decoding, frame streaming |
| `src/probe.rs` | ffprobe parsing (rotation, FPS, audio, alpha) |
| `src/app.rs` | egui GUI |
| `src/main.rs` | Entry point, CLI |
| `tests/encode.rs` | End-to-end encode tests |
