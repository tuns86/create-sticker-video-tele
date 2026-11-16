//! Giao diện egui.

use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind,
    TextureHandle, TextureOptions, Vec2,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use create_sticker_video_tele::VideoInfo;
use create_sticker_video_tele::ffmpeg::{self, Frame};
use create_sticker_video_tele::probe;
use create_sticker_video_tele::sticker::{
    self, Background, Bounds, CropSquare, EMOJI_SIDE, EncodeResult, MAX_DURATION, MAX_FPS,
    MAX_SIDE, Progress, Segment, StickerParams, crop_bounds,
};

const PREVIEW_MAX: u32 = 960;
/// Cạnh dài của frame dùng khi phát (nhỏ hơn để giữ RAM: 3s × 30fps).
const PLAY_MAX: u32 = 640;
const RESULT_PREVIEW: u32 = 256;
const MIN_SEG: f64 = 0.05;
const ACCENT: Color32 = Color32::from_rgb(64, 160, 255);
const SEG_COLORS: [Color32; 4] = [
    Color32::from_rgb(255, 170, 60),
    Color32::from_rgb(120, 210, 120),
    Color32::from_rgb(220, 120, 220),
    Color32::from_rgb(240, 220, 90),
];

#[derive(Clone, Copy, PartialEq)]
enum SizeMode {
    Sticker,
    Emoji,
    Custom,
}

#[derive(Clone, Copy)]
enum CropDrag {
    Move { grab: Vec2 },
    Resize { anchor: (f32, f32) },
}

#[derive(Clone, Copy, PartialEq)]
enum TimeDrag {
    Start,
    End,
    Body { grab: f64 },
    Playhead,
}

struct FrameRequest {
    path: PathBuf,
    time: f64,
    w: u32,
    h: u32,
    decoder: Option<String>,
}

struct FrameReply {
    path: PathBuf,
    time: f64,
    frame: anyhow::Result<Frame>,
}

struct ExportJob {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Progress>>,
    rx: Receiver<anyhow::Result<EncodeResult>>,
    started: Instant,
}

struct ResultPreview {
    frames: Vec<TextureHandle>,
    fps: u32,
    started: Instant,
}

/// Đoạn đã giải mã sẵn cho nút phát: đổi file / vùng chọn / FPS thì phải giải mã lại.
#[derive(Clone, PartialEq)]
struct PlayKey {
    path: PathBuf,
    start: f64,
    end: f64,
    fps: u32,
}

struct PlayCache {
    key: PlayKey,
    frames: Vec<TextureHandle>,
}

type PlayReply = (PlayKey, anyhow::Result<Vec<Frame>>);

type StreamMsg = anyhow::Result<Option<(f64, Frame)>>;

/// Phát video gốc từ vị trí con trỏ tới hết, 1x, frame được đọc dần từ ffmpeg.
struct OriginalPlayer {
    stop: Arc<AtomicBool>,
    rx: Receiver<StreamMsg>,
}

impl OriginalPlayer {
    fn start(ctx: egui::Context, info: &VideoInfo, from: f64, fps: u32, w: u32, h: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel::<StreamMsg>(2);
        let (path, dec, stop2) = (info.path.clone(), info.alpha_decoder.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut stream = match ffmpeg::FrameStream::open(&path, from, fps, w, h, dec.as_deref())
            {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    ctx.request_repaint();
                    return;
                }
            };
            let wall0 = Instant::now();
            while !stop2.load(Ordering::Relaxed) {
                let msg = stream.next_frame();
                let done = !matches!(msg, Ok(Some(_)));
                if let Ok(Some((t, _))) = &msg {
                    // Giữ nhịp thời gian thực.
                    let due = std::time::Duration::from_secs_f64((t - from).max(0.0));
                    if let Some(wait) = due.checked_sub(wall0.elapsed()) {
                        std::thread::sleep(wait);
                    }
                }
                if stop2.load(Ordering::Relaxed) || tx.send(msg).is_err() {
                    break;
                }
                ctx.request_repaint();
                if done {
                    break;
                }
            }
            // `stream` bị drop ở đây → kill ffmpeg.
        });
        Self { stop, rx }
    }
}

impl Drop for OriginalPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub struct StickerApp {
    info: Option<VideoInfo>,
    params: Option<StickerParams>,
    preview_dims: (u32, u32),

    playhead: f64,
    selection: Segment,
    segments: Vec<Segment>,
    playing: Option<Instant>,
    play_cache: Option<PlayCache>,
    play_rx: Option<Receiver<PlayReply>>,
    /// Đã bấm phát, đang chờ giải mã xong.
    play_pending: bool,
    original: Option<OriginalPlayer>,
    resume_original: bool,

    /// Cho ô cắt vượt ra ngoài video (phần thừa là nền).
    allow_outside: bool,
    /// Màu nền gần nhất, giữ lại khi bật/tắt "Trong suốt".
    last_bg_color: [u8; 3],

    texture: Option<TextureHandle>,
    frame_tx: Sender<FrameRequest>,
    frame_rx: Receiver<FrameReply>,
    last_requested: Option<f64>,
    frame_in_flight: bool,

    crop_drag: Option<CropDrag>,
    time_drag: Option<TimeDrag>,
    size_mode: SizeMode,

    export: Option<ExportJob>,
    result: Option<EncodeResult>,
    result_preview: Option<ResultPreview>,
    result_rx: Option<Receiver<anyhow::Result<(Vec<ColorImage>, u32)>>>,
    error: Option<String>,
}

