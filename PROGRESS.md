# PROGRESS

Verify: `cargo test` (needs ffmpeg/ffprobe on PATH).

- [x] Probe video with ffprobe (rotation, fps, audio, alpha) — `probe::tests`
- [x] Telegram constraints: ≤3s, 1:1, ≤512px, ≤30 FPS, no audio, VP9/WEBM — `sticker::tests`, `tests/encode.rs`
- [x] Length control: selection capped at 3s of source; speed only speeds up that range — `speed_up_does_not_extend_selection_beyond_3s`, `selection_never_exceeds_budget`
- [x] Square crop on the frame (drag/resize/scroll) — `resize_stays_square_and_inside`, `crop_outside_frame_*`, `encodes_single_segment_with_crop`
- [x] Crop extending beyond the video with background (black default, any color, transparent) — `bounds_allow_square_covering_portrait_video`, `fit_all_pads_with_black_by_default`, `partial_overflow_pads_only_needed_side`, `transparent_background_outputs_alpha_only_when_padding`, `crop_without_any_video_is_rejected`, `resize_can_extend_outside_when_allowed`, `portrait_fit_all_adds_black_side_bars`, `portrait_fit_all_with_transparent_background`
- [x] Timeline range selection + joining segments — `filter_for_multiple_segments_concats`, `concatenates_segments_with_speed_and_emoji_size`
- [x] Re-encode when over 256 KB — `small_size_limit_forces_reencode`
- [x] Preview frame grab — `extracts_preview_frame`
- [x] Smooth selection playback (decode range once) — `extracts_range_for_smooth_playback`, `playback_index_follows_speed_and_loops`
- [x] Original video playback (ffmpeg stream, Space, seek while playing) — `streams_original_video_until_end`, `stream_stops_cleanly_when_dropped_early`, `original_playback_keeps_source_fps_up_to_60`; manual: Space → playhead at 2.500s after 2.5s
- [x] Logs / errors / CLI output / GUI labels / docs in English
- [x] egui GUI + CLI — manual: GUI screenshots; CLI demo 512x512 3.000s 57.8 KB
