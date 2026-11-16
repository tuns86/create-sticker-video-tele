//! Tham số sticker, kiểm tra ràng buộc Telegram, dựng lệnh ffmpeg và encode.

use anyhow::{Context, Result, anyhow, bail};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ffmpeg;
use crate::probe::{self, VideoInfo};

/// Thời lượng tối đa của video sticker (giây).
pub const MAX_DURATION: f64 = 3.0;
/// Cạnh tối đa (px).
pub const MAX_SIDE: u32 = 512;
/// Kích thước emoji bắt buộc.
pub const EMOJI_SIDE: u32 = 100;
/// FPS tối đa.
pub const MAX_FPS: u32 = 30;
/// Dung lượng tối đa (byte).
pub const MAX_BYTES: u64 = 256 * 1024;

const EPS: f64 = 1e-6;

/// Một đoạn cắt trên timeline của video gốc (giây).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
}

impl Segment {
    pub fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }
    pub fn len(&self) -> f64 {
        self.end - self.start
    }
}

/// Vùng cắt hình vuông trên khung hình gốc (px, theo kích thước hiển thị đã xoay).
/// `x`, `y` có thể âm / vượt khung: phần ngoài video được tô bằng `Background`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropSquare {
    pub x: i32,
    pub y: i32,
    pub size: u32,
}

/// Vùng được phép đặt ô cắt (x0, y0, x1, y1) theo toạ độ video gốc.
pub type Bounds = (i32, i32, i32, i32);

/// Khung chứa: chỉ trong video, hoặc mở rộng ra ngoài đủ để ô vuông cạnh max(w, h)
/// trượt dọc theo cạnh ngắn mà vẫn phủ trọn video.
pub fn crop_bounds(width: u32, height: u32, allow_outside: bool) -> Bounds {
    let (w, h) = (width as i32, height as i32);
    if !allow_outside {
        return (0, 0, w, h);
    }
    let s = w.max(h);
    ((w - s).min(0), (h - s).min(0), w.max(s), h.max(s))
}

impl CropSquare {
    /// Hình vuông lớn nhất, nằm giữa khung hình.
    pub fn centered(width: u32, height: u32) -> Self {
        let size = width.min(height);
        Self {
            x: ((width - size) / 2) as i32,
            y: ((height - size) / 2) as i32,
            size,
        }
    }

    /// Hình vuông cạnh max(w, h) chứa trọn video ở giữa (phần thừa là nền).
    pub fn fit_all(width: u32, height: u32) -> Self {
        let size = width.max(height);
        Self {
            x: (width as i32 - size as i32) / 2,
            y: (height as i32 - size as i32) / 2,
            size,
        }
    }

    /// Ép vùng cắt nằm trọn trong khung `width`x`height`.
    pub fn clamped(self, width: u32, height: u32) -> Self {
        self.clamped_to(crop_bounds(width, height, false))
    }

    /// Ép vùng cắt nằm trọn trong `bounds`.
    pub fn clamped_to(self, (x0, y0, x1, y1): Bounds) -> Self {
        let max = (x1 - x0).min(y1 - y0).max(1) as u32;
        let size = self.size.clamp(1, max);
        let s = size as i32;
        Self {
            x: self.x.clamp(x0, x1 - s),
            y: self.y.clamp(y0, y1 - s),
            size,
        }
    }

    /// Ô cắt có nằm trọn trong video không (không cần thêm nền).
    pub fn is_inside(&self, width: u32, height: u32) -> bool {
        let s = self.size as i32;
        self.x >= 0 && self.y >= 0 && self.x + s <= width as i32 && self.y + s <= height as i32
    }
}

/// Màu nền cho phần ô cắt nằm ngoài video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    Color([u8; 3]),
    /// Trong suốt (xuất VP9 có kênh alpha).
    Transparent,
}

impl Default for Background {
    fn default() -> Self {
        Background::Color([0, 0, 0])
    }
}

