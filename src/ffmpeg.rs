//! Xây dựng lệnh ffmpeg và chạy ffprobe/ffmpeg qua `tokio::process::Command`.
//!
//! Giai đoạn 2 — pipeline 3 bước:
//! 1. **Chuẩn hoá** từng clip về cùng kích thước/fps/pix_fmt (`build_normalize_args`)
//!    để concat được với nhau (concat demuxer yêu cầu các đoạn đồng nhất).
//! 2. **Ghép** các clip đã chuẩn hoá thành 1 chuỗi: concat demuxer (`build_concat_args`)
//!    hoặc chuỗi `xfade` khi có chuyển cảnh (`build_xfade_args`).
//! 3. **Hoàn thiện** (`build_final_args`): lặp chuỗi cho hết audio, map audio
//!    radio (bỏ tiếng gốc), volume/fade, cắt `-t <audio_duration>`.
//!
//! Lọc hình ảnh của bước 1 được tạo bởi [`build_filter_complex`] — hàm thuần,
//! có unit test cho từng preset × từng chế độ khớp khung.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::jobs::AudioOptions;

// ---------------------------------------------------------------------------
// Định vị binary ffmpeg/ffprobe — dùng crate open-source `ffmpeg-sidecar`
// (https://github.com/nathanbabcock/ffmpeg-sidecar).
//
// Thứ tự resolve (chạy đúng 1 lần):
// 1. Override thủ công: `RVM_FFMPEG_PATH` + `RVM_FFPROBE_PATH`.
// 2. Đã tải về ./bin từ lần chạy trước.
// 3. Trong PATH (discovery của sidecar, hỗ trợ cả Windows).
// 4. Không có → TỰ TẢI bản build chính thức của ffmpeg về ./bin (Windows
//    không cần cài ffmpeg thủ công nữa).
// ---------------------------------------------------------------------------

/// Binary đã resolve: (ffmpeg, ffprobe).
static BINS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

fn bin_name(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Đường dẫn ffmpeg đã resolve — mọi spawn đều dùng hàm này.
pub fn ffmpeg_bin() -> &'static PathBuf {
    &bins().0
}

/// Đường dẫn ffprobe đã resolve.
pub fn ffprobe_bin() -> &'static PathBuf {
    &bins().1
}

fn bins() -> &'static (PathBuf, PathBuf) {
    BINS.get_or_init(|| {
        resolve_bins().unwrap_or_else(|e| {
            tracing::warn!("Không resolve được ffmpeg tự động ({e}) — dùng 'ffmpeg' trong PATH");
            (PathBuf::from("ffmpeg"), PathBuf::from("ffprobe"))
        })
    })
}

/// Resolve ffmpeg/ffprobe và kiểm tra chạy được NGAY lúc khởi động. Trước đây
/// `main` kiểm tra `which` rồi thoát trước khi kịp tới bước tự tải của sidecar
/// — máy không có ffmpeg không bao giờ được tải về. Giờ lần resolve (tự tải
/// nếu cần mạng) diễn ra ở đây và lỗi trả kèm hướng dẫn cài.
pub fn ensure_bins() -> Result<(), String> {
    for (bin, name) in [(ffmpeg_bin(), "ffmpeg"), (ffprobe_bin(), "ffprobe")] {
        let ok = std::process::Command::new(bin)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return Err(format!(
                "\n==================== THIẾU CÔNG CỤ: {name} ====================\n\
                 Ứng dụng cần ffmpeg và ffprobe để xử lý media (không chạy được `{}`).\n\
                 Hãy cài rồi chạy lại:\n\n\
                 Windows (PowerShell):\n  winget install ffmpeg\n\n\
                 macOS:\n  brew install ffmpeg\n\n\
                 Linux (Debian/Ubuntu):\n  sudo apt install ffmpeg\n\n\
                 Sau khi cài, mở terminal/cửa sổ mới để PATH được cập nhật.\n\
                 ===============================================================",
                bin.display()
            ));
        }
    }
    Ok(())
}

fn resolve_bins() -> Result<(PathBuf, PathBuf), String> {
    // 1. Override thủ công qua biến môi trường.
    if let (Ok(f), Ok(p)) = (
        std::env::var("RVM_FFMPEG_PATH"),
        std::env::var("RVM_FFPROBE_PATH"),
    ) {
        let (f, p) = (PathBuf::from(f), PathBuf::from(p));
        if f.exists() && p.exists() {
            tracing::info!("ffmpeg (env override): {}", f.display());
            return Ok((f, p));
        }
    }
    // 2. Đã tải về ./bin từ lần chạy trước (app tự quản).
    let bin_dir = PathBuf::from("bin");
    let (f, p) = (bin_dir.join(bin_name("ffmpeg")), bin_dir.join(bin_name("ffprobe")));
    if f.exists() && p.exists() {
        tracing::info!("ffmpeg (./bin): {}", f.display());
        return Ok((f, p));
    }
    // 3. Trong PATH hoặc cạnh binary (discovery của sidecar, đa nền tảng).
    if ffmpeg_sidecar::command::ffmpeg_is_installed()
        && ffmpeg_sidecar::ffprobe::ffprobe_is_installed()
    {
        let f = ffmpeg_sidecar::paths::ffmpeg_path();
        let p = ffmpeg_sidecar::ffprobe::ffprobe_path();
        tracing::info!("ffmpeg (hệ thống/sidecar): {}", f.display());
        return Ok((f, p));
    }
    // 4. Không có — tự tải bản build chính thức qua sidecar (mạng cần hoạt
    //    động). Binary được đặt cạnh executable → dùng luôn, copy thêm vào
    //    ./bin cho các lần chạy sau từ thư mục khác.
    tracing::info!("Không thấy ffmpeg — tự tải bản build chính thức qua ffmpeg-sidecar…");
    ffmpeg_sidecar::download::auto_download()
        .map_err(|e| format!("Tự tải ffmpeg thất bại: {e}"))?;
    let src_f = ffmpeg_sidecar::paths::ffmpeg_path();
    let src_p = ffmpeg_sidecar::ffprobe::ffprobe_path();
    if !src_f.exists() || !src_p.exists() {
        return Err("sidecar tải xong nhưng không tìm thấy binary".to_string());
    }
    let _ = std::fs::create_dir_all(&bin_dir);
    let dst_f = bin_dir.join(bin_name("ffmpeg"));
    let dst_p = bin_dir.join(bin_name("ffprobe"));
    if src_f != dst_f {
        let _ = std::fs::copy(&src_f, &dst_f);
        let _ = std::fs::copy(&src_p, &dst_p);
    }
    tracing::info!("ffmpeg (sidecar → {}): dùng ngay", src_f.display());
    Ok((src_f, src_p))
}

// ---------------------------------------------------------------------------
// FitMode
// ---------------------------------------------------------------------------

/// Cách đưa video nguồn vào khung đích.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitMode {
    /// Crop lấp đầy: scale cho cạnh nhỏ chạm khung rồi cắt phần tràn.
    Cover,
    /// Giữ nguyên + viền đen: scale vừa trong khung, pad phần thiếu.
    Pad,
    /// Giữ nguyên + nền mờ: scale-to-fill + boxblur làm nền, overlay ảnh
    /// rõ nét cỡ vừa ở giữa (nguồn được nhân đôi bằng `split`).
    Blur,
}

// ---------------------------------------------------------------------------
// MediaInfo + probe
// ---------------------------------------------------------------------------

/// Kết quả probe 1 file media bằng ffprobe.
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    pub has_video: bool,
    pub has_audio: bool,
    /// Thời lượng tính bằng giây (0 nếu ffprobe không đọc được).
    pub duration: f64,
    /// Độ phân giải video (None nếu không có stream video).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Góc xoay hiển thị (side-data rotation, độ) — 90/180/270 của video
    /// quay bằng điện thoại. 0 = không xoay.
    pub rotation: i32,
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    #[serde(default)]
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    #[serde(default)]
    side_data_list: Vec<FfprobeSideData>,
}

