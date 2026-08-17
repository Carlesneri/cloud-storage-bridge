use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _},
    process::Command,
};

use crate::{wait_cancelled, CANCELLED};

pub const MAY_NOT_PLAY: &str = "may not play in browser";

const IMAGE_SUBTITLE_CODECS: &[&str] = &[
    "hdmv_pgs_subtitle",
    "pgs_subtitle",
    "dvd_subtitle",
    "dvb_subtitle",
    "dvb_teletext",
    "arib_caption",
];

#[derive(Clone, Serialize)]
struct PrepareStartEvent {
    index: usize,
    path: String,
    action: String,
    duration: f64,
}

#[derive(Clone, Serialize)]
struct PrepareProgressEvent {
    index: usize,
    path: String,
    seconds: f64,
    duration: f64,
}

#[derive(Clone, Debug)]
pub struct Sidecars {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

#[cfg(target_os = "macos")]
const OS_TRIPLE: &str = "apple-darwin";
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
const OS_TRIPLE: &str = "unknown-linux-gnu";
#[cfg(target_os = "windows")]
const OS_TRIPLE: &str = "pc-windows-msvc";

fn exe_suffix() -> &'static str {
    if cfg!(target_os = "windows") {
        ".exe"
    } else {
        ""
    }
}

/// Locate the bundled ffmpeg/ffprobe sidecar binaries: next to the app
/// executable when bundled, or in `src-tauri/binaries/` during development.
pub fn resolve_sidecars() -> Option<Sidecars> {
    let suffix = exe_suffix();
    let triple = format!("{}-{}", std::env::consts::ARCH, OS_TRIPLE);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries"));
    for dir in &dirs {
        let ffmpeg = dir.join(format!("ffmpeg{suffix}"));
        let ffprobe = dir.join(format!("ffprobe{suffix}"));
        if ffmpeg.is_file() && ffprobe.is_file() {
            return Some(Sidecars { ffmpeg, ffprobe });
        }
        let ffmpeg = dir.join(format!("ffmpeg-{triple}{suffix}"));
        let ffprobe = dir.join(format!("ffprobe-{triple}{suffix}"));
        if ffmpeg.is_file() && ffprobe.is_file() {
            return Some(Sidecars { ffmpeg, ffprobe });
        }
    }
    None
}