impl StickerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        setup_fonts(&cc.egui_ctx);
        let (frame_tx, frame_rx) = spawn_frame_worker(cc.egui_ctx.clone());
        let mut app = Self {
            info: None,
            params: None,
            preview_dims: (0, 0),
            playhead: 0.0,
            selection: Segment::new(0.0, 0.0),
            segments: Vec::new(),
            playing: None,
            play_cache: None,
            play_rx: None,
            play_pending: false,
            original: None,
            resume_original: false,
            allow_outside: false,
            last_bg_color: [0, 0, 0],
            texture: None,
            frame_tx,
            frame_rx,
            last_requested: None,
            frame_in_flight: false,
            crop_drag: None,
            time_drag: None,
            size_mode: SizeMode::Sticker,
            export: None,
            result: None,
            result_preview: None,
            result_rx: None,
            error: None,
        };
        if let Some(p) = initial {
            app.open(&p);
        }
        app
    }

    fn open(&mut self, path: &Path) {
        match probe::probe(path) {
            Ok(info) => {
                let output = default_output(path);
                let params = StickerParams::from_info(&info, output);
                let (pw, ph) = fit_dims(info.width, info.height, PREVIEW_MAX);
                self.preview_dims = (pw, ph);
                self.selection = params.segments[0];
                self.segments.clear();
                self.playhead = 0.0;
                self.playing = None;
                self.play_cache = None;
                self.play_rx = None;
                self.play_pending = false;
                self.original = None;
                self.texture = None;
                self.last_requested = None;
                self.result = None;
                self.result_preview = None;
                self.error = None;
                self.size_mode = SizeMode::Sticker;
                self.info = Some(info);
                self.params = Some(params);
            }
            Err(e) => self.error = Some(format!("Cannot open video: {e:#}")),
        }
    }

    fn effective_segments(&self) -> Vec<Segment> {
        if self.segments.is_empty() {
            vec![self.selection]
        } else {
            self.segments.clone()
        }
    }

    /// Tham số cuối cùng sẽ được encode.
    fn export_params(&self) -> Option<StickerParams> {
        let mut p = self.params.clone()?;
        p.segments = self.effective_segments();
        Some(p)
    }

    /// Độ dài tối đa (giây trên video gốc) cho vùng chọn.
    fn selection_budget(&self) -> f64 {
        let speed = self.params.as_ref().map_or(1.0, |p| p.speed);
        let used: f64 = self.segments.iter().map(Segment::len).sum();
        selection_budget(speed, used)
    }

    fn play_key(&self) -> Option<PlayKey> {
        Some(PlayKey {
            path: self.info.as_ref()?.path.clone(),
            start: self.selection.start,
            end: self.selection.end,
            fps: self.params.as_ref()?.fps,
        })
    }

    /// Frame đang phát: chạy theo thời gian thực × tốc độ, lặp lại trong đoạn.
    fn play_index(&self) -> Option<usize> {
        let t0 = self.playing?;
        let cache = self.play_cache.as_ref()?;
        let speed = self.params.as_ref()?.speed;
        play_frame_index(
            t0.elapsed().as_secs_f64(),
            speed,
            cache.key.fps,
            cache.frames.len(),
        )
    }

    /// Phát / dừng video gốc từ vị trí con trỏ (hết video thì phát lại từ đầu).
    fn toggle_original(&mut self, ctx: &egui::Context) {
        if self.original.take().is_some() {
            return;
        }
        let Some(info) = &self.info else { return };
        self.playing = None;
        self.play_pending = false;
        let from = if self.playhead >= info.duration - 0.1 {
            0.0
        } else {
            self.playhead
        };
        let fps = original_play_fps(info.fps);
        let (w, h) = fit_dims(info.width, info.height, PLAY_MAX);
        self.original = Some(OriginalPlayer::start(ctx.clone(), info, from, fps, w, h));
    }

    fn toggle_play(&mut self, ctx: &egui::Context) {
        self.original = None;
        if self.playing.is_some() || self.play_pending {
            self.playing = None;
            self.play_pending = false;
            return;
        }
        let (Some(key), Some(info)) = (self.play_key(), self.info.as_ref()) else {
            return;
        };
        if self.play_cache.as_ref().map(|c| &c.key) == Some(&key) {
            self.playing = Some(Instant::now());
            return;
        }
        let (w, h) = fit_dims(info.width, info.height, PLAY_MAX);
        let decoder = info.alpha_decoder.clone();
        let (tx, rx) = mpsc::channel();
        let ctx2 = ctx.clone();
        let k = key.clone();
        std::thread::spawn(move || {
            let res = ffmpeg::extract_range(
                &k.path,
                k.start,
                k.end - k.start,
                k.fps,
                w,
                h,
                decoder.as_deref(),
            );
            let _ = tx.send((k, res));
            ctx2.request_repaint();
        });
        self.play_rx = Some(rx);
        self.play_pending = true;
    }

    fn request_frame(&mut self) {
        let Some(info) = &self.info else { return };
        if self.playing.is_some()
            || self.original.is_some()
            || self.frame_in_flight
            || self.last_requested == Some(self.playhead)
        {
            return;
        }
        let (w, h) = self.preview_dims;
        let t = self.playhead.min((info.duration - 0.04).max(0.0));
        self.frame_in_flight = true;
        self.last_requested = Some(self.playhead);
        let _ = self.frame_tx.send(FrameRequest {
            path: info.path.clone(),
            time: t,
            w,
            h,
            decoder: info.alpha_decoder.clone(),
        });
    }

    fn poll_workers(&mut self, ctx: &egui::Context) {
        while let Ok(reply) = self.frame_rx.try_recv() {
            self.frame_in_flight = false;
            if self.info.as_ref().map(|i| &i.path) != Some(&reply.path) {
                continue;
            }
            match reply.frame {
                Ok(f) => {
                    let img = ColorImage::from_rgba_unmultiplied(
                        [f.width as usize, f.height as usize],
                        &f.rgba,
                    );
                    match &mut self.texture {
                        Some(t) => t.set(img, TextureOptions::LINEAR),
                        None => {
                            self.texture =
                                Some(ctx.load_texture("frame", img, TextureOptions::LINEAR))
                        }
                    }
                }
                Err(e) => {
                    self.error = Some(format!("Failed to grab frame at {:.2}s: {e:#}", reply.time))
                }
            }
        }

        // Video gốc đang phát: lấy frame mới nhất.
        let mut latest = None;
        let mut ended = false;
        if let Some(p) = &self.original {
            while let Ok(msg) = p.rx.try_recv() {
                match msg {
                    Ok(Some(tf)) => latest = Some(tf),
                    Ok(None) => ended = true,
                    Err(e) => {
                        self.error = Some(format!("Original video playback failed: {e:#}"));
                        ended = true;
                    }
                }
            }
        }
        if let Some((t, f)) = latest {
            let img =
                ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
            match &mut self.texture {
                Some(tex) => tex.set(img, TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("frame", img, TextureOptions::LINEAR)),
            }
            // Không đặt last_requested: khi dừng sẽ tải lại frame nét (960px) tại vị trí này.
            self.playhead = t;
        }
        if ended {
            self.original = None;
        }

        if let Some(rx) = &self.play_rx
            && let Ok((key, res)) = rx.try_recv()
        {
            self.play_rx = None;
            match res {
                Ok(frames) => {
                    let textures = frames
                        .into_iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let img = ColorImage::from_rgba_unmultiplied(
                                [f.width as usize, f.height as usize],
                                &f.rgba,
                            );
                            ctx.load_texture(format!("play{i}"), img, TextureOptions::LINEAR)
                        })
                        .collect();
                    // Chỉ phát nếu người dùng chưa huỷ và vùng chọn chưa đổi trong lúc giải mã.
                    let current = self.play_key().as_ref() == Some(&key);
                    self.play_cache = Some(PlayCache {
                        key,
                        frames: textures,
                    });
                    if self.play_pending && current {
                        self.playing = Some(Instant::now());
                    }
                }
                Err(e) => self.error = Some(format!("Cannot play selection: {e:#}")),
            }
            self.play_pending = false;
        }

        if let Some(job) = &self.export
            && let Ok(res) = job.rx.try_recv()
        {
            self.export = None;
            match res {
                Ok(r) => {
                    self.start_result_preview(ctx, &r.output);
                    self.result = Some(r);
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }

        if let Some(rx) = &self.result_rx
            && let Ok(res) = rx.try_recv()
        {
            self.result_rx = None;
            match res {
                Ok((imgs, fps)) => {
                    let frames = imgs
                        .into_iter()
                        .enumerate()
                        .map(|(i, img)| {
                            ctx.load_texture(format!("result{i}"), img, TextureOptions::LINEAR)
                        })
                        .collect();
                    self.result_preview = Some(ResultPreview {
                        frames,
                        fps,
                        started: Instant::now(),
                    });
                }
                Err(e) => self.error = Some(format!("Cannot preview result: {e:#}")),
            }
        }
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        let (Some(info), Some(params)) = (self.info.clone(), self.export_params()) else {
            return;
        };
        if let Err(e) = params.validate(&info) {
            self.error = Some(format!("{e:#}"));
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(Progress {
            attempt: 1,
            fraction: 0.0,
        }));
        let (tx, rx) = mpsc::channel();
        let (c, p, ctx2) = (cancel.clone(), progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let res = sticker::encode(&params, &info, &c, |pr| {
                *p.lock().unwrap() = pr;
                ctx2.request_repaint();
            });
            let _ = tx.send(res);
            ctx2.request_repaint();
        });
        self.result = None;
        self.result_preview = None;
        self.error = None;
        self.export = Some(ExportJob {
            cancel,
            progress,
            rx,
            started: Instant::now(),
        });
    }

    fn start_result_preview(&mut self, ctx: &egui::Context, path: &Path) {
        let (tx, rx) = mpsc::channel();
        let path = path.to_path_buf();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(load_all_frames(&path, RESULT_PREVIEW));
            ctx2.request_repaint();
        });
        self.result_rx = Some(rx);
    }

    // ------------------------------------------------------------------ UI

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("📂 Mở video…").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter(
                        "Video",
                        &[
                            "mp4", "mov", "mkv", "webm", "avi", "gif", "m4v", "flv", "wmv",
                        ],
                    )
                    .add_filter("Tất cả", &["*"])
                    .pick_file()
            {
                self.open(&p);
            }
            match &self.info {
                Some(i) => {
                    ui.label(format!(
                        "{}  •  {}x{}  •  {:.2}s  •  {:.2} fps  •  {}{}{}",
                        i.path
                            .file_name()
                            .map(|s| s.to_string_lossy())
                            .unwrap_or_default(),
                        i.width,
                        i.height,
                        i.duration,
                        i.fps,
                        i.codec,
                        if i.has_audio {
                            "  •  có audio (sẽ bỏ)"
                        } else {
                            ""
                        },
                        if i.has_alpha() {
                            "  •  trong suốt"
                        } else {
                            ""
                        },
                    ));
                }
                None => {
                    ui.label("Chưa mở video. Bấm \"Mở video\" hoặc kéo thả file vào cửa sổ.");
                }
            }
        });
    }

    fn crop_view(&mut self, ui: &mut egui::Ui) {
        let Some(info) = self.info.clone() else {
            ui.centered_and_justified(|ui| ui.heading("Kéo thả video vào đây"));
            return;
        };
        let avail = ui.available_rect_before_wrap();
        // Khung vẽ = vùng được phép đặt ô cắt (rộng hơn video khi bật mở rộng).
        let bounds = crop_bounds(info.width, info.height, self.allow_outside);
        let (bx0, by0, bx1, by1) = bounds;
        let (cw, ch) = ((bx1 - bx0) as f32, (by1 - by0) as f32);
        let aspect = cw / ch;
        let mut size = avail.size();
        if size.x / size.y > aspect {
            size.x = size.y * aspect;
        } else {
            size.y = size.x / aspect;
        }
        let canvas = Rect::from_center_size(avail.center(), size);
        let scale = canvas.width() / cw;
        let to_screen = |x: f32, y: f32| {
            Pos2::new(
                canvas.min.x + (x - bx0 as f32) * scale,
                canvas.min.y + (y - by0 as f32) * scale,
            )
        };
        let img_rect = Rect::from_min_max(
            to_screen(0.0, 0.0),
            to_screen(info.width as f32, info.height as f32),
        );
        let resp = ui.allocate_rect(canvas, Sense::click_and_drag());
        let painter = ui.painter_at(avail);
        if self.allow_outside {
            let bg = self
                .params
                .as_ref()
                .map(|p| p.background)
                .unwrap_or_default();
            match bg {
                Background::Color([r, g, b]) => {
                    painter.rect_filled(canvas, CornerRadius::ZERO, Color32::from_rgb(r, g, b));
                }
                Background::Transparent => paint_checker(&painter, canvas),
            }
        }
        painter.rect_filled(img_rect, CornerRadius::ZERO, Color32::from_gray(20));
        let playing_tex = self
            .play_index()
            .and_then(|i| self.play_cache.as_ref()?.frames.get(i));
        if let Some(tex) = playing_tex.or(self.texture.as_ref()) {
            painter.image(
                tex.id(),
                img_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        if self.allow_outside {
            painter.rect_stroke(
                img_rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, Color32::from_white_alpha(90)),
                StrokeKind::Outside,
            );
        }

        let to_src = |p: Pos2| {
            (
                (p.x - canvas.min.x) / scale + bx0 as f32,
                (p.y - canvas.min.y) / scale + by0 as f32,
            )
        };
        let Some(params) = self.params.as_mut() else {
            return;
        };
        let c = params.crop;
        let crop_rect = Rect::from_min_size(
            to_screen(c.x as f32, c.y as f32),
            Vec2::splat(c.size as f32 * scale),
        );

        // --- tương tác
        let handle_r = 10.0;
        let corners = [
            crop_rect.left_top(),
            crop_rect.right_top(),
            crop_rect.left_bottom(),
            crop_rect.right_bottom(),
        ];
        if let Some(pos) = resp.hover_pos() {
            let near_corner = corners.iter().any(|k| k.distance(pos) < handle_r * 1.5);
            if near_corner {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
            } else if crop_rect.contains(pos) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        if resp.drag_started()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            if let Some(i) = (0..4).find(|&i| corners[i].distance(pos) < handle_r * 1.5) {
                let opp = corners[3 - i];
                self.crop_drag = Some(CropDrag::Resize {
                    anchor: to_src(opp),
                });
            } else if crop_rect.contains(pos) {
                self.crop_drag = Some(CropDrag::Move {
                    grab: pos - crop_rect.min,
                });
            } else {
                // Kéo ở ngoài: vẽ vùng mới từ điểm bấm.
                self.crop_drag = Some(CropDrag::Resize {
                    anchor: to_src(pos),
                });
            }
        }
        if resp.dragged()
            && let (Some(drag), Some(pos)) = (self.crop_drag, resp.interact_pointer_pos())
        {
            params.crop = match drag {
                CropDrag::Move { grab } => {
                    let (x, y) = to_src(pos - grab);
                    CropSquare {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        size: c.size,
                    }
                    .clamped_to(bounds)
                }
                CropDrag::Resize { anchor } => resize_from_anchor(anchor, to_src(pos), bounds),
            };
        }
        if resp.drag_stopped() {
            self.crop_drag = None;
        }
        // Lăn chuột: phóng/thu vùng cắt quanh tâm.
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let factor = (1.0 - scroll * 0.002).clamp(0.5, 1.5);
                let cx = c.x as f32 + c.size as f32 / 2.0;
                let cy = c.y as f32 + c.size as f32 / 2.0;
                let max = (bx1 - bx0).min(by1 - by0) as u32;
                let ns = ((c.size as f32 * factor).round() as u32).clamp(16.min(max), max);
                params.crop = CropSquare {
                    x: (cx - ns as f32 / 2.0).round() as i32,
                    y: (cy - ns as f32 / 2.0).round() as i32,
                    size: ns,
                }
                .clamped_to(bounds);
            }
        }

        // --- vẽ lớp phủ
        let c = params.crop;
        let crop_rect = Rect::from_min_size(
            to_screen(c.x as f32, c.y as f32),
            Vec2::splat(c.size as f32 * scale),
        );
        let shade = Color32::from_black_alpha(160);
        let area = if self.allow_outside { canvas } else { img_rect };
        for r in [
            Rect::from_min_max(area.min, Pos2::new(area.max.x, crop_rect.min.y)),
            Rect::from_min_max(Pos2::new(area.min.x, crop_rect.max.y), area.max),
            Rect::from_min_max(
                Pos2::new(area.min.x, crop_rect.min.y),
                Pos2::new(crop_rect.min.x, crop_rect.max.y),
            ),
            Rect::from_min_max(
                Pos2::new(crop_rect.max.x, crop_rect.min.y),
                Pos2::new(area.max.x, crop_rect.max.y),
            ),
        ] {
            if r.is_positive() {
                painter.rect_filled(r, CornerRadius::ZERO, shade);
            }
        }
        let thin = Stroke::new(1.0, Color32::from_white_alpha(70));
        for k in 1..3 {
            let f = k as f32 / 3.0;
            let x = crop_rect.min.x + crop_rect.width() * f;
            let y = crop_rect.min.y + crop_rect.height() * f;
            painter.line_segment(
                [Pos2::new(x, crop_rect.min.y), Pos2::new(x, crop_rect.max.y)],
                thin,
            );
            painter.line_segment(
                [Pos2::new(crop_rect.min.x, y), Pos2::new(crop_rect.max.x, y)],
                thin,
            );
        }
        painter.rect_stroke(
            crop_rect,
            CornerRadius::ZERO,
            Stroke::new(2.0, ACCENT),
            StrokeKind::Middle,
        );
        for k in [
            crop_rect.left_top(),
            crop_rect.right_top(),
            crop_rect.left_bottom(),
            crop_rect.right_bottom(),
        ] {
            painter.circle_filled(k, 6.0, ACCENT);
            painter.circle_stroke(k, 6.0, Stroke::new(1.5, Color32::WHITE));
        }
        painter.text(
            crop_rect.left_top() + Vec2::new(6.0, 6.0),
            Align2::LEFT_TOP,
            format!("{}×{} @ ({}, {})", c.size, c.size, c.x, c.y),
            FontId::proportional(13.0),
            Color32::WHITE,
        );
        if self.playing.is_none() && (self.texture.is_none() || self.frame_in_flight) {
            painter.text(
                img_rect.right_bottom() - Vec2::splat(8.0),
                Align2::RIGHT_BOTTOM,
                "đang tải frame…",
                FontId::proportional(12.0),
                Color32::LIGHT_GRAY,
            );
        }
    }

    fn timeline(&mut self, ui: &mut egui::Ui) {
        let Some(info) = self.info.clone() else {
            return;
        };
        let dur = info.duration.max(0.001);
        let step = 1.0 / info.fps.max(1.0);
        let budget = self.selection_budget();

        // Hàng điều khiển
        ui.horizontal_wrapped(|ui| {
            let orig_label = if self.original.is_some() {
                "⏸ Dừng video gốc"
            } else {
                "▶ Phát video gốc"
            };
            if ui
                .button(orig_label)
                .on_hover_text(
                    "Phát toàn bộ video gốc từ vị trí con trỏ đỏ, tốc độ 1x (phím Space)",
                )
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.toggle_original(&ctx);
            }
            let play_label = if self.playing.is_some() {
                "⏸ Dừng"
            } else if self.play_pending {
                "⏳ Đang tải…"
            } else {
                "▶ Phát vùng chọn"
            };
            if ui
                .button(play_label)
                .on_hover_text("Phát lặp vùng chọn đúng FPS và tốc độ sẽ xuất")
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.toggle_play(&ctx);
            }
            if ui.button("⏮").on_hover_text("Lùi 1 frame").clicked() {
                self.original = None;
                self.playhead = (self.playhead - step).max(0.0);
            }
            if ui.button("⏭").on_hover_text("Tiến 1 frame").clicked() {
                self.original = None;
                self.playhead = (self.playhead + step).min(dur);
            }
            ui.label("Vị trí:");
            ui.add(
                egui::DragValue::new(&mut self.playhead)
                    .range(0.0..=dur)
                    .speed(0.01)
                    .fixed_decimals(3)
                    .suffix(" s"),
            );
            ui.separator();
            if ui
                .button("[ Đặt điểm đầu")
                .on_hover_text("Đặt điểm đầu vùng chọn tại vị trí hiện tại")
                .clicked()
            {
                let len = self.selection.len().min(budget);
                self.selection.start = self.playhead.min(dur - MIN_SEG).max(0.0);
                self.selection.end = (self.selection.start + len.max(MIN_SEG)).min(dur);
            }
            if ui
                .button("Đặt điểm cuối ]")
                .on_hover_text("Đặt điểm cuối vùng chọn tại vị trí hiện tại")
                .clicked()
            {
                let end = self.playhead.max(MIN_SEG);
                self.selection.end = end;
                if self.selection.start >= end - MIN_SEG {
                    self.selection.start = (end - MIN_SEG).max(0.0);
                }
                if self.selection.len() > budget {
                    self.selection.start = end - budget;
                }
            }
            ui.separator();
            ui.label("Từ");
            let mut s = self.selection.start;
            let mut e = self.selection.end;
            ui.add(
                egui::DragValue::new(&mut s)
                    .range(0.0..=dur)
                    .speed(0.01)
                    .fixed_decimals(3)
                    .suffix(" s"),
            );
            ui.label("đến");
            ui.add(
                egui::DragValue::new(&mut e)
                    .range(0.0..=dur)
                    .speed(0.01)
                    .fixed_decimals(3)
                    .suffix(" s"),
            );
            if s != self.selection.start || e != self.selection.end {
                self.selection =
                    clamp_selection(Segment::new(s, e), dur, budget, s != self.selection.start);
            }
            ui.label(format!("= {:.3}s", self.selection.len()));
            ui.separator();
            let can_add = self.segments.iter().map(Segment::len).sum::<f64>()
                + self.selection.len()
                <= MAX_DURATION * self.params.as_ref().map_or(1.0, |p| p.speed) + 1e-6;
            if ui
                .add_enabled(can_add, egui::Button::new("+ Thêm vào danh sách ghép"))
                .on_hover_text(
                    "Lưu vùng chọn thành một đoạn. Các đoạn sẽ được ghép nối theo thứ tự.",
                )
                .on_disabled_hover_text("Tổng thời lượng sẽ vượt 3s")
                .clicked()
            {
                self.segments.push(self.selection);
                let left = self.selection_budget();
                let start = self.selection.end.min(dur - MIN_SEG);
                self.selection = Segment::new(start, (start + left).min(dur));
            }
        });

        // Thanh timeline
        let (rect, resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), 46.0),
            Sense::click_and_drag(),
        );
        let painter = ui.painter_at(rect.expand(2.0));
        let bar = Rect::from_min_max(
            rect.min + Vec2::new(8.0, 14.0),
            rect.max - Vec2::new(8.0, 6.0),
        );
        let x_of = |t: f64| bar.min.x + (t / dur) as f32 * bar.width();
        let t_of = |x: f32| (((x - bar.min.x) / bar.width()) as f64 * dur).clamp(0.0, dur);
        painter.rect_filled(bar, CornerRadius::same(4), Color32::from_gray(45));

        // vạch giây
        let tick_every = nice_tick(dur, bar.width());
        let mut t = 0.0;
        while t <= dur + 1e-9 {
            let x = x_of(t);
            painter.line_segment(
                [Pos2::new(x, bar.min.y - 4.0), Pos2::new(x, bar.min.y)],
                Stroke::new(1.0, Color32::GRAY),
            );
            painter.text(
                Pos2::new(x + 2.0, rect.min.y),
                Align2::LEFT_TOP,
                format!("{t:.1}"),
                FontId::proportional(10.0),
                Color32::GRAY,
            );
            t += tick_every;
        }
        for (i, s) in self.segments.iter().enumerate() {
            let r = Rect::from_min_max(
                Pos2::new(x_of(s.start), bar.min.y + 2.0),
                Pos2::new(x_of(s.end), bar.max.y - 2.0),
            );
            let col = SEG_COLORS[i % SEG_COLORS.len()];
            painter.rect_filled(r, CornerRadius::same(2), col.gamma_multiply(0.55));
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                format!("{}", i + 1),
                FontId::proportional(12.0),
                Color32::BLACK,
            );
        }
        let sel = Rect::from_min_max(
            Pos2::new(x_of(self.selection.start), bar.min.y),
            Pos2::new(x_of(self.selection.end), bar.max.y),
        );
        painter.rect_filled(sel, CornerRadius::ZERO, ACCENT.gamma_multiply(0.35));
        painter.rect_stroke(
            sel,
            CornerRadius::ZERO,
            Stroke::new(1.5, ACCENT),
            StrokeKind::Middle,
        );
        for x in [sel.min.x, sel.max.x] {
            painter.rect_filled(
                Rect::from_center_size(
                    Pos2::new(x, bar.center().y),
                    Vec2::new(6.0, bar.height() + 6.0),
                ),
                CornerRadius::same(2),
                ACCENT,
            );
        }
        let px = x_of(self.playhead);
        painter.line_segment(
            [Pos2::new(px, rect.min.y + 10.0), Pos2::new(px, rect.max.y)],
            Stroke::new(2.0, Color32::from_rgb(255, 80, 80)),
        );

        // tương tác
        if (resp.drag_started() || resp.clicked())
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let near = |x: f32| (pos.x - x).abs() < 8.0;
            self.time_drag = Some(if near(sel.min.x) {
                TimeDrag::Start
            } else if near(sel.max.x) {
                TimeDrag::End
            } else if sel.contains(pos) && resp.drag_started() {
                TimeDrag::Body {
                    grab: t_of(pos.x) - self.selection.start,
                }
            } else {
                TimeDrag::Playhead
            });
            self.playing = None;
            // Đang phát video gốc: dừng trong lúc tua, thả chuột thì phát tiếp từ vị trí mới.
            self.resume_original = self.original.take().is_some();
        }
        if let (Some(drag), Some(pos)) = (self.time_drag, resp.interact_pointer_pos())
            && (resp.dragged() || resp.clicked() || resp.drag_started())
        {
            let t = t_of(pos.x);
            match drag {
                TimeDrag::Start => {
                    self.selection =
                        clamp_selection(Segment::new(t, self.selection.end), dur, budget, true);
                    self.playhead = self.selection.start;
                }
                TimeDrag::End => {
                    self.selection =
                        clamp_selection(Segment::new(self.selection.start, t), dur, budget, false);
                    self.playhead = self.selection.end;
                }
                TimeDrag::Body { grab } => {
                    let len = self.selection.len();
                    let start = (t - grab).clamp(0.0, (dur - len).max(0.0));
                    self.selection = Segment::new(start, start + len);
                    self.playhead = start;
                }
                TimeDrag::Playhead => self.playhead = t,
            }
        }
        if resp.drag_stopped() || resp.clicked() {
            self.time_drag = None;
            if std::mem::take(&mut self.resume_original) {
                let ctx = ui.ctx().clone();
                self.toggle_original(&ctx);
            }
        }
        if let Some(pos) = resp.hover_pos()
            && ((pos.x - sel.min.x).abs() < 8.0 || (pos.x - sel.max.x).abs() < 8.0)
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }

        // Danh sách đoạn ghép
        ui.horizontal_wrapped(|ui| {
            if self.segments.is_empty() {
                ui.weak("Danh sách ghép trống → sẽ xuất vùng chọn hiện tại. Bấm \"Thêm vào danh sách ghép\" để nối nhiều đoạn.");
                return;
            }
            ui.label("Ghép:");
            let mut remove = None;
            let mut swap = None;
            let n = self.segments.len();
            for (i, s) in self.segments.iter().enumerate() {
                ui.group(|ui| {
                    ui.colored_label(SEG_COLORS[i % SEG_COLORS.len()], format!("#{}", i + 1));
                    if ui.link(format!("{:.2}–{:.2}s ({:.2}s)", s.start, s.end, s.len())).on_hover_text("Chọn lại đoạn này").clicked() {
                        self.selection = *s;
                        self.playhead = s.start;
                    }
                    if i > 0 && ui.small_button("◀").on_hover_text("Đưa lên trước").clicked() {
                        swap = Some((i, i - 1));
                    }
                    if i + 1 < n && ui.small_button("▶").on_hover_text("Đưa ra sau").clicked() {
                        swap = Some((i, i + 1));
                    }
                    if ui.small_button("✕").on_hover_text("Xoá đoạn").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some((a, b)) = swap {
                self.segments.swap(a, b);
            }
            if let Some(i) = remove {
                self.segments.remove(i);
            }
            if ui.button("Xoá hết").clicked() {
                self.segments.clear();
            }
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(info) = self.info.clone() else {
            ui.heading("Video sticker Telegram");
            ui.label("Yêu cầu đầu ra:\n• WEBM / VP9, không audio\n• 1:1, tối đa 512px (sticker: 512, emoji: 100)\n• ≤ 3 giây, ≤ 30 FPS\n• ≤ 256 KB");
            return;
        };
        let exporting = self.export.is_some();
        let segs = self.effective_segments();
        let seg_source: f64 = segs.iter().map(Segment::len).sum();
        // Tạm lấy params ra khỏi self để closure dưới dùng được cả hai.
        let Some(mut params_owned) = self.params.take() else {
            return;
        };
        let params = &mut params_owned;
        let mut want_export = false;

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_enabled_ui(!exporting, |ui| {
                ui.heading("Vùng cắt (1:1)");
                let (w, h) = (info.width, info.height);
                if ui
                    .checkbox(&mut self.allow_outside, "Mở rộng ra ngoài video (thêm nền)")
                    .on_hover_text("Cho ô vuông vượt ra ngoài khung video; phần thừa được tô màu nền")
                    .changed()
                    && !self.allow_outside
                {
                    params.crop = params.crop.clamped(w, h);
                }
                let bounds = crop_bounds(w, h, self.allow_outside);
                let (bx0, by0, bx1, by1) = bounds;
                let maxs = (bx1 - bx0).min(by1 - by0) as u32;
                let mut c = params.crop;
                egui::Grid::new("crop").num_columns(2).show(ui, |ui| {
                    ui.label("X");
                    ui.add(egui::DragValue::new(&mut c.x).range(bx0..=bx1 - 1));
                    ui.end_row();
                    ui.label("Y");
                    ui.add(egui::DragValue::new(&mut c.y).range(by0..=by1 - 1));
                    ui.end_row();
                    ui.label("Cạnh");
                    ui.add(
                        egui::DragValue::new(&mut c.size)
                            .range(1..=maxs)
                            .suffix(" px"),
                    );
                    ui.end_row();
                });
                if c != params.crop {
                    params.crop = c.clamped_to(bounds);
                }
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("Lấy trọn video")
                        .on_hover_text("Ô vuông cạnh dài nhất, chứa toàn bộ video; phần thừa là nền")
                        .clicked()
                    {
                        self.allow_outside = true;
                        params.crop = CropSquare::fit_all(w, h);
                    }
                    if ui.button("Giữa, tối đa").clicked() {
                        params.crop = CropSquare::centered(w, h);
                    }
                    if ui.button("Căn giữa").clicked() {
                        let s = params.crop.size as i32;
                        params.crop = CropSquare {
                            x: (w as i32 - s) / 2,
                            y: (h as i32 - s) / 2,
                            size: params.crop.size,
                        }
                        .clamped_to(bounds);
                    }
                });
                if self.allow_outside {
                    ui.horizontal(|ui| {
                        ui.label("Màu nền:");
                        let mut transparent = params.background == Background::Transparent;
                        let mut rgb = match params.background {
                            Background::Color(c) => c,
                            Background::Transparent => self.last_bg_color,
                        };
                        ui.add_enabled_ui(!transparent, |ui| {
                            ui.color_edit_button_srgb(&mut rgb);
                        });
                        ui.checkbox(&mut transparent, "Trong suốt");
                        self.last_bg_color = rgb;
                        params.background = if transparent {
                            Background::Transparent
                        } else {
                            Background::Color(rgb)
                        };
                    });
                }
                ui.weak("Kéo ô vuông để di chuyển, kéo góc để đổi cỡ, lăn chuột để phóng/thu.");
                if params.crop.size < params.out_size {
                    ui.colored_label(
                        Color32::YELLOW,
                        format!(
                            "Vùng cắt {}px nhỏ hơn đầu ra {}px → sẽ bị phóng to, có thể mờ.",
                            params.crop.size, params.out_size
                        ),
                    );
                }

                ui.separator();
                ui.heading("Kích thước đầu ra");
                ui.horizontal(|ui| {
                    if ui
                        .radio_value(&mut self.size_mode, SizeMode::Sticker, "Sticker 512")
                        .changed()
                    {
                        params.out_size = MAX_SIDE;
                    }
                    if ui
                        .radio_value(&mut self.size_mode, SizeMode::Emoji, "Emoji 100")
                        .changed()
                    {
                        params.out_size = EMOJI_SIDE;
                    }
                    ui.radio_value(&mut self.size_mode, SizeMode::Custom, "Tuỳ chỉnh");
                });
                if self.size_mode == SizeMode::Custom {
                    ui.add(egui::Slider::new(&mut params.out_size, 16..=MAX_SIDE).suffix(" px"));
                    if params.out_size != MAX_SIDE {
                        ui.colored_label(
                            Color32::YELLOW,
                            "Telegram yêu cầu sticker có cạnh 512px (emoji 100px).",
                        );
                    }
                }
                ui.label(format!("→ {}×{} px", params.out_size, params.out_size));

                ui.separator();
                ui.heading("Thời lượng");
                ui.horizontal(|ui| {
                    ui.label("FPS");
                    ui.add(egui::Slider::new(&mut params.fps, 1..=MAX_FPS));
                });
                ui.horizontal(|ui| {
                    ui.label("Tốc độ");
                    ui.add(
                        egui::Slider::new(&mut params.speed, 0.25..=4.0)
                            .logarithmic(true)
                            .fixed_decimals(2)
                            .suffix("x"),
                    );
                });
                ui.weak("Vùng chọn tối đa 3s trên video gốc. Tốc độ > 1x làm đoạn đó chạy nhanh hơn (video ra ngắn hơn).");
                if ui.button("Tốc độ gốc 1x").clicked() {
                    params.speed = 1.0;
                }
                let out = seg_source / params.speed;
                let ok = out <= MAX_DURATION + 1e-6;
                ui.label(format!("Đoạn gốc: {seg_source:.3}s"));
                ui.colored_label(
                    if ok {
                        Color32::LIGHT_GREEN
                    } else {
                        Color32::from_rgb(255, 90, 90)
                    },
                    format!("Đầu ra: {out:.3}s / tối đa {MAX_DURATION:.0}s"),
                );

                ui.separator();
                ui.heading("Chất lượng");
                ui.horizontal(|ui| {
                    ui.label("CRF");
                    ui.add(egui::Slider::new(&mut params.crf, 4..=50))
                        .on_hover_text(
                            "Nhỏ = đẹp hơn, file lớn hơn. App tự giảm bitrate nếu vượt 256 KB.",
                        );
                });

                ui.separator();
                ui.heading("File đầu ra");
                ui.horizontal(|ui| {
                    ui.label(
                        params
                            .output
                            .file_name()
                            .map(|s| s.to_string_lossy())
                            .unwrap_or_default(),
                    )
                    .on_hover_text(params.output.display().to_string());
                    if ui.button("Chọn…").clicked() {
                        let mut d = rfd::FileDialog::new().add_filter("WEBM", &["webm"]);
                        if let Some(dir) = params.output.parent() {
                            d = d.set_directory(dir);
                        }
                        if let Some(name) = params.output.file_name() {
                            d = d.set_file_name(name.to_string_lossy());
                        }
                        if let Some(mut p) = d.save_file() {
                            p.set_extension("webm");
                            params.output = p;
                        }
                    }
                });
            });

            ui.separator();
            let validation = {
                let mut p = params.clone();
                p.segments = segs.clone();
                Some(p.validate(&info))
            };
            match &self.export {
                Some(job) => {
                    let pr = *job.progress.lock().unwrap();
                    ui.add(
                        egui::ProgressBar::new(pr.fraction)
                            .show_percentage()
                            .text(format!(
                                "Lần {} • {:.0}%  ({:.0}s)",
                                pr.attempt,
                                pr.fraction * 100.0,
                                job.started.elapsed().as_secs_f32()
                            )),
                    );
                    if pr.attempt > 1 {
                        ui.weak("File vượt 256 KB → đang nén lại với bitrate thấp hơn.");
                    }
                    if ui.button("Huỷ").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                }
                None => {
                    let valid = matches!(validation, Some(Ok(())));
                    let btn = egui::Button::new(egui::RichText::new("💾  Xuất sticker").size(18.0))
                        .min_size(Vec2::new(ui.available_width(), 36.0));
                    if ui.add_enabled(valid, btn).clicked() {
                        want_export = true;
                    }
                    if let Some(Err(e)) = &validation {
                        ui.colored_label(Color32::from_rgb(255, 90, 90), format!("{e:#}"));
                    }
                }
            }

            if let Some(r) = &self.result {
                ui.separator();
                ui.colored_label(Color32::LIGHT_GREEN, "✔ Đã xuất");
                ui.label(format!(
                    "{}×{} • {:.3}s • {:.1} KB\n{} kbps, CRF {}, {} lần encode",
                    r.width,
                    r.height,
                    r.duration,
                    r.bytes as f64 / 1024.0,
                    r.bitrate_kbps,
                    r.crf,
                    r.attempts
                ));
                ui.label(r.output.display().to_string());
                ui.horizontal(|ui| {
                    if ui.button("Mở thư mục").clicked() {
                        open_in_explorer(&r.output);
                    }
                });
            }
            if self.result_rx.is_some() {
                ui.spinner();
            }
            if let Some(rp) = &self.result_preview
                && !rp.frames.is_empty()
            {
                let idx = ((rp.started.elapsed().as_secs_f64() * rp.fps as f64) as usize)
                    % rp.frames.len();
                let side = ui.available_width().min(RESULT_PREVIEW as f32);
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
                paint_checker(ui.painter(), rect);
                ui.painter().image(
                    rp.frames[idx].id(),
                    rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
                ui.weak("Xem trước kết quả (lặp lại)");
                ctx.request_repaint();
            }
        });
        self.params = Some(params_owned);
        if want_export {
            self.start_export(ctx);
        }
    }
}