#[derive(Debug, Deserialize)]
struct FfprobeSideData {
    /// "Display Matrix" — chứa góc xoay hiển thị.
    side_data_type: Option<String>,
    rotation: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
}

/// Chạy `ffprobe -v error -show_entries stream=codec_type,width,height:format=duration -of json`.
pub async fn probe(path: &Path) -> Result<MediaInfo> {
    let mut probe_cmd = Command::new(ffprobe_bin().clone());
    crate::fontconfig::apply_env_tokio(&mut probe_cmd);
    let output = probe_cmd
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,width,height,side_data:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .await
        .context("Không chạy được ffprobe")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe không đọc được file: {}", stderr.trim());
    }

    let parsed: FfprobeOutput =
        serde_json::from_slice(&output.stdout).context("ffprobe trả về JSON không hợp lệ")?;

    let video = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"));
    let has_audio = parsed
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("audio"));

    let duration = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|d| d.parse::<f64>().ok())
        .unwrap_or(0.0);

    // Rotation hiển thị: side-data "Display Matrix" của stream video.
    // ffprobe in góc đã chuẩn hoá về [-180,180] — chuẩn về dương.
    let rotation = video
        .and_then(|v| {
            v.side_data_list
                .iter()
                .find(|sd| sd.side_data_type.as_deref() == Some("Display Matrix"))
                .and_then(|sd| sd.rotation)
        })
        .map(|r| (r.round() as i32).rem_euclid(360))
        .unwrap_or(0);

    Ok(MediaInfo {
        has_video: video.is_some(),
        has_audio,
        duration,
        width: video.and_then(|s| s.width),
        height: video.and_then(|s| s.height),
        rotation,
    })
}

// ---------------------------------------------------------------------------
// build_filter_complex — hàm thuần, trung tâm của việc khớp khung
// ---------------------------------------------------------------------------

/// Xây chuỗi lọc hình ảnh đưa luồng `input` (vd `[0:v]`) về khung `WxH`, ghi
/// kết quả vào nhãn `out` (vd `[v]`).
///
/// - `cover`: scale theo `force_original_aspect_ratio=increase` (lấp đầy) rồi
///   `crop` giữa đúng WxH.
/// - `pad`: scale theo `decrease` (vừa khung, không tràn) rồi `pad` đen quanh.
/// - `blur`: `split` nguồn làm 2 — nhánh A cover + `boxblur` + hạ sáng làm
///   nền, nhánh B scale vừa khung làm ảnh chính, `overlay` giữa nền.
///
/// Mọi kích thước trung gian được `force_divisible_by=2` làm chẵn (yuv420p).
pub fn build_filter_complex(input: &str, out: &str, width: u32, height: u32, fit: FitMode) -> String {
    // Chuẩn hoá nhãn: caller truyền "0:v" hoặc "[0:v]" đều được.
    let input = if input.starts_with('[') {
        input.to_string()
    } else {
        format!("[{input}]")
    };
    let out = if out.starts_with('[') {
        out.to_string()
    } else {
        format!("[{out}]")
    };
    // Nhãn trung gian cho blur: phái sinh từ out để chạy nhiều lần không đụng nhau.
    let base = out.trim_matches(|c| c == '[' || c == ']').to_string();
    match fit {
        FitMode::Cover => format!(
            "{input}scale=w='max({w},iw*{h}/ih)':h='max({h},ih*{w}/iw)':\
             force_original_aspect_ratio=increase:force_divisible_by=2,crop={w}:{h}{out}",
            w = width,
            h = height
        ),
        FitMode::Pad => format!(
            "{input}scale=w='min({w},iw*{h}/ih)':h='min({h},ih*{w}/iw)':\
             force_original_aspect_ratio=decrease:force_divisible_by=2,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black{out}",
            w = width,
            h = height
        ),
        FitMode::Blur => format!(
            "{input}split[{base}a][{base}b];\
             [{base}a]scale=w='max({w},iw*{h}/ih)':h='max({h},ih*{w}/iw)':\
             force_original_aspect_ratio=increase:force_divisible_by=2,crop={w}:{h},\
             boxblur=luma_radius=32:luma_power=2:chroma_radius=16:chroma_power=2,\
             eq=brightness=-0.15[{base}bg];\
             [{base}b]scale=w='min({w},iw*{h}/ih)':h='min({h},ih*{w}/iw)':\
             force_original_aspect_ratio=decrease:force_divisible_by=2[{base}fg];\
             [{base}bg][{base}fg]overlay=(W-w)/2:(H-h)/2{out}",
            w = width,
            h = height
        ),
    }
}

/// Filter khớp khung `(width, height)` **không label input/output** — dùng để
/// chèn ĐẦU chuỗi filter_complex của pass cuối (chain chung 1 lần cho nhiều
/// preset: scale/crop/blur từ chuỗi ghép khung chung sang khung đích).
/// `base` đặt tên label trung gian riêng để chạy nhiều lần không đụng nhau.
pub fn build_fit_filter(width: u32, height: u32, fit: FitMode, base: &str) -> String {
    match fit {
        FitMode::Cover => format!(
            "scale=w='max({w},iw*{h}/ih)':h='max({h},ih*{w}/iw)':\
             force_original_aspect_ratio=increase:force_divisible_by=2,crop={w}:{h}",
            w = width,
            h = height
        ),
        FitMode::Pad => format!(
            "scale=w='min({w},iw*{h}/ih)':h='min({h},ih*{w}/iw)':\
             force_original_aspect_ratio=decrease:force_divisible_by=2,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black",
            w = width,
            h = height
        ),
        FitMode::Blur => format!(
            "split[{b}a][{b}b];\
             [{b}a]scale=w='max({w},iw*{h}/ih)':h='max({h},ih*{w}/iw)':\
             force_original_aspect_ratio=increase:force_divisible_by=2,crop={w}:{h},\
             boxblur=luma_radius=32:luma_power=2:chroma_radius=16:chroma_power=2,\
             eq=brightness=-0.15[{b}bg];\
             [{b}b]scale=w='min({w},iw*{h}/ih)':h='min({h},ih*{w}/iw)':\
             force_original_aspect_ratio=decrease:force_divisible_by=2[{b}fg];\
             [{b}bg][{b}fg]overlay=(W-w)/2:(H-h)/2",
            w = width,
            h = height,
            b = base
        ),
    }
}

/// Args chuẩn hoá 1 clip video về `(width, height, fps)`: bỏ audio, bỏ metadata,
/// đúng pix_fmt, timescale đồng nhất — để concat demuxer ghép được.
///
/// `width/height = None` (preset "giữ nguyên gốc"): chỉ đặt fps + pix_fmt,
/// giữ nguyên kích thước từng clip. Nếu các clip khác kích thước, concat sẽ
/// lỗi và job hiển thị lỗi ffmpeg rõ ràng (caller đã cảnh báo trên UI).
pub fn build_normalize_args(
    input: &Path,
    output: &Path,
    width: Option<u32>,
    height: Option<u32>,
    fps: u32,
    fit: FitMode,
    rotation: i32,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-an".into(),
    ];

    // B1: -filter_complex TẮT autorotate của ffmpeg → tự áp transpose theo
    // rotation metadata (video quay điện thoại) TRƯỚC khi scale/crop.
    let rotate_filter = match rotation.rem_euclid(360) {
        90 => Some("transpose=1"),
        180 => Some("transpose=1,transpose=1"),
        270 => Some("transpose=2"),
        _ => None,
    };

    if let (Some(w), Some(h)) = (width, height) {
        let mut filter = build_filter_complex("0:v", "[v]", w, h, fit);
        if let Some(rf) = rotate_filter {
            // Chèn ngay sau input label: [0:v]transpose=1,scale=...
            filter = filter.replacen("[0:v]", &format!("[0:v]{rf},"), 1);
        }
        args.push("-filter_complex".into());
        args.push(filter);
        args.push("-map".into());
        args.push("[v]".into());
    } else {
        // Giữ nguyên gốc nhưng VẪN xoay đúng chiều hiển thị (người dùng
        // thấy video dọc thì output phải dọc), chốt size chẵn cho yuv420p.
        if let Some(rf) = rotate_filter {
            args.push("-vf".into());
            args.push(format!("{rf},scale=trunc(iw/2)*2:trunc(ih/2)*2"));
        }
        args.push("-map".into());
        args.push("0:v:0".into());
    }

    args.push("-r".into());
    args.push(fps.to_string());
    args.push("-c:v".into());
    args.push("libx264".into());
    args.push("-preset".into());
    args.push("veryfast".into());
    args.push("-crf".into());
    args.push("23".into());
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());
    args.push("-video_track_timescale".into());
    args.push("15360".into());
    args.push("-y".into());
    args.push(output.to_string_lossy().into_owned());
    args
}

