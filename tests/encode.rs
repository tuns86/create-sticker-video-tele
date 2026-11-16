//! Test encode thật bằng ffmpeg: kiểm tra file đầu ra đúng yêu cầu Telegram.

use create_sticker_video_tele::ffmpeg;
use create_sticker_video_tele::probe;
use create_sticker_video_tele::sticker::{
    self, Background, CropSquare, MAX_BYTES, Segment, StickerParams,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

fn workdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join("sticker-tele-tests").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Video 1280x720, 60 fps, 6s, có audio, nhiều chi tiết (khó nén).
fn make_source(dir: &Path) -> PathBuf {
    let p = dir.join("src.mp4");
    let mut cmd = ffmpeg::command(&ffmpeg::ffmpeg().unwrap());
    cmd.args([
        "-y",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1280x720:rate=60:duration=6",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=6",
        "-vf",
        "noise=alls=40:allf=t",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-c:a",
        "aac",
        "-shortest",
    ])
    .arg(&p);
    ffmpeg::run_checked(cmd, "create source video").unwrap();
    p
}

fn ffprobe_json(path: &Path) -> Value {
    let mut cmd = ffmpeg::command(&ffmpeg::ffprobe().unwrap());
    cmd.args([
        "-v",
        "error",
        "-count_frames",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
    ])
    .arg(path);
    let out = ffmpeg::run_checked(cmd, "ffprobe").unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

fn assert_telegram_compliant(path: &Path, side: u64, max_fps: f64) -> Value {
    let j = ffprobe_json(path);
    let streams = j["streams"].as_array().unwrap();
    assert_eq!(
        streams.len(),
        1,
        "expected exactly 1 video stream, no audio: {streams:?}"
    );
    let v = &streams[0];
    assert_eq!(v["codec_type"], "video");
    assert_eq!(v["codec_name"], "vp9");
    assert_eq!(v["width"].as_u64().unwrap(), side);
    assert_eq!(v["height"].as_u64().unwrap(), side);
    assert!(
        j["format"]["format_name"]
            .as_str()
            .unwrap()
            .contains("webm")
    );
    let dur: f64 = j["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!(dur <= 3.0 + 1e-9, "duration {dur} > 3s");
    let (n, d) = v["avg_frame_rate"]
        .as_str()
        .unwrap()
        .split_once('/')
        .unwrap();
    let fps = n.parse::<f64>().unwrap() / d.parse::<f64>().unwrap();
    assert!(fps <= max_fps + 1e-6 && fps <= 30.0, "fps {fps}");
    let bytes = std::fs::metadata(path).unwrap().len();
    assert!(bytes <= MAX_BYTES, "size {bytes} > 256KB");
    j
}

#[test]
fn encodes_single_segment_with_crop() {
    let dir = workdir("single");
    let src = make_source(&dir);
    let info = probe::probe(&src).unwrap();
    assert_eq!((info.width, info.height), (1280, 720));
    assert!(info.has_audio);

    let mut p = StickerParams::from_info(&info, dir.join("out.webm"));
    assert_eq!(p.fps, 30, "60fps must be capped to 30");
    p.segments = vec![Segment::new(1.0, 4.0)];
    p.crop = CropSquare {
        x: 600,
        y: 100,
        size: 500,
    };
    let r = sticker::encode(&p, &info, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(r.bytes <= MAX_BYTES);
    assert!(!dir.join("out.part.webm").exists());
    let j = assert_telegram_compliant(&r.output, 512, 30.0);
    assert_eq!(j["streams"][0]["nb_read_frames"].as_str().unwrap(), "90");
}

#[test]
fn concatenates_segments_with_speed_and_emoji_size() {
    let dir = workdir("concat");
    let src = make_source(&dir);
    let info = probe::probe(&src).unwrap();

    let mut p = StickerParams::from_info(&info, dir.join("emoji.webm"));
    p.segments = vec![Segment::new(0.0, 2.0), Segment::new(4.0, 6.0)];
    assert!(p.validate(&info).is_err(), "4s must be rejected");
    p.speed = p.speed_to_fit();
    p.out_size = 100;
    p.fps = 24;
    let r = sticker::encode(&p, &info, &AtomicBool::new(false), |_| {}).unwrap();
    let j = assert_telegram_compliant(&r.output, 100, 24.0);
    assert_eq!(j["streams"][0]["nb_read_frames"].as_str().unwrap(), "72");
}

#[test]
fn rejects_over_3s_without_running_ffmpeg() {
    let dir = workdir("reject");
    let src = make_source(&dir);
    let info = probe::probe(&src).unwrap();
    let mut p = StickerParams::from_info(&info, dir.join("x.webm"));
    p.segments = vec![Segment::new(0.0, 3.5)];
    let err = sticker::encode(&p, &info, &AtomicBool::new(false), |_| {}).unwrap_err();
    assert!(err.to_string().contains("exceeds 3s"), "{err}");
    assert!(!dir.join("x.webm").exists());
}

#[test]
fn small_size_limit_forces_reencode() {
    let dir = workdir("shrink");
    let src = make_source(&dir);
    let info = probe::probe(&src).unwrap();
    let mut p = StickerParams::from_info(&info, dir.join("small.webm"));
    p.crf = 4; // chất lượng rất cao → lần đầu chắc chắn vượt giới hạn nhỏ
    p.max_bytes = 40 * 1024;
    let r = sticker::encode(&p, &info, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(r.bytes <= 40 * 1024, "{} bytes", r.bytes);
    assert!(
        r.attempts >= 2,
        "must re-encode at least once, attempts={}",
        r.attempts
    );
}

#[test]
fn extracts_range_for_smooth_playback() {
    let dir = workdir("range");
    let src = make_source(&dir);
    // Nguồn 60fps, lấy mẫu 30fps trong 2s → 60 frame.
    let frames = ffmpeg::extract_range(&src, 1.0, 2.0, 30, 160, 90, None).unwrap();
    assert!((59..=61).contains(&frames.len()), "{} frame", frames.len());
    assert!(frames.iter().all(|f| f.rgba.len() == 160 * 90 * 4));
    assert_ne!(frames[0].rgba, frames[frames.len() - 1].rgba);
}

#[test]
fn streams_original_video_until_end() {
    let dir = workdir("stream");
    let src = make_source(&dir); // 6s
    let mut s = ffmpeg::FrameStream::open(&src, 4.0, 30, 160, 90, None).unwrap();
    let mut times = Vec::new();
    while let Some((t, f)) = s.next_frame().unwrap() {
        assert_eq!(f.rgba.len(), 160 * 90 * 4);
        times.push(t);
    }
    // Từ 4s tới hết 6s ở 30fps → khoảng 60 frame, thời điểm tăng dần từ 4.0.
    assert!((58..=62).contains(&times.len()), "{} frame", times.len());
    assert_eq!(times[0], 4.0);
    assert!((times[30] - 5.0).abs() < 1e-9);
}

#[test]
fn stream_stops_cleanly_when_dropped_early() {
    let dir = workdir("stream_drop");
    let src = make_source(&dir);
    let mut s = ffmpeg::FrameStream::open(&src, 0.0, 30, 160, 90, None).unwrap();
    assert!(s.next_frame().unwrap().is_some());
    drop(s); // không được treo
}

/// Video dọc 464x848 (9:16), nền trắng toàn khung để dễ phân biệt với phần nền thêm vào.
fn make_portrait(dir: &Path) -> PathBuf {
    let p = dir.join("portrait.mp4");
    let mut cmd = ffmpeg::command(&ffmpeg::ffmpeg().unwrap());
    cmd.args([
        "-y",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "color=c=white:size=464x848:rate=30:duration=2",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
    ])
    .arg(&p);
    ffmpeg::run_checked(cmd, "create portrait video").unwrap();
    p
}

/// RGBA của điểm (x, y) trong frame đầu của file webm (giải mã giữ alpha).
fn pixel(path: &Path, x: u32, y: u32) -> [u8; 4] {
    let info = probe::probe(path).unwrap();
    let f = ffmpeg::extract_frame(path, 0.5, 512, 512, info.alpha_decoder.as_deref()).unwrap();
    let i = ((y * 512 + x) * 4) as usize;
    [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2], f.rgba[i + 3]]
}

#[test]
fn portrait_fit_all_adds_black_side_bars() {
    let dir = workdir("fit_all_black");
    let src = make_portrait(&dir);
    let info = probe::probe(&src).unwrap();
    let mut p = StickerParams::from_info(&info, dir.join("full.webm"));
    p.segments = vec![Segment::new(0.0, 1.0)];
    p.crop = CropSquare::fit_all(info.width, info.height);
    let r = sticker::encode(&p, &info, &AtomicBool::new(false), |_| {}).unwrap();
    assert_telegram_compliant(&r.output, 512, 30.0);
    // 464/848*512 ≈ 280px video ở giữa → dải đen ~116px mỗi bên.
    let edge = pixel(&r.output, 20, 256);
    let center = pixel(&r.output, 256, 256);
    assert!(
        edge[..3].iter().all(|&c| c < 30),
        "edge must be black background: {edge:?}"
    );
    assert!(
        center[..3].iter().all(|&c| c > 220),
        "center must be white video: {center:?}"
    );
}

#[test]
fn portrait_fit_all_with_transparent_background() {
    let dir = workdir("fit_all_alpha");
    let src = make_portrait(&dir);
    let info = probe::probe(&src).unwrap();
    let mut p = StickerParams::from_info(&info, dir.join("alpha.webm"));
    p.segments = vec![Segment::new(0.0, 1.0)];
    p.crop = CropSquare::fit_all(info.width, info.height);
    p.background = Background::Transparent;
    let r = sticker::encode(&p, &info, &AtomicBool::new(false), |_| {}).unwrap();
    assert_telegram_compliant(&r.output, 512, 30.0);
    let out = probe::probe(&r.output).unwrap();
    assert!(out.has_alpha(), "webm must have an alpha channel");
    assert!(
        pixel(&r.output, 20, 256)[3] < 20,
        "edge must be transparent"
    );
    assert!(pixel(&r.output, 256, 256)[3] > 235, "center must be opaque");
}

#[test]
fn extracts_preview_frame() {
    let dir = workdir("frame");
    let src = make_source(&dir);
    let f = ffmpeg::extract_frame(&src, 2.5, 320, 180, None).unwrap();
    assert_eq!((f.width, f.height, f.rgba.len()), (320, 180, 320 * 180 * 4));
    assert!(f.rgba.iter().any(|&b| b != 0));
}