impl eframe::App for StickerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_workers(&ctx);

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if let Some(p) = dropped.first()
            && self.export.is_none()
        {
            self.open(p);
        }

        // Space: phát / dừng video gốc (khi không gõ trong ô nhập số).
        let typing = ctx.memory(|m| m.focused().is_some());
        if self.info.is_some() && !typing && ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.toggle_original(&ctx);
        }

        // Đổi tốc độ < 1 làm giảm giới hạn → co vùng chọn lại cho vừa.
        let budget = self.selection_budget();
        if self.info.is_some() && self.selection.len() > budget + 1e-9 {
            self.selection.end = self.selection.start + budget;
        }

        // Phát vùng chọn từ các frame đã giải mã sẵn.
        if self.playing.is_some() {
            let cache_key = self.play_cache.as_ref().map(|c| c.key.clone());
            if cache_key.is_none() || cache_key != self.play_key() {
                // Vùng chọn / FPS đổi trong lúc phát → dừng, để không phát nhầm đoạn cũ.
                self.playing = None;
            } else if let (Some(i), Some(k)) = (self.play_index(), cache_key) {
                self.playhead = k.start + i as f64 / k.fps as f64;
                ctx.request_repaint();
            }
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            if let Some(e) = self.error.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(255, 90, 90), e);
                    if ui.small_button("✕").clicked() {
                        self.error = None;
                    }
                });
            }
            ui.add_space(4.0);
        });
        egui::Panel::right("side")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                self.side_panel(ui, &ctx);
            });
        if self.info.is_some() {
            egui::Panel::bottom("timeline").show(ui, |ui| {
                ui.add_space(4.0);
                self.timeline(ui);
                ui.add_space(4.0);
            });
        }
        egui::CentralPanel::default().show(ui, |ui| self.crop_view(ui));

        self.request_frame();
        if self.frame_in_flight {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
    }
}