#[derive(Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    #[serde(default)]
    codec_type: String,
    #[serde(default)]
    codec_name: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    avg_frame_rate: String,
    #[serde(default)]
    r_frame_rate: String,
    #[serde(default)]
    disposition: HashMap<String, Option<i64>>,
    #[serde(default)]
    tags: HashMap<String, Option<String>>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    #[serde(default)]
    format_name: String,
    #[serde(default)]
    duration: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ProbeInfo {
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub subtitles: Vec<SubtitleTrack>,
    pub duration: f64,
    pub format_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

#[derive(Clone, Debug)]
pub struct SubtitleTrack {
    pub index: usize,
    pub codec: String,
    pub language: Option<String>,
}

fn is_attached_pic(s: &ProbeStream) -> bool {
    s.disposition
        .get("attached_pic")
        .and_then(|v| *v)
        .unwrap_or(0)
        == 1
}

/// Parse an ffprobe rational like "24000/1001" into frames per second.
fn parse_frame_rate(rate: &str) -> Option<f64> {
    let (num, den) = rate.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    if den > 0.0 {
        Some(num / den)
    } else {
        None
    }
}

async fn probe(sidecars: &Sidecars, input: &Path) -> Result<ProbeInfo, String> {
    let out = Command::new(&sidecars.ffprobe)
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_streams")
        .arg("-show_format")
        .arg(input)
        .output()
        .await
        .map_err(|e| format!("ffprobe failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let parsed: FfprobeOutput =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("ffprobe output: {e}"))?;

    let mut info = ProbeInfo {
        video_codec: None,
        audio_codec: None,
        subtitles: Vec::new(),
        duration: 0.0,
        format_name: parsed
            .format
            .as_ref()
            .map(|f| f.format_name.clone())
            .unwrap_or_default(),
        width: 0,
        height: 0,
        fps: 0.0,
    };
    if let Some(d) = parsed.format.as_ref().and_then(|f| f.duration.as_deref()) {
        info.duration = d.trim().parse().unwrap_or(0.0);
    }

    for s in &parsed.streams {
        if is_attached_pic(s) {
            continue;
        }
        match s.codec_type.as_str() {
            "video" => {
                if info.video_codec.is_none() {
                    info.video_codec = Some(s.codec_name.clone());
                    info.width = s.width;
                    info.height = s.height;
                    info.fps = parse_frame_rate(&s.avg_frame_rate)
                        .or_else(|| parse_frame_rate(&s.r_frame_rate))
                        .unwrap_or(0.0);
                }
            }
            "audio" => {
                if info.audio_codec.is_none() {
                    info.audio_codec = Some(s.codec_name.clone());
                }
            }
            "subtitle" => {
                let index = info.subtitles.len();
                info.subtitles.push(SubtitleTrack {
                    index,
                    codec: s.codec_name.clone(),
                    language: s
                        .tags
                        .get("language")
                        .cloned()
                        .flatten()
                        .map(|l| l.trim().to_ascii_lowercase())
                        .filter(|l| !l.is_empty()),
                });
            }
            _ => {}
        }
    }
    Ok(info)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoAction {
    None,
    Remux,
    TranscodeAudio,
    TranscodeVideo,
}

/// Decide what to do with a video based on the probe result and its extension.
/// Only `.mp4`/`.m4v` files that really hold H.264 + AAC are considered done.
pub fn decide_action(info: &ProbeInfo, ext: &str) -> VideoAction {
    let Some(video) = info.video_codec.as_deref() else {
        return VideoAction::None;
    };
    let audio_ok = matches!(info.audio_codec.as_deref(), None | Some("aac"));
    let container_ok = matches!(ext, "mp4" | "m4v")
        && info
            .format_name
            .split(',')
            .any(|f| f == "mp4" || f == "mov");
    match (video, audio_ok, container_ok) {
        ("h264", true, true) => VideoAction::None,
        ("h264", true, false) => VideoAction::Remux,
        ("h264", false, _) => VideoAction::TranscodeAudio,
        _ => VideoAction::TranscodeVideo,
    }
}

pub fn action_label(action: VideoAction) -> &'static str {
    match action {
        VideoAction::None => "checking",
        VideoAction::Remux => "remuxing",
        VideoAction::TranscodeAudio => "converting audio",
        VideoAction::TranscodeVideo => "transcoding",
    }
}

/// The uploaded key for a video: same name, `.mp4` extension.
pub fn mp4_key(relative: &str) -> String {
    let relative = relative.trim_start_matches('/');
    let after_slash = relative.rfind('/').map_or(0, |p| p + 1);
    match relative.rfind('.') {
        Some(pos) if pos > after_slash => format!("{}.mp4", &relative[..pos]),
        _ => format!("{relative}.mp4"),
    }
}

/// An object key with its final extension removed (used to derive subtitle
/// sidecar keys that share the video's base name).
pub fn key_stem(key: &str) -> String {
    let after_slash = key.rfind('/').map_or(0, |p| p + 1);
    match key.rfind('.') {
        Some(pos) if pos > after_slash => key[..pos].to_string(),
        _ => key.to_string(),
    }
}

fn file_stem_of(name: &str) -> String {
    match name.rfind('.') {
        Some(pos) if pos > 0 => name[..pos].to_string(),
        _ => name.to_string(),
    }
}

/// `<basename>.vtt` for the first track, `<basename>.<lang|sN>.vtt` for the
/// rest (deduplicated with the track index when tags repeat or are missing).
pub fn vtt_names(tracks: &[SubtitleTrack], base: &str) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    let mut names = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        let candidate = if i == 0 {
            format!("{base}.vtt")
        } else {
            let tag = t
                .language
                .clone()
                .unwrap_or_else(|| format!("s{}", t.index + 1));
            format!("{base}.{tag}.vtt")
        };
        let name = if used.contains(&candidate) {
            format!("{}.s{}.vtt", file_stem_of(&candidate), t.index + 1)
        } else {
            candidate
        };
        used.push(name.clone());
        names.push(name);
    }
    names
}

pub struct Prepared {
    pub file: PathBuf,
    pub size: u64,
    pub warning: Option<String>,
    pub temp_dir: Option<PathBuf>,
    /// (local file, suffix inserted between the key stem and `.vtt`)
    pub sidecars: Vec<(PathBuf, String)>,
}

/// Progress events emitted while preparing a file.
pub enum PrepEvent {
    Start { action: &'static str, duration: f64 },
    Progress { seconds: f64, duration: f64 },
}

fn emit_prepare_start(app: &AppHandle, index: usize, path: &str, action: &str, duration: f64) {
    let _ = app.emit(
        "upload://prepare-start",
        PrepareStartEvent {
            index,
            path: path.to_string(),
            action: action.to_string(),
            duration,
        },
    );
}

fn emit_prepare_progress(app: &AppHandle, index: usize, path: &str, seconds: f64, duration: f64) {
    let _ = app.emit(
        "upload://prepare-progress",
        PrepareProgressEvent {
            index,
            path: path.to_string(),
            seconds,
            duration,
        },
    );
}

/// Run ffmpeg, reporting progress parsed from `-progress pipe:1`.
async fn run_ffmpeg(
    sidecars: &Sidecars,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    args: &[String],
    duration: f64,
    with_progress: bool,
    on_event: &mut (dyn FnMut(PrepEvent) + Send),
) -> Result<(), String> {
    if *cancel_rx.borrow_and_update() {
        return Err(CANCELLED.to_string());
    }
    let mut command_args: Vec<String> = Vec::with_capacity(args.len() + 2);
    if with_progress {
        command_args.extend(["-progress", "pipe:1", "-nostats"].map(String::from));
    }
    command_args.extend(args.iter().cloned());

    let mut child = Command::new(&sidecars.ffmpeg)
        .args(&command_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("ffmpeg failed to start: {e}"))?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let read_progress = async {
        let mut reader = tokio::io::BufReader::new(&mut stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Some(value) = line.trim().strip_prefix("out_time_us=") {
                        if let Ok(us) = value.trim().parse::<f64>() {
                            let seconds = us / 1_000_000.0;
                            if duration <= 0.0 || seconds <= duration + 1.0 {
                                on_event(PrepEvent::Progress { seconds, duration });
                            }
                        }
                    }
                }
            }
        }
    };
    let read_stderr = async {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    };

    let run = async {
        let ((), err) = tokio::join!(read_progress, read_stderr);
        let status = child.wait().await.map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            let tail = err.lines().rev().take(4).collect::<Vec<_>>().join(" | ");
            Err(if tail.is_empty() {
                format!("ffmpeg exited with {status}")
            } else {
                tail
            })
        }
    };

    tokio::select! {
        _ = wait_cancelled(cancel_rx) => {
            let _ = child.kill().await;
            Err(CANCELLED.to_string())
        }
        res = run => res,
    }
}