impl Background {
    fn ffmpeg_color(&self) -> String {
        match self {
            Background::Color([r, g, b]) => format!("0x{r:02X}{g:02X}{b:02X}"),
            Background::Transparent => "0x000000@0".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StickerParams {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Các đoạn được ghép nối theo thứ tự.
    pub segments: Vec<Segment>,
    pub crop: CropSquare,
    /// Màu cho phần ô cắt nằm ngoài video.
    pub background: Background,
    /// Kích thước video gốc (đã xoay), cần để dựng filter thêm nền.
    pub src_width: u32,
    pub src_height: u32,
    /// Cạnh của video đầu ra (vuông), 1..=512. Sticker Telegram cần 512, emoji cần 100.
    pub out_size: u32,
    /// FPS đầu ra (số nguyên), 1..=30.
    pub fps: u32,
    /// Hệ số tốc độ: 2.0 = nhanh gấp đôi (rút ngắn thời lượng còn một nửa).
    pub speed: f64,
    /// CRF khởi đầu cho VP9 (0..=63, nhỏ = đẹp hơn).
    pub crf: u32,
    pub max_bytes: u64,
    /// Decoder giữ alpha cho webm trong suốt.
    pub alpha_decoder: Option<String>,
}

impl StickerParams {
    /// Tham số mặc định từ thông tin video: cắt giữa, 3s đầu, 512px.
    pub fn from_info(info: &VideoInfo, output: PathBuf) -> Self {
        Self {
            input: info.path.clone(),
            output,
            segments: vec![Segment::new(0.0, info.duration.min(MAX_DURATION))],
            crop: CropSquare::centered(info.width, info.height),
            background: Background::default(),
            src_width: info.width,
            src_height: info.height,
            out_size: MAX_SIDE,
            fps: default_fps(info.fps),
            speed: 1.0,
            crf: 30,
            max_bytes: MAX_BYTES,
            alpha_decoder: info.alpha_decoder.clone(),
        }
    }

    /// Tổng thời lượng các đoạn trên video gốc.
    pub fn source_duration(&self) -> f64 {
        self.segments.iter().map(Segment::len).sum()
    }

    /// Thời lượng video đầu ra sau khi áp tốc độ.
    pub fn output_duration(&self) -> f64 {
        self.source_duration() / self.speed
    }

    /// Số frame đầu ra; thời lượng thực = frames / fps, luôn <= output_duration().
    pub fn output_frames(&self) -> u64 {
        (self.output_duration() * self.fps as f64 + EPS).floor() as u64
    }

    /// Tốc độ tối thiểu để tổng thời lượng vừa 3s.
    pub fn speed_to_fit(&self) -> f64 {
        (self.source_duration() / MAX_DURATION).max(1.0)
    }

    /// Kiểm tra mọi ràng buộc; trả lỗi mô tả rõ vi phạm đầu tiên.
    pub fn validate(&self, info: &VideoInfo) -> Result<()> {
        if self.segments.is_empty() {
            bail!("No segment selected on the timeline");
        }
        for (i, s) in self.segments.iter().enumerate() {
            let n = i + 1;
            if !(s.start.is_finite() && s.end.is_finite()) || s.start < 0.0 {
                bail!(
                    "Segment {n}: invalid times ({:.3}s – {:.3}s)",
                    s.start,
                    s.end
                );
            }
            if s.len() <= EPS {
                bail!(
                    "Segment {n}: end ({:.3}s) must be after start ({:.3}s)",
                    s.end,
                    s.start
                );
            }
            if s.end > info.duration + 0.05 {
                bail!(
                    "Segment {n}: end {:.3}s exceeds video length {:.3}s",
                    s.end,
                    info.duration
                );
            }
        }
        if !(self.speed.is_finite() && self.speed > 0.0) {
            bail!("Invalid speed: {}", self.speed);
        }
        let d = self.output_duration();
        if d > MAX_DURATION + EPS {
            bail!(
                "Output duration {d:.3}s exceeds {MAX_DURATION}s. Shorten the segments or raise speed to at least {:.2}x",
                self.speed_to_fit()
            );
        }
        if self.fps == 0 || self.fps > MAX_FPS {
            bail!("FPS must be in 1..={MAX_FPS} (got {})", self.fps);
        }
        if self.output_frames() == 0 {
            bail!("Segment too short: less than 1 frame at {} FPS", self.fps);
        }
        if self.out_size == 0 || self.out_size > MAX_SIDE {
            bail!(
                "Output size must be in 1..={MAX_SIDE}px (got {})",
                self.out_size
            );
        }
        if (self.src_width, self.src_height) != (info.width, info.height) {
            bail!(
                "Source size in params ({}x{}) differs from video ({}x{})",
                self.src_width,
                self.src_height,
                info.width,
                info.height
            );
        }
        let c = self.crop;
        let (x0, y0, x1, y1) = crop_bounds(info.width, info.height, true);
        let s = c.size as i32;
        if c.size == 0 || c.x < x0 || c.y < y0 || c.x + s > x1 || c.y + s > y1 {
            bail!(
                "Crop {}x{} at ({}, {}) is out of bounds for frame {}x{}",
                c.size,
                c.size,
                c.x,
                c.y,
                info.width,
                info.height
            );
        }
        if c.x >= info.width as i32 || c.y >= info.height as i32 || c.x + s <= 0 || c.y + s <= 0 {
            bail!("Crop contains no part of the video");
        }
        if self.crf > 63 {
            bail!("CRF must be in 0..=63");
        }
        if self.input == self.output {
            bail!("Output file is the same as the input file");
        }
        Ok(())
    }

    fn needs_pad(&self) -> bool {
        !self.crop.is_inside(self.src_width, self.src_height)
    }

    /// Đầu ra có kênh alpha: nguồn trong suốt, hoặc nền trong suốt đang được dùng.
    pub fn output_has_alpha(&self) -> bool {
        self.alpha_decoder.is_some()
            || (self.needs_pad() && self.background == Background::Transparent)
    }

    fn pix_fmt(&self) -> &'static str {
        if self.output_has_alpha() {
            "yuva420p"
        } else {
            "yuv420p"
        }
    }

    /// Filter cắt vuông; nếu ô cắt vượt khung thì thêm nền (pad) trước rồi mới cắt.
    fn crop_filter(&self) -> String {
        let c = self.crop;
        let s = c.size as i32;
        if !self.needs_pad() {
            return format!("crop={s}:{s}:{}:{}", c.x, c.y);
        }
        let (w, h) = (self.src_width as i32, self.src_height as i32);
        let px = (-c.x).max(0);
        let py = (-c.y).max(0);
        let pw = w.max(c.x + s) + px;
        let ph = h.max(c.y + s) + py;
        // rgba trước khi pad: offset lẻ vẫn chính xác và giữ được alpha của nền.
        format!(
            "format=rgba,pad={pw}:{ph}:{px}:{py}:color={col},crop={s}:{s}:{cx}:{cy}",
            col = self.background.ffmpeg_color(),
            cx = c.x + px,
            cy = c.y + py,
        )
    }

    /// Chuỗi filter_complex: cắt từng đoạn → ghép → crop vuông → scale → tốc độ → fps.
    pub fn filter_complex(&self) -> String {
        let n = self.segments.len();
        let mut parts = Vec::new();
        let post = format!(
            "{crop},scale={o}:{o}:flags=lanczos,setsar=1,setpts=PTS/{speed},fps={fps},format={pf}",
            crop = self.crop_filter(),
            o = self.out_size,
            speed = fmt_num(self.speed),
            fps = self.fps,
            pf = self.pix_fmt(),
        );
        if n == 1 {
            parts.push(format!("[0:v]setpts=PTS-STARTPTS,{post}[out]"));
        } else {
            let mut labels = String::new();
            for i in 0..n {
                parts.push(format!("[{i}:v]setpts=PTS-STARTPTS[s{i}]"));
                labels.push_str(&format!("[s{i}]"));
            }
            parts.push(format!("{labels}concat=n={n}:v=1:a=0[c]"));
            parts.push(format!("[c]{post}[out]"));
        }
        parts.join(";")
    }

    /// Đối số ffmpeg cho một lần encode với bitrate/crf cho trước.
    pub fn ffmpeg_args(&self, output: &Path, bitrate_kbps: u32, crf: u32) -> Vec<String> {
        let mut a: Vec<String> = [
            "-hide_banner",
            "-nostdin",
            "-y",
            "-loglevel",
            "error",
            "-progress",
            "pipe:1",
            "-nostats",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for s in &self.segments {
            if let Some(dec) = &self.alpha_decoder {
                a.extend(["-c:v".into(), dec.clone()]);
            }
            a.extend([
                "-ss".into(),
                format!("{:.3}", s.start),
                "-t".into(),
                format!("{:.3}", s.len()),
                "-i".into(),
                self.input.to_string_lossy().into_owned(),
            ]);
        }
        a.extend(["-filter_complex".into(), self.filter_complex()]);
        a.extend(
            [
                "-map",
                "[out]",
                "-an",
                "-sn",
                "-dn",
                "-map_metadata",
                "-1",
                "-c:v",
                "libvpx-vp9",
                "-pix_fmt",
                self.pix_fmt(),
                "-deadline",
                "good",
                "-cpu-used",
                "1",
                "-row-mt",
                "1",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        a.extend([
            "-b:v".into(),
            format!("{bitrate_kbps}k"),
            "-crf".into(),
            crf.to_string(),
        ]);
        a.extend([
            "-frames:v".into(),
            self.output_frames().to_string(),
            "-f".into(),
            "webm".into(),
        ]);
        a.push(output.to_string_lossy().into_owned());
        a
    }

    /// Bitrate (kbps) mục tiêu để vừa giới hạn dung lượng, chừa 15% cho container.
    pub fn target_bitrate_kbps(&self) -> u32 {
        let d = (self.output_frames() as f64 / self.fps as f64).max(0.1);
        ((self.max_bytes as f64 * 8.0 / d / 1000.0) * 0.85)
            .floor()
            .max(1.0) as u32
    }
}

pub fn default_fps(source_fps: f64) -> u32 {
    (source_fps.round() as u32).clamp(1, MAX_FPS)
}

fn fmt_num(v: f64) -> String {
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncodeResult {
    pub output: PathBuf,
    pub bytes: u64,
    pub attempts: u32,
    pub bitrate_kbps: u32,
    pub crf: u32,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
}

/// Tiến trình encode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    /// Lần thử hiện tại (bắt đầu từ 1).
    pub attempt: u32,
    /// 0.0..=1.0 trong lần thử hiện tại.
    pub fraction: f32,
}

const MAX_ATTEMPTS: u32 = 6;
const MIN_BITRATE_KBPS: u32 = 20;

/// Encode sticker. Nếu file vượt `max_bytes`, giảm bitrate và tăng CRF rồi encode lại.
pub fn encode(
    params: &StickerParams,
    info: &VideoInfo,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(Progress),
) -> Result<EncodeResult> {
    params.validate(info)?;
    let bin = ffmpeg::ffmpeg()?;
    let tmp = tmp_path(&params.output);
    let expected_us = params.output_frames() as f64 / params.fps as f64 * 1e6;

    let mut bitrate = params.target_bitrate_kbps();
    let mut crf = params.crf;
    for attempt in 1..=MAX_ATTEMPTS {
        on_progress(Progress {
            attempt,
            fraction: 0.0,
        });
        let args = params.ffmpeg_args(&tmp, bitrate, crf);
        let res = run_with_progress(&bin, &args, cancel, |us| {
            let f = (us as f64 / expected_us).clamp(0.0, 1.0) as f32;
            on_progress(Progress {
                attempt,
                fraction: f,
            });
        });
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let bytes = std::fs::metadata(&tmp)
            .with_context(|| format!("ffmpeg did not create file {}", tmp.display()))?
            .len();
        if bytes <= params.max_bytes {
            let out_info = probe::probe(&tmp).context("Cannot re-read the encoded file")?;
            if out_info.duration > MAX_DURATION + 0.001 {
                let _ = std::fs::remove_file(&tmp);
                bail!(
                    "Output is {:.3}s long, exceeds {MAX_DURATION}s",
                    out_info.duration
                );
            }
            if params.output.exists() {
                std::fs::remove_file(&params.output)
                    .with_context(|| format!("Cannot overwrite {}", params.output.display()))?;
            }
            std::fs::rename(&tmp, &params.output).with_context(|| {
                format!(
                    "Cannot rename {} → {}",
                    tmp.display(),
                    params.output.display()
                )
            })?;
            on_progress(Progress {
                attempt,
                fraction: 1.0,
            });
            return Ok(EncodeResult {
                output: params.output.clone(),
                bytes,
                attempts: attempt,
                bitrate_kbps: bitrate,
                crf,
                duration: out_info.duration,
                width: out_info.width,
                height: out_info.height,
            });
        }
        // Quá dung lượng: hạ bitrate theo tỉ lệ vượt, thêm 10% dư.
        let ratio = params.max_bytes as f64 / bytes as f64;
        let next = (bitrate as f64 * ratio * 0.9).floor() as u32;
        if next < MIN_BITRATE_KBPS {
            let _ = std::fs::remove_file(&tmp);
            bail!(
                "Cannot compress below {} KB (attempt {attempt}: {} KB at {bitrate} kbps). Shorten the video or lower FPS.",
                params.max_bytes / 1024,
                bytes / 1024
            );
        }
        bitrate = next;
        crf = (crf + 4).min(63);
    }
    let _ = std::fs::remove_file(&tmp);
    Err(anyhow!(
        "Still over {} KB after {MAX_ATTEMPTS} attempts. Shorten the video or lower FPS.",
        params.max_bytes / 1024
    ))
}

fn tmp_path(output: &Path) -> PathBuf {
    let mut name = output
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".part.webm");
    output.with_file_name(name)
}

fn run_with_progress(
    bin: &Path,
    args: &[String],
    cancel: &AtomicBool,
    mut on_time_us: impl FnMut(u64),
) -> Result<()> {
    let mut child = ffmpeg::command(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to run ffmpeg")?;
    let mut stderr = child.stderr.take().expect("stderr piped");
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let stdout = child.stdout.take().expect("stdout piped");
    for line in BufReader::new(stdout).lines() {
        let line = line.context("Failed to read ffmpeg progress")?;
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Cancelled");
        }
        if let Some(v) = line.strip_prefix("out_time_us=")
            && let Ok(us) = v.trim().parse::<u64>()
        {
            on_time_us(us);
        }
    }
    let status = child.wait().context("Failed to wait for ffmpeg")?;
    let err = err_thread.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        bail!("Cancelled");
    }
    if !status.success() {
        let tail: Vec<&str> = err.lines().rev().take(15).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("ffmpeg encode failed ({status}):\n{}", tail.join("\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> VideoInfo {
        VideoInfo {
            path: "in.mp4".into(),
            width: 1280,
            height: 720,
            duration: 10.0,
            fps: 29.97,
            codec: "h264".into(),
            has_audio: true,
            alpha_decoder: None,
        }
    }

    fn params() -> StickerParams {
        StickerParams::from_info(&info(), "out.webm".into())
    }

    #[test]
    fn defaults_are_valid_and_telegram_compliant() {
        let p = params();
        p.validate(&info()).unwrap();
        assert_eq!(
            p.crop,
            CropSquare {
                x: 280,
                y: 0,
                size: 720
            }
        );
        assert_eq!(p.out_size, 512);
        assert_eq!(p.fps, 30);
        assert_eq!(p.output_duration(), 3.0);
        assert_eq!(p.output_frames(), 90);
    }

    #[test]
    fn fps_is_capped_at_30() {
        assert_eq!(default_fps(60.0), 30);
        assert_eq!(default_fps(23.976), 24);
        let mut p = params();
        p.fps = 31;
        assert!(p.validate(&info()).is_err());
    }

    #[test]
    fn duration_over_3s_is_rejected_and_speed_fixes_it() {
        let mut p = params();
        p.segments = vec![Segment::new(1.0, 3.0), Segment::new(5.0, 7.0)];
        let err = p.validate(&info()).unwrap_err().to_string();
        assert!(err.contains("exceeds 3s"), "{err}");
        assert!((p.speed_to_fit() - 4.0 / 3.0).abs() < 1e-9);
        p.speed = p.speed_to_fit();
        p.validate(&info()).unwrap();
        assert!(p.output_duration() <= MAX_DURATION + 1e-9);
        assert!(p.output_frames() as f64 / p.fps as f64 <= MAX_DURATION);
    }

    #[test]
    fn size_over_512_rejected() {
        let mut p = params();
        p.out_size = 513;
        assert!(p.validate(&info()).is_err());
        p.out_size = EMOJI_SIDE;
        p.validate(&info()).unwrap();
    }

    #[test]
    fn crop_outside_frame_rejected_and_clamped() {
        let mut p = params();
        p.crop = CropSquare {
            x: 700,
            y: 0,
            size: 720,
        };
        assert!(p.validate(&info()).is_err());
        p.crop = p.crop.clamped(1280, 720);
        assert_eq!(
            p.crop,
            CropSquare {
                x: 560,
                y: 0,
                size: 720
            }
        );
        p.validate(&info()).unwrap();
        assert_eq!(
            CropSquare {
                x: 5,
                y: 5,
                size: 5000
            }
            .clamped(1280, 720),
            CropSquare {
                x: 5,
                y: 0,
                size: 720
            }
        );
    }

    #[test]
    fn segment_validation() {
        let mut p = params();
        p.segments = vec![Segment::new(2.0, 2.0)];
        assert!(p.validate(&info()).is_err());
        p.segments = vec![Segment::new(9.0, 11.0)];
        assert!(p.validate(&info()).is_err());
        p.segments.clear();
        assert!(p.validate(&info()).is_err());
    }

    #[test]
    fn filter_for_single_segment() {
        let p = params();
        assert_eq!(
            p.filter_complex(),
            "[0:v]setpts=PTS-STARTPTS,crop=720:720:280:0,scale=512:512:flags=lanczos,setsar=1,setpts=PTS/1,fps=30,format=yuv420p[out]"
        );
    }

    #[test]
    fn filter_for_multiple_segments_concats() {
        let mut p = params();
        p.segments = vec![Segment::new(0.0, 1.0), Segment::new(4.0, 5.5)];
        p.speed = 1.25;
        let f = p.filter_complex();
        assert!(f.starts_with("[0:v]setpts=PTS-STARTPTS[s0];[1:v]setpts=PTS-STARTPTS[s1];[s0][s1]concat=n=2:v=1:a=0[c];[c]crop="));
        assert!(f.contains("setpts=PTS/1.25"));
    }

    #[test]
    fn ffmpeg_args_strip_audio_and_use_vp9() {
        let mut p = params();
        p.segments = vec![Segment::new(0.5, 1.5), Segment::new(4.0, 5.0)];
        let a = p.ffmpeg_args(Path::new("o.webm"), 500, 30);
        let joined = a.join(" ");
        assert!(joined.contains("-ss 0.500 -t 1.000 -i in.mp4"));
        assert!(joined.contains("-ss 4.000 -t 1.000 -i in.mp4"));
        assert!(joined.contains("-an"));
        assert!(joined.contains("-c:v libvpx-vp9"));
        assert!(joined.contains("-b:v 500k -crf 30"));
        assert!(joined.contains("-frames:v 60 -f webm o.webm"));
    }

    #[test]
    fn alpha_source_keeps_alpha() {
        let mut i = info();
        i.alpha_decoder = Some("libvpx-vp9".into());
        let p = StickerParams::from_info(&i, "o.webm".into());
        let a = p.ffmpeg_args(Path::new("o.webm"), 500, 30).join(" ");
        assert!(a.starts_with("-hide_banner") && a.contains("-c:v libvpx-vp9 -ss 0.000"));
        assert!(a.contains("-pix_fmt yuva420p"));
    }

    fn portrait() -> VideoInfo {
        VideoInfo {
            path: "p.mov".into(),
            width: 464,
            height: 848,
            duration: 10.0,
            fps: 30.0,
            codec: "h264".into(),
            has_audio: true,
            alpha_decoder: None,
        }
    }

    #[test]
    fn bounds_allow_square_covering_portrait_video() {
        assert_eq!(crop_bounds(464, 848, false), (0, 0, 464, 848));
        assert_eq!(crop_bounds(464, 848, true), (-384, 0, 848, 848));
        assert_eq!(crop_bounds(1280, 720, true), (0, -560, 1280, 1280));
        let c = CropSquare::fit_all(464, 848);
        assert_eq!(
            c,
            CropSquare {
                x: -192,
                y: 0,
                size: 848
            }
        );
        assert!(!c.is_inside(464, 848));
        assert_eq!(c.clamped_to(crop_bounds(464, 848, true)), c);
        // tắt mở rộng → co lại vào trong video
        assert_eq!(
            c.clamped(464, 848),
            CropSquare {
                x: 0,
                y: 0,
                size: 464
            }
        );
    }

    #[test]
    fn fit_all_pads_with_black_by_default() {
        let i = portrait();
        let mut p = StickerParams::from_info(&i, "o.webm".into());
        p.crop = CropSquare::fit_all(i.width, i.height);
        p.validate(&i).unwrap();
        assert_eq!(
            p.filter_complex(),
            "[0:v]setpts=PTS-STARTPTS,format=rgba,pad=848:848:192:0:color=0x000000,crop=848:848:0:0,\
             scale=512:512:flags=lanczos,setsar=1,setpts=PTS/1,fps=30,format=yuv420p[out]"
        );
    }

    #[test]
    fn partial_overflow_pads_only_needed_side() {
        let i = portrait();
        let mut p = StickerParams::from_info(&i, "o.webm".into());
        p.crop = CropSquare {
            x: -100,
            y: 300,
            size: 500,
        };
        p.background = Background::Color([255, 255, 255]);
        p.validate(&i).unwrap();
        // 500px ô cắt bắt đầu ở x=-100 → pad trái 100, rộng 464+100; cao giữ 848.
        assert!(
            p.filter_complex()
                .contains("pad=564:848:100:0:color=0xFFFFFF,crop=500:500:0:300")
        );
    }

    #[test]
    fn transparent_background_outputs_alpha_only_when_padding() {
        let i = portrait();
        let mut p = StickerParams::from_info(&i, "o.webm".into());
        p.background = Background::Transparent;
        assert!(!p.output_has_alpha(), "crop inside video → no alpha needed");
        p.crop = CropSquare::fit_all(i.width, i.height);
        assert!(p.output_has_alpha());
        let a = p.ffmpeg_args(Path::new("o.webm"), 500, 30).join(" ");
        assert!(a.contains("color=0x000000@0"));
        assert!(a.contains("-pix_fmt yuva420p"));
    }

    #[test]
    fn crop_without_any_video_is_rejected() {
        let i = portrait();
        let mut p = StickerParams::from_info(&i, "o.webm".into());
        p.crop = CropSquare {
            x: -384,
            y: 0,
            size: 300,
        };
        let e = p.validate(&i).unwrap_err().to_string();
        assert!(e.contains("contains no part"), "{e}");
        p.crop = CropSquare {
            x: -400,
            y: 0,
            size: 848,
        };
        assert!(p.validate(&i).is_err(), "past left bound");
    }

    #[test]
    fn target_bitrate_fits_256kb() {
        let p = params();
        let kbps = p.target_bitrate_kbps();
        assert!(kbps as f64 * 3.0 * 1000.0 / 8.0 <= MAX_BYTES as f64);
        assert!(kbps > 400);
    }
}
