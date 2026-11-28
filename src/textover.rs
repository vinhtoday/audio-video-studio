//! Lớp phủ hình ảnh lên video: chữ (drawtext), phụ đề (ASS/libass), watermark
//! chữ + logo, visualizer, thanh tiến độ — ghép tất cả thành một chuỗi
//! `-filter_complex` cho pass cuối.
//!
//! Nguyên tắc:
//! - Mọi hàm builder là **thuần** (không I/O, không spawn) để unit test được
//!   từng tổ hợp. Việc ghi file text/ASS nằm ở `jobs.rs`.
//! - Font được phân giải MỘT lần thành [`ResolvedFont`] (file + family +
//!   metric) rồi truyền vào builder. Bản 0.3 truyền nhầm đường dẫn file CHỮ
//!   làm `fontfile` → font upload bị bỏ qua im lặng.
//! - Mọi kích thước theo px của khung đích, nhân `scale_factor`
//!   (min(W,H)/1080) để chữ đồng đều giữa các tỉ lệ khung; chữ và watermark
//!   né vùng an toàn của nền tảng (safe-zone) lẫn vùng phụ đề/visualizer.
//!
//! Chi tiết escape đã kiểm chứng trên ffmpeg 7.1 thật (scripts/probe_escape.sh):
//! - Nội dung chữ luôn đi qua **`textfile=`** (job ghi file text sạch, đường
//!   dẫn do app sinh) — tránh 2 lớp escape của `text=`.
//! - Biểu thức x/y/enable/alpha bọc trong nháy đơn: dấu phẩy KHÔNG cần escape.
//! - `%` trong text: `expansion=none` tắt mọi expansion → `%` an toàn.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::fontconfig;
use crate::fontinfo::FontMetrics;
use crate::presets::SafeZone;
use crate::subtitle::{self, AssStyle, SubtitleStyleKind};

// ---------------------------------------------------------------------------
// Vị trí
// ---------------------------------------------------------------------------

/// Vị trí chữ overlay: trên / giữa / dưới (căn ngang giữa).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextPosition {
    Top,
    Middle,
    #[default]
    Bottom,
}

/// Vị trí watermark: 4 góc + giữa.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WmPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
    Center,
}

/// Watermark dạng chữ hay logo ảnh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WmKind {
    #[default]
    Text,
    Logo,
}

/// Hiệu ứng vào/ra của chữ overlay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAnimation {
    /// Mờ dần vào lúc đầu, mờ dần ra lúc cuối (mặc định).
    #[default]
    Fade,
    /// Không hiệu ứng.
    #[serde(rename = "none")]
    Off,
    /// Trượt vào từ phía ngoài khung.
    Slide,
    /// Nảy bật vào (ease-out-back).
    Pop,
}

// ---------------------------------------------------------------------------
// Font
// ---------------------------------------------------------------------------

/// Font cho chữ/watermark/phụ đề: mặc định, font hệ thống (family) hoặc font
/// người dùng upload (id file trong job dir).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FontChoice {
    #[default]
    Default,
    System {
        family: String,
    },
    Upload {
        id: String,
    },
}

/// Font đã phân giải — thứ duy nhất các builder cần.
#[derive(Debug, Clone, Default)]
pub struct ResolvedFont {
    /// File font đưa thẳng vào drawtext bằng `fontfile` (chính xác tuyệt đối).
    pub file: Option<PathBuf>,
    /// Tên family cho ASS (libass chỉ tra font theo tên + `fontsdir`).
    pub family: Option<String>,
    /// Thư mục để libass nạp font upload (`fontsdir`).
    pub fonts_dir: Option<PathBuf>,
    /// Metric để chọn ký tự giãn chữ (xem `fontinfo`).
    pub metrics: Option<FontMetrics>,
}

impl FontChoice {
    /// Phân giải font với danh sách file upload `id → đường dẫn`.
    ///
    /// Đây là bước DUY NHẤT chạm đĩa (đọc metric/namedata của font); mọi
    /// builder sau đó thuần. Font upload không tồn tại → rơi về font mặc định.
    pub fn resolve(&self, uploads: &HashMap<String, PathBuf>) -> ResolvedFont {
        match self {
            Self::Default => ResolvedFont::default_font(),
            Self::System { family } => {
                let clean: String = family
                    .chars()
                    .filter(|c| *c != '\'' && !c.is_control())
                    .collect::<String>()
                    .trim()
                    .to_string();
                if clean.is_empty() {
                    return ResolvedFont::default_font();
                }
                let file = fontconfig::system_font_file(&clean);
                ResolvedFont {
                    metrics: file.as_deref().and_then(crate::fontinfo::from_file),
                    file,
                    family: Some(clean),
                    fonts_dir: None,
                }
            }
            Self::Upload { id } => match uploads.get(id).filter(|p| p.is_file()) {
                Some(path) => ResolvedFont {
                    metrics: crate::fontinfo::from_file(path),
                    file: Some(path.clone()),
                    family: fontconfig::font_family_name(path).or_else(|| Some("Arial".into())),
                    fonts_dir: path.parent().map(Path::to_path_buf),
                },
                None => ResolvedFont::default_font(),
            },
        }
    }
}

impl ResolvedFont {
    /// Font mặc định của nền tảng — tính một lần rồi cache cho cả tiến trình.
    pub fn default_font() -> Self {
        static CACHE: OnceLock<ResolvedFont> = OnceLock::new();
        CACHE
            .get_or_init(|| {
                let file = fontconfig::default_font_path();
                let family = file.as_deref().and_then(fontconfig::font_family_name);
                let metrics = file.as_deref().and_then(crate::fontinfo::from_file);
                Self { file, family, metrics, fonts_dir: None }
            })
            .clone()
    }

    /// Tuỳ chọn font cho drawtext: ưu tiên `fontfile=` (FreeType nạp thẳng,
    /// không phụ thuộc fontconfig), không có file thì `font=<family>`.
    pub fn drawtext_options(&self) -> String {
        if let Some(f) = &self.file {
            return format!(":fontfile='{}'", escape_filter_path(f));
        }
        match &self.family {
            Some(fam) if !fam.is_empty() => format!(":font='{fam}'"),
            _ => String::new(),
        }
    }

    /// Tên font ghi vào `Style:` của ASS.
    pub fn ass_fontname(&self) -> String {
        self.family.clone().unwrap_or_else(|| "Arial".into())
    }
}

// ---------------------------------------------------------------------------
// Tuỳ chọn từng lớp
// ---------------------------------------------------------------------------

/// Chữ overlay (tiêu đề, câu chat radio...).
#[derive(Debug, Clone, Deserialize)]
pub struct TextOverlayOptions {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub font: FontChoice,
    /// Cỡ chữ theo baseline 1080px — tự co giãn theo preset (min(W,H)/1080).
    #[serde(default = "default_text_size")]
    pub size_px: u32,
    #[serde(default = "default_white")]
    pub color: String,
    #[serde(default = "default_border_w")]
    pub border_w: u32,
    #[serde(default = "default_black")]
    pub border_color: String,
    #[serde(default)]
    pub position: TextPosition,
    /// None = hiện từ đầu phần video.
    #[serde(default)]
    pub start_sec: Option<f64>,
    /// None = hiện đến hết phần video.
    #[serde(default)]
    pub end_sec: Option<f64>,
    // ---- Style nâng cao (bản 0.3 — thư viện template) ----
    /// Nền sau chữ: màu hex. None/rỗng = không nền.
    #[serde(default)]
    pub box_color: Option<String>,
    /// Độ mờ nền chữ 0–100 (mặc định 80).
    #[serde(default = "default_box_opacity")]
    pub box_opacity: u32,
    /// Đệm nền chữ (padding) theo baseline 1080px, mặc định 16.
    #[serde(default = "default_box_padding")]
    pub box_padding: u32,
    /// Bóng đổ: None = không; Some(x) = offset scaled.
    #[serde(default)]
    pub shadow_offset: Option<f64>,
    /// Màu bóng đổ (mặc định đen).
    #[serde(default = "default_black")]
    pub shadow_color: String,
    /// Khoảng cách THẬT giữa các ký tự, px @1080 (drawtext không có option
    /// tracking nên phải chèn ký tự khoảng cách mảnh — xem `style_text`).
    #[serde(default)]
    pub letter_spacing: f64,
    /// VIẾT HOA toàn bộ nội dung.
    #[serde(default)]
    pub uppercase: bool,
    /// Lớp chữ thứ 2 (đổ màu lệch 1–2px) tạo hiệu ứng 3D/neon.
    #[serde(default)]
    pub accent_layer: Option<AccentLayer>,
    // ---- Bản 0.4 ----
    /// Hiệu ứng vào/ra.
    #[serde(default)]
    pub animation: TextAnimation,
    /// Thời lượng hiệu ứng (giây), kẹp 0.1–3.0.
    #[serde(default = "default_anim_duration")]
    pub anim_duration: f64,
    /// Kịch bản nhiều cảnh: có cảnh thì thay chữ chính `content`.
    #[serde(default)]
    pub scenes: Vec<TextScene>,
}

/// Một cảnh chữ: nội dung + khoảng thời gian hiện (giây theo timeline audio).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TextScene {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub start_sec: Option<f64>,
    /// None = hiện đến khi cảnh kế tiếp bắt đầu (hoặc hết video).
    #[serde(default)]
    pub end_sec: Option<f64>,
    /// None = dùng vị trí của lớp chữ.
    #[serde(default)]
    pub position: Option<TextPosition>,
}

/// Lớp accent vẽ dưới chữ chính (đúng thứ tự filter: accent trước, chính sau).
#[derive(Debug, Clone, Deserialize)]
pub struct AccentLayer {
    /// Màu lớp nền (đỏ neon, vàng, xanh...).
    pub color: String,
    /// Lệch theo trục x px @1080 (âm = trái).
    #[serde(default)]
    pub dx: f64,
    /// Lệch theo trục y px @1080 (âm = lên).
    #[serde(default)]
    pub dy: f64,
}

impl Default for TextOverlayOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            content: String::new(),
            font: FontChoice::Default,
            size_px: default_text_size(),
            color: default_white(),
            border_w: default_border_w(),
            border_color: default_black(),
            position: TextPosition::Bottom,
            start_sec: None,
            end_sec: None,
            box_color: None,
            box_opacity: default_box_opacity(),
            box_padding: default_box_padding(),
            shadow_offset: None,
            shadow_color: default_black(),
            letter_spacing: 0.0,
            uppercase: false,
            accent_layer: None,
            animation: TextAnimation::default(),
            anim_duration: default_anim_duration(),
            scenes: Vec::new(),
        }
    }
}

/// Phụ đề burn-in: file .srt upload hoặc tự nhận diện (ASR), app sinh ASS.
#[derive(Debug, Clone, Deserialize)]
pub struct SubtitleOptions {
    #[serde(default)]
    pub enabled: bool,
    /// Phụ đề Tự động: sinh .srt bằng ASR từ audio thay vì upload file.
    #[serde(default)]
    pub auto: bool,
    /// Cỡ chữ theo baseline 1080px (px THẬT của khung — PlayRes = khung).
    #[serde(default = "default_sub_size")]
    pub font_size: u32,
    #[serde(default = "default_white")]
    pub color: String,
    #[serde(default)]
    pub font: FontChoice,
    /// Kiểu: "classic" | "box" | "neon" | "soft".
    #[serde(default)]
    pub style: String,
    /// Vị trí: dưới / giữa / trên.
    #[serde(default)]
    pub position: TextPosition,
    /// Màu viền cho kiểu "neon" (mặc định xanh neon).
    #[serde(default = "default_neon")]
    pub outline_color: String,
}

impl Default for SubtitleOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            auto: false,
            font_size: default_sub_size(),
            color: default_white(),
            font: FontChoice::Default,
            style: String::new(),
            position: TextPosition::Bottom,
            outline_color: default_neon(),
        }
    }
}

impl SubtitleOptions {
    pub fn active(&self) -> bool {
        self.enabled
    }

    pub fn kind(&self) -> SubtitleStyleKind {
        SubtitleStyleKind::parse(&self.style)
    }
}