/// Cap ffmpeg's CPU usage so transcodes don't saturate every core and
/// thermally throttle the machine: at most half the logical cores
/// (bounded to 2..8). Applied to both the decoder and the encoder.
fn transcode_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .div_ceil(2)
        .clamp(2, 8)
}

fn base_conversion_args(input: &Path, threads: Option<usize>) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(threads) = threads {
        args.extend(["-threads", &threads.to_string()].map(String::from));
    }
    args.push("-i".to_string());
    args.push(input.to_string_lossy().into_owned());
    args
}

/// H.264 hardware encoders bundled with the sidecar, in preference order.
const HW_ENCODERS: &[&str] = &["h264_videotoolbox", "h264_nvenc", "h264_qsv", "h264_amf"];

/// Rough visual-quality target bitrate for hardware encoders (most of them
/// don't support CRF): ~0.08 bits per pixel per frame, clamped to a sane
/// range.
fn hw_bitrate(width: u32, height: u32, fps: f64) -> u64 {
    let fps = if fps > 0.0 { fps } else { 25.0 };
    let pixels = width.max(1) as f64 * height.max(1) as f64;
    ((pixels * fps * 0.08) as u64).clamp(500_000, 20_000_000)
}

/// Quality/speed arguments per hardware encoder. `bitrate_bps` is used by
/// the encoders that have no quality-mode support in the bundled builds
/// (VideoToolbox in ffmpeg 6.1 rejects `-q:v`, AMF defaults are low).
fn hw_encoder_args(encoder: &str, bitrate_bps: u64) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    match encoder {
        // -allow_sw 1 lets VideoToolbox fall back to its software path on
        // machines without a hardware encoder instead of failing outright.
        "h264_videotoolbox" => {
            args.extend(["-b:v", &bitrate_bps.to_string()].map(String::from));
            args.extend(["-allow_sw", "1"].map(String::from));
        }
        "h264_nvenc" => {
            args.extend(["-preset", "p4", "-rc", "vbr", "-cq", "23"].map(String::from));
        }
        "h264_qsv" => {
            args.extend(["-preset", "veryfast", "-global_quality", "23"].map(String::from));
        }
        "h264_amf" => {
            args.extend(["-quality", "speed", "-b:v", &bitrate_bps.to_string()].map(String::from));
        }
        _ => {}
    }
    args
}

static HW_ENCODER: OnceLock<Option<&'static str>> = OnceLock::new();

/// Check that an encoder actually works on this machine (being listed by
/// `-encoders` does not mean the GPU/driver is present).
async fn hw_encoder_works(sidecars: &Sidecars, encoder: &str) -> bool {
    let out = Command::new(&sidecars.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=black:size=128x96:rate=10:duration=0.2",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            encoder,
        ])
        .args(hw_encoder_args(encoder, 2_000_000))
        .args(["-f", "null", "-"])
        .output()
        .await;
    matches!(out, Ok(o) if o.status.success())
}

/// Pick the best usable hardware H.264 encoder, cached for the process.
async fn detect_hw_encoder(sidecars: &Sidecars) -> Option<&'static str> {
    if let Some(cached) = HW_ENCODER.get() {
        return *cached;
    }
    let found = async {
        let out = Command::new(&sidecars.ffmpeg)
            .args(["-hide_banner", "-encoders"])
            .output()
            .await
            .ok()?;
        let list = String::from_utf8_lossy(&out.stdout).into_owned();
        for encoder in HW_ENCODERS {
            if list.contains(encoder) && hw_encoder_works(sidecars, encoder).await {
                return Some(*encoder);
            }
        }
        None
    }
    .await;
    let _ = HW_ENCODER.set(found);
    found
}

/// How to encode the video track of a full transcode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EncoderChoice {
    Hardware(&'static str),
    Libx264,
}

