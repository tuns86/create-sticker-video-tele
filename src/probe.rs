//! Đọc thông tin video bằng ffprobe.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::ffmpeg;

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub path: PathBuf,
    /// Kích thước hiển thị (đã tính xoay theo metadata).
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub fps: f64,
    pub codec: String,
    pub has_audio: bool,
    /// Decoder cần dùng để giữ kênh alpha (webm VP8/VP9 trong suốt), nếu có.
    pub alpha_decoder: Option<String>,
}

impl VideoInfo {
    pub fn has_alpha(&self) -> bool {
        self.alpha_decoder.is_some()
    }
}

pub fn probe(path: &Path) -> Result<VideoInfo> {
    let bin = ffmpeg::ffprobe()?;
    let mut cmd = ffmpeg::command(&bin);
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "stream=codec_type,codec_name,width,height,avg_frame_rate,r_frame_rate,duration:stream_tags:stream_side_data:format=duration",
        "-of",
        "json",
    ])
    .arg(path);
    let out = ffmpeg::run_checked(cmd, "ffprobe")?;
    let json: Value =
        serde_json::from_slice(&out.stdout).context("ffprobe returned invalid JSON")?;
    parse_probe(path, &json)
}

fn parse_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let n: f64 = n.trim().parse().ok()?;
    let d: f64 = d.trim().parse().ok()?;
    if d == 0.0 || n <= 0.0 {
        return None;
    }
    Some(n / d)
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn parse_probe(path: &Path, json: &Value) -> Result<VideoInfo> {
    let streams = json["streams"]
        .as_array()
        .ok_or_else(|| anyhow!("ffprobe output has no streams"))?;
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .ok_or_else(|| anyhow!("File has no video stream: {}", path.display()))?;
    let has_audio = streams.iter().any(|s| s["codec_type"] == "audio");

    let w = video["width"]
        .as_u64()
        .ok_or_else(|| anyhow!("Cannot read video width"))? as u32;
    let h = video["height"]
        .as_u64()
        .ok_or_else(|| anyhow!("Cannot read video height"))? as u32;
    if w == 0 || h == 0 {
        bail!("Invalid video size: {w}x{h}");
    }

    // Xoay: side_data rotation (ffmpeg mới) hoặc tag "rotate" (cũ).
    let mut rotation = 0i64;
    if let Some(sd) = video["side_data_list"].as_array() {
        for d in sd {
            if let Some(r) = num(&d["rotation"]) {
                rotation = r.round() as i64;
            }
        }
    }
    if let Some(r) = num(&video["tags"]["rotate"]) {
        rotation = r.round() as i64;
    }
    let (width, height) = if rotation.rem_euclid(180) == 90 {
        (h, w)
    } else {
        (w, h)
    };

    let duration = num(&json["format"]["duration"])
        .or_else(|| num(&video["duration"]))
        .ok_or_else(|| anyhow!("Cannot read video duration"))?;

    let fps = video["avg_frame_rate"]
        .as_str()
        .and_then(parse_rate)
        .or_else(|| video["r_frame_rate"].as_str().and_then(parse_rate))
        .ok_or_else(|| anyhow!("Cannot read video FPS"))?;

    let codec = video["codec_name"].as_str().unwrap_or("").to_string();
    let alpha_tag = video["tags"]
        .as_object()
        .and_then(|t| t.iter().find(|(k, _)| k.eq_ignore_ascii_case("alpha_mode")))
        .and_then(|(_, v)| v.as_str())
        == Some("1");
    let alpha_decoder = match (alpha_tag, codec.as_str()) {
        (true, "vp9") => Some("libvpx-vp9".to_string()),
        (true, "vp8") => Some("libvpx".to_string()),
        _ => None,
    };

    Ok(VideoInfo {
        path: path.to_path_buf(),
        width,
        height,
        duration,
        fps,
        codec,
        has_audio,
        alpha_decoder,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rotated_video_swaps_dimensions() {
        let j = json!({
            "streams": [{"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
                "avg_frame_rate":"30000/1001","side_data_list":[{"rotation":-90}]},
                {"codec_type":"audio","codec_name":"aac"}],
            "format": {"duration":"12.5"}
        });
        let info = parse_probe(Path::new("a.mp4"), &j).unwrap();
        assert_eq!((info.width, info.height), (1080, 1920));
        assert!((info.fps - 29.97).abs() < 0.01);
        assert_eq!(info.duration, 12.5);
        assert!(info.has_audio);
        assert!(!info.has_alpha());
    }

    #[test]
    fn webm_alpha_uses_libvpx_decoder() {
        let j = json!({
            "streams": [{"codec_type":"video","codec_name":"vp9","width":512,"height":512,
                "avg_frame_rate":"30/1","tags":{"ALPHA_MODE":"1"}}],
            "format": {"duration":"2.9"}
        });
        let info = parse_probe(Path::new("a.webm"), &j).unwrap();
        assert_eq!(info.alpha_decoder.as_deref(), Some("libvpx-vp9"));
        assert!(!info.has_audio);
    }

    #[test]
    fn missing_video_stream_is_error() {
        let j = json!({"streams":[{"codec_type":"audio"}],"format":{"duration":"1"}});
        assert!(parse_probe(Path::new("a.mp3"), &j).is_err());
    }
}