/// Watermark tên kênh / logo. Tên kênh để trống (kind=text) = ẩn hoàn toàn.
#[derive(Debug, Clone, Deserialize)]
pub struct WatermarkOptions {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub kind: WmKind,
    /// Tên kênh (chế độ text). Rỗng = không watermark text.
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub font: FontChoice,
    #[serde(default = "default_wm_size")]
    pub size_px: u32,
    #[serde(default = "default_white")]
    pub color: String,
    #[serde(default = "default_wm_border_w")]
    pub border_w: u32,
    #[serde(default = "default_black")]
    pub border_color: String,
    /// Bóng đổ (chế độ text).
    #[serde(default)]
    pub shadow: bool,
    /// Độ mờ 0–100 (100 = đục).
    #[serde(default = "default_wm_opacity")]
    pub opacity_pct: u32,
    /// Lề từ mép khung, px theo baseline 1080.
    #[serde(default = "default_wm_margin")]
    pub margin_px: u32,
    #[serde(default)]
    pub position: WmPosition,
    /// Logo chiếm % chiều rộng video (chế độ logo).
    #[serde(default = "default_wm_logo_scale")]
    pub logo_scale_pct: u32,
    /// Chế độ di chuyển: đổi góc mỗi N giây (chống crop logo).
    #[serde(default)]
    pub moving: bool,
    #[serde(default = "default_wm_move_every")]
    pub move_every_sec: u32,
}

impl Default for WatermarkOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            kind: WmKind::Text,
            channel_name: String::new(),
            font: FontChoice::Default,
            size_px: default_wm_size(),
            color: default_white(),
            border_w: default_wm_border_w(),
            border_color: default_black(),
            shadow: false,
            opacity_pct: default_wm_opacity(),
            margin_px: default_wm_margin(),
            position: WmPosition::BottomRight,
            logo_scale_pct: default_wm_logo_scale(),
            moving: false,
            move_every_sec: default_wm_move_every(),
        }
    }
}

impl WatermarkOptions {
    /// Watermark text có render không? (bật + kind=text + tên kênh khác rỗng)
    pub fn active_text(&self) -> bool {
        self.enabled && self.kind == WmKind::Text && !self.channel_name.trim().is_empty()
    }

    /// Watermark logo có render không? (bật + kind=logo — file do multipart
    /// cung cấp, jobs kiểm tra tồn tại.)
    pub fn active_logo(&self) -> bool {
        self.enabled && self.kind == WmKind::Logo
    }
}

impl TextOverlayOptions {
    /// Có lớp chữ để render không (bật + có nội dung hoặc có cảnh)?
    pub fn active(&self) -> bool {
        self.enabled && (!self.content.trim().is_empty() || !self.scenes.is_empty())
    }
}

fn default_box_opacity() -> u32 {
    80
}
fn default_box_padding() -> u32 {
    16
}
fn default_text_size() -> u32 {
    42
}
fn default_wm_size() -> u32 {
    36
}
fn default_white() -> String {
    "#FFFFFF".into()
}
fn default_black() -> String {
    "#000000".into()
}
fn default_border_w() -> u32 {
    3
}
fn default_wm_border_w() -> u32 {
    2
}
fn default_wm_opacity() -> u32 {
    70
}
fn default_wm_margin() -> u32 {
    24
}
fn default_wm_logo_scale() -> u32 {
    18
}
fn default_wm_move_every() -> u32 {
    5
}
/// 52px @1080 ≈ cỡ phụ đề YouTube — px thật của khung (bản 0.3 ghi "28" nhưng
/// libass phóng lên 105–187px tuỳ tỉ lệ khung).
fn default_sub_size() -> u32 {
    52
}
fn default_neon() -> String {
    "#39FF14".into()
}
fn default_anim_duration() -> f64 {
    0.6
}

// ---------------------------------------------------------------------------
// Màu
// ---------------------------------------------------------------------------

/// "#RRGGBB" → (R, G, B). None nếu không đúng dạng.
pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Màu + alpha cho `fontcolor`/`bordercolor` của drawtext.
/// Hex "#RRGGBB" → "0xRRGGBB"; giá trị khác coi là tên màu ffmpeg.
pub fn drawtext_color(color: &str, alpha: f64) -> String {
    let base = match parse_hex_color(color) {
        Some((r, g, b)) => format!("0x{r:02X}{g:02X}{b:02X}"),
        None => color.trim().to_string(),
    };
    if (alpha - 1.0).abs() < f64::EPSILON {
        base
    } else {
        format!("{base}@{alpha:.3}")
    }
}

/// Màu cho ASS `PrimaryColour`: "#RRGGBB" → "&H00BBGGRR&".
/// Kịch bản cảnh chữ: backend sẵn sàng, chưa có nút UI — dành cho bản sau.
#[allow(dead_code)]
pub fn ass_color(color: &str) -> String {
    match parse_hex_color(color) {
        Some((r, g, b)) => format!("&H00{b:02X}{g:02X}{r:02X}&"),
        None => "&H00FFFFFF&".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Ngữ cảnh khung + safe-zone
// ---------------------------------------------------------------------------

/// Kích thước khung đích + safe-zone của preset.
#[derive(Debug, Clone, Copy)]
pub struct FrameCtx {
    pub width: u32,
    pub height: u32,
    pub safe: Option<SafeZone>,
    pub fps: u32,
    /// `false` = bỏ qua safe-zone (người dùng tự chịu trách nhiệm với nền
    /// tảng che chữ).
    pub respect_safe_zone: bool,
}

impl FrameCtx {
    pub fn new(width: u32, height: u32, safe: Option<SafeZone>) -> Self {
        Self { width, height, safe, fps: 30, respect_safe_zone: true }
    }

    /// Hệ số co giãn kích thước chữ/lề: min(W,H)/1080.
    pub fn scale_factor(&self) -> f64 {
        self.width.min(self.height) as f64 / 1080.0
    }

    /// Vùng an toàn cần né ở từng cạnh (top, bottom, left, right) — px của
    /// khung. Zero khi preset không có safe-zone hoặc người dùng tắt.
    pub fn insets(&self) -> (f64, f64, f64, f64) {
        let s = self.safe.unwrap_or(SafeZone { top: 0, bottom: 0, left: 0, right: 0 });
        if !self.respect_safe_zone {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let k = self.scale_factor();
        (s.top as f64 * k, s.bottom as f64 * k, s.left as f64 * k, s.right as f64 * k)
    }

    /// Lề hiệu dụng từng cạnh cho watermark: margin người dùng + vùng an toàn
    /// + vùng bị chiếm (visualizer/thanh tiến độ), để watermark không bị
    /// nền tảng che và không đè lên visualizer. Trả (top, bottom, left, right).
    fn wm_edges(&self, margin_px: u32, reserve: Reserves) -> (f64, f64, f64, f64) {
        let (top, bottom, left, right) = self.insets();
        let m = margin_px as f64 * self.scale_factor();
        (top + m + reserve.top, bottom + m + reserve.bottom, left + m, right + m)
    }
}

/// Vùng đã bị chiếm (visualizer, thanh tiến độ…) mà chữ/watermark phải né.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Reserves {
    pub top: f64,
    pub bottom: f64,
}

/// Ngữ cảnh thời gian của phần video đang render.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TextLayout {
    /// Dời thời gian: `t` của phần video → timeline của audio gốc (chia phần).
    pub offset: f64,
    /// Thời lượng phần video — để làm mờ dần chữ chính ở cuối (None = không).
    pub duration: Option<f64>,
    /// Vùng bị chiếm (phụ đề, visualizer) tính từ mép trên/dưới.
    pub reserve: Reserves,
}

impl TextLayout {
    /// Biến thời gian trong biểu thức: `t` hoặc `(t+offset)`.
    fn t_expr(&self) -> String {
        if self.offset.abs() < 1e-6 {
            "t".into()
        } else {
            format!("(t+{:.3})", self.offset)
        }
    }
}

// ---------------------------------------------------------------------------
// Kết quả lắp overlay
// ---------------------------------------------------------------------------

/// Thanh tiến độ dưới đáy khung: rãnh tĩnh + thanh màu trượt theo thời gian.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressSpec {
    /// Bề rộng khung — thanh màu rộng bằng khung, trượt từ phải sang trái.
    pub width: u32,
    pub fps: u32,
    /// Thời lượng phần video (giây).
    pub duration: f64,
    /// Màu thanh, dạng `0xRRGGBB`.
    pub color: String,
    pub track_y: u32,
    pub track_h: u32,
    pub bar_y: u32,
    pub bar_h: u32,
}

impl ProgressSpec {
    /// Tính hình học cho khung `frame`: rãnh 12px @1080, thanh 8px @1080, nằm
    /// trên vùng an toàn dưới của preset.
    pub fn new(frame: &FrameCtx, duration: f64, color: &str) -> Self {
        let k = frame.scale_factor();
        let track_h = (12.0 * k).round().max(4.0) as u32;
        let bar_h = (8.0 * k).round().max(2.0) as u32;
        let (_, bottom, _, _) = frame.insets();
        let track_y = (frame.height as f64 - bottom - track_h as f64).round().max(0.0) as u32;
        Self {
            width: frame.width,
            fps: frame.fps.max(1),
            duration: duration.max(0.1),
            color: drawtext_color(color, 1.0),
            track_y,
            track_h,
            bar_y: track_y + (track_h - bar_h) / 2,
            bar_h,
        }
    }
}

/// Kết quả lắp overlay cho pass cuối.
#[derive(Debug, Clone, Default)]
pub struct OverlaySpec {
    /// Chuỗi filter áp nối tiếp trên luồng video (fit → subtitles → text → wm text).
    pub chain: Vec<String>,
    /// Filter khớp khung đích chèn ĐẦU chuỗi (scale/crop/blur từ chuỗi ghép
    /// sang khung preset) — không label.
    pub pre_scale: Option<String>,
    /// Graph visualizer `[1:a]showwaves…[viz]` + filter `overlay=…` lắp sau.
    pub viz_graph: Option<String>,
    pub viz_overlay: Option<String>,
    /// Thanh tiến độ (stage overlay).
    pub progress: Option<ProgressSpec>,
    /// Watermark logo: `[2:v]scale…[wm]` + toạ độ overlay.
    pub logo_prep: Option<String>,
    pub logo_xy: Option<(String, String)>,
}

impl OverlaySpec {
    /// Có cần decode + re-encode video trong pass cuối không.
    pub fn has_video_filters(&self) -> bool {
        !self.chain.is_empty()
            || self.pre_scale.as_deref().is_some_and(|p| !p.is_empty())
            || self.viz_overlay.is_some()
            || self.progress.is_some()
            || self.logo_xy.is_some()
    }

    /// Pass cuối có cần thêm input logo (input thứ 3) không?
    pub fn needs_logo_input(&self) -> bool {
        self.logo_xy.is_some()
    }
}

/// Ghép thành `-filter_complex` hoàn chỉnh. Thứ tự stage: chữ/phụ đề (trong
/// `chain`) → visualizer → thanh tiến độ → logo; stage cuối ghi nhãn `[vout]`.
pub fn compose_filter_complex(spec: &OverlaySpec) -> String {
    let mut g = String::from("[0:v]");
    let mut has_filter = false;
    if let Some(pre) = spec.pre_scale.as_deref().filter(|p| !p.is_empty()) {
        g.push_str(pre);
        has_filter = true;
    }
    if !spec.chain.is_empty() {
        if has_filter {
            g.push(',');
        }
        g.push_str(&spec.chain.join(","));
        has_filter = true;
    }
    let stages = usize::from(spec.viz_overlay.is_some())
        + usize::from(spec.progress.is_some())
        + usize::from(spec.logo_xy.is_some());
    if stages == 0 {
        g.push_str("[vout]");
        return g;
    }
    g.push_str(if has_filter { "[v0];" } else { "null[v0];" });

    let mut cur = String::from("v0");
    let mut done = 0usize;
    let next_label = |done: &mut usize| -> String {
        *done += 1;
        if *done == stages { "vout".into() } else { format!("v{done}") }
    };

    if let (Some(graph), Some(ov)) = (spec.viz_graph.as_deref(), spec.viz_overlay.as_deref()) {
        let n = next_label(&mut done);
        g.push_str(graph);
        g.push(';');
        let _ = write!(g, "[{cur}][viz]{ov}[{n}];");
        cur = n;
    }
    if let Some(p) = &spec.progress {
        let n = next_label(&mut done);
        let _ = write!(
            g,
            "[{cur}]drawbox=x=0:y={}:w=iw:h={}:color=black@0.35:t=fill[{cur}d];",
            p.track_y, p.track_h
        );
        let _ = write!(g, "color=c={}:s={}x{}:r={}[pbar];", p.color, p.width, p.bar_h, p.fps);
        let _ = write!(
            g,
            "[{cur}d][pbar]overlay=x='-{}+{}*t/{}':y={}:shortest=0[{n}];",
            p.width,
            p.width,
            fmt_num(p.duration),
            p.bar_y
        );
        cur = n;
    }
    if let (Some(prep), Some((x, y))) = (spec.logo_prep.as_deref(), spec.logo_xy.as_ref()) {
        let n = next_label(&mut done);
        g.push_str(prep);
        g.push(';');
        let _ = write!(g, "[{cur}][wm]overlay=x='{x}':y='{y}'[{n}]");
    }
    while g.ends_with(';') {
        g.pop();
    }
    g
}