// ---------------------------------------------------------------------- helpers

fn spawn_frame_worker(ctx: egui::Context) -> (Sender<FrameRequest>, Receiver<FrameReply>) {
    let (req_tx, req_rx) = mpsc::channel::<FrameRequest>();
    let (rep_tx, rep_rx) = mpsc::channel::<FrameReply>();
    std::thread::spawn(move || {
        while let Ok(mut req) = req_rx.recv() {
            // Chỉ xử lý yêu cầu mới nhất.
            while let Ok(newer) = req_rx.try_recv() {
                req = newer;
            }
            let frame =
                ffmpeg::extract_frame(&req.path, req.time, req.w, req.h, req.decoder.as_deref());
            if rep_tx
                .send(FrameReply {
                    path: req.path,
                    time: req.time,
                    frame,
                })
                .is_err()
            {
                break;
            }
            ctx.request_repaint();
        }
    });
    (req_tx, rep_rx)
}

fn load_all_frames(path: &Path, side: u32) -> anyhow::Result<(Vec<ColorImage>, u32)> {
    let info = probe::probe(path)?;
    let bin = ffmpeg::ffmpeg()?;
    let mut cmd = ffmpeg::command(&bin);
    cmd.args(["-hide_banner", "-loglevel", "error"]);
    if let Some(dec) = &info.alpha_decoder {
        cmd.args(["-c:v", dec]);
    }
    cmd.arg("-i")
        .arg(path)
        .args(["-vf"])
        .arg(format!("scale={side}:{side}"))
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
    let out = ffmpeg::run_checked(cmd, "ffmpeg (result preview)")?;
    let fsz = (side * side * 4) as usize;
    let frames: Vec<ColorImage> = out
        .stdout
        .chunks_exact(fsz)
        .map(|c| ColorImage::from_rgba_unmultiplied([side as usize, side as usize], c))
        .collect();
    if frames.is_empty() {
        anyhow::bail!("Result file has no frames");
    }
    Ok((frames, info.fps.round().max(1.0) as u32))
}