// ---------------------------------------------------------------------------
// Bước 2: concat / xfade
// ---------------------------------------------------------------------------

/// Kiểm tra chuyển cảnh hợp lệ trước khi chạy ffmpeg (fail nhanh).
/// `dur <= 0` = tắt (dùng concat thường).
/// Các transition xfade an toàn — có mặt ổn định ở mọi bản ffmpeg 5.x/6.x/7.x.
pub const XFADE_TRANSITIONS: &[&str] = &[
    "fade", "dissolve", "smoothleft", "smoothright", "smoothup", "smoothdown",
    "circleopen", "circleclose", "rectcrop", "distance", "fadeblack", "fadewhite",
    "radial", "pixelize", "slideleft", "slideright", "slideup", "slidedown",
];

/// Kiểm tra tên transition do client gửi; "none"/rỗng = không chuyển cảnh.
pub fn validate_xfade_transition(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() || n.eq_ignore_ascii_case("none") {
        return Ok(());
    }
    if XFADE_TRANSITIONS.contains(&n.to_ascii_lowercase().as_str()) {
        Ok(())
    } else {
        Err(format!(
            "Hiệu ứng chuyển cảnh \"{n}\" không hỗ trợ. Chọn một trong: {}.",
            XFADE_TRANSITIONS.join(", ")
        ))
    }
}

pub fn validate_xfade(dur: f64, clips: usize, clip_durations: &[f64]) -> Result<(), String> {
    if dur <= 0.0 || clips < 2 {
        return Ok(());
    }
    if !(0.0..=1.0).contains(&dur) {
        return Err(format!("Thời lượng chuyển cảnh phải từ 0 đến 1 giây (đang {dur}s)."));
    }
    // xfade cần mỗi clip (trừ cái cuối) dài hơn transition; kiểm đơn giản theo
    // clip ngắn nhất cho an toàn.
    if let Some(shortest) = clip_durations.iter().cloned().fold(None::<f64>, |acc, d| {
        Some(match acc { None => d, Some(m) => m.min(d) })
    }) {
        if shortest <= dur {
            return Err(format!(
                "Có clip ngắn hơn thời lượng chuyển cảnh ({shortest:.2}s ≤ {dur:.2}s) — \
                 giảm chuyển cảnh hoặc bỏ clip ngắn."
            ));
        }
    }
    Ok(())
}

/// Args ghép các clip ĐÃ CHUẨN HOÁ bằng concat demuxer (không xfade, `-c copy`).
/// Caller viết file list.txt bằng [`concat_list_content`] trước.
pub fn build_concat_args(list_txt: &Path, output: &Path) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list_txt.to_string_lossy().into_owned(),
        "-c".into(),
        "copy".into(),
        "-y".into(),
        output.to_string_lossy().into_owned(),
    ]
}

/// Args ghép các clip ĐÃ CHUẨN HOÁ bằng chuỗi `xfade` giữa từng cặp clip kề
/// nhau (re-encode x264). `clip_durations` là độ dài TƯƠNG ĐỐI của từng clip
/// sau chuẩn hoá — caller lấy bằng ffprobe.
///
/// Offset của xfade thứ i là thời điểm kết thúc trừ transition, cộng dồn theo
/// timeline đã ghép: `offset_i = Σ(dur_j) - (i+1) * dur`.
pub fn build_xfade_args(
    inputs: &[PathBuf],
    output: &Path,
    transition: &str,
    dur: f64,
    clip_durations: &[f64],
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    for p in inputs {
        args.push("-i".into());
        args.push(p.to_string_lossy().into_owned());
    }

    let filter = if inputs.len() <= 1 {
        "[0:v]null[vout]".to_string()
    } else {
        let mut f = String::new();
        let mut last_label = "0:v".to_string();
        let mut cumulative = 0.0; // tổng độ dài thô của các clip đã vào chuỗi
        for i in 0..inputs.len() - 1 {
            cumulative += clip_durations[i];
            // Mỗi transition "ăn" mất `dur` giây: offset_i = Σdur - (i+1)*dur.
            let offset = cumulative - dur * (i as f64 + 1.0);
            let out = if i + 2 == inputs.len() { "vout" } else { &format!("v{i}") };
            f.push_str(&format!(
                "[{last_label}][{}:v]xfade=transition={transition}:duration={dur:.3}:offset={offset:.3}[{out}];",
                i + 1
            ));
            last_label = out.to_string();
        }
        f.trim_end_matches(';').to_string()
    };

    args.push("-filter_complex".into());
    args.push(filter);
    args.push("-map".into());
    args.push("[vout]".into());
    args.push("-c:v".into());
    args.push("libx264".into());
    args.push("-preset".into());
    args.push("veryfast".into());
    args.push("-crf".into());
    args.push("23".into());
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());
    args.push("-y".into());
    args.push(output.to_string_lossy().into_owned());
    args
}

/// Nội dung file list.txt cho concat demuxer (escape quote theo format concat).
pub fn concat_list_content(paths: &[PathBuf]) -> String {
    let mut s = String::new();
    for p in paths {
        // Concat demuxer đọc path kiểu shell: `\` là escape sequence trên mọi
        // nền tảng → luôn chuẩn hoá sang `/` (Windows API chấp nhận `/`),
        // nháy đơn trong tên file escape kiểu shell `'…'\''…'`.
        let clean = p.to_string_lossy().replace('\\', "/");
        s.push_str(&format!("file '{}'\n", clean.replace('\'', "'\\''")));
    }
    s
}

// ---------------------------------------------------------------------------
// Bước 3: pass cuối — lặp chuỗi + audio radio + cắt theo audio
// ---------------------------------------------------------------------------

/// Thông số encoder theo mức chất lượng người dùng chọn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeProfile {
    pub preset: &'static str,
    pub crf: String,
    pub audio_rate: String,
}

/// `fast` = file nhỏ, render nhanh; `balanced` = mặc định (đúng thông số cũ);
/// `high` = nét hơn, chậm hơn. Giá trị lạ/rỗng → balanced.
pub fn encode_profile(quality: &str) -> EncodeProfile {
    let (preset, crf, audio_rate) = match quality.trim().to_ascii_lowercase().as_str() {
        "fast" => ("veryfast", "26", "128k"),
        "high" => ("slow", "18", "256k"),
        _ => ("veryfast", "23", "192k"),
    };
    EncodeProfile { preset, crf: crf.into(), audio_rate: audio_rate.into() }
}