/// Số thập phân gọn: 8 → "8", 7.5 → "7.5" (cho biểu thức filter).
fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.3}")
    }
}

// ---------------------------------------------------------------------------
// Escape đường dẫn cho FFmpeg (bắt buộc đa nền tảng)
// ---------------------------------------------------------------------------

/// Escape đường dẫn file khi nhúng vào filtergraph (`-vf`, `-filter_complex`):
/// drawtext `textfile=`/`fontfile=`, subtitles `filename=`…
///
/// Trong filtergraph, `:` phân tách tham số và `\` là ký tự thoát, nên với
/// đường dẫn Windows tuyệt đối:
/// - `\` đổi thành `/` (tránh bị hiểu là escape sequence);
/// - `:` của ổ đĩa (`C:`) escape thành `C\:` để không cắt đôi tham số.
///
/// Giá trị trả về nằm **bên trong** cặp nháy đơn `'…'` của tham số —
/// dấu nháy đơn trong tên file (hiếm) cũng được escape `\'`.
pub fn escape_filter_path(path: &Path) -> Cow<'_, str> {
    let s = path.to_string_lossy();
    if !s.contains(['\\', ':', '\'']) {
        return s;
    }
    // `/` an toàn tuyệt đối trong filtergraph (kể cả Windows API chấp nhận).
    let s = s.replace('\\', "/");
    // Chỉ escape `:` của ổ đĩa (vị trí 2, sau chữ cái) — đường dẫn UNC
    // `//server/share` không chứa `:` nên không dính.
    let mut out = String::with_capacity(s.len() + 4);
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' && i == 1 && bytes[0].is_ascii_alphabetic() {
            out.push_str("\\:");
        } else if b == b'\'' {
            out.push_str("\\'");
        } else {
            out.push(b as char);
        }
    }
    Cow::Owned(out)
}

// ---------------------------------------------------------------------------
// Nội dung chữ: giãn chữ, xuống dòng, kịch bản nhiều cảnh
// ---------------------------------------------------------------------------

/// Ký tự "dính" vào ký tự trước (dấu kết hợp, ZWJ, variation selector) —
/// không được chèn khoảng cách ngay trước hay ngay sau nó.
pub fn is_combining(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0x200D)
}

/// Chèn `reps` ký tự `spacer` giữa các cặp ký tự liền nhau. Bỏ qua: sau ký tự
/// cuối cùng, hai bên khoảng trắng/xuống dòng, hai bên dấu kết hợp.
fn space_out(s: &str, spacer: char, reps: usize) -> String {
    let gap: String = std::iter::repeat(spacer).take(reps).collect();
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + chars.len() * reps * 4);
    for (i, &c) in chars.iter().enumerate() {
        out.push(c);
        // ZWJ / variation selector dính vào ký tự SAU (emoji ghép) — không được
        // chèn gì sau chúng nếu không emoji bị tách rời.
        let joins_next = |c: char| matches!(c as u32, 0x200D | 0xFE00..=0xFE0F);
        if c == '\n' || c == '\r' || joins_next(c) || (c.is_whitespace() && !is_combining(c)) {
            continue;
        }
        match chars.get(i + 1) {
            // Có khoảng giãn TRƯỚC khoảng trắng (các từ cách nhau rộng hơn,
            // giống letter-spacing của CSS) nhưng KHÔNG sau nó — để khi xuống
            // dòng, đầu dòng không bị thụt vào một ký tự khoảng cách. Dấu
            // kết hợp thì dính vào chữ TRƯỚC, nên khoảng giãn nằm sau cả cụm.
            Some(&n) if !is_combining(n) && n != '\n' && n != '\r' => out.push_str(&gap),
            _ => {}
        }
    }
    out
}

/// VIẾT HOA + giãn chữ. `spacing_px` = khoảng cách thật giữa hai ký tự với cỡ
/// `font_px`; chỉ chèn khi font CÓ ký tự khoảng cách hẹp đủ (xem
/// `FontMetrics::spacer`) — font thiếu glyph sẽ không bị vẽ ô vuông.
pub fn style_text(
    content: &str,
    uppercase: bool,
    spacing_px: f64,
    font_px: f64,
    metrics: Option<&FontMetrics>,
) -> String {
    let text: Cow<str> = if uppercase { Cow::Owned(content.to_uppercase()) } else { Cow::Borrowed(content) };
    let (spacer, reps) = match metrics {
        Some(m) if spacing_px > 0.0 => m.spacer(spacing_px, font_px),
        _ => (None, 0),
    };
    match spacer {
        Some(ch) if reps > 0 => space_out(&text, ch, reps),
        _ => text.into_owned(),
    }
}

/// Bề rộng ước lượng theo em (bảng `char_width_em` của module subtitle).
fn width_em(s: &str) -> f64 {
    s.chars().map(subtitle::char_width_em).sum()
}

/// Tự xuống dòng các đoạn dài hơn `max_width_px` tại khoảng trắng. Xuống
/// dòng do người dùng gõ (`\n`) được giữ nguyên.
pub fn wrap_text(text: &str, max_width_px: f64, font_px: f64, uppercase: bool) -> String {
    // Chữ hoa rộng hơn trung bình → cộng thêm 15% để không ước lượng thiếu.
    let factor = if uppercase { 1.15 } else { 1.0 };
    let mut out = String::new();
    for (i, para) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let fits = |s: &str| width_em(s) * font_px * factor <= max_width_px;
        if fits(para) || !para.contains(' ') {
            out.push_str(para);
            continue;
        }
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if line.is_empty() || fits(&candidate) {
                line = candidate;
            } else {
                out.push_str(&line);
                out.push('\n');
                line = word.to_string();
            }
        }
        out.push_str(&line);
    }
    out
}

/// Bề rộng khả dụng cho một dòng chữ: khung trừ vùng an toàn hai bên, trừ lề
/// 4% mỗi cạnh, trừ đệm nền chữ (nếu có nền).
fn usable_text_width(t: &TextOverlayOptions, frame: &FrameCtx) -> f64 {
    let (_, _, left, right) = frame.insets();
    let w = frame.width as f64;
    let mut usable = w - left - right - w * 0.08;
    if t.box_color.as_deref().is_some_and(|c| !c.is_empty()) {
        usable -= 2.0 * t.box_padding as f64 * frame.scale_factor();
    }
    usable.max(w * 0.3)
}

/// Một lớp chữ cần vẽ: nội dung đã style (sẽ ghi ra file riêng) + thời gian hiện.
#[derive(Debug, Clone, PartialEq)]
pub struct TextItem {
    pub content: String,
    pub start_sec: Option<f64>,
    pub end_sec: Option<f64>,
    pub position: TextPosition,
}

/// Liệt kê các lớp chữ của `t` (kèm nội dung đã HOA/giãn/xuống dòng): chữ
/// chính khi không có cảnh, ngược lại là từng cảnh. Trả rỗng nếu không active.
///
/// Mục 0 là "chính": `end_sec` None = hiện đến hết; các cảnh sau được mặc
/// định kết thúc khi cảnh kế tiếp bắt đầu.
pub fn text_items(t: &TextOverlayOptions, font: &ResolvedFont, frame: &FrameCtx) -> Vec<TextItem> {
    if !t.enabled {
        return Vec::new();
    }
    let scale = frame.scale_factor();
    let font_px = t.size_px as f64 * scale;
    let spacing_px = t.letter_spacing.max(0.0) * scale;
    let max_width = usable_text_width(t, frame);

    // (nội dung gốc, bắt đầu, kết thúc, vị trí)
    let mut plan: Vec<(String, Option<f64>, Option<f64>, TextPosition)> = Vec::new();
    for sc in &t.scenes {
        if sc.content.trim().is_empty() {
            continue;
        }
        let start = sc.start_sec.map(|v| v.max(0.0));
        let end = sc.end_sec.filter(|e| start.is_none_or(|s| *e > s));
        plan.push((sc.content.clone(), start, end, sc.position.unwrap_or(t.position)));
    }
    let from_scenes = !plan.is_empty();
    if !from_scenes {
        if t.content.trim().is_empty() {
            return Vec::new();
        }
        plan.push((t.content.clone(), t.start_sec, t.end_sec, t.position));
    }
    // Sắp theo thời điểm bắt đầu; cảnh chưa có mốc kết thúc lấy mốc bắt đầu
    // của cảnh kế tiếp làm mốc kết thúc.
    plan.sort_by(|a, b| a.1.unwrap_or(f64::NEG_INFINITY).total_cmp(&b.1.unwrap_or(f64::NEG_INFINITY)));
    if from_scenes {
        for i in 0..plan.len() {
            if plan[i].2.is_some() {
                continue;
            }
            let s = plan[i].1.unwrap_or(0.0);
            plan[i].2 = plan[i + 1..].iter().filter_map(|n| n.1).find(|ns| *ns > s);
        }
    }
    plan.into_iter()
        .map(|(content, start_sec, end_sec, position)| {
            let styled = style_text(&content, t.uppercase, spacing_px, font_px, font.metrics.as_ref());
            TextItem { content: wrap_text(&styled, max_width, font_px, t.uppercase), start_sec, end_sec, position }
        })
        .collect()
}

/// Đọc kịch bản nhiều cảnh — mỗi dòng một cảnh, các dạng thời gian:
/// `[mm:ss-mm:ss] nội dung` · `[mm:ss] nội dung` · `mm:ss | nội dung` ·
/// `90 | nội dung` (giây). Dòng không có mốc thời gian đứng ĐẦU tiên = cảnh
/// hiện từ đầu video; đứng sau thì nối tiếp nội dung cảnh trước (nhiều dòng).
/// Kịch bản cảnh chữ: backend sẵn sàng, chưa có nút UI — dành cho bản sau.
#[allow(dead_code)]
pub fn parse_scene_script(script: &str) -> Vec<TextScene> {
    let mut scenes: Vec<TextScene> = Vec::new();
    for raw in script.lines() {
        let line = strip_list_marker(raw.trim());
        if line.is_empty() {
            continue;
        }
        match split_time_prefix(line) {
            Some(((start, end), content)) => {
                if content.trim().is_empty() {
                    continue;
                }
                scenes.push(TextScene {
                    content: content.trim().to_string(),
                    start_sec: Some(start),
                    end_sec: end,
                    position: None,
                });
            }
            None => match scenes.last_mut() {
                Some(prev) => prev.content = format!("{}\n{line}", prev.content),
                None => scenes.push(TextScene { content: line.to_string(), ..Default::default() }),
            },
        }
    }
    scenes
}

/// Bỏ dấu đầu dòng kiểu danh sách: "- ", "* ", "• ", "1. ", "1) ".
/// Kịch bản cảnh chữ: backend sẵn sàng, chưa có nút UI — dành cho bản sau.
#[allow(dead_code)]
fn strip_list_marker(line: &str) -> &str {
    if let Some(rest) = ["- ", "* ", "• "].iter().find_map(|m| line.strip_prefix(*m)) {
        return rest.trim_start();
    }
    let bytes = line.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0
        && digits <= 3
        && matches!(bytes.get(digits), Some(b'.') | Some(b')'))
        && bytes.get(digits + 1) == Some(&b' ')
    {
        return line[digits + 2..].trim_start();
    }
    line
}