/// Args for a full video transcode attempt (faststart and the output path
/// are appended by the caller). Hardware encoders run uncapped — the GPU
/// does the work — while the libx264 fallback keeps the CPU thread cap.
fn transcode_args(
    input: &Path,
    enc: EncoderChoice,
    threads: usize,
    bitrate_bps: u64,
) -> Vec<String> {
    let mut args = match enc {
        EncoderChoice::Hardware(_) => base_conversion_args(input, None),
        EncoderChoice::Libx264 => base_conversion_args(input, Some(threads)),
    };
    args.extend(["-map", "0:v:0", "-map", "0:a:0?"].map(String::from));
    match enc {
        EncoderChoice::Hardware(name) => {
            args.extend(["-c:v", name].map(String::from));
            args.extend(hw_encoder_args(name, bitrate_bps));
            // Hardware encoders reject odd dimensions; round down to even.
            args.extend(["-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2"].map(String::from));
            args.extend(["-pix_fmt", "yuv420p"].map(String::from));
        }
        EncoderChoice::Libx264 => {
            args.extend(
                [
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-crf",
                    "20",
                    "-pix_fmt",
                    "yuv420p",
                ]
                .iter()
                .map(|s| s.to_string()),
            );
            args.extend(["-threads", &threads.to_string()].map(String::from));
        }
    }
    args.extend(["-c:a", "aac", "-b:a", "192k"].iter().map(|s| s.to_string()));
    args
}

/// Try each ffmpeg attempt in order; only cancellation aborts the chain.
/// Returns Ok(true) when an attempt produced the output file.
async fn run_transcode_attempts(
    sidecars: &Sidecars,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    attempts: &[Vec<String>],
    output: &Path,
    duration: f64,
    on_event: &mut (dyn FnMut(PrepEvent) + Send),
) -> Result<Result<(), String>, String> {
    let mut last_err = String::new();
    for args in attempts {
        match run_ffmpeg(sidecars, cancel_rx, args, duration, true, on_event).await {
            Ok(()) => {
                if output.is_file() {
                    return Ok(Ok(()));
                }
                last_err = "no output file produced".to_string();
            }
            Err(e) if e == CANCELLED => return Err(CANCELLED.to_string()),
            Err(e) => last_err = e,
        }
    }
    Ok(Err(last_err))
}

/// Prepare one video file for browser playback. Falls back to uploading the
/// original (with a warning) if ffmpeg fails; only cancellation propagates.
pub async fn prepare_video(
    sidecars: &Sidecars,
    input: &Path,
    relative: &str,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    on_event: &mut (dyn FnMut(PrepEvent) + Send),
) -> Result<Prepared, String> {
    let original_size = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);
    let fallback = |warning: Option<String>| Prepared {
        file: input.to_path_buf(),
        size: original_size,
        warning,
        temp_dir: None,
        sidecars: Vec::new(),
    };

    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let info = match probe(sidecars, input).await {
        Ok(i) => i,
        Err(_) => return Ok(fallback(Some(MAY_NOT_PLAY.to_string()))),
    };
    if info.video_codec.is_none() {
        return Ok(fallback(None));
    }
    let action = decide_action(&info, &ext);
    if action == VideoAction::None && info.subtitles.is_empty() {
        return Ok(fallback(None));
    }

    let file_name = relative
        .trim_start_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("video");
    let base = file_stem_of(file_name);

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_dir = std::env::temp_dir().join(format!("cloud-storage-bridge-{unique}"));
    if let Err(e) = tokio::fs::create_dir_all(&temp_dir).await {
        return Ok(fallback(Some(format!("{MAY_NOT_PLAY} (temp dir: {e})"))));
    }

    let mut warning = None;
    let mut result_file = input.to_path_buf();

    if action != VideoAction::None {
        let output = temp_dir.join(format!("{base}.mp4"));
        // Attempts to try in order: a hardware encoder first when one is
        // usable, then the capped libx264 CPU path as fallback. Remuxes and
        // audio-only transcodes have a single attempt.
        let attempts: Vec<Vec<String>> = match action {
            VideoAction::Remux => vec![
                base_conversion_args(input, None)
                    .into_iter()
                    .chain(["-map", "0:v:0", "-map", "0:a:0?", "-c", "copy"].map(String::from))
                    .collect(),
            ],
            VideoAction::TranscodeAudio => {
                let threads = transcode_threads();
                vec![base_conversion_args(input, Some(threads))
                    .into_iter()
                    .chain(
                        [
                            "-map",
                            "0:v:0",
                            "-map",
                            "0:a:0?",
                            "-c:v",
                            "copy",
                            "-c:a",
                            "aac",
                            "-b:a",
                            "192k",
                            "-threads",
                        ]
                        .iter()
                        .map(|s| s.to_string()),
                    )
                    .chain([threads.to_string()])
                    .collect()]
            }
            VideoAction::TranscodeVideo => {
                let threads = transcode_threads();
                let bitrate = hw_bitrate(info.width, info.height, info.fps);
                let mut list = Vec::new();
                if let Some(hw) = detect_hw_encoder(sidecars).await {
                    list.push(transcode_args(
                        input,
                        EncoderChoice::Hardware(hw),
                        threads,
                        bitrate,
                    ));
                }
                list.push(transcode_args(input, EncoderChoice::Libx264, threads, bitrate));
                list
            }
            VideoAction::None => unreachable!(),
        };
        let attempts: Vec<Vec<String>> = attempts
            .into_iter()
            .map(|mut args| {
                args.extend(["-movflags", "+faststart"].map(String::from));
                args.push(output.to_string_lossy().into_owned());
                args
            })
            .collect();

        on_event(PrepEvent::Start {
            action: action_label(action),
            duration: info.duration,
        });
        match run_transcode_attempts(
            sidecars,
            cancel_rx,
            &attempts,
            &output,
            info.duration,
            on_event,
        )
        .await
        {
            Ok(Ok(())) => result_file = output,
            Ok(Err(e)) => warning = Some(format!("{MAY_NOT_PLAY} (ffmpeg: {e})")),
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                return Err(e);
            }
        }
    }

    if *cancel_rx.borrow_and_update() {
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        return Err(CANCELLED.to_string());
    }

    // Subtitles: an external .srt/.vtt next to the video wins over embedded
    // tracks; otherwise every text subtitle stream is extracted as WebVTT.
    let mut sidecars_out: Vec<(PathBuf, String)> = Vec::new();
    if let Some(ext_path) = find_external_subtitle(input, &base) {
        let target = temp_dir.join(format!("{base}.vtt"));
        let ok = if ext_path.extension().and_then(|e| e.to_str()) == Some("vtt") {
            tokio::fs::copy(&ext_path, &target).await.is_ok()
        } else {
            let mut args = vec![
                "-hide_banner".to_string(),
                "-loglevel".to_string(),
                "error".to_string(),
                "-y".to_string(),
                "-i".to_string(),
                ext_path.to_string_lossy().into_owned(),
            ];
            args.push(target.to_string_lossy().into_owned());
            run_ffmpeg(sidecars, cancel_rx, &args, 0.0, false, on_event)
                .await
                .is_ok()
        };
        if ok {
            sidecars_out.push((target, String::new()));
        } else {
            warning = warning.or(Some("subtitles not converted".to_string()));
        }
    } else {
        let text_tracks: Vec<SubtitleTrack> = info
            .subtitles
            .iter()
            .filter(|t| !IMAGE_SUBTITLE_CODECS.contains(&t.codec.as_str()))
            .cloned()
            .collect();
        let names = vtt_names(&text_tracks, &base);
        for (track, name) in text_tracks.iter().zip(names) {
            if *cancel_rx.borrow_and_update() {
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                return Err(CANCELLED.to_string());
            }
            let target = temp_dir.join(&name);
            let mut args = base_conversion_args(input, None);
            args.push("-map".to_string());
            args.push(format!("0:s:{}", track.index));
            args.push(target.to_string_lossy().into_owned());
            match run_ffmpeg(sidecars, cancel_rx, &args, 0.0, false, on_event).await {
                Ok(()) if target.is_file() => {
                    let suffix = file_stem_of(&name)
                        .strip_prefix(&base)
                        .and_then(|s| s.strip_prefix('.'))
                        .unwrap_or("")
                        .to_string();
                    sidecars_out.push((target, suffix));
                }
                Err(e) if e == CANCELLED => {
                    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                    return Err(CANCELLED.to_string());
                }
                _ => {
                    warning = warning.or(Some(format!("subtitle track {} not extracted", track.index + 1)));
                }
            }
        }
    }

    let size = std::fs::metadata(&result_file)
        .map(|m| m.len())
        .unwrap_or(0);
    Ok(Prepared {
        file: result_file,
        size,
        warning,
        temp_dir: Some(temp_dir),
        sidecars: sidecars_out,
    })
}