/// Args pass cuối (giai đoạn 3 mở rộng):
/// - `-stream_loop -1` trước `-i chain`: chuỗi lặp vô hạn.
/// - Chỉ map video chuỗi + `1:a:0` (audio radio là âm thanh duy nhất).
/// - `-t audio_duration` ép đầu ra đúng độ dài audio.
/// - `overlay`: filter_complex dựng sẵn từ textover (subtitles/drawtext/logo);
///   khi có overlay, `-c:v copy` bị tắt (filter buộc re-encode).
/// - `logo`: input thứ 3 (index 2) cho watermark logo.
pub fn build_final_args(
    chain: &Path,
    audio: &Path,
    output: &Path,
    audio_duration: f64,
    max_duration_sec: Option<u64>,
    options: &AudioOptions,
    overlay: &crate::textover::OverlaySpec,
    logo: Option<&Path>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    args.push("-hide_banner".into());
    args.push("-loglevel".into());
    args.push("error".into());

    args.push("-stream_loop".into());
    args.push("-1".into());
    args.push("-i".into());
    args.push(chain.to_string_lossy().into_owned());

    args.push("-i".into());
    args.push(audio.to_string_lossy().into_owned());

    // Input logo cho watermark (index 2) — chỉ khi graph logo có mặt.
    if overlay.needs_logo_input() {
        if let Some(logo) = logo {
            args.push("-i".into());
            args.push(logo.to_string_lossy().into_owned());
        }
    }

    let has_video_filters = overlay.has_video_filters();
    if has_video_filters {
        args.push("-filter_complex".into());
        args.push(crate::textover::compose_filter_complex(overlay));
        args.push("-map".into());
        args.push("[vout]".into());
    } else {
        args.push("-map".into());
        args.push("0:v:0".into());
    }
    args.push("-map".into());
    args.push("1:a:0".into());

    // Filter buộc re-encode — copy bị vô hiệu khi có filter video.
    if options.fast_mode && !has_video_filters {
        args.push("-c:v".into());
        args.push("copy".into());
    } else {
        let profile = encode_profile(&options.quality);
        for a in [
            "-c:v",
            "libx264",
            "-preset",
            profile.preset,
            "-crf",
            profile.crf.as_str(),
            "-pix_fmt",
            "yuv420p",
        ] {
            args.push(a.into());
        }
    }

    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-b:a".into());
    args.push(encode_profile(&options.quality).audio_rate);

    // Thời lượng output: min(audio, giới hạn nền tảng) — phải tính TRƯỚC khi
    // dựng `-af`. (Bản 0.3 tính sau rồi tìm lại chuỗi "afade=…" trong args nên
    // không bao giờ khớp khi có `volume=` đứng trước: fade-out rơi ra sau điểm
    // cắt và người xem không nghe thấy.)
    let out_duration = match max_duration_sec {
        Some(max) if (max as f64) < audio_duration => max as f64,
        _ => audio_duration,
    };
    let mut filters: Vec<String> = Vec::new();
    if options.volume != 100.0 {
        let factor = options.volume / 100.0;
        filters.push(format!("volume={factor:.4}"));
    }
    // Fade-out kết thúc đúng ở điểm cắt thật; fade dài hơn file thì co bằng
    // chính file (để thật sự về im lặng ở giây cuối).
    if options.fade_out > 0.0 {
        let fade = options.fade_out.min(out_duration).max(0.0);
        let start = (out_duration - fade).max(0.0);
        filters.push(format!("afade=t=out:st={start:.3}:d={fade:.3}"));
    }
    if !filters.is_empty() {
        args.push("-af".into());
        args.push(filters.join(","));
    }

    args.push("-t".into());
    args.push(format!("{out_duration:.6}"));
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push("-y".into());
    args.push(output.to_string_lossy().into_owned());
    args
}

/// A1 — "Tách âm thanh": args ffmpeg xuất MP3 từ audio đã ghép.
///
/// Chỉ map dòng audio (bỏ video), áp dụng cùng bộ lọc volume + fade-out
/// như bản video (đúng `out_duration` = min(audio, max_duration)), encode
/// libmp3lame 320 kbps chuẩn 44,1 kHz stereo. Không dùng `-stream_loop`
/// vì không có dòng video cần lặp.
pub fn build_mp3_args(
    audio: &Path,
    output: &Path,
    audio_duration: f64,
    max_duration_sec: Option<u64>,
    options: &AudioOptions,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.push("-hide_banner".into());
    args.push("-loglevel".into());
    args.push("error".into());

    args.push("-i".into());
    args.push(audio.to_string_lossy().into_owned());

    args.push("-vn".into());
    args.push("-map".into());
    args.push("0:a:0".into());

    let out_duration = match max_duration_sec {
        Some(max) if (max as f64) < audio_duration => max as f64,
        _ => audio_duration,
    };
    let mut filters: Vec<String> = Vec::new();
    if options.volume != 100.0 {
        let factor = options.volume / 100.0;
        filters.push(format!("volume={factor:.4}"));
    }
    if options.fade_out > 0.0 {
        let fade = options.fade_out.min(out_duration).max(0.0);
        let start = (out_duration - fade).max(0.0);
        filters.push(format!("afade=t=out:st={start:.3}:d={fade:.3}"));
    }
    if !filters.is_empty() {
        args.push("-af".into());
        args.push(filters.join(","));
    }

    args.push("-c:a".into());
    args.push("libmp3lame".into());
    args.push("-b:a".into());
    args.push("320k".into());
    args.push("-ar".into());
    args.push("44100".into());
    args.push("-ac".into());
    args.push("2".into());
    args.push("-t".into());
    args.push(format!("{out_duration:.6}"));
    args.push("-y".into());
    args.push(output.to_string_lossy().into_owned());
    args
}

// ---------------------------------------------------------------------------
// Chạy ffmpeg + parse progress (giữ nguyên từ giai đoạn 1)
// ---------------------------------------------------------------------------

/// Kết thúc của 1 lần chạy ffmpeg.
pub struct FfmpegOutcome {
    pub success: bool,
    pub cancelled: bool,
    /// ~20 dòng stderr cuối (để hiển thị khi lỗi).
    pub stderr_tail: Vec<String>,
}

/// Chạy ffmpeg đến cùng, parse tiến độ từ `-progress pipe:1`.
///
/// - `on_progress(out_time_seconds)`: gọi mỗi khi ffmpeg báo out_time mới.
/// - `cancel`: `CancellationToken` — khi cancel() được gọi, ffmpeg bị kill.
pub async fn run_ffmpeg(
    args: &[String],
    cancel: tokio_util::sync::CancellationToken,
    on_progress: impl Fn(f64) + Send + Sync + 'static,
) -> anyhow::Result<FfmpegOutcome> {
    let mut cmd = Command::new(ffmpeg_bin().clone());
    crate::fontconfig::apply_env_tokio(&mut cmd);
    cmd.arg("-progress")
        .arg("pipe:1")
        .arg("-nostats")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("Không chạy được ffmpeg")?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg không mở được stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg không mở được stderr"))?;

    // Task đọc progress từ stdout: các cặp key=value mỗi dòng, dùng
    // out_time_us (ffmpeg 7+), out_time_ms (legacy, thực chất là us) hoặc
    // out_time=HH:MM:SS.ffffff.
    let progress_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(secs) = parse_progress_line(&line) {
                on_progress(secs);
            }
        }
    });

    // Task giữ tối đa 20 dòng stderr cuối (bỏ dòng rỗng).
    let stderr_task = tokio::spawn(async move {
        let mut lines: Vec<String> = Vec::new();
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            if lines.len() >= 20 {
                lines.remove(0);
            }
            lines.push(line);
        }
        lines
    });

    // Chờ ffmpeg xong hoặc bị cancel.
    let wait = tokio::select! {
        status = child.wait() => Some(status),
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            Some(child.wait().await)
        }
    };

    let _ = progress_task.await;
    let stderr_tail = stderr_task.await.unwrap_or_default();

    let (success, cancelled) = match wait {
        Some(Ok(status)) => (status.success(), cancel.is_cancelled()),
        Some(Err(e)) => {
            return Err(anyhow::anyhow!("Không chờ được ffmpeg: {e}"));
        }
        None => (false, cancel.is_cancelled()),
    };

    Ok(FfmpegOutcome {
        success,
        cancelled,
        stderr_tail,
    })
}