/// Tách "[mm:ss-mm:ss] nội dung" / "[mm:ss] nội dung" / "mm:ss | nội dung".
/// Kịch bản cảnh chữ: backend sẵn sàng, chưa có nút UI — dành cho bản sau.
#[allow(dead_code)]
fn split_time_prefix(line: &str) -> Option<((f64, Option<f64>), &str)> {
    if let Some(rest) = line.strip_prefix('[') {
        let close = rest.find(']')?;
        let span = parse_time_span(&rest[..close])?;
        let content = rest[close + 1..].trim_start_matches([' ', '-', '–']).trim();
        return Some((span, content));
    }
    let bar = line.find('|')?;
    let span = parse_time_span(&line[..bar])?;
    Some((span, line[bar + 1..].trim()))
}

/// "mm:ss-mm:ss" → (bắt đầu, Some(kết thúc)); "mm:ss" → (giây, None).
/// Kịch bản cảnh chữ: backend sẵn sàng, chưa có nút UI — dành cho bản sau.
#[allow(dead_code)]
fn parse_time_span(s: &str) -> Option<(f64, Option<f64>)> {
    let s = s.trim();
    if let Some((i, c)) = s.char_indices().find(|(_, c)| matches!(c, '-' | '–' | '→')) {
        let a = parse_clock(&s[..i])?;
        let b = parse_clock(&s[i + c.len_utf8()..])?;
        return Some((a, Some(b)));
    }
    Some((parse_clock(s)?, None))
}

