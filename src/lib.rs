//! Lõi tạo video sticker Telegram (WEBM/VP9) bằng ffmpeg.
//!
//! Yêu cầu theo https://core.telegram.org/stickers/webm-vp9-encoding :
//! - WEBM, codec VP9, tối đa 30 FPS, không có audio
//! - Sticker: một cạnh 512px (app này luôn xuất 1:1), emoji: 100x100
//! - Thời lượng <= 3s, dung lượng <= 256 KB

pub mod ffmpeg;
pub mod probe;
pub mod sticker;

pub use probe::VideoInfo;
pub use sticker::{CropSquare, EncodeResult, Segment, StickerParams};