/// Parse 1 dòng `-progress` thành giây.
fn parse_progress_line(line: &str) -> Option<f64> {
    let (key, value) = line.split_once('=')?;
    match key.trim() {
        "out_time_us" => value.trim().parse::<f64>().ok().map(|us| us / 1_000_000.0),
        // Quy ước legacy của ffmpeg: out_time_ms vẫn ghi micro giây.
        "out_time_ms" => value.trim().parse::<f64>().ok().map(|us| us / 1_000_000.0),
        "out_time" => parse_timecode(value.trim()),
        _ => None,
    }
}

/// "HH:MM:SS.ffffff" -> giây.
fn parse_timecode(s: &str) -> Option<f64> {
    let negative = s.starts_with('-');
    let s = s.trim_start_matches('-');
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for part in parts {
        let v: f64 = part.parse().ok()?;
        total = total * 60.0 + v;
    }
    Some(if negative { -total } else { total })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ==== build_filter_complex: preset × chế độ khớp khung =================

    /// Nguồn cùng tỷ lệ khung: cover scale đúng WxH.
    #[test]
    fn fit_cover_scales_to_exact_target() {
        let f = build_filter_complex("[0:v]", "[v]", 1080, 1920, FitMode::Cover);
        assert!(f.starts_with("[0:v]scale="), "{f}");
        assert!(f.contains("force_original_aspect_ratio=increase"), "{f}");
        assert!(f.contains("crop=1080:1920[v]"), "{f}");
        assert!(f.contains("force_divisible_by=2"), "{f}");
        assert!(!f.contains("pad="), "{f}");
        assert!(!f.contains("boxblur"), "{f}");
    }

    #[test]
    fn fit_pad_pads_with_black_bars() {
        let f = build_filter_complex("[0:v]", "[v]", 1080, 1350, FitMode::Pad);
        assert!(f.contains("force_original_aspect_ratio=decrease"), "{f}");
        assert!(f.contains("pad=1080:1350:(ow-iw)/2:(oh-ih)/2:color=black[v]"), "{f}");
        assert!(!f.contains("crop="), "{f}");
        assert!(!f.contains("boxblur"), "{f}");
    }

    #[test]
    fn fit_blur_splits_background_and_foreground() {
        let f = build_filter_complex("[0:v]", "[v]", 1080, 1080, FitMode::Blur);
        assert!(f.contains("[0:v]split[va][vb]"), "{f}");
        // Nền: cover + blur + hạ sáng.
        assert!(f.contains("[va]scale="), "{f}");
        assert!(f.contains("force_original_aspect_ratio=increase"), "{f}");
        assert!(f.contains("boxblur"), "{f}");
        assert!(f.contains("eq=brightness=-0.15"), "{f}");
        // Ảnh chính: fit trong khung.
        assert!(f.contains("[vb]scale="), "{f}");
        assert!(f.contains("force_original_aspect_ratio=decrease"), "{f}");
        // Ghép lại đúng nhãn out.
        assert!(f.ends_with("[vbg][vfg]overlay=(W-w)/2:(H-h)/2[v]"), "{f}");
    }

    /// Mỗi preset cố định kích thước × mỗi fit: filter luôn chốt đúng W×H.
    #[test]
    fn every_preset_x_fit_mentions_target_size() {
        // (tên, W, H) theo bảng yêu cầu giai đoạn 2.
        let targets = [
            ("tiktok_9x16", 1080u32, 1920u32),
            ("fb_feed_4x5", 1080, 1350),
            ("fb_yt_16x9", 1920, 1080),
            ("ig_feed_1x1", 1080, 1080),
            ("threads_4x5", 1080, 1350),
            ("x_16x9", 1280, 720),
        ];
        for (name, w, h) in targets {
            for fit in [FitMode::Cover, FitMode::Pad, FitMode::Blur] {
                let f = build_filter_complex("[0:v]", "[v]", w, h, fit);
                assert!(
                    f.contains(&format!("crop={w}:{h}")) || f.contains(&format!("pad={w}:{h}")),
                    "{name} x {:?} thiếu đích {w}x{h}: {f}",
                    fit
                );
            }
        }
    }

    /// Kích thước tuỳ chỉnh: W/H vào đã được presets.rs làm chẵn; filter dùng
    /// đúng giá trị nhận vào.
    #[test]
    fn fit_custom_size_uses_given_dimensions() {
        for fit in [FitMode::Cover, FitMode::Pad, FitMode::Blur] {
            let f = build_filter_complex("[0:v]", "[v]", 720, 720, fit);
            assert!(f.contains("720"));
            assert!(!f.contains("1080"));
        }
    }

    /// Nhãn trung gian phái sinh từ out → 2 filter trong cùng graph không đụng nhau.
    #[test]
    fn blur_labels_derived_from_out_label() {
        let f1 = build_filter_complex("[0:v]", "[v1]", 1080, 1080, FitMode::Blur);
        let f2 = build_filter_complex("[1:v]", "[v2]", 1080, 1080, FitMode::Blur);
        assert!(f1.contains("[v1a]") && f1.contains("[v1bg]") && f1.contains("[v1fg]"), "{f1}");
        assert!(f2.contains("[v2a]") && f2.contains("[v2bg]") && f2.contains("[v2fg]"), "{f2}");
    }

    #[test]
    fn fit_mode_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&FitMode::Cover).unwrap(), "\"cover\"");
        assert_eq!(serde_json::to_string(&FitMode::Pad).unwrap(), "\"pad\"");
        assert_eq!(serde_json::to_string(&FitMode::Blur).unwrap(), "\"blur\"");
        assert_eq!(serde_json::from_str::<FitMode>("\"blur\"").unwrap(), FitMode::Blur);
        // Giá trị lạ → lỗi parse rõ ràng (UI chỉ gửi 3 giá trị hợp lệ).
        assert!(serde_json::from_str::<FitMode>("\"xyz\"").is_err());
    }

    // ==== build_normalize_args ==============================================

    #[test]
    fn normalize_drops_audio_and_sets_fps() {
        let args = build_normalize_args(
            Path::new("/tmp/in.mp4"),
            Path::new("/tmp/n.mp4"),
            Some(1080),
            Some(1920),
            30,
            FitMode::Cover,
            0,
        );
        let s = args.join(" ");
        assert!(s.contains("-an"), "phải bỏ audio: {s}");
        assert!(!s.contains("-c:a"), "không encode audio: {s}");
        assert!(s.contains("-r 30"), "thiếu fps: {s}");
        assert!(s.contains("-pix_fmt yuv420p"));
        assert!(s.contains("-video_track_timescale 15360"));
        assert!(s.contains("-filter_complex"));
        assert!(s.contains("crop=1080:1920"));
        assert!(s.contains("-map [v]"));
    }

    #[test]
    fn normalize_original_preset_has_no_scale_filter() {
        let args = build_normalize_args(
            Path::new("/tmp/in.mp4"),
            Path::new("/tmp/n.mp4"),
            None,
            None,
            30,
            FitMode::Cover,
            0,
        );
        let s = args.join(" ");
        assert!(!s.contains("-filter_complex"), "giữ nguyên gốc không scale: {s}");
        assert!(s.contains("-map 0:v:0"));
        assert!(s.contains("-r 30"));
    }

    // ==== Q1: concat list chuẩn hoá \ → / ====================================

    #[test]
    fn concat_list_windows_path_dung_gach_cheo_xuoi() {
        let content = concat_list_content(&[PathBuf::from(r"C:\job\norm_0.mp4")]);
        assert_eq!(content, "file 'C:/job/norm_0.mp4'\n", "{content}");
    }

    #[test]
    fn concat_list_unix_path_giu_nguyen() {
        let content = concat_list_content(&[PathBuf::from("/tmp/job/norm_0.mp4")]);
        assert_eq!(content, "file '/tmp/job/norm_0.mp4'\n");
    }

    #[test]
    fn concat_list_escape_quote() {
        let content = concat_list_content(&[PathBuf::from("/tmp/it's.mp4")]);
        assert_eq!(content, "file '/tmp/it'\\''s.mp4'\n", "{content}");
    }

    // ==== build_fit_filter (chain chung 1 lần cho nhiều preset) ==============

    #[test]
    fn fit_filter_cover_has_no_labels_and_targets_size() {
        let f = build_fit_filter(1080, 1920, FitMode::Cover, "fit_tiktok");
        assert!(!f.starts_with('['), "không label input: {f}");
        assert!(f.contains("crop=1080:1920"));
        assert!(!f.ends_with(']'), "không label output: {f}");
    }

    #[test]
    fn fit_filter_pad_pads_black() {
        let f = build_fit_filter(1080, 1080, FitMode::Pad, "x");
        assert!(f.contains("pad=1080:1080:(ow-iw)/2:(oh-ih)/2:color=black"), "{f}");
    }

    #[test]
    fn fit_filter_blur_uses_unique_labels() {
        let f = build_fit_filter(1280, 720, FitMode::Blur, "fit_x_16x9");
        assert!(f.contains("[fit_x_16x9a]"), "{f}");
        assert!(f.contains("[fit_x_16x9fg]overlay=(W-w)/2:(H-h)/2"), "{f}");
    }

    // ==== compose_filter_complex với pre_scale ===============================

    #[test]
    fn compose_with_pre_scale_and_chain_and_logo() {
        use crate::textover::{compose_filter_complex, OverlaySpec};
        let spec = OverlaySpec {
            pre_scale: Some("scale=1080:1920,crop=1080:1920".into()),
            chain: vec!["drawtext=textfile='x'".into()],
            logo_prep: Some("[2:v]scale=100:-2[wm]".into()),
            logo_xy: Some(("10".into(), "20".into())),
            ..Default::default()
        };
        assert!(spec.needs_logo_input());
        let s = compose_filter_complex(&spec);
        assert!(s.starts_with("[0:v]scale=1080:1920,crop=1080:1920,drawtext="), "{s}");
        assert!(s.contains("[v0];[2:v]scale=100:-2[wm];"), "{s}");
        assert!(s.ends_with("[v0][wm]overlay=x='10':y='20'[vout]"), "{s}");
    }

    #[test]
    fn compose_with_pre_scale_only() {
        use crate::textover::{compose_filter_complex, OverlaySpec};
        let mut spec = OverlaySpec::default();
        spec.pre_scale = Some("crop=1080:1080".into());
        let s = compose_filter_complex(&spec);
        assert_eq!(s, "[0:v]crop=1080:1080[vout]");
    }

    #[test]
    fn final_args_reencodes_with_pre_scale_even_without_chain() {
        use crate::textover::OverlaySpec;
        let mut overlay = OverlaySpec::default();
        overlay.pre_scale = Some("crop=1080:1080".into());
        let args = build_final_args(
            Path::new("/tmp/chain.mp4"),
            Path::new("/tmp/a.mp3"),
            Path::new("/tmp/out.mp4"),
            8.0,
            None,
            &AudioOptions::default(),
            &overlay,
            None,
        );
        let s = args.join(" ");
        assert!(s.contains("-c:v libx264"), "pre_scale buộc re-encode: {s}");
        assert!(s.contains("crop=1080:1080"));
    }

    #[test]
    fn normalize_pad_mode_pads() {
        let args = build_normalize_args(
            Path::new("/tmp/in.mp4"),
            Path::new("/tmp/n.mp4"),
            Some(1280),
            Some(720),
            30,
            FitMode::Pad,
            0,
        );
        let s = args.join(" ");
        assert!(s.contains("pad=1280:720"), "{s}");
    }

    #[test]
    fn normalize_blur_mode_has_split_and_overlay() {
        let args = build_normalize_args(
            Path::new("/tmp/in.mp4"),
            Path::new("/tmp/n.mp4"),
            Some(1080),
            Some(1080),
            30,
            FitMode::Blur,
            0,
        );
        let s = args.join(" ");
        assert!(s.contains("split"), "{s}");
        assert!(s.contains("boxblur"), "{s}");
        assert!(s.contains("overlay=(W-w)/2:(H-h)/2"), "{s}");
        assert!(s.contains("-map [v]"), "{s}");
    }

    // ==== concat list + args ================================================

    #[test]
    fn concat_list_escapes_and_lists_all() {
        let content = concat_list_content(&[
            PathBuf::from("/tmp/a.mp4"),
            PathBuf::from("/tmp/b's c.mp4"),
        ]);
        assert!(content.starts_with("file '/tmp/a.mp4'\n"));
        assert!(content.contains("file '/tmp/b'\\''s c.mp4'\n"));
    }

    #[test]
    fn concat_args_uses_copy_and_list_input() {
        let args = build_concat_args(Path::new("/tmp/list.txt"), Path::new("/tmp/chain.mp4"));
        let s = args.join(" ");
        assert!(s.contains("-f concat"));
        assert!(s.contains("-safe 0"));
        assert!(s.contains("-i /tmp/list.txt"));
        assert!(s.contains("-c copy"));
    }

    // ==== xfade =============================================================

    #[test]
    fn xfade_validates_range() {
        assert!(validate_xfade(0.0, 3, &[5.0, 5.0, 5.0]).is_ok());
        assert!(validate_xfade(0.5, 3, &[5.0, 5.0, 5.0]).is_ok());
        assert!(validate_xfade(1.0, 2, &[5.0, 5.0]).is_ok());
        assert!(validate_xfade(1.5, 2, &[5.0, 5.0]).is_err(), "> 1s phải lỗi");
        assert!(validate_xfade(-0.1, 2, &[5.0, 5.0]).is_ok(), "âm = tắt");
        assert!(validate_xfade(0.5, 1, &[5.0]).is_ok(), "1 clip không cần xfade");
    }

    #[test]
    fn xfade_rejects_clip_shorter_than_transition() {
        assert!(validate_xfade(0.8, 2, &[5.0, 0.5]).is_err());
        assert!(validate_xfade(0.4, 2, &[5.0, 0.5]).is_ok());
    }

    #[test]
    fn xfade_single_clip_is_passthrough() {
        let args = build_xfade_args(
            &[PathBuf::from("/tmp/only.mp4")],
            Path::new("/tmp/chain.mp4"),
            "fade",
            0.5,
            &[5.0],
        );
        let s = args.join(" ");
        assert!(s.contains("[0:v]null[vout]"), "{s}");
        assert!(!s.contains("xfade="), "1 clip không xfade: {s}");
    }

    #[test]
    fn xfade_offsets_accumulate() {
        // 3 clip 5s, transition 0.5s:
        //   offset1 = 5 - 0.5 = 4.5
        //   offset2 = (5 - 0.5) + (5 - 0.5) = 9
        let args = build_xfade_args(
            &[
                PathBuf::from("/tmp/a.mp4"),
                PathBuf::from("/tmp/b.mp4"),
                PathBuf::from("/tmp/c.mp4"),
            ],
            Path::new("/tmp/chain.mp4"),
            "fade",
            0.5,
            &[5.0, 5.0, 5.0],
        );
        let s = args.join(" ");
        assert!(s.contains("[0:v][1:v]xfade=transition=fade:duration=0.500:offset=4.500"), "{s}");
        assert!(s.contains("[v0][2:v]xfade=transition=fade:duration=0.500:offset=9.000[vout]"), "{s}");
    }

    #[test]
    fn xfade_transition_name_in_filter() {
        let args = build_xfade_args(
            &[PathBuf::from("/tmp/a.mp4"), PathBuf::from("/tmp/b.mp4")],
            Path::new("/tmp/chain.mp4"),
            "wipeleft",
            1.0,
            &[10.0, 10.0],
        );
        let s = args.join(" ");
        assert!(s.contains("transition=wipeleft"), "{s}");
        assert!(s.contains("duration=1.000"), "{s}");
        assert!(s.contains("offset=9.000"), "{s}");
    }

    // ==== build_final_args (giữ hành vi giai đoạn 1) ========================

    fn aopts(volume: f64, fade_out: f64, fast_mode: bool) -> AudioOptions {
        AudioOptions { volume, fade_out, fast_mode, ..Default::default() }
    }

    fn final_args_for(options: &AudioOptions) -> Vec<String> {
        build_final_args(
            Path::new("/tmp/chain.mp4"),
            Path::new("/tmp/a.mp3"),
            Path::new("/tmp/out.mp4"),
            120.0,
            None,
            options,
            &crate::textover::OverlaySpec::default(),
            None,
        )
    }

    /// Overlay chỉ có 1 drawtext trong chain.
    fn overlay_chain_only() -> crate::textover::OverlaySpec {
        crate::textover::OverlaySpec {
            chain: vec!["drawtext=textfile='/job/t.txt':fontsize=28".into()],
            ..Default::default()
        }
    }

    #[test]
    fn final_overlay_adds_filter_complex_and_maps_vout() {
        let args = build_final_args(
            Path::new("/tmp/chain.mp4"),
            Path::new("/tmp/a.mp3"),
            Path::new("/tmp/out.mp4"),
            120.0,
            None,
            &aopts(100.0, 0.0, false),
            &overlay_chain_only(),
            None,
        );
        let s = args.join(" ");
        let fc = args.iter().position(|a| a == "-filter_complex").expect("thiếu filter_complex");
        assert!(args[fc + 1].contains("[0:v]drawtext=textfile='/job/t.txt':fontsize=28[vout]"), "{}", args[fc + 1]);
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "[vout]"));
        // Không còn map 0:v:0.
        assert!(!args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v:0"));
        // Overlay buộc re-encode kể cả fast_mode.
        assert!(s.contains("-c:v libx264"));
        assert!(!s.contains("-c:v copy"));
    }

    #[test]
    fn final_overlay_logo_adds_third_input() {
        let spec = crate::textover::OverlaySpec {
            logo_prep: Some("[2:v]scale=100:-2[wm]".into()),
            logo_xy: Some(("main_w-overlay_w-10".into(), "main_h-overlay_h-10".into())),
            ..Default::default()
        };
        let args = build_final_args(
            Path::new("/tmp/chain.mp4"),
            Path::new("/tmp/a.mp3"),
            Path::new("/tmp/out.mp4"),
            120.0,
            None,
            &aopts(100.0, 0.0, true), // fast_mode bật nhưng logo buộc encode
            &spec,
            Some(Path::new("/tmp/logo.png")),
        );
        let s = args.join(" ");
        assert!(s.contains("-i /tmp/logo.mp3 -i /tmp/logo.png") || s.contains("-i /tmp/a.mp3 -i /tmp/logo.png"), "{s}");
        assert!(s.contains("-i /tmp/logo.png"), "{s}");
        assert!(s.contains("-c:v libx264"), "logo buộc re-encode: {s}");
        // Logo với filter khác: các nhãn trung gian nối tường minh, không còn
        // dựa vào auto-connect của ffmpeg (từng sinh graph 2 output → lỗi).
        assert!(s.contains("[v0][wm]overlay=x='main_w-overlay_w-10':y='main_h-overlay_h-10'[vout]"), "{s}");
    }

    #[test]
    fn final_no_overlay_keeps_legacy_behavior() {
        let args = final_args_for(&aopts(100.0, 0.0, false));
        assert!(!args.contains(&"-filter_complex".to_string()));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v:0"));
    }

    #[test]
    fn final_stream_loop_before_chain_input() {
        let args = final_args_for(&aopts(100.0, 0.0, false));
        let s = args.join(" ");
        let loop_pos = s.find("-stream_loop -1").expect("thiếu -stream_loop -1");
        let input_pos = s.find("-i /tmp/chain.mp4").expect("thiếu -i chain");
        assert!(loop_pos < input_pos);
    }

    #[test]
    fn final_maps_only_chain_video_and_radio_audio() {
        let args = final_args_for(&aopts(100.0, 0.0, false));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v:0"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "1:a:0"));
        assert_eq!(args.iter().filter(|a| *a == "-map").count(), 2);
    }

    #[test]
    fn final_volume_fade_and_duration() {
        let args = final_args_for(&aopts(150.0, 2.0, false));
        let af = args.iter().position(|a| a == "-af").unwrap();
        assert!(args[af + 1].contains("volume=1.5"));
        assert!(args[af + 1].contains("afade=t=out:st=118.000:d=2.000"));
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "120.000000");
    }

    #[test]
    fn mp3_args_encode_flac_to_mp3_320() {
        // A1: tách audio — không video, libmp3lame 320k, đúng duration + filter.
        let args = build_mp3_args(
            Path::new("/tmp/radio.flac"),
            Path::new("/tmp/out.mp3"),
            120.0,
            None,
            &aopts(150.0, 2.0, false),
        );
        let s = args.join(" ");
        assert!(s.contains("-i /tmp/radio.flac"), "{s}");
        assert!(s.contains("-vn"), "phải bỏ video: {s}");
        assert!(s.contains("-map 0:a:0"), "{s}");
        assert!(s.contains("-c:a libmp3lame"), "{s}");
        assert!(s.contains("-b:a 320k"), "{s}");
        assert!(s.contains("-ar 44100"), "{s}");
        assert!(!s.contains("-stream_loop"), "audio không lặp: {s}");
        let af = args.iter().position(|a| a == "-af").unwrap();
        assert!(args[af + 1].contains("volume=1.5"));
        assert!(args[af + 1].contains("afade=t=out:st=118.000:d=2.000"));
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "120.000000");
    }

    #[test]
    fn mp3_args_respect_max_duration() {
        let args = build_mp3_args(
            Path::new("/tmp/a.wav"),
            Path::new("/tmp/o.mp3"),
            300.0,
            Some(60),
            &aopts(100.0, 0.0, false),
        );
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "60.000000");
    }

    #[test]
    fn final_fade_never_negative_start() {
        let args = build_final_args(
            Path::new("/tmp/chain.mp4"),
            Path::new("/tmp/a.mp3"),
            Path::new("/tmp/out.mp4"),
            2.0,
            None,
            &aopts(100.0, 5.0, false),
            &crate::textover::OverlaySpec::default(),
            None,
        );
        let af = args.iter().position(|a| a == "-af").unwrap();
        // Audio 2s + fade 5s: fade co bằng đúng độ dài file để về im lặng.
        assert!(args[af + 1].contains("afade=t=out:st=0.000:d=2.000"), "{}", args[af + 1]);
    }

    /// HỒI QUY bản 0.3: có `volume=` thì fade-out KHÔNG được tính lại theo
    /// điểm cắt (tìm chuỗi "afade=…" đứng đầu arg nên không khớp) — fade bắt
    /// đầu ở 118s trong khi file dừng ở 60s, tức không nghe thấy fade nào.
    #[test]
    fn final_fade_recomputed_even_with_volume() {
        let args = build_final_args(
            Path::new("/tmp/chain.mp4"),
            Path::new("/tmp/a.mp3"),
            Path::new("/tmp/out.mp4"),
            120.0,
            Some(60),
            &aopts(150.0, 2.0, false),
            &crate::textover::OverlaySpec::default(),
            None,
        );
        let af = args.iter().position(|a| a == "-af").unwrap();
        assert_eq!(args[af + 1], "volume=1.5000,afade=t=out:st=58.000:d=2.000");
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "60.000000");
    }

    #[test]
    fn final_quality_profile_changes_encoder() {
        let s = |q: &str| {
            let o = AudioOptions { quality: q.into(), ..Default::default() };
            build_final_args(
                Path::new("/c.mp4"),
                Path::new("/a.mp3"),
                Path::new("/o.mp4"),
                60.0,
                None,
                &o,
                &crate::textover::OverlaySpec::default(),
                None,
            )
            .join(" ")
        };
        let balanced = s("balanced");
        assert!(balanced.contains("-preset veryfast -crf 23 -pix_fmt yuv420p"), "{balanced}");
        assert!(balanced.ends_with("-b:a 192k -t 60.000000 -movflags +faststart -y /o.mp4"), "{balanced}");
        assert!(s("high").contains("-preset slow -crf 18"));
        assert!(s("FAST").contains("-crf 26"));
        assert!(s("").contains("-preset veryfast -crf 23"), "giá trị rỗng → mặc định");
    }

    #[test]
    fn final_no_af_when_volume_100_no_fade() {
        let args = final_args_for(&aopts(100.0, 0.0, false));
        assert!(!args.contains(&"-af".to_string()));
    }

    #[test]
    fn final_max_duration_trims_output() {
        // max 60s < audio 120s → -t 60.
        let args = build_final_args(
            Path::new("/tmp/chain.mp4"),
            Path::new("/tmp/a.mp3"),
            Path::new("/tmp/out.mp4"),
            120.0,
            Some(60),
            &aopts(100.0, 0.0, false),
            &crate::textover::OverlaySpec::default(),
            None,
        );
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "60.000000");
    }

    #[test]
    fn final_max_duration_no_trim_when_audio_shorter() {
        // max 300s > audio 120s → giữ -t 120.
        let args = build_final_args(
            Path::new("/tmp/chain.mp4"),
            Path::new("/tmp/a.mp3"),
            Path::new("/tmp/out.mp4"),
            120.0,
            Some(300),
            &aopts(100.0, 0.0, false),
            &crate::textover::OverlaySpec::default(),
            None,
        );
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "120.000000");
    }

    #[test]
    fn final_max_duration_fade_recomputed() {
        // fade 10s nhưng cắt ở 60s → afade bắt đầu 50 (60-10), không phải 110.
        let args = build_final_args(
            Path::new("/tmp/chain.mp4"),
            Path::new("/tmp/a.mp3"),
            Path::new("/tmp/out.mp4"),
            120.0,
            Some(60),
            &aopts(100.0, 10.0, false),
            &crate::textover::OverlaySpec::default(),
            None,
        );
        let af = args.iter().position(|a| a == "-af").unwrap();
        assert!(
            args[af + 1].contains("afade=t=out:st=50.000:d=10.000"),
            "{}",
            args[af + 1]
        );
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "60.000000");
    }

    #[test]
    fn final_fast_mode_copies_video() {
        let args = final_args_for(&aopts(100.0, 0.0, true));
        assert!(args.windows(2).any(|w| w[0] == "-c:v" && w[1] == "copy"));
        assert!(!args.windows(2).any(|w| w[0] == "-c:v" && w[1] == "libx264"));
        assert!(args.windows(2).any(|w| w[0] == "-c:a" && w[1] == "aac"));
    }

    #[test]
    fn final_faststart_present() {
        let args = final_args_for(&aopts(100.0, 0.0, false));
        assert!(args.windows(2).any(|w| w[0] == "-movflags" && w[1] == "+faststart"));
    }

    // ==== progress parsing ==================================================

    #[test]
    fn parse_progress_prefers_microseconds() {
        assert_eq!(parse_progress_line("out_time_us=1500000"), Some(1.5));
        assert_eq!(parse_progress_line("out_time_ms=1500000"), Some(1.5));
        assert_eq!(parse_progress_line("out_time=00:01:02.500"), Some(62.5));
        assert_eq!(parse_progress_line("frame=10"), None);
        assert_eq!(parse_progress_line("garbage"), None);
    }

    #[test]
    fn parse_timecode_negative() {
        assert_eq!(parse_timecode("-00:00:01.000"), Some(-1.0));
        assert_eq!(parse_timecode("00:02:00.000"), Some(120.0));
        assert_eq!(parse_timecode("45.5"), Some(45.5));
    }
}