/// Tauri-facing wrapper: forwards preparation events to the web UI.
pub async fn prepare_video_emitting(
    app: &AppHandle,
    sidecars: &Sidecars,
    input: &Path,
    relative: &str,
    index: usize,
    src_path: &str,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Prepared, String> {
    let mut on_event = move |event: PrepEvent| match event {
        PrepEvent::Start { action, duration } => {
            emit_prepare_start(app, index, src_path, action, duration);
        }
        PrepEvent::Progress { seconds, duration } => {
            emit_prepare_progress(app, index, src_path, seconds, duration);
        }
    };
    prepare_video(sidecars, input, relative, cancel_rx, &mut on_event).await
}

fn find_external_subtitle(input: &Path, base: &str) -> Option<PathBuf> {
    let dir = input.parent()?;
    let vtt = dir.join(format!("{base}.vtt"));
    if vtt.is_file() {
        return Some(vtt);
    }
    let srt = dir.join(format!("{base}.srt"));
    if srt.is_file() {
        return Some(srt);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(video: Option<&str>, audio: Option<&str>, format: &str) -> ProbeInfo {
        ProbeInfo {
            video_codec: video.map(String::from),
            audio_codec: audio.map(String::from),
            subtitles: Vec::new(),
            duration: 10.0,
            format_name: format.to_string(),
            width: 1920,
            height: 1080,
            fps: 24.0,
        }
    }

    #[test]
    fn decides_actions() {
        assert_eq!(
            decide_action(&info(Some("h264"), Some("aac"), "mov,mp4,m4a,3gp,3g2,mj2"), "mp4"),
            VideoAction::None
        );
        assert_eq!(
            decide_action(&info(Some("h264"), None, "mov,mp4,m4a,3gp,3g2,mj2"), "mp4"),
            VideoAction::None
        );
        assert_eq!(
            decide_action(&info(Some("h264"), Some("aac"), "matroska,webm"), "mkv"),
            VideoAction::Remux
        );
        assert_eq!(
            decide_action(&info(Some("h264"), Some("aac"), "avi"), "avi"),
            VideoAction::Remux
        );
        assert_eq!(
            decide_action(&info(Some("h264"), Some("ac3"), "matroska,webm"), "mkv"),
            VideoAction::TranscodeAudio
        );
        assert_eq!(
            decide_action(&info(Some("h264"), Some("eac3"), "avi"), "avi"),
            VideoAction::TranscodeAudio
        );
        assert_eq!(
            decide_action(&info(Some("hevc"), Some("aac"), "matroska,webm"), "mkv"),
            VideoAction::TranscodeVideo
        );
        assert_eq!(
            decide_action(&info(Some("vp9"), Some("opus"), "matroska,webm"), "webm"),
            VideoAction::TranscodeVideo
        );
        assert_eq!(
            decide_action(&info(Some("av1"), Some("aac"), "mov,mp4"), "mp4"),
            VideoAction::TranscodeVideo
        );
        // A matroska file misnamed as .mp4 still gets remuxed.
        assert_eq!(
            decide_action(&info(Some("h264"), Some("aac"), "matroska,webm"), "mp4"),
            VideoAction::Remux
        );
        assert_eq!(decide_action(&info(None, None, "mp3"), "mp3"), VideoAction::None);
    }

    #[test]
    fn builds_mp4_keys() {
        assert_eq!(mp4_key("Movie.2013.mkv"), "Movie.2013.mp4");
        assert_eq!(mp4_key("Movies/Movie.2013.mkv"), "Movies/Movie.2013.mp4");
        assert_eq!(mp4_key("a.b/c"), "a.b/c.mp4");
        assert_eq!(mp4_key("noext"), "noext.mp4");
        assert_eq!(key_stem("movies/Movie.2013.mp4"), "movies/Movie.2013");
        assert_eq!(key_stem("movies/Movie.2013.mkv"), "movies/Movie.2013");
        assert_eq!(key_stem("movies/noext"), "movies/noext");
    }

    fn track(index: usize, language: Option<&str>) -> SubtitleTrack {
        SubtitleTrack {
            index,
            codec: "subrip".to_string(),
            language: language.map(String::from),
        }
    }

    #[test]
    fn names_vtt_sidecars() {
        let tracks = vec![track(0, Some("eng")), track(1, Some("spa"))];
        assert_eq!(
            vtt_names(&tracks, "Movie.2013"),
            vec!["Movie.2013.vtt", "Movie.2013.spa.vtt"]
        );

        let untagged = vec![track(0, None), track(1, None)];
        assert_eq!(
            vtt_names(&untagged, "M"),
            vec!["M.vtt", "M.s2.vtt"]
        );

        let dup = vec![track(0, Some("eng")), track(1, Some("eng"))];
        assert_eq!(
            vtt_names(&dup, "M"),
            vec!["M.vtt", "M.eng.vtt"]
        );

        let dup3 = vec![track(0, Some("eng")), track(1, Some("eng")), track(2, Some("spa"))];
        assert_eq!(
            vtt_names(&dup3, "M"),
            vec!["M.vtt", "M.eng.vtt", "M.spa.vtt"]
        );
    }

    fn test_sidecars() -> Option<Sidecars> {
        resolve_sidecars()
    }

    #[test]
    fn computes_hw_bitrates() {
        // 1080p24 ≈ 4 Mbps
        assert_eq!(hw_bitrate(1920, 1080, 24.0), 3_981_312);
        // 720p30 ≈ 2.2 Mbps
        assert_eq!(hw_bitrate(1280, 720, 30.0), 2_211_840);
        // tiny/unknown inputs clamp to a floor, 4K clamps near the ceiling
        assert_eq!(hw_bitrate(0, 0, 0.0), 500_000);
        assert_eq!(hw_bitrate(3840, 2160, 60.0), 20_000_000);
    }

    #[test]
    fn builds_hw_transcode_args() {
        let args = transcode_args(
            Path::new("/in/v.mkv"),
            EncoderChoice::Hardware("h264_videotoolbox"),
            4,
            4_000_000,
        );
        assert!(args.contains(&"-c:v".to_string()));
        assert!(args.contains(&"h264_videotoolbox".to_string()));
        assert!(args.contains(&"-b:v".to_string()));
        assert!(args.contains(&"4000000".to_string()));
        assert!(args.contains(&"-allow_sw".to_string()));
        assert!(args.contains(&"scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string()));
        // GPU work: no thread cap anywhere.
        assert!(!args.contains(&"-threads".to_string()));

        let nv = transcode_args(
            Path::new("/in/v.mkv"),
            EncoderChoice::Hardware("h264_nvenc"),
            4,
            4_000_000,
        );
        assert!(nv.contains(&"h264_nvenc".to_string()));
        assert!(nv.contains(&"-cq".to_string()));
        assert!(!nv.contains(&"-b:v".to_string()));
    }

    #[test]
    fn builds_libx264_fallback_args() {
        let args = transcode_args(Path::new("/in/v.mkv"), EncoderChoice::Libx264, 4, 4_000_000);
        assert!(args.contains(&"libx264".to_string()));
        assert!(args.contains(&"veryfast".to_string()));
        assert!(args.contains(&"-crf".to_string()));
        assert!(args.contains(&"20".to_string()));
        // Thread cap on both the decoder side (before -i) and the encoder.
        assert_eq!(
            args.iter()
                .filter(|a| *a == "-threads")
                .count(),
            2
        );
        assert!(args.iter().position(|a| a == "-threads")
            < args.iter().position(|a| a == "-i"));
    }

    #[tokio::test]
    async fn transcode_falls_back_to_next_attempt() {
        let Some(sidecars) = test_sidecars() else {
            eprintln!("sidecars not fetched; skipping");
            return;
        };
        let dir = fixture_dir().await;
        let input = make_video(&sidecars, &dir, "v.mkv", "aac", false).await;
        let output = dir.join("out.mp4");
        let mut rx = no_cancel().await;
        let mut events = Vec::new();

        // First attempt uses a bogus encoder, second succeeds.
        let finalize = |mut args: Vec<String>| {
            args.extend(["-movflags", "+faststart"].map(String::from));
            args.push(output.to_string_lossy().into_owned());
            args
        };
        let good = finalize(transcode_args(
            &input,
            EncoderChoice::Libx264,
            transcode_threads(),
            4_000_000,
        ));
        let mut bogus_args = good.clone();
        let pos = bogus_args.iter().position(|a| a == "libx264").unwrap();
        bogus_args[pos] = "definitely_not_an_encoder".to_string();
        let attempts = vec![bogus_args, good];

        let res = run_transcode_attempts(
            &sidecars,
            &mut rx,
            &attempts,
            &output,
            2.0,
            &mut |e| events.push(e),
        )
        .await
        .expect("not cancelled");
        assert!(res.is_ok());
        assert!(output.is_file());

        // All attempts failing reports the last error.
        let output2 = dir.join("out2.mp4");
        let finalize2 = |mut args: Vec<String>| {
            args.extend(["-movflags", "+faststart"].map(String::from));
            args.push(output2.to_string_lossy().into_owned());
            args
        };
        let mut all_bad: Vec<Vec<String>> = attempts;
        let pos = all_bad[1].iter().position(|a| a == "libx264").unwrap();
        all_bad[1][pos] = "definitely_not_an_encoder".to_string();
        all_bad[1] = finalize2(all_bad[1].clone());
        let res = run_transcode_attempts(
            &sidecars,
            &mut rx,
            &all_bad,
            &output2,
            2.0,
            &mut |e| events.push(e),
        )
        .await
        .expect("not cancelled");
        assert!(res.is_err());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    async fn run_quiet(sidecars: &Sidecars, args: &[&str]) {
        let out = Command::new(&sidecars.ffmpeg)
            .args(args)
            .output()
            .await
            .expect("ffmpeg spawn");
        assert!(
            out.status.success(),
            "ffmpeg {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    async fn fixture_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("csb-test-{unique}"));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        dir
    }

    async fn make_video(
        sidecars: &Sidecars,
        dir: &Path,
        name: &str,
        audio: &str,
        with_subs: bool,
    ) -> PathBuf {
        let out = dir.join(name);
        if !with_subs {
            run_quiet(
                sidecars,
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=duration=2:size=128x96:rate=10",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=2",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    audio,
                    out.to_string_lossy().as_ref(),
                ],
            )
            .await;
            return out;
        }

        let tmp = dir.join("video.tmp.mkv");
        run_quiet(
            sidecars,
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=2:size=128x96:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                audio,
                tmp.to_string_lossy().as_ref(),
            ],
        )
        .await;
        let _ = tokio::fs::write(
            dir.join("sub0.srt"),
            "1\n00:00:00,000 --> 00:00:01,000\nhello\n",
        )
        .await;
        let _ = tokio::fs::write(
            dir.join("sub1.srt"),
            "1\n00:00:00,000 --> 00:00:01,000\nhola\n",
        )
        .await;
        run_quiet(
            sidecars,
            &[
                "-i",
                tmp.to_string_lossy().as_ref(),
                "-i",
                dir.join("sub0.srt").to_string_lossy().as_ref(),
                "-i",
                dir.join("sub1.srt").to_string_lossy().as_ref(),
                "-map",
                "0",
                "-map",
                "1",
                "-map",
                "2",
                "-c",
                "copy",
                "-metadata:s:s:0",
                "language=eng",
                "-metadata:s:s:1",
                "language=spa",
                out.to_string_lossy().as_ref(),
            ],
        )
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        out
    }

    async fn make_hevc(sidecars: &Sidecars, dir: &Path, name: &str) -> PathBuf {
        let out = dir.join(name);
        run_quiet(
            sidecars,
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=2:size=128x96:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                out.to_string_lossy()
                    .as_ref(),
            ],
        )
        .await;
        out
    }

    async fn no_cancel() -> tokio::sync::watch::Receiver<bool> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        std::mem::forget(tx); // keep the channel open for the whole test
        rx
    }

    async fn prep(sidecars: &Sidecars, input: &Path, relative: &str) -> Prepared {
        let mut rx = no_cancel().await;
        let mut events = Vec::new();
        let prepared = prepare_video(sidecars, input, relative, &mut rx, &mut |e| events.push(e))
            .await
            .expect("prepare_video");
        prepared
    }

    #[tokio::test]
    async fn prepares_videos_end_to_end() {
        let Some(sidecars) = test_sidecars() else {
            eprintln!("sidecars not fetched; skipping");
            return;
        };

        // (a) h264+aac mkv + two srt subs → remux + two vtt sidecars
        let dir = fixture_dir().await;
        let input = make_video(&sidecars, &dir, "Movie.2013.mkv", "aac", true).await;
        let info = probe(&sidecars, &input).await.unwrap();
        assert_eq!(info.video_codec.as_deref(), Some("h264"));
        assert_eq!(info.audio_codec.as_deref(), Some("aac"));
        assert_eq!(info.subtitles.len(), 2);
        assert_eq!(decide_action(&info, "mkv"), VideoAction::Remux);

        let prepared = prep(&sidecars, &input, "Movie.2013.mkv").await;
        assert!(prepared.warning.is_none());
        assert!(prepared.file.is_file());
        assert_eq!(
            prepared.file.file_name().unwrap().to_str().unwrap(),
            "Movie.2013.mp4"
        );
        assert_ne!(prepared.file, input);
        assert!(prepared.temp_dir.is_some());
        let out_info = probe(&sidecars, &prepared.file).await.unwrap();
        assert_eq!(out_info.video_codec.as_deref(), Some("h264"));
        assert_eq!(out_info.audio_codec.as_deref(), Some("aac"));
        assert!(out_info.format_name.contains("mp4"));
        assert!(out_info.subtitles.is_empty());
        let suffixes: Vec<&str> = prepared.sidecars.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(suffixes, vec!["", "spa"]);
        for (path, _) in &prepared.sidecars {
            let content = tokio::fs::read_to_string(path).await.unwrap();
            assert!(content.starts_with("WEBVTT"), "{}", content);
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let _ = tokio::fs::remove_dir_all(prepared.temp_dir.clone().unwrap()).await;

        // (b) h264+ac3 → audio transcode only
        let dir = fixture_dir().await;
        let input = make_video(&sidecars, &dir, "ac3.mkv", "ac3", false).await;
        let info = probe(&sidecars, &input).await.unwrap();
        assert_eq!(info.audio_codec.as_deref(), Some("ac3"));
        assert_eq!(decide_action(&info, "mkv"), VideoAction::TranscodeAudio);
        let prepared = prep(&sidecars, &input, "ac3.mkv").await;
        assert!(prepared.warning.is_none());
        let out_info = probe(&sidecars, &prepared.file).await.unwrap();
        assert_eq!(out_info.video_codec.as_deref(), Some("h264"));
        assert_eq!(out_info.audio_codec.as_deref(), Some("aac"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let _ = tokio::fs::remove_dir_all(prepared.temp_dir.clone().unwrap()).await;

        // (c) hevc → full transcode (hardware encoder when available,
        // e.g. VideoToolbox on macOS)
        let dir = fixture_dir().await;
        let input = make_hevc(&sidecars, &dir, "hevc.mkv").await;
        let info = probe(&sidecars, &input).await.unwrap();
        assert_eq!(info.video_codec.as_deref(), Some("hevc"));
        assert_eq!(decide_action(&info, "mkv"), VideoAction::TranscodeVideo);
        #[cfg(target_os = "macos")]
        assert_eq!(detect_hw_encoder(&sidecars).await, Some("h264_videotoolbox"));
        let prepared = prep(&sidecars, &input, "hevc.mkv").await;
        assert!(prepared.warning.is_none());
        let out_info = probe(&sidecars, &prepared.file).await.unwrap();
        assert_eq!(out_info.video_codec.as_deref(), Some("h264"));
        assert_eq!(out_info.audio_codec.as_deref(), Some("aac"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let _ = tokio::fs::remove_dir_all(prepared.temp_dir.clone().unwrap()).await;

        // mp4 that already plays → untouched
        let dir = fixture_dir().await;
        let input = make_video(&sidecars, &dir, "fine.mp4", "aac", false).await;
        let prepared = prep(&sidecars, &input, "fine.mp4").await;
        assert_eq!(prepared.file, input);
        assert!(prepared.temp_dir.is_none());
        assert!(prepared.warning.is_none());
        let _ = tokio::fs::remove_dir_all(&dir).await;

        // external .srt wins over embedded tracks
        let dir = fixture_dir().await;
        let input = make_video(&sidecars, &dir, "Movie.2013.mkv", "aac", true).await;
        tokio::fs::write(dir.join("Movie.2013.srt"), "1\n00:00:00,000 --> 00:00:01,000\nexternal\n")
            .await
            .unwrap();
        let prepared = prep(&sidecars, &input, "Movie.2013.mkv").await;
        assert_eq!(prepared.sidecars.len(), 1);
        assert_eq!(prepared.sidecars[0].1, "");
        let content = tokio::fs::read_to_string(&prepared.sidecars[0].0).await.unwrap();
        assert!(content.starts_with("WEBVTT"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let _ = tokio::fs::remove_dir_all(prepared.temp_dir.clone().unwrap()).await;

        // garbage input → fallback to original with warning
        let dir = fixture_dir().await;
        let input = dir.join("broken.mkv");
        tokio::fs::write(&input, b"not a video").await.unwrap();
        let prepared = prep(&sidecars, &input, "broken.mkv").await;
        assert_eq!(prepared.file, input);
        assert!(prepared.warning.is_some());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
