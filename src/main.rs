#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use anyhow::{Context, Result, anyhow, bail};
use create_sticker_video_tele::probe;
use create_sticker_video_tele::sticker::{self, Background, CropSquare, Segment, StickerParams};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

const USAGE: &str = "\
Create Telegram video stickers (WEBM/VP9, 1:1, <=512px, <=3s, <=256KB)

GUI:
  create-sticker-video-tele [video]

CLI:
  create-sticker-video-tele -i <video> [-o out.webm] [options]
    --seg <start>-<end>   Segment in seconds; repeat to concatenate several. Default 0-3
    --crop <x>,<y>,<size> Square crop in px (x/y may be negative to extend past the video).
                          --crop full: whole video, padded with background. Default: largest centered square
    --bg <color>          Background for area outside the video: black (default), white, transparent, RRGGBB
    --size <px>           Output side (<=512, emoji: 100). Default 512
    --fps <n>             FPS (<=30). Default: source FPS, capped at 30
    --speed <x>           Speed factor. Default 1
    --fit                 Raise speed so the output fits in 3s
    --crf <n>             Initial CRF (0-63). Default 30
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        attach_console();
        println!("{USAGE}");
        return Ok(());
    }
    if args.iter().any(|a| a == "-i") {
        attach_console();
        return run_cli(&args).map_err(|e| {
            eprintln!("Error: {e:#}");
            e
        });
    }
    let initial = args.first().map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_drag_and_drop(true)
            .with_title("Telegram Video Sticker Maker"),
        ..Default::default()
    };
    eframe::run_native(
        "create-sticker-video-tele",
        options,
        Box::new(move |cc| Ok(Box::new(app::StickerApp::new(cc, initial)))),
    )
    .map_err(|e| anyhow!("GUI error: {e}"))
}

/// Bản release chạy ở chế độ windows subsystem: gắn vào console cha để in được ra terminal.
fn attach_console() {
    #[cfg(all(windows, not(debug_assertions)))]
    unsafe {
        unsafe extern "system" {
            fn AttachConsole(pid: u32) -> i32;
        }
        AttachConsole(u32::MAX);
    }
}

enum CropArg {
    Full,
    Square(CropSquare),
}

/// `black`, `white`, `transparent` hoặc mã hex `RRGGBB` / `#RRGGBB`.
fn parse_bg(s: &str) -> Result<Background> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "black" => Background::Color([0, 0, 0]),
        "white" => Background::Color([255, 255, 255]),
        "transparent" | "none" => Background::Transparent,
        hex => {
            let hex = hex.trim_start_matches('#');
            if hex.len() != 6 {
                bail!("--bg expects black, white, transparent or RRGGBB (got: {s})");
            }
            let byte = |i: usize| {
                u8::from_str_radix(&hex[i..i + 2], 16)
                    .with_context(|| format!("--bg: invalid color code {s}"))
            };
            Background::Color([byte(0)?, byte(2)?, byte(4)?])
        }
    })
}

fn run_cli(args: &[String]) -> Result<()> {
    let mut input = None;
    let mut output = None;
    let mut segs = Vec::new();
    let mut crop = None;
    let mut size = None;
    let mut fps = None;
    let mut speed = None;
    let mut crf = None;
    let mut fit = false;
    let mut bg = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| anyhow!("Missing value for {a}"));
        match a.as_str() {
            "-i" => input = Some(PathBuf::from(val()?)),
            "-o" => output = Some(PathBuf::from(val()?)),
            "--seg" => {
                let v = val()?;
                let (s, e) = v
                    .split_once('-')
                    .ok_or_else(|| anyhow!("--seg expects start-end, e.g. 1.5-3"))?;
                segs.push(Segment::new(
                    s.trim().parse().context("--seg")?,
                    e.trim().parse().context("--seg")?,
                ));
            }
            "--crop" => {
                let v = val()?;
                if v == "full" {
                    crop = Some(CropArg::Full);
                    continue;
                }
                let n: Vec<i64> = v
                    .split(',')
                    .map(|x| x.trim().parse())
                    .collect::<Result<_, _>>()
                    .context("--crop")?;
                if n.len() != 3 || n[2] <= 0 {
                    bail!("--crop expects x,y,size (size > 0) or full");
                }
                crop = Some(CropArg::Square(CropSquare {
                    x: i32::try_from(n[0]).context("--crop x")?,
                    y: i32::try_from(n[1]).context("--crop y")?,
                    size: u32::try_from(n[2]).context("--crop size")?,
                }));
            }
            "--bg" => bg = Some(parse_bg(val()?)?),
            "--size" => size = Some(val()?.parse().context("--size")?),
            "--fps" => fps = Some(val()?.parse().context("--fps")?),
            "--speed" => speed = Some(val()?.parse().context("--speed")?),
            "--crf" => crf = Some(val()?.parse().context("--crf")?),
            "--fit" => fit = true,
            other => bail!("Unknown argument: {other}\n\n{USAGE}"),
        }
    }
    let input = input.ok_or_else(|| anyhow!("Missing -i <video>"))?;
    let info = probe::probe(&input)?;
    let output = output.unwrap_or_else(|| {
        let stem = input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        input.with_file_name(format!("{stem}_sticker.webm"))
    });
    let mut p = StickerParams::from_info(&info, output);
    if !segs.is_empty() {
        p.segments = segs;
    }
    match crop {
        Some(CropArg::Full) => p.crop = CropSquare::fit_all(info.width, info.height),
        Some(CropArg::Square(c)) => p.crop = c,
        None => {}
    }
    if let Some(b) = bg {
        p.background = b;
    }
    if let Some(v) = size {
        p.out_size = v;
    }
    if let Some(v) = fps {
        p.fps = v;
    }
    if let Some(v) = speed {
        p.speed = v;
    }
    if let Some(v) = crf {
        p.crf = v;
    }
    if fit {
        p.speed = p.speed_to_fit();
    }
    let cancel = AtomicBool::new(false);
    let mut last = (0, -1i32);
    let r = sticker::encode(&p, &info, &cancel, |pr| {
        let pct = (pr.fraction * 100.0) as i32 / 10 * 10;
        if (pr.attempt, pct) != last {
            last = (pr.attempt, pct);
            eprintln!("  attempt {} — {pct}%", pr.attempt);
        }
    })?;
    println!(
        "Exported {} — {}x{}, {:.3}s, {:.1} KB ({} kbps, CRF {}, {} encode pass(es))",
        r.output.display(),
        r.width,
        r.height,
        r.duration,
        r.bytes as f64 / 1024.0,
        r.bitrate_kbps,
        r.crf,
        r.attempts
    );
    Ok(())
}