#[cfg(test)]
mod rotation_tests {
    use super::*;

    #[test]
    fn normalize_applies_transpose_for_rotated_video() {
        // Video xoay 90°: chèn transpose=1 trước scale trong filter_complex.
        let args = build_normalize_args(
            Path::new("/in.mp4"),
            Path::new("/n.mp4"),
            Some(1080),
            Some(1920),
            30,
            FitMode::Cover,
            90,
        );
        let s = args.join(" ");
        assert!(s.contains("transpose=1"), "{s}");
        // transpose đứng TRƯỚC scale.
        let tp = s.find("transpose=1").unwrap();
        let sc = s.find("scale=").unwrap();
        assert!(tp < sc, "transpose phải trước scale: {s}");
    }

    #[test]
    fn normalize_270_uses_transpose_2() {
        let args = build_normalize_args(
            Path::new("/in.mp4"),
            Path::new("/n.mp4"),
            Some(1080),
            Some(1920),
            30,
            FitMode::Cover,
            270,
        );
        assert!(args.join(" ").contains("transpose=2"));
    }

    #[test]
    fn normalize_180_double_transpose() {
        let args = build_normalize_args(
            Path::new("/in.mp4"),
            Path::new("/n.mp4"),
            Some(1080),
            Some(1920),
            30,
            FitMode::Cover,
            180,
        );
        assert!(args.join(" ").contains("transpose=1,transpose=1"));
    }