fn setup_fonts(ctx: &egui::Context) {
    // Font mặc định của egui thiếu một số dấu tiếng Việt → nạp font hệ thống nếu có.
    let candidates = [
        "C:\\Windows\\Fonts\\segoeui.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    for c in candidates {
        if let Ok(bytes) = std::fs::read(c) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("system".into(), egui::FontData::from_owned(bytes).into());
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(fam)
                    .or_default()
                    .insert(0, "system".into());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
    eprintln!("No system font with Vietnamese glyphs found, using egui default font.");
}

fn default_output(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sticker".into());
    input.with_file_name(format!("{stem}_sticker.webm"))
}

fn fit_dims(w: u32, h: u32, max: u32) -> (u32, u32) {
    let s = (max as f64 / w.max(h) as f64).min(1.0);
    let ew = ((w as f64 * s).round() as u32).max(2) & !1;
    let eh = ((h as f64 * s).round() as u32).max(2) & !1;
    (ew, eh)
}

/// Tạo hình vuông từ điểm neo đến con trỏ, nằm trọn trong khung.
fn resize_from_anchor(anchor: (f32, f32), pos: (f32, f32), bounds: Bounds) -> CropSquare {
    let (x0, y0, x1, y1) = (
        bounds.0 as f32,
        bounds.1 as f32,
        bounds.2 as f32,
        bounds.3 as f32,
    );
    let (ax, ay) = (anchor.0.clamp(x0, x1), anchor.1.clamp(y0, y1));
    let left = pos.0 < ax;
    let up = pos.1 < ay;
    let room_x = if left { ax - x0 } else { x1 - ax };
    let room_y = if up { ay - y0 } else { y1 - ay };
    let want = (pos.0 - ax).abs().max((pos.1 - ay).abs());
    let size = want
        .min(room_x)
        .min(room_y)
        .max(16.0f32.min((x1 - x0).min(y1 - y0)))
        .round() as u32;
    let x = if left { ax - size as f32 } else { ax };
    let y = if up { ay - size as f32 } else { ay };
    CropSquare {
        x: x.round() as i32,
        y: y.round() as i32,
        size,
    }
    .clamped_to(bounds)
}

/// Giới hạn độ dài vùng chọn trên video gốc: tối đa 3s (trừ phần đã có trong danh sách ghép).
/// Tốc độ > 1x chỉ làm video ra ngắn hơn, không nới vùng chọn; tốc độ < 1x (chậm lại)
/// thì phải thu vùng chọn để đầu ra vẫn <= 3s.
fn selection_budget(speed: f64, used: f64) -> f64 {
    (MAX_DURATION * speed.min(1.0) - used).max(MIN_SEG)
}

/// FPS khi xem video gốc: giữ FPS gốc (tối đa 60) để thấy đúng chuyển động thật.
fn original_play_fps(source_fps: f64) -> u32 {
    (source_fps.round() as u32).clamp(1, 60)
}

/// Chỉ số frame khi phát: `elapsed` giây thực × `speed`, lặp vòng qua `n` frame ở `fps`.
fn play_frame_index(elapsed: f64, speed: f64, fps: u32, n: usize) -> Option<usize> {
    if n == 0 || fps == 0 {
        return None;
    }
    Some(((elapsed * speed * fps as f64).floor() as usize) % n)
}

/// Giữ vùng chọn hợp lệ: trong [0, dur], dài >= MIN_SEG và <= budget.
/// `moved_start`: đầu nào vừa được chỉnh (đầu còn lại được kéo theo khi vượt budget).
fn clamp_selection(s: Segment, dur: f64, budget: f64, moved_start: bool) -> Segment {
    let budget = budget.max(MIN_SEG);
    let mut start = s.start.clamp(0.0, (dur - MIN_SEG).max(0.0));
    let mut end = s.end.clamp(MIN_SEG.min(dur), dur);
    if moved_start {
        start = start.min(end - MIN_SEG).max(0.0);
        if end - start > budget {
            end = start + budget;
        }
    } else {
        end = end.max(start + MIN_SEG).min(dur);
        if end - start > budget {
            start = end - budget;
        }
    }
    Segment::new(start, end)
}

fn nice_tick(dur: f64, width: f32) -> f64 {
    let target = dur / (width as f64 / 60.0).max(1.0);
    for t in [
        0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ] {
        if t >= target {
            return t;
        }
    }
    1200.0
}

fn paint_checker(p: &egui::Painter, rect: Rect) {
    let n = 8;
    let cell = rect.width() / n as f32;
    for i in 0..n {
        for j in 0..n {
            let g = if (i + j) % 2 == 0 { 200 } else { 150 };
            let r = Rect::from_min_size(
                rect.min + Vec2::new(i as f32 * cell, j as f32 * cell),
                Vec2::splat(cell),
            );
            p.rect_filled(r, CornerRadius::ZERO, Color32::from_gray(g));
        }
    }
}

fn open_in_explorer(path: &Path) {
    #[cfg(windows)]
    let r = std::process::Command::new("explorer")
        .arg("/select,")
        .arg(path)
        .spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(Path::new(".")))
        .spawn();
    if let Err(e) = r {
        eprintln!("Cannot open folder: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_up_does_not_extend_selection_beyond_3s() {
        assert_eq!(selection_budget(1.0, 0.0), 3.0);
        assert_eq!(selection_budget(4.0, 0.0), 3.0);
        assert_eq!(selection_budget(4.0, 1.0), 2.0);
        // chậm lại 0.5x: 1.5s gốc → 3s đầu ra
        assert_eq!(selection_budget(0.5, 0.0), 1.5);
        assert_eq!(selection_budget(1.0, 3.0), MIN_SEG);
    }

    #[test]
    fn original_playback_keeps_source_fps_up_to_60() {
        assert_eq!(original_play_fps(29.97), 30);
        assert_eq!(original_play_fps(59.94), 60);
        assert_eq!(original_play_fps(120.0), 60);
        assert_eq!(original_play_fps(0.0), 1);
    }

    #[test]
    fn playback_index_follows_speed_and_loops() {
        assert_eq!(play_frame_index(0.0, 1.0, 30, 90), Some(0));
        assert_eq!(play_frame_index(1.0, 1.0, 30, 90), Some(30));
        assert_eq!(play_frame_index(1.0, 2.0, 30, 90), Some(60));
        assert_eq!(play_frame_index(3.5, 1.0, 30, 90), Some(15));
        assert_eq!(play_frame_index(1.0, 1.0, 30, 0), None);
    }

    #[test]
    fn selection_never_exceeds_budget() {
        let s = clamp_selection(Segment::new(1.0, 9.0), 10.0, 3.0, false);
        assert_eq!(s, Segment::new(6.0, 9.0));
        let s = clamp_selection(Segment::new(1.0, 9.0), 10.0, 3.0, true);
        assert_eq!(s, Segment::new(1.0, 4.0));
        let s = clamp_selection(Segment::new(5.0, 4.0), 10.0, 3.0, true);
        assert!(s.len() >= MIN_SEG - 1e-9 && s.end <= 10.0);
    }

    #[test]
    fn resize_stays_square_and_inside() {
        let inside = crop_bounds(1280, 720, false);
        let c = resize_from_anchor((100.0, 100.0), (2000.0, 150.0), inside);
        assert_eq!(
            c,
            CropSquare {
                x: 100,
                y: 100,
                size: 620
            }
        );
        let c = resize_from_anchor((100.0, 100.0), (0.0, 0.0), inside);
        assert_eq!(
            c,
            CropSquare {
                x: 0,
                y: 0,
                size: 100
            }
        );
    }

    #[test]
    fn resize_can_extend_outside_when_allowed() {
        // Video dọc 464x848: kéo từ góc phải dưới ra hết bên trái → ô 848 phủ trọn video.
        let b = crop_bounds(464, 848, true);
        let c = resize_from_anchor((464.0, 848.0), (-1000.0, -1000.0), b);
        assert_eq!(
            c,
            CropSquare {
                x: -384,
                y: 0,
                size: 848
            }
        );
        // Không cho mở rộng → kẹt trong video.
        let c = resize_from_anchor(
            (464.0, 848.0),
            (-1000.0, -1000.0),
            crop_bounds(464, 848, false),
        );
        assert_eq!(
            c,
            CropSquare {
                x: 0,
                y: 384,
                size: 464
            }
        );
    }

    #[test]
    fn preview_dims_fit_and_even() {
        assert_eq!(fit_dims(1920, 1080, 960), (960, 540));
        assert_eq!(fit_dims(321, 241, 960), (320, 240));
    }
}
