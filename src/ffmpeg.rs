//! Tìm và gọi ffmpeg / ffprobe.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[cfg(windows)]
const EXE_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
const EXE_SUFFIX: &str = "";

/// Tìm binary: ưu tiên cạnh file chạy của app, sau đó trong PATH.
pub fn find_binary(name: &str) -> Result<PathBuf> {
    let file = format!("{name}{EXE_SUFFIX}");
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let p = dir.join(&file);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(&file);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    bail!(
        "{file} not found. Install ffmpeg and add it to PATH, or put {file} next to the app executable."
    )
}

pub fn ffmpeg() -> Result<PathBuf> {
    find_binary("ffmpeg")
}

pub fn ffprobe() -> Result<PathBuf> {
    find_binary("ffprobe")
}

/// Tạo Command không bật cửa sổ console trên Windows.
pub fn command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null());
    cmd
}

/// Chạy lệnh, lỗi nếu exit code khác 0 (kèm stderr).
pub fn run_checked(mut cmd: Command, what: &str) -> Result<Output> {
    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("Failed to run {what}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(15).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("{what} failed ({}):\n{}", out.status, tail.join("\n"));
    }
    Ok(out)
}

/// Ảnh RGBA của một frame.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Lấy 1 frame tại thời điểm `time` (giây), scale về `width`x`height` (đã xoay theo metadata).
pub fn extract_frame(
    input: &Path,
    time: f64,
    width: u32,
    height: u32,
    alpha_decoder: Option<&str>,
) -> Result<Frame> {
    let bin = ffmpeg()?;
    let mut cmd = command(&bin);
    cmd.args(["-hide_banner", "-loglevel", "error", "-ss"])
        .arg(format!("{:.3}", time.max(0.0)));
    if let Some(dec) = alpha_decoder {
        cmd.args(["-c:v", dec]);
    }
    cmd.arg("-i")
        .arg(input)
        .args(["-frames:v", "1", "-an", "-vf"])
        .arg(format!("scale={width}:{height}:flags=bilinear"))
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
    let out = run_checked(cmd, "ffmpeg (grab frame)")?;
    let expected = (width * height * 4) as usize;
    if out.stdout.len() < expected {
        bail!(
            "ffmpeg returned no frame at {time:.3}s (got {} bytes, expected {expected})",
            out.stdout.len()
        );
    }
    let mut rgba = out.stdout;
    rgba.truncate(expected);
    Ok(Frame {
        width,
        height,
        rgba,
    })
}

/// Giải mã cả đoạn [start, start+len) một lần, lấy mẫu ở `fps`, scale về `width`x`height`.
/// Dùng để phát xem trước mượt thay vì seek từng frame.
pub fn extract_range(
    input: &Path,
    start: f64,
    len: f64,
    fps: u32,
    width: u32,
    height: u32,
    alpha_decoder: Option<&str>,
) -> Result<Vec<Frame>> {
    if len <= 0.0 || fps == 0 {
        bail!("Invalid playback range: {len:.3}s long, {fps} fps");
    }
    let bin = ffmpeg()?;
    let mut cmd = command(&bin);
    cmd.args(["-hide_banner", "-loglevel", "error"]);
    if let Some(dec) = alpha_decoder {
        cmd.args(["-c:v", dec]);
    }
    cmd.arg("-ss")
        .arg(format!("{:.3}", start.max(0.0)))
        .arg("-t")
        .arg(format!("{len:.3}"))
        .arg("-i")
        .arg(input)
        .args(["-an", "-vf"])
        .arg(format!("fps={fps},scale={width}:{height}:flags=bilinear"))
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
    let out = run_checked(cmd, "ffmpeg (decode playback range)")?;
    let fsz = (width * height * 4) as usize;
    let frames: Vec<Frame> = out
        .stdout
        .chunks_exact(fsz)
        .map(|c| Frame {
            width,
            height,
            rgba: c.to_vec(),
        })
        .collect();
    if frames.is_empty() {
        bail!(
            "ffmpeg returned no frames for range {start:.3}s – {:.3}s",
            start + len
        );
    }
    Ok(frames)
}

/// Luồng frame RGBA đọc dần từ ffmpeg (để phát video dài mà không giữ hết trong RAM).
/// Drop sẽ kill tiến trình ffmpeg.
pub struct FrameStream {
    child: std::process::Child,
    stdout: std::process::ChildStdout,
    stderr: Option<std::thread::JoinHandle<String>>,
    width: u32,
    height: u32,
    /// Thời điểm (giây trong video gốc) của frame đầu tiên.
    pub start: f64,
    pub fps: u32,
    index: u64,
}

impl FrameStream {
    /// Bắt đầu giải mã từ `start` (giây), lấy mẫu ở `fps`, scale về `width`x`height`.
    pub fn open(
        input: &Path,
        start: f64,
        fps: u32,
        width: u32,
        height: u32,
        alpha_decoder: Option<&str>,
    ) -> Result<Self> {
        if fps == 0 {
            bail!("Playback FPS must be > 0");
        }
        let bin = ffmpeg()?;
        let mut cmd = command(&bin);
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        if let Some(dec) = alpha_decoder {
            cmd.args(["-c:v", dec]);
        }
        cmd.arg("-ss")
            .arg(format!("{:.3}", start.max(0.0)))
            .arg("-i")
            .arg(input)
            .args(["-an", "-vf"])
            .arg(format!("fps={fps},scale={width}:{height}:flags=bilinear"))
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("Failed to start ffmpeg for playback")?;
        let stdout = child.stdout.take().context("ffmpeg has no stdout")?;
        let mut err = child.stderr.take().context("ffmpeg has no stderr")?;
        let stderr = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut err, &mut s);
            s
        });
        Ok(Self {
            child,
            stdout,
            stderr: Some(stderr),
            width,
            height,
            start: start.max(0.0),
            fps,
            index: 0,
        })
    }

    /// Frame kế tiếp kèm thời điểm trong video gốc. `Ok(None)` khi hết video.
    pub fn next_frame(&mut self) -> Result<Option<(f64, Frame)>> {
        use std::io::Read;
        let fsz = (self.width * self.height * 4) as usize;
        let mut buf = vec![0u8; fsz];
        let mut filled = 0;
        while filled < fsz {
            let n = self
                .stdout
                .read(&mut buf[filled..])
                .context("Failed to read frame from ffmpeg")?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled < fsz {
            // Hết dữ liệu: kiểm tra ffmpeg có thoát lỗi không.
            let status = self.child.wait().context("Failed to wait for ffmpeg")?;
            let err = self
                .stderr
                .take()
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            if !status.success() {
                bail!("ffmpeg playback failed ({status}): {}", err.trim());
            }
            return Ok(None);
        }
        let t = self.start + self.index as f64 / self.fps as f64;
        self.index += 1;
        Ok(Some((
            t,
            Frame {
                width: self.width,
                height: self.height,
                rgba: buf,
            },
        )))
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