    #[test]
    fn normalize_no_rotation_no_transpose() {
        let args = build_normalize_args(
            Path::new("/in.mp4"),
            Path::new("/n.mp4"),
            Some(1080),
            Some(1920),
            30,
            FitMode::Cover,
            0,
        );
        let s = args.join(" ");
        assert!(!s.contains("transpose"), "{s}");
        // 360 ≡ 0.
        let args360 = build_normalize_args(
            Path::new("/in.mp4"),
            Path::new("/n.mp4"),
            Some(1080),
            Some(1920),
            30,
            FitMode::Cover,
            360,
        );
        assert!(!args360.join(" ").contains("transpose"));
    }

    #[test]
    fn normalize_original_preset_rotated_still_uprights() {
        // Preset giữ nguyên gốc + rotation 90 → -vf transpose + chẵn hoá size.
        let args = build_normalize_args(
            Path::new("/in.mp4"),
            Path::new("/n.mp4"),
            None,
            None,
            30,
            FitMode::Cover,
            90,
        );
        let s = args.join(" ");
        assert!(s.contains("-vf transpose=1,scale=trunc(iw/2)*2:trunc(ih/2)*2"), "{s}");
    }

    #[test]
    fn extract_json_object_brace_matching() {
        use crate::jobs::extract_json_object_for_test;
        // Text rác quanh JSON, có '}' trong chuỗi.
        let s = r#"rác {"a": 1} rác {"b": } } cuối"#;
        assert_eq!(extract_json_object_for_test(s), Some(r#"{"a": 1}"#));
        // Ngoặc kép chứa '{' và '}'.
        let s2 = r#"✅ done {"text": "has } brace", "n": 2}"#;
        assert_eq!(extract_json_object_for_test(s2), Some(r#"{"text": "has } brace", "n": 2}"#));
        // Không có object.
        assert_eq!(extract_json_object_for_test("no json"), None);
        // Object chứa chuỗi thoát \" — vẫn khớp đúng.
        let s3 = r#"x {"a": "quote\" inside", "b": [1,2]}"#;
        assert_eq!(extract_json_object_for_test(s3), Some(r#"{"a": "quote\" inside", "b": [1,2]}"#));
    }
}