/// "90" / "90.5" / "mm:ss" / "h:mm:ss" → giây.
/// Kịch bản cảnh chữ: backend sẵn sàng, chưa có nút UI — dành cho bản sau.
#[allow(dead_code)]
fn parse_clock(s: &str) -> Option<f64> {
    let nums: Vec<f64> = s.trim().split(':').map(|p| p.trim().parse::<f64>().ok()).collect::<Option<Vec<_>>>()?;
    if nums.iter().any(|n| !n.is_finite() || *n < 0.0) {
        return None;
    }
    match nums.as_slice() {
        [s] => Some(*s),
        [m, s] if *s < 60.0 => Some(m * 60.0 + s),
        [h, m, s] if *m < 60.0 && *s < 60.0 => Some(h * 3600.0 + m * 60.0 + s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// drawtext dùng textfile (nội dung nằm trong file, đường dẫn sạch)
// ---------------------------------------------------------------------------

/// Tham số hiệu ứng vào/ra đã tính thành biểu thức của ffmpeg.
#[derive(Debug, Clone, Default, PartialEq)]
struct Anim {
    /// Biểu thức `alpha=` (None = luôn đục).
    alpha: Option<String>,
    /// Phần nối thêm vào `y=` để dịch chữ khi vào (None = đứng yên).
    y_shift: Option<String>,
}

/// `s` = 3 → "+3", `-3` → "-3", `0` → "" (không nối gì).
fn signed(v: i32) -> String {
    match v {
        0 => String::new(),
        v => format!("{v:+}"),
    }
}

fn build_anim(
    anim: TextAnimation,
    t_expr: &str,
    start: Option<f64>,
    end: Option<f64>,
    duration_req: f64,
    size: u32,
    from_top: bool,
) -> Anim {
    if anim == TextAnimation::Off {
        return Anim::default();
    }
    let s = start.unwrap_or(0.0).max(0.0);
    let mut d = duration_req.clamp(0.1, 3.0);
    // Cảnh quá ngắn: thời lượng hiệu ứng ≤ nửa cảnh để kịp vào lẫn ra.
    if let Some(e) = end {
        d = d.min(((e - s) / 2.0).max(0.05));
    }
    let a_in = format!("clip(({t_expr}-{s:.3})/{d:.3},0,1)");
    let alpha = match end {
        Some(e) => format!("min({a_in},clip(({e:.3}-{t_expr})/{d:.3},0,1))"),
        None => a_in.clone(),
    };
    let dir = if from_top { "-" } else { "+" };
    let y_shift = match anim {
        TextAnimation::Slide => Some(format!("{dir}{}*(1-{a_in})", (1.5 * size as f64).round() as u32)),
        TextAnimation::Pop => Some(format!(
            "{dir}{size}*(1-(1+2.70158*pow({a}-1,3)+1.70158*pow({a}-1,2)))",
            a = a_in
        )),
        _ => None,
    };
    Anim { alpha: Some(alpha), y_shift }
}

/// Toạ độ y của lớp chữ: `margin` = lề từ mép trên/dưới, `dy` = dịch thêm.
fn y_expr(position: TextPosition, margin: u32, dy: i32) -> String {
    match position {
        TextPosition::Top => format!("{margin}{}", signed(dy)),
        TextPosition::Middle => format!("(h-text_h)/2{}", signed(dy)),
        TextPosition::Bottom => format!("h-text_h-{margin}{}", signed(-dy)),
    }
}

/// Build filter `drawtext` cho MỘT lớp chữ (`TextItem` của [`text_items`]).
/// Nội dung đã style được `jobs.rs` ghi sẵn vào `text_path`. Không có nội dung
/// → None. `layout` cho biết dời thời gian, thời lượng phần và vùng phải né.
pub fn build_text_filter(
    item: &TextItem,
    o: &TextOverlayOptions,
    text_path: &Path,
    font: &ResolvedFont,
    frame: &FrameCtx,
    layout: &TextLayout,
) -> Option<String> {
    if item.content.trim().is_empty() {
        return None;
    }
    let scale = frame.scale_factor();
    let size = (o.size_px as f64 * scale).round().max(10.0) as u32;
    let font_opt = font.drawtext_options();

    // Lề dọc: 4% chiều cao, nhưng không nhỏ hơn vùng an toàn/vùng bị chiếm.
    let (inset_top, inset_bottom, _, _) = frame.insets();
    let margin_y = frame.height as f64 * 0.04;
    let margin = (match item.position {
        TextPosition::Top => margin_y.max(inset_top).max(layout.reserve.top),
        TextPosition::Middle => 0.0,
        TextPosition::Bottom => margin_y.max(inset_bottom).max(layout.reserve.bottom),
    })
    .round() as u32;

    // Thời gian hiện (timeline audio gốc — trùng timeline video sau ghép).
    let t_expr = layout.t_expr();
    let enable = match (item.start_sec, item.end_sec) {
        (Some(s), Some(e)) if e > s => Some(format!("between({t_expr},{s:.3},{e:.3})")),
        (Some(s), None) if s > 0.0 => Some(format!("gte({t_expr},{s:.3})")),
        (None, Some(e)) => Some(format!("lte({t_expr},{e:.3})")),
        _ => None,
    };
    let enable_opt = enable.as_ref().map(|e| format!(":enable='{e}'")).unwrap_or_default();

    // Hiệu ứng: mờ dần ra ở cuối phần video nếu không có mốc kết thúc.
    let anim = build_anim(
        o.animation,
        &t_expr,
        item.start_sec,
        item.end_sec.or(layout.duration),
        o.anim_duration,
        size,
        item.position == TextPosition::Top,
    );
    let alpha_opt = anim.alpha.as_ref().map(|a| format!(":alpha='{a}'")).unwrap_or_default();
    let shift = anim.y_shift.as_deref().unwrap_or("");

    // ---- Nền chữ (box) ----
    let box_opt = match (&o.box_color, o.box_opacity) {
        (Some(c), _) if !c.is_empty() => {
            let alpha = (o.box_opacity as f64 / 100.0).clamp(0.0, 1.0);
            let pad = (o.box_padding as f64 * scale).round().max(0.0) as u32;
            format!(":box=1:boxcolor={}:boxborderw={pad}", drawtext_color(c, alpha))
        }
        _ => String::new(),
    };

    // ---- Bóng đổ ----
    let shadow_opt = match o.shadow_offset {
        Some(off) if off > 0.0 => {
            let d = (off * scale).round().max(1.0) as i32;
            format!(":shadowcolor={}:shadowx={d}:shadowy={d}", drawtext_color(&o.shadow_color, 0.6))
        }
        _ => String::new(),
    };

    let textfile = escape_filter_path(text_path);
    let mut filters: Vec<String> = Vec::new();

    // ---- Lớp accent (vẽ TRƯỚC để chữ chính đè lên) ----
    if let Some(acc) = &o.accent_layer {
        let dx = (acc.dx * scale).round() as i32;
        let dy = (acc.dy * scale).round() as i32;
        let mut fa = format!("drawtext=textfile='{textfile}'");
        fa.push_str(&font_opt);
        let _ = write!(fa, ":fontsize={size}:fontcolor={}:expansion=none", drawtext_color(&acc.color, 1.0));
        let _ = write!(
            fa,
            ":x='(w-text_w)/2{}':y='{}{shift}'",
            signed(dx),
            y_expr(item.position, margin, dy)
        );
        fa.push_str(&enable_opt);
        fa.push_str(&alpha_opt);
        filters.push(fa);
    }

    // ---- Chữ chính ----
    let mut f = format!("drawtext=textfile='{textfile}'");
    f.push_str(&font_opt);
    let _ = write!(f, ":fontsize={size}:fontcolor={}:expansion=none", drawtext_color(&o.color, 1.0));
    if o.border_w > 0 {
        let bw = (o.border_w as f64 * scale).round().max(1.0) as u32;
        let _ = write!(f, ":borderw={bw}:bordercolor={}", drawtext_color(&o.border_color, 1.0));
    }
    f.push_str(&box_opt);
    f.push_str(&shadow_opt);
    let _ = write!(f, ":x='(w-text_w)/2':y='{}{shift}'", y_expr(item.position, margin, 0));
    f.push_str(&enable_opt);
    f.push_str(&alpha_opt);
    filters.push(f);
    Some(filters.join(","))
}

// ---------------------------------------------------------------------------
// Watermark chữ
// ---------------------------------------------------------------------------

/// Build filter `drawtext` cho watermark tên kênh (nội dung qua textfile).
pub fn build_watermark_text_filter(
    w: &WatermarkOptions,
    text_path: &Path,
    font: &ResolvedFont,
    frame: &FrameCtx,
    reserve: Reserves,
) -> Option<String> {
    if !w.active_text() {
        return None;
    }
    let alpha = (w.opacity_pct as f64 / 100.0).clamp(0.0, 1.0);
    let size = (w.size_px as f64 * frame.scale_factor()).round().max(8.0) as u32;
    let edges = frame.wm_edges(w.margin_px, reserve);

    let mut f = format!("drawtext=textfile='{}'", escape_filter_path(text_path));
    f.push_str(&font.drawtext_options());
    let _ = write!(f, ":fontsize={size}:fontcolor={}", drawtext_color(&w.color, alpha));
    if w.border_w > 0 {
        let bw = (w.border_w as f64 * frame.scale_factor()).round().max(1.0) as u32;
        let _ = write!(f, ":borderw={bw}:bordercolor={}", drawtext_color(&w.border_color, alpha));
    }
    if w.shadow {
        let d = (2.0 * frame.scale_factor()).round().max(1.0) as i32;
        let _ = write!(f, ":shadowcolor=black@0.6:shadowx={d}:shadowy={d}");
    }
    f.push_str(":expansion=none");

    let (x, y) = wm_xy(edges, w.position, w.moving.then_some(w.move_every_sec).filter(|p| *p > 0), false);
    let _ = write!(f, ":x='{x}':y='{y}'");
    Some(f)
}

/// Toạ độ (x, y) của watermark — `logo` đổi sang tên biến của overlay
/// (`main_w`/`overlay_w`), chữ dùng `w`/`text_w` của drawtext.
fn wm_xy(
    edges: (f64, f64, f64, f64),
    position: WmPosition,
    moving_every: Option<u32>,
    logo: bool,
) -> (String, String) {
    let (top, bottom, left, right) = edges;
    let (fw, fh, ow, oh) = if logo { ("main_w", "main_h", "overlay_w", "overlay_h") } else { ("w", "h", "text_w", "text_h") };
    // Số lẻ là px (24.0 → "24"), tránh "204.0" trong biểu thức.
    let px = |v: f64| format!("{}", v.round() as i64);
    let corner = |p: WmPosition| -> (String, String) {
        match p {
            WmPosition::TopLeft => (px(left), px(top)),
            WmPosition::TopRight => (format!("{fw}-{ow}-{}", px(right)), px(top)),
            WmPosition::BottomLeft => (px(left), format!("{fh}-{oh}-{}", px(bottom))),
            WmPosition::BottomRight => (format!("{fw}-{ow}-{}", px(right)), format!("{fh}-{oh}-{}", px(bottom))),
            WmPosition::Center => (format!("({fw}-{ow})/2"), format!("({fh}-{oh})/2")),
        }
    };
    match moving_every {
        // Xoay vòng 4 góc mỗi N giây theo mod(t/N,4) — dấu phẩy an toàn vì
        // biểu thức nằm trong nháy đơn (đã probe PASS).
        Some(p) => {
            let corners = [WmPosition::TopLeft, WmPosition::TopRight, WmPosition::BottomRight, WmPosition::BottomLeft];
            let pick = |v: Vec<String>| {
                let (v0, v1, v2, v3) = (v[0].clone(), v[1].clone(), v[2].clone(), v[3].clone());
                format!("if(lt(mod(t/{p},4),1),{v0},if(lt(mod(t/{p},4),2),{v1},if(lt(mod(t/{p},4),3),{v2},{v3})))")
            };
            (pick(corners.iter().map(|c| corner(*c).0).collect()), pick(corners.iter().map(|c| corner(*c).1).collect()))
        }
        None => corner(position),
    }
}

// ---------------------------------------------------------------------------
// Watermark logo
// ---------------------------------------------------------------------------

/// Watermark logo tách làm 2 phần: `prep` scale/alpha về nhãn `[wm]`, còn
/// toạ độ overlay do `compose_filter_complex` lắp ở stage cuối.
#[derive(Debug, Clone, PartialEq)]
pub struct LogoOverlay {
    pub prep: String,
    pub x: String,
    pub y: String,
}

/// `logo_index` = thứ tự input của file logo trong lệnh ffmpeg (2).
pub fn build_watermark_logo(w: &WatermarkOptions, frame: &FrameCtx, logo_index: usize, reserve: Reserves) -> LogoOverlay {
    let alpha = (w.opacity_pct as f64 / 100.0).clamp(0.0, 1.0);
    let target_w = (frame.width as f64 * w.logo_scale_pct as f64 / 100.0).round().max(16.0) as u32;
    let edges = frame.wm_edges(w.margin_px, reserve);
    let (x, y) = wm_xy(edges, w.position, w.moving.then_some(w.move_every_sec).filter(|p| *p > 0), true);
    LogoOverlay {
        prep: format!("[{logo_index}:v]scale={target_w}:-2,format=rgba,colorchannelmixer=aa={alpha:.3}[wm]"),
        x,
        y,
    }
}

// ---------------------------------------------------------------------------
// Phụ đề (ASS do app sinh — xem module `subtitle`)
// ---------------------------------------------------------------------------

/// Filter burn-in file ASS. `fonts_dir` = thư mục chứa font upload để libass
/// nạp được (libass chỉ tra font theo TÊN, không nhận `fontfile`).
pub fn build_subtitle_filter(ass_path: &Path, fonts_dir: Option<&Path>) -> String {
    let mut f = format!("subtitles=filename='{}'", escape_filter_path(ass_path));
    if let Some(d) = fonts_dir {
        f.push_str(&format!(":fontsdir='{}'", escape_filter_path(d)));
    }
    f
}

/// Style ASS cho phụ đề: cỡ chữ = px thật của khung (PlayRes = khung), lề né
/// vùng an toàn + vùng bị chiếm (visualizer, thanh tiến độ).
pub fn subtitle_style(
    s: &SubtitleOptions,
    font: &ResolvedFont,
    frame: &FrameCtx,
    reserve: Reserves,
) -> AssStyle {
    let (top, bottom, left, right) = frame.insets();
    let (w, h) = (frame.width as f64, frame.height as f64);
    let kind = s.kind();
    let outline_color = match kind {
        SubtitleStyleKind::Neon => subtitle::ass_style_color(&s.outline_color),
        // BorderStyle 3 lấy màu nền từ OutlineColour (alpha 0x40 = mờ 25%).
        SubtitleStyleKind::Box => "&H40000000".into(),
        _ => "&H00000000".into(),
    };
    let (alignment, margin_v) = match s.position {
        TextPosition::Bottom => (2u8, (h * 0.06).max(bottom).max(reserve.bottom)),
        TextPosition::Top => (8u8, (h * 0.06).max(top).max(reserve.top)),
        TextPosition::Middle => (5u8, 0.0),
    };
    AssStyle {
        fontname: font.ass_fontname(),
        fontsize: (s.font_size as f64 * frame.scale_factor()).round().max(8.0) as u32,
        primary: subtitle::ass_style_color(&s.color),
        outline_color,
        bold: true,
        alignment,
        margin_l: (w * 0.05).max(left).round() as u32,
        margin_r: (w * 0.05).max(right).round() as u32,
        margin_v: margin_v.round() as u32,
        kind,
        play_res_x: frame.width,
        play_res_y: frame.height,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::approx_constant)] // 0.318 = số đo font tổng hợp trong test, không phải hằng số toán
mod tests {
    use super::*;

    fn frame_tiktok() -> FrameCtx {
        FrameCtx::new(1080, 1920, Some(SafeZone { top: 140, bottom: 440, left: 40, right: 180 }))
    }

    fn frame_720p() -> FrameCtx {
        FrameCtx::new(1280, 720, None)
    }

    fn clean_path() -> PathBuf {
        PathBuf::from("/job/text_a1b2.txt")
    }

    /// Font không có file (chỉ family) để chuỗi filter ổn định trên mọi máy.
    fn test_font() -> ResolvedFont {
        ResolvedFont { family: Some("DejaVu Sans".into()), ..Default::default() }
    }

    fn text_opts(content: &str) -> TextOverlayOptions {
        TextOverlayOptions { enabled: true, content: content.into(), ..Default::default() }
    }

    fn build(t: &TextOverlayOptions, frame: &FrameCtx, layout: &TextLayout) -> String {
        let font = test_font();
        let items = text_items(t, &font, frame);
        assert!(!items.is_empty(), "phải có ít nhất 1 lớp chữ");
        build_text_filter(&items[0], t, &clean_path(), &font, frame, layout).unwrap()
    }

    fn find_font(name: &str) -> Option<PathBuf> {
        fn rec(dir: &Path, name: &str, depth: u8) -> Option<PathBuf> {
            if depth == 0 {
                return None;
            }
            for e in std::fs::read_dir(dir).ok()?.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if let Some(hit) = rec(&p, name, depth - 1) {
                        return Some(hit);
                    }
                } else if p.file_name().map(|f| f == name).unwrap_or(false) {
                    return Some(p);
                }
            }
            None
        }
        ["/usr/share/fonts", "/usr/local/share/fonts"].iter().find_map(|r| rec(Path::new(r), name, 4))
    }

    // ==== Q1: escape đường dẫn Windows trong filtergraph =====================

    #[test]
    fn escape_windows_drive_path() {
        // C:\job\overlay_text.txt → C\:/job/overlay_text.txt
        assert_eq!(escape_filter_path(Path::new(r"C:\job\overlay_text.txt")), "C\\:/job/overlay_text.txt");
    }

    #[test]
    fn escape_unix_path_khong_doi() {
        assert_eq!(escape_filter_path(Path::new("/tmp/rvm/jobs/x/overlay_text.txt")), "/tmp/rvm/jobs/x/overlay_text.txt");
    }

    #[test]
    fn escape_windows_unc_khong_co_colon() {
        // UNC \\server\share\f.txt → //server/share/f.txt (không có `:` để escape)
        assert_eq!(escape_filter_path(Path::new(r"\\server\share\f.txt")), "//server/share/f.txt");
    }

    #[test]
    fn escape_quote_trong_ten_file() {
        assert_eq!(escape_filter_path(Path::new("/tmp/it's.txt")), "/tmp/it\\'s.txt");
    }

    #[test]
    fn drawtext_textfile_windows_path_duoc_escape() {
        let t = TextOverlayOptions { enabled: true, content: "test".into(), ..Default::default() };
        let font = ResolvedFont::default_font();
        let frame = FrameCtx::new(1080, 1920, None);
        let items = text_items(&t, &font, &frame);
        let f = build_text_filter(&items[0], &t, Path::new(r"C:\job\overlay_text.txt"), &font, &frame, &TextLayout::default())
            .unwrap();
        assert!(f.contains("textfile='C\\:/job/overlay_text.txt'"), "{f}");
    }

    // ==== màu ==============================================================

    #[test]
    fn hex_color_parsing() {
        assert_eq!(parse_hex_color("#FF8040"), Some((0xFF, 0x80, 0x40)));
        assert_eq!(parse_hex_color("ff8040"), None);
        assert_eq!(parse_hex_color("#FF80"), None);
        assert_eq!(parse_hex_color("#GG8040"), None);
    }

    #[test]
    fn drawtext_color_hex_named_alpha() {
        assert_eq!(drawtext_color("#FFFFFF", 1.0), "0xFFFFFF");
        assert_eq!(drawtext_color("white", 1.0), "white");
        assert_eq!(drawtext_color("#FF8040", 0.7), "0xFF8040@0.700");
    }

    #[test]
    fn ass_color_bgr_order() {
        assert_eq!(ass_color("#FF8040"), "&H004080FF&");
        assert_eq!(ass_color("#FFFFFF"), "&H00FFFFFF&");
        assert_eq!(ass_color("not-a-color"), "&H00FFFFFF&");
    }

    // ==== FrameCtx =========================================================

    #[test]
    fn scale_factor_by_min_edge() {
        assert_eq!(frame_tiktok().scale_factor(), 1.0);
        assert!((frame_720p().scale_factor() - 720.0 / 1080.0).abs() < 1e-9);
        assert!((FrameCtx::new(720, 720, None).scale_factor() - 720.0 / 1080.0).abs() < 1e-9);
    }

    #[test]
    fn insets_theo_safe_zone_va_co_the_tat() {
        let (t, b, l, r) = frame_tiktok().insets();
        assert_eq!((t, b, l, r), (140.0, 440.0, 40.0, 180.0));
        assert_eq!(frame_720p().insets(), (0.0, 0.0, 0.0, 0.0));
        let mut off = frame_tiktok();
        off.respect_safe_zone = false;
        assert_eq!(off.insets(), (0.0, 0.0, 0.0, 0.0));
        // Safe-zone tính theo px của preset, nhân hệ số khung nhỏ hơn.
        let small = FrameCtx::new(540, 960, Some(SafeZone { top: 140, bottom: 440, left: 40, right: 180 }));
        let (t, b, _, _) = small.insets();
        assert!((t - 70.0).abs() < 1e-9 && (b - 220.0).abs() < 1e-9, "{t} {b}");
    }

    // ==== chữ overlay ======================================================

    #[test]
    fn text_filter_uses_textfile_and_expansion_none() {
        let f = build(&text_opts("Radio: 100% don't stop"), &frame_720p(), &TextLayout::default());
        assert!(f.starts_with("drawtext=textfile='/job/text_a1b2.txt'"), "{f}");
        assert!(f.contains(":expansion=none"), "{f}");
        // Nội dung KHÔNG xuất hiện trong filter (nằm trong file).
        assert!(!f.contains("100%"), "{f}");
        // Mặc định: bottom, lề 4% chiều cao, viền đen 3px * 0.667 = 2.
        assert!(f.contains("y='h-text_h-29'"), "{f}");
        assert!(f.contains("borderw=2"), "{f}");
        assert!(f.contains("bordercolor=0x000000"), "{f}");
        assert!(f.contains("x='(w-text_w)/2'"), "{f}");
        assert_eq!(f.matches("drawtext=").count(), 1, "{f}");
    }

    #[test]
    fn text_filter_positions() {
        let mut t = text_opts("X");
        t.position = TextPosition::Top;
        let f = build(&t, &frame_720p(), &TextLayout::default());
        assert!(f.contains("y='29'"), "{f}"); // 720*0.04=28.8→29

        t.position = TextPosition::Middle;
        let f = build(&t, &frame_720p(), &TextLayout::default());
        assert!(f.contains("y='(h-text_h)/2'"), "{f}");
    }

    #[test]
    fn text_filter_ne_safe_zone_cua_nen_tang() {
        let mut t = text_opts("X");
        // TikTok: vùng an toàn dưới 440px → chữ nằm trên vùng đó.
        let f = build(&t, &frame_tiktok(), &TextLayout::default());
        assert!(f.contains("y='h-text_h-440'"), "{f}");

        t.position = TextPosition::Top;
        let f = build(&t, &frame_tiktok(), &TextLayout::default());
        assert!(f.contains("y='140'"), "{f}");

        // Tắt né safe-zone → lề 4% như cũ (1920*0.04 = 76.8 → 77).
        let mut frame = frame_tiktok();
        frame.respect_safe_zone = false;
        t.position = TextPosition::Bottom;
        let f = build(&t, &frame, &TextLayout::default());
        assert!(f.contains("y='h-text_h-77'"), "{f}");
    }

    #[test]
    fn text_filter_ne_vung_bi_chiếm() {
        let t = text_opts("X");
        let layout = TextLayout { reserve: Reserves { bottom: 500.0, top: 0.0 }, ..Default::default() };
        let f = build(&t, &frame_tiktok(), &layout);
        assert!(f.contains("y='h-text_h-500'"), "{f}");
        // Vùng bị chiếm nhỏ hơn safe-zone → safe-zone vẫn thắng.
        let layout = TextLayout { reserve: Reserves { bottom: 300.0, top: 0.0 }, ..Default::default() };
        let f = build(&t, &frame_tiktok(), &layout);
        assert!(f.contains("y='h-text_h-440'"), "{f}");
    }

    #[test]
    fn text_filter_time_window() {
        let mut t = text_opts("X");
        t.start_sec = Some(2.0);
        t.end_sec = Some(8.5);
        let f = build(&t, &frame_720p(), &TextLayout::default());
        assert!(f.contains("enable='between(t,2.000,8.500)'"), "{f}");

        t.end_sec = None;
        let f = build(&t, &frame_720p(), &TextLayout::default());
        assert!(f.contains("enable='gte(t,2.000)'"), "{f}");

        t.start_sec = None;
        t.end_sec = Some(5.0);
        let f = build(&t, &frame_720p(), &TextLayout::default());
        assert!(f.contains("enable='lte(t,5.000)'"), "{f}");

        // Phần video thứ 2 (dời 30s): thời gian tính theo timeline audio gốc.
        t.start_sec = Some(2.0);
        t.end_sec = Some(8.5);
        let layout = TextLayout { offset: 30.0, ..Default::default() };
        let f = build(&t, &frame_720p(), &layout);
        assert!(f.contains("enable='between((t+30.000),2.000,8.500)'"), "{f}");
    }

    #[test]
    fn text_filter_size_scales() {
        // 720p: 42*0.667=28; TikTok: 42.
        let f = build(&text_opts("X"), &frame_720p(), &TextLayout::default());
        assert!(f.contains("fontsize=28"), "{f}");
        let f = build(&text_opts("X"), &frame_tiktok(), &TextLayout::default());
        assert!(f.contains("fontsize=42"), "{f}");
    }

    // ==== style nâng cao (bản 0.3) =========================================

    #[test]
    fn text_filter_box_nen() {
        let mut t = text_opts("X");
        t.box_color = Some("#111827".into());
        t.box_opacity = 80;
        t.box_padding = 16;
        let f = build(&t, &frame_tiktok(), &TextLayout::default());
        assert!(f.contains("box=1"), "{f}");
        assert!(f.contains("boxcolor=0x111827@0.800"), "{f}");
        assert!(f.contains("boxborderw=16"), "{f}");
    }

    #[test]
    fn text_filter_box_rong_khong_box() {
        let mut t = text_opts("X");
        t.box_color = Some(String::new());
        let f = build(&t, &frame_720p(), &TextLayout::default());
        assert!(!f.contains("box=1"), "{f}");
    }

    #[test]
    fn text_filter_box_padding_scales() {
        let mut t = text_opts("X");
        t.box_color = Some("#000000".into());
        t.box_padding = 30;
        let f = build(&t, &frame_720p(), &TextLayout::default());
        assert!(f.contains("boxborderw=20"), "{f}"); // 720p: 30*0.667=20
    }

    #[test]
    fn text_filter_shadow() {
        let mut t = text_opts("X");
        t.shadow_offset = Some(3.0);
        t.shadow_color = "#000000".into();
        let f = build(&t, &frame_tiktok(), &TextLayout::default());
        assert!(f.contains("shadowcolor=0x000000@0.600"), "{f}");
        assert!(f.contains("shadowx=3"), "{f}");
        assert!(f.contains("shadowy=3"), "{f}");
        let f2 = build(&text_opts("X"), &frame_tiktok(), &TextLayout::default());
        assert!(!f2.contains("shadowx="), "{f2}");
    }

    #[test]
    fn text_filter_accent_layer_ve_truoc() {
        let mut t = text_opts("X");
        t.accent_layer = Some(AccentLayer { color: "#EF4444".into(), dx: 3.0, dy: 3.0 });
        let f = build(&t, &frame_tiktok(), &TextLayout::default());
        assert_eq!(f.matches("drawtext=").count(), 2, "{f}");
        let split = f.find(",drawtext=").unwrap();
        let first = &f[..split];
        assert!(first.contains("fontcolor=0xEF4444"), "{f}");
        assert!(first.contains("x='(w-text_w)/2+3'"), "{f}");
        // dy = +3 → accent thấp hơn chữ chính 3px (margins 440 → 437).
        assert!(first.contains("y='h-text_h-440-3'"), "{f}");
        let second = &f[split..];
        assert!(second.contains("fontcolor=0xFFFFFF"), "{f}");
        assert!(second.contains("y='h-text_h-440'"), "{f}");
    }

    #[test]
    fn style_text_hoa_va_gian_chu_theo_metric_font() {
        let dejavu = FontMetrics { units_per_em: 2048, hair_space: Some(0.1), thin_space: Some(0.2), space: Some(0.318) };
        assert_eq!(style_text("Đài phát thanh", true, 0.0, 42.0, Some(&dejavu)), "ĐÀI PHÁT THANH");
        // 72px: hair = 7.2px. Giãn 7.2 → 1 hair giữa các ký tự, không sau ký tự cuối.
        assert_eq!(style_text("AB CD", false, 7.2, 72.0, Some(&dejavu)), "A\u{200A}B\u{200A} C\u{200A}D");
        // Giãn 30 → 4 hair; nhiều dòng: không chèn quanh xuống dòng.
        assert_eq!(style_text("AB\nCD", false, 30.0, 72.0, Some(&dejavu)), "A\u{200A}\u{200A}\u{200A}\u{200A}B\nC\u{200A}\u{200A}\u{200A}\u{200A}D");
        // Dấu kết hợp dính vào chữ trước, không bị chen khoảng cách.
        assert_eq!(style_text("a\u{301}b", false, 7.2, 72.0, Some(&dejavu)), "a\u{301}\u{200A}b");
        // Không biết metric / font không có hair space → KHÔNG giãn (không vẽ ô vuông).
        assert_eq!(style_text("AB CD", false, 7.2, 72.0, None), "AB CD");
        let no_hair = FontMetrics { units_per_em: 1000, hair_space: None, thin_space: None, space: Some(0.256) };
        assert_eq!(style_text("AB CD", false, 7.2, 72.0, Some(&no_hair)), "AB CD");
        // Font chỉ có thin space: 30px @72 → 0.2em*72 = 14.4 → 2 thin space.
        let thin = FontMetrics { units_per_em: 1000, hair_space: None, thin_space: Some(0.2), space: Some(0.25) };
        assert_eq!(style_text("AB", false, 30.0, 72.0, Some(&thin)), "A\u{2009}\u{2009}B");
    }

    #[test]
    fn wrap_text_xuong_dong_tai_khoang_trang_giu_xuong_dong_cu() {
        let f = build(&text_opts("Dòng một\nDòng hai"), &frame_720p(), &TextLayout::default());
        assert!(f.contains("textfile="), "{f}");
        // text_items trả nội dung đã xuống dòng: đoạn ngắn giữ nguyên.
        let font = test_font();
        let items = text_items(&text_opts("Dòng một\nDòng hai"), &font, &frame_720p());
        assert_eq!(items[0].content, "Dòng một\nDòng hai");

        // Đoạn dài: mỗi dòng sau khi ngắt phải vừa bề rộng khả dụng.
        let long = "và đây là một câu rất dài để buộc phải tự xuống dòng khi vượt quá bề rộng của khung video ";
        let items = text_items(&text_opts(format!("{long}{long}{long}").trim()), &font, &frame_720p());
        let lines: Vec<&str> = items[0].content.lines().collect();
        assert!(lines.len() >= 2, "{lines:?}");
        let usable = 1280.0 - 1280.0 * 0.08;
        for l in &lines {
            assert!(width_em(l) * 28.0 <= usable + 0.5, "dòng quá rộng: {l:?}");
        }
        // Không mất chữ.
        assert_eq!(items[0].content.replace('\n', " "), format!("{long}{long}{long}").trim());

        // Từ đơn lẻ dài hơn cả dòng vẫn giữ nguyên (không cắt giữa từ).
        let word = "a".repeat(120);
        assert_eq!(wrap_text(&word, 100.0, 28.0, false), word);
    }

    #[test]
    fn text_filter_inactive_cases() {
        let mut t = text_opts("X");
        t.enabled = false;
        assert!(text_items(&t, &test_font(), &frame_720p()).is_empty());
        assert!(text_items(&text_opts("   "), &test_font(), &frame_720p()).is_empty());
        assert!(!t.active());
        assert!(!text_opts("   ").active());
    }

    // ==== hiệu ứng vào/ra ==================================================

    fn alpha_of(f: &str) -> &str {
        let at = f.find(":alpha='").expect("phải có alpha");
        let rest = &f[at + 8..];
        &rest[..rest.find('\'').unwrap()]
    }

    #[test]
    fn animation_mac_dinh_la_fade() {
        let f = build(&text_opts("X"), &frame_720p(), &TextLayout::default());
        assert_eq!(alpha_of(&f), "clip((t-0.000)/0.600,0,1)");
        // Biết thời lượng phần → mờ dần ra ở cuối.
        let layout = TextLayout { duration: Some(120.0), ..Default::default() };
        let f = build(&text_opts("X"), &frame_720p(), &layout);
        assert_eq!(alpha_of(&f), "min(clip((t-0.000)/0.600,0,1),clip((120.000-t)/0.600,0,1))");
    }

    #[test]
    fn animation_off_khong_co_alpha() {
        let mut t = text_opts("X");
        t.animation = TextAnimation::Off;
        let layout = TextLayout { duration: Some(120.0), ..Default::default() };
        let f = build(&t, &frame_720p(), &layout);
        assert!(!f.contains("alpha="), "{f}");
    }

    #[test]
    fn animation_slide_va_pop_dich_y() {
        let mut t = text_opts("X");
        t.animation = TextAnimation::Slide;
        let f = build(&t, &frame_tiktok(), &TextLayout::default());
        // Trượt từ DƯỚI lên: 1.5 × cỡ chữ 42 = 63px.
        assert!(f.contains("y='h-text_h-440+63*(1-clip((t-0.000)/0.600,0,1))'"), "{f}");

        t.position = TextPosition::Top;
        let f = build(&t, &frame_tiktok(), &TextLayout::default());
        // Trên khung: trượt từ TRÊN xuống.
        assert!(f.contains("y='140-63*(1-clip((t-0.000)/0.600,0,1))'"), "{f}");

        t.position = TextPosition::Bottom;
        t.animation = TextAnimation::Pop;
        let f = build(&t, &frame_tiktok(), &TextLayout::default());
        assert!(
            f.contains(
                "y='h-text_h-440+42*(1-(1+2.70158*pow(clip((t-0.000)/0.600,0,1)-1,3)+1.70158*pow(clip((t-0.000)/0.600,0,1)-1,2)))'"
            ),
            "{f}"
        );
    }

    #[test]
    fn animation_duration_kemp_va_canh_ngan() {
        let mut t = text_opts("X");
        t.anim_duration = 5.0;
        let f = build(&t, &frame_720p(), &TextLayout::default());
        assert_eq!(alpha_of(&f), "clip((t-0.000)/3.000,0,1)");

        // Cảnh 0.2s: hiệu ứng ≤ nửa cảnh → 0.1s vào, 0.1s ra.
        let mut t = text_opts("X");
        t.start_sec = Some(2.0);
        t.end_sec = Some(2.2);
        t.anim_duration = 5.0;
        let f = build(&t, &frame_720p(), &TextLayout::default());
        assert_eq!(alpha_of(&f), "min(clip((t-2.000)/0.100,0,1),clip((2.200-t)/0.100,0,1))");
    }

    // ==== kịch bản nhiều cảnh ==============================================

    #[test]
    fn scenes_thay_chu_chinh_va_mac_dinh_ket_thuc_khi_cang_ke_bat_dau() {
        let mut t = text_opts("Chữ chính bị bỏ qua");
        t.scenes = vec![
            TextScene { content: "Cảnh B".into(), start_sec: Some(10.0), end_sec: Some(15.0), position: None },
            TextScene { content: "Cảnh A".into(), start_sec: Some(2.0), end_sec: None, position: None },
            TextScene { content: "Cảnh C".into(), start_sec: Some(20.0), end_sec: None, position: None },
        ];
        let font = test_font();
        let items = text_items(&t, &font, &frame_720p());
        assert_eq!(items.len(), 3);
        // Sắp theo thời điểm bắt đầu.
        assert_eq!(items[0].content, "Cảnh A");
        assert_eq!((items[0].start_sec, items[0].end_sec), (Some(2.0), Some(10.0)));
        assert_eq!((items[1].start_sec, items[1].end_sec), (Some(10.0), Some(15.0)));
        // Cảnh cuối: hiện đến hết.
        assert_eq!((items[2].start_sec, items[2].end_sec), (Some(20.0), None));

        let f = build_text_filter(&items[0], &t, &clean_path(), &font, &frame_720p(), &TextLayout::default()).unwrap();
        assert!(f.contains("enable='between(t,2.000,10.000)'"), "{f}");
        // Cảnh cuối không có end_sec nhưng biết thời lượng phần → vẫn mờ dần ra.
        let layout = TextLayout { duration: Some(60.0), ..Default::default() };
        let f = build_text_filter(&items[2], &t, &clean_path(), &font, &frame_720p(), &layout).unwrap();
        assert!(f.contains("enable='gte(t,20.000)'"), "{f}");
        assert_eq!(alpha_of(&f), "min(clip((t-20.000)/0.600,0,1),clip((60.000-t)/0.600,0,1))");
    }

    #[test]
    fn scene_vi_tri_rieng() {
        let mut t = text_opts("");
        t.scenes = vec![TextScene { content: "Trên đầu".into(), start_sec: Some(1.0), end_sec: None, position: Some(TextPosition::Top) }];
        let font = test_font();
        let items = text_items(&t, &font, &frame_tiktok());
        assert_eq!(items[0].position, TextPosition::Top);
        let f = build_text_filter(&items[0], &t, &clean_path(), &font, &frame_tiktok(), &TextLayout::default()).unwrap();
        assert!(f.contains("y='140'"), "{f}");
    }

    #[test]
    fn parse_scene_script_cac_dang_thoi_gian() {
        let script = "Mở đầu không có mốc\n[00:12-00:20] Chào các bạn\n00:25 | Tiết mục thứ nhất\n* 90 | Tiết mục hai\nDòng nối tiếp\n[01:05-01:30] Tiết mục ba";
        let scenes = parse_scene_script(script);
        let brief: Vec<(&str, Option<f64>, Option<f64>)> =
            scenes.iter().map(|s| (s.content.as_str(), s.start_sec, s.end_sec)).collect();
        assert_eq!(
            brief,
            vec![
                ("Mở đầu không có mốc", None, None),
                ("Chào các bạn", Some(12.0), Some(20.0)),
                ("Tiết mục thứ nhất", Some(25.0), None),
                ("Tiết mục hai\nDòng nối tiếp", Some(90.0), None),
                ("Tiết mục ba", Some(65.0), Some(90.0)),
            ]
        );
        // "[abc] …" không parse được mốc → là dòng nối tiếp của cảnh trước… nhưng
        // ở đây nó ĐỨNG CUỐI nên phải nối vào cảnh "Tiết mục ba".
        let _ = brief;
    }

    #[test]
    fn parse_scene_script_dong_la_noi_vao_cang_truoc() {
        let scenes = parse_scene_script("[00:05] Một\n[abc] hai");
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].content, "Một\n[abc] hai");
    }

    // ==== font =============================================================

    #[test]
    fn font_upload_dung_dung_duong_dan_font_khong_phai_file_chu() {
        // HỒI QUY bản 0.3: fontfile trỏ vào file CHỮ thay vì file font.
        let Some(src) = find_font("DejaVuSansMono.ttf") else { return };
        let dir = std::env::temp_dir().join(format!("rvm_font_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let font_path = dir.join("font_xyz.ttf");
        std::fs::copy(&src, &font_path).unwrap();

        let mut uploads = HashMap::new();
        uploads.insert("font_xyz".to_string(), font_path.clone());
        let rf = FontChoice::Upload { id: "font_xyz".into() }.resolve(&uploads);
        assert_eq!(rf.file.as_deref(), Some(font_path.as_path()));
        assert_eq!(rf.family.as_deref(), Some("DejaVu Sans Mono"));
        assert_eq!(rf.fonts_dir.as_deref(), Some(dir.as_path()));
        // DejaVu Sans Mono là font MONO: mọi glyph (kể cả hair space) cùng bề
        // rộng 1233/2048 ≈ 0.602em — đọc được metric thật từ file là điều cần kiểm.
        assert!(
            rf.metrics.and_then(|m| m.hair_space).is_some_and(|w| (0.60..0.61).contains(&w)),
            "{:?}",
            rf.metrics
        );
        let opts = rf.drawtext_options();
        assert!(opts.starts_with(":fontfile='"), "{opts}");
        assert!(opts.contains("font_xyz.ttf"), "{opts}");
        assert!(!opts.contains("overlay_text.txt"), "{opts}");
        assert_eq!(rf.ass_fontname(), "DejaVu Sans Mono");

        // Filter thật với font upload phải chứa đường dẫn FONT, không phải file chữ.
        let t = TextOverlayOptions { enabled: true, content: "X".into(), font: FontChoice::Upload { id: "font_xyz".into() }, ..Default::default() };
        let frame = FrameCtx::new(1080, 1920, None);
        let items = text_items(&t, &rf, &frame);
        let f = build_text_filter(&items[0], &t, &clean_path(), &rf, &frame, &TextLayout::default()).unwrap();
        assert!(f.contains(&format!("fontfile='{}'", escape_filter_path(&font_path))), "{f}");
        assert!(f.contains("textfile='/job/text_a1b2.txt'"), "{f}");

        // Id không có trong danh sách upload → rơi về font mặc định (không panic).
        assert_eq!(
            FontChoice::Upload { id: "nope".into() }.resolve(&uploads).drawtext_options(),
            ResolvedFont::default_font().drawtext_options()
        );

        // Family hệ thống → tìm file thật để đọc metric, tên family cho ASS.
        let sys = FontChoice::System { family: "DejaVu Sans".into() }.resolve(&HashMap::new());
        assert_eq!(sys.family.as_deref(), Some("DejaVu Sans"));
        assert!(sys.file.as_deref().is_some_and(|p| p.is_file()), "{sys:?}");
        // DejaVu Sans: hair space = 204/2048 = 0.0996em (so trong dung sai).
        assert!(sys.metrics.and_then(|m| m.hair_space).is_some_and(|w| (w - 0.1).abs() < 0.002), "{:?}", sys.metrics);
        assert!(sys.drawtext_options().starts_with(":fontfile='"), "{}", sys.drawtext_options());
        // Tên family chứa nháy đơn bị lọc.
        let dirty = FontChoice::System { family: "Fo'nt X".into() }.resolve(&HashMap::new());
        assert_eq!(dirty.family.as_deref(), Some("Font X"));
        assert_eq!(dirty.drawtext_options(), ":font='Font X'");
        // Family rỗng → font mặc định.
        assert_eq!(
            FontChoice::System { family: "   ".into() }.resolve(&HashMap::new()).drawtext_options(),
            ResolvedFont::default_font().drawtext_options()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolved_font_mac_dinh_khong_co_file_thi_dung_family() {
        let f = ResolvedFont { family: Some("Arial".into()), ..Default::default() };
        assert_eq!(f.drawtext_options(), ":font='Arial'");
        assert_eq!(f.ass_fontname(), "Arial");
        assert_eq!(ResolvedFont::default().drawtext_options(), "");
        assert_eq!(ResolvedFont::default().ass_fontname(), "Arial");
    }

    // ==== subtitles ========================================================

    #[test]
    fn subtitle_filter_ass_va_fontsdir() {
        assert_eq!(build_subtitle_filter(Path::new("/job/sub.ass"), None), "subtitles=filename='/job/sub.ass'");
        assert_eq!(
            build_subtitle_filter(Path::new("/job/sub.ass"), Some(Path::new("/job/fonts"))),
            "subtitles=filename='/job/sub.ass':fontsdir='/job/fonts'"
        );
        assert_eq!(
            build_subtitle_filter(Path::new(r"C:\job\sub.ass"), Some(Path::new(r"C:\job\fonts"))),
            "subtitles=filename='C\\:/job/sub.ass':fontsdir='C\\:/job/fonts'"
        );
    }

    #[test]
    fn subtitle_style_px_that_cua_khung() {
        let s = SubtitleOptions { enabled: true, ..Default::default() };
        let st = subtitle_style(&s, &test_font(), &frame_tiktok(), Reserves::default());
        assert_eq!((st.fontsize, st.alignment, st.margin_v), (52, 2, 440)); // né safe-zone 440
        assert_eq!((st.margin_l, st.margin_r), (54, 180));
        assert_eq!((st.play_res_x, st.play_res_y), (1080, 1920));
        assert_eq!(st.fontname, "DejaVu Sans");
        assert_eq!(st.outline_color, "&H00000000");
        assert_eq!(st.kind, SubtitleStyleKind::Classic);

        // 16:9 không có safe-zone: lề 6% chiều cao; cỡ chữ co theo hệ số khung.
        let st = subtitle_style(&s, &test_font(), &frame_720p(), Reserves::default());
        assert_eq!((st.fontsize, st.margin_v, st.margin_l), (35, 43, 64));

        // Né visualizer ở đáy (386px) khi preset không có safe-zone.
        let st = subtitle_style(&s, &test_font(), &frame_720p(), Reserves { top: 0.0, bottom: 300.0 });
        assert_eq!(st.margin_v, 300);

        let top = SubtitleOptions { enabled: true, position: TextPosition::Top, ..Default::default() };
        assert_eq!(subtitle_style(&top, &test_font(), &frame_tiktok(), Reserves::default()).alignment, 8);
        assert_eq!(subtitle_style(&top, &test_font(), &frame_tiktok(), Reserves { top: 900.0, bottom: 0.0 }).margin_v, 900);
        let mid = SubtitleOptions { enabled: true, position: TextPosition::Middle, ..Default::default() };
        let st = subtitle_style(&mid, &test_font(), &frame_tiktok(), Reserves::default());
        assert_eq!((st.alignment, st.margin_v), (5, 0));
    }

    #[test]
    fn subtitle_style_cac_kieu() {
        let neon = SubtitleOptions { enabled: true, style: "neon".into(), color: "#FFFF00".into(), ..Default::default() };
        let st = subtitle_style(&neon, &test_font(), &frame_tiktok(), Reserves::default());
        assert_eq!(st.kind, SubtitleStyleKind::Neon);
        assert_eq!(st.outline_color, "&H0014FF39"); // #39FF14 theo BGR
        assert_eq!(st.primary, "&H0000FFFF");

        let boxed = SubtitleOptions { enabled: true, style: "box".into(), ..Default::default() };
        let st = subtitle_style(&boxed, &test_font(), &frame_tiktok(), Reserves::default());
        assert_eq!(st.kind, SubtitleStyleKind::Box);
        assert_eq!(st.outline(), 7); // 52 * 0.14 — viền dày làm nền cho chữ
        assert_eq!(st.outline_color, "&H40000000"); // nền đen mờ
        assert_eq!(boxed.kind(), SubtitleStyleKind::Box);
        assert_eq!(SubtitleOptions::default().kind(), SubtitleStyleKind::Classic);
    }

    // ==== watermark text ===================================================

    fn wm_text(name: &str) -> WatermarkOptions {
        WatermarkOptions { enabled: true, kind: WmKind::Text, channel_name: name.into(), ..Default::default() }
    }

    #[test]
    fn wm_text_bottom_right_avoids_safe_zone() {
        let f = build_watermark_text_filter(&wm_text("@kenh.radio"), &clean_path(), &test_font(), &frame_tiktok(), Reserves::default())
            .unwrap();
        // Lề = 24 (margin) + 180 (safe phải) = 204; dưới = 24 + 440 = 464.
        assert!(f.contains("x='w-text_w-204'"), "{f}");
        assert!(f.contains("y='h-text_h-464'"), "{f}");
        assert!(f.contains("fontcolor=0xFFFFFF@0.700"), "{f}");
        assert!(f.contains("borderw=2"), "{f}");
        assert!(f.contains(":expansion=none"), "{f}");
        assert!(f.starts_with("drawtext=textfile='/job/text_a1b2.txt'"), "{f}");
    }

    #[test]
    fn wm_text_no_safe_zone_margin_only() {
        let f = build_watermark_text_filter(&wm_text("@kenh"), &clean_path(), &test_font(), &frame_720p(), Reserves::default()).unwrap();
        assert!(f.contains("x='w-text_w-16'"), "{f}");
        assert!(f.contains("y='h-text_h-16'"), "{f}");
    }

    #[test]
    fn wm_text_ne_vung_bi_chiếm() {
        // Visualizer ở đáy (386px) → watermark chữ nâng lên trên visualizer.
        let f = build_watermark_text_filter(
            &wm_text("@kenh"),
            &clean_path(),
            &test_font(),
            &frame_tiktok(),
            Reserves { top: 0.0, bottom: 386.0 },
        )
        .unwrap();
        assert!(f.contains("y='h-text_h-850'"), "{f}");
    }

    #[test]
    fn wm_text_all_positions() {
        let mut w = wm_text("K");
        for (pos, x, y) in [
            (WmPosition::TopLeft, "x='16'", "y='16'"),
            (WmPosition::TopRight, "x='w-text_w-16'", "y='16'"),
            (WmPosition::BottomLeft, "x='16'", "y='h-text_h-16'"),
            (WmPosition::BottomRight, "x='w-text_w-16'", "y='h-text_h-16'"),
            (WmPosition::Center, "x='(w-text_w)/2'", "y='(h-text_h)/2'"),
        ] {
            w.position = pos;
            let f = build_watermark_text_filter(&w, &clean_path(), &test_font(), &frame_720p(), Reserves::default()).unwrap();
            assert!(f.contains(&format!("{x}:")), "{pos:?} thiếu {x}: {f}");
            assert!(f.contains(y), "{pos:?} thiếu {y}: {f}");
        }
    }

    #[test]
    fn wm_text_moving_cycles_4_corners() {
        let mut w = wm_text("Di");
        w.moving = true;
        w.move_every_sec = 3;
        let f = build_watermark_text_filter(&w, &clean_path(), &test_font(), &frame_720p(), Reserves::default()).unwrap();
        assert!(f.contains("mod(t/3,4)"), "{f}");
        // Cả x và y đều có chuỗi if lồng.
        assert_eq!(f.matches("mod(t/3,4)").count(), 6, "x+y mỗi cái 3 lần: {f}");
        // Không còn toạ độ tĩnh của 1 góc cố định.
        assert!(!f.contains("x='w-text_w-16'"), "{f}");
    }

    #[test]
    fn wm_text_hidden_when_empty_or_logo_kind() {
        let mut w = wm_text("   ");
        assert!(build_watermark_text_filter(&w, &clean_path(), &test_font(), &frame_720p(), Reserves::default()).is_none());
        w.channel_name = "X".into();
        w.kind = WmKind::Logo;
        assert!(build_watermark_text_filter(&w, &clean_path(), &test_font(), &frame_720p(), Reserves::default()).is_none());
        assert!(!w.active_text());
        assert!(w.active_logo());
    }

    #[test]
    fn wm_text_shadow_and_full_opacity() {
        let mut w = wm_text("S");
        w.shadow = true;
        w.opacity_pct = 100;
        let f = build_watermark_text_filter(&w, &clean_path(), &test_font(), &frame_720p(), Reserves::default()).unwrap();
        assert!(f.contains("shadowcolor=black@0.6"), "{f}");
        assert!(f.contains("shadowx=1"), "{f}");
        // 100% → không có @ sau màu chính.
        assert!(f.contains("fontcolor=0xFFFFFF:"), "{f}");
    }

    // ==== watermark logo ===================================================

    #[test]
    fn wm_logo_static() {
        let w = WatermarkOptions {
            enabled: true,
            kind: WmKind::Logo,
            logo_scale_pct: 20,
            opacity_pct: 50,
            margin_px: 24,
            position: WmPosition::BottomRight,
            ..Default::default()
        };
        let l = build_watermark_logo(&w, &frame_tiktok(), 2, Reserves::default());
        assert_eq!(l.prep, "[2:v]scale=216:-2,format=rgba,colorchannelmixer=aa=0.500[wm]"); // 20% * 1080
        assert_eq!(l.x, "main_w-overlay_w-204"); // 24+180
        assert_eq!(l.y, "main_h-overlay_h-464"); // 24+440
    }

    #[test]
    fn wm_logo_moving_min_scale_va_ne_vung_chiếm() {
        let w = WatermarkOptions {
            enabled: true,
            kind: WmKind::Logo,
            moving: true,
            move_every_sec: 7,
            logo_scale_pct: 1, // quá nhỏ → kẹp 16px
            ..Default::default()
        };
        let l = build_watermark_logo(&w, &frame_720p(), 2, Reserves::default());
        assert!(l.prep.contains("scale=16:-2"), "{}", l.prep);
        assert!(l.x.contains("mod(t/7,4)") && l.y.contains("mod(t/7,4)"), "{} {}", l.x, l.y);
        assert!(!l.x.contains("text_w"), "{}", l.x);

        let still = WatermarkOptions { enabled: true, kind: WmKind::Logo, ..Default::default() };
        let l = build_watermark_logo(&still, &frame_tiktok(), 2, Reserves { top: 0.0, bottom: 386.0 });
        assert_eq!(l.y, "main_h-overlay_h-850");
    }

    // ==== thanh tiến độ ====================================================

    #[test]
    fn progress_spec_hinh_hoc() {
        let p = ProgressSpec::new(&frame_tiktok(), 120.0, "#39FF14");
        assert_eq!(
            p,
            ProgressSpec {
                width: 1080,
                fps: 30,
                duration: 120.0,
                color: "0x39FF14".into(),
                track_y: 1468, // 1920 - 440 (safe) - 12
                track_h: 12,
                bar_y: 1470,
                bar_h: 8,
            }
        );
        // 720p: mọi thứ co theo hệ số khung, dính sát đáy (không có safe-zone).
        let p = ProgressSpec::new(&frame_720p(), 0.0, "red");
        assert_eq!((p.track_y, p.track_h, p.bar_y, p.bar_h), (712, 8, 713, 5));
        assert_eq!((p.duration, p.color), (0.1, "red".into()));
    }

    // ==== compose ==========================================================

    #[test]
    fn compose_chain_only() {
        let empty = OverlaySpec::default();
        assert_eq!(compose_filter_complex(&empty), "[0:v][vout]");
        assert!(!empty.has_video_filters());

        let chain_only = OverlaySpec { chain: vec!["f1".into(), "f2".into()], ..Default::default() };
        assert_eq!(compose_filter_complex(&chain_only), "[0:v]f1,f2[vout]");

        let pre = OverlaySpec { chain: vec!["f1".into()], pre_scale: Some("pre".into()), ..Default::default() };
        assert_eq!(compose_filter_complex(&pre), "[0:v]pre,f1[vout]");
    }

    #[test]
    fn compose_viz_only_va_viz_cuoi_khong_co_dau_cham_phay() {
        let viz = OverlaySpec {
            viz_graph: Some("[1:a]w[viz]".into()),
            viz_overlay: Some("overlay=x=0:y='9':eof_action=repeat".into()),
            ..Default::default()
        };
        assert!(viz.has_video_filters());
        assert_eq!(
            compose_filter_complex(&viz),
            "[0:v]null[v0];[1:a]w[viz];[v0][viz]overlay=x=0:y='9':eof_action=repeat[vout]"
        );
        let both = OverlaySpec { chain: vec!["f1".into()], ..viz };
        assert_eq!(compose_filter_complex(&both), "[0:v]f1[v0];[1:a]w[viz];[v0][viz]overlay=x=0:y='9':eof_action=repeat[vout]");
    }

    #[test]
    fn compose_logo_khong_con_dung_nhan_base_tu_dong() {
        let with_logo = OverlaySpec {
            chain: vec!["f1".into()],
            logo_prep: Some("[2:v]n[wm]".into()),
            logo_xy: Some(("a".into(), "b".into())),
            ..Default::default()
        };
        assert!(with_logo.needs_logo_input());
        assert_eq!(compose_filter_complex(&with_logo), "[0:v]f1[v0];[2:v]n[wm];[v0][wm]overlay=x='a':y='b'[vout]");

        let logo_only = OverlaySpec { chain: vec![], ..with_logo.clone() };
        assert_eq!(compose_filter_complex(&logo_only), "[0:v]null[v0];[2:v]n[wm];[v0][wm]overlay=x='a':y='b'[vout]");

        let pre_logo = OverlaySpec { pre_scale: Some("pre".into()), ..with_logo };
        // `with_logo` còn mang chain "f1" → pre và chain nối bằng dấu phẩy.
        assert_eq!(compose_filter_complex(&pre_logo), "[0:v]pre,f1[v0];[2:v]n[wm];[v0][wm]overlay=x='a':y='b'[vout]");
        assert!(!OverlaySpec::default().needs_logo_input());
    }

    #[test]
    fn compose_day_du_viz_tien_do_logo() {
        let spec = OverlaySpec {
            chain: vec!["f1".into()],
            viz_graph: Some("[1:a]w[viz]".into()),
            viz_overlay: Some("overlay=x=0:y='9':eof_action=repeat".into()),
            progress: Some(ProgressSpec::new(&frame_tiktok(), 120.0, "#39FF14")),
            logo_prep: Some("[2:v]n[wm]".into()),
            logo_xy: Some(("a".into(), "b".into())),
            pre_scale: None,
        };
        assert_eq!(
            compose_filter_complex(&spec),
            "[0:v]f1[v0];[1:a]w[viz];[v0][viz]overlay=x=0:y='9':eof_action=repeat[v1];\
             [v1]drawbox=x=0:y=1468:w=iw:h=12:color=black@0.35:t=fill[v1d];\
             color=c=0x39FF14:s=1080x8:r=30[pbar];\
             [v1d][pbar]overlay=x='-1080+1080*t/120':y=1470:shortest=0[v2];\
             [2:v]n[wm];[v2][wm]overlay=x='a':y='b'[vout]"
        );
    }

    #[test]
    fn fmt_num_gon() {
        assert_eq!(fmt_num(120.0), "120");
        assert_eq!(fmt_num(7.5), "7.500");
        assert_eq!(fmt_num(61.25), "61.250");
    }
}
