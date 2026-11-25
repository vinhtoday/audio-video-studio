//! Hiệu ứng hình ảnh không phụ thuộc nội dung: visualizer theo audio, hiệu
//! ứng Ken Burns cho ảnh bìa, nền gradient — mọi hàm thuần; chuỗi filter đã
//! đo thật trên ffmpeg 7.1 (scripts/probe_features4.py, P-H…P-J).
//!
//! - Visualizer dùng chính audio của output (`[1:a]` trong filter_complex,
//!   `-map 1:a:0` vẫn hoạt động song song — đã đo rc 0 + audio đủ dài).
//! - Thanh tiến độ: `drawbox` không có biến thời gian nên vẽ rãnh tĩnh + thanh
//!   `color` source overlay trượt theo `t` (đo bề rộng 16/80/320/632 px tại
//!   0.2/1/4/7.9 s của 8 s).
//! - Ken Burns: supersample 2× rồi `zoompan` cho chuyển động mượt hơn.

use serde::Deserialize;

use crate::textover::{drawtext_color, FrameCtx, Reserves};

/// Kiểu visualizer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VisualizerKind {
    #[default]
    Off,
    Wave,
    Bars,
    Spectrum,
}

impl VisualizerKind {
    /// Tên gửi từ UI/API: "none" | "wave" | "bars" | "spectrum".
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "wave" | "waveform" | "sóng" => Self::Wave,
            "bars" | "freq" | "frequency" => Self::Bars,
            "spectrum" => Self::Spectrum,
            _ => Self::Off,
        }
    }
}

/// Vị trí dọc của visualizer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VerticalPlace {
    #[default]
    Bottom,
    Top,
    Middle,
}

impl VerticalPlace {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "top" => Self::Top,
            "middle" | "center" => Self::Middle,
            _ => Self::Bottom,
        }
    }
}

/// Chuyển động Ken Burns cho ảnh bìa.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// Dành cho bản ảnh bìa (Ken Burns + nền gradient) — chưa nối vào pipeline.
#[allow(dead_code)]
pub enum CoverMotion {
    #[default]
    Off,
    ZoomIn,
    ZoomOut,
    PanLeft,
    PanRight,
}

impl CoverMotion {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "zoom_in" | "zoomin" => Self::ZoomIn,
            "zoom_out" | "zoomout" => Self::ZoomOut,
            "pan_left" | "panleft" => Self::PanLeft,
            "pan_right" | "panright" => Self::PanRight,
            _ => Self::Off,
        }
    }
}

/// Tuỳ chọn hiệu ứng hình ảnh (JSON `visuals` trong options của job).
#[derive(Debug, Clone, Deserialize)]
pub struct VisualOptions {
    /// "none" | "wave" | "bars" | "spectrum".
    #[serde(default)]
    pub visualizer: String,
    /// "bottom" | "top" | "middle".
    #[serde(default)]
    pub visualizer_position: String,
    /// Chiều cao visualizer theo % chiều cao khung (6–45).
    #[serde(default = "default_viz_height")]
    pub visualizer_height_pct: u32,
    #[serde(default = "default_accent")]
    pub visualizer_color: String,
    /// Độ mờ 0–100.
    #[serde(default = "default_viz_opacity")]
    pub visualizer_opacity_pct: u32,
    /// Thanh tiến độ dưới đáy khung.
    #[serde(default)]
    pub progress_bar: bool,
    #[serde(default = "default_accent")]
    pub progress_color: String,
    /// "none" | "zoom_in" | "zoom_out" | "pan_left" | "pan_right" (ảnh bìa).
    /// Dành cho bản ảnh bìa — UI chưa gửi, nhưng giữ field để JSON options
    /// tương thích khi nối vào.
    #[serde(default)]
    #[allow(dead_code)]
    pub cover_motion: String,
    /// "gradient" | "black" — nền sau video chưa đầy khung / khi không có
    /// video lẫn ảnh bìa. Dành cho bản ảnh bìa.
    #[serde(default = "default_background")]
    #[allow(dead_code)]
    pub background: String,
    #[serde(default = "default_grad_from")]
    #[allow(dead_code)]
    pub gradient_from: String,
    #[serde(default = "default_grad_to")]
    #[allow(dead_code)]
    pub gradient_to: String,
}

impl Default for VisualOptions {
    fn default() -> Self {
        Self {
            visualizer: String::new(),
            visualizer_position: String::new(),
            visualizer_height_pct: default_viz_height(),
            visualizer_color: default_accent(),
            visualizer_opacity_pct: default_viz_opacity(),
            progress_bar: false,
            progress_color: default_accent(),
            cover_motion: String::new(),
            background: default_background(),
            gradient_from: default_grad_from(),
            gradient_to: default_grad_to(),
        }
    }
}

fn default_viz_height() -> u32 {
    18
}
fn default_viz_opacity() -> u32 {
    90
}
fn default_accent() -> String {
    "#39FF14".into()
}
fn default_background() -> String {
    "gradient".into()
}
fn default_grad_from() -> String {
    "#1E1B4B".into()
}
fn default_grad_to() -> String {
    "#EC4899".into()
}

/// Khoảng hở giữa visualizer và vùng an toàn, px @1080.
const VIZ_GAP_PX: f64 = 40.0;
/// Độ zoom tối đa của Ken Burns (15%).
const KENBURNS_ZOOM: f64 = 0.15;

impl VisualOptions {
    pub fn visualizer_kind(&self) -> VisualizerKind {
        VisualizerKind::parse(&self.visualizer)
    }

    pub fn visualizer_place(&self) -> VerticalPlace {
        VerticalPlace::parse(&self.visualizer_position)
    }

    /// Dành cho bản ảnh bìa (Ken Burns + nền gradient).
    #[allow(dead_code)]
    pub fn motion(&self) -> CoverMotion {
        CoverMotion::parse(&self.cover_motion)
    }

    /// Nền sau video là gradient chuyển động (chế độ fit "pad").
    /// Dành cho bản ảnh bìa.
    #[allow(dead_code)]
    pub fn gradient_background(&self) -> bool {
        self.background.trim().eq_ignore_ascii_case("gradient")
    }

    /// Màu nền `pad` khi không dùng gradient. Dành cho bản ảnh bìa.
    #[allow(dead_code)]
    pub fn pad_color(&self) -> String {
        if self.gradient_background() {
            // Không có lavfi (ví dụ lệnh cắt/normalize đơn giản): dùng màu đầu
            // của gradient cho đồng bộ.
            match crate::textover::parse_hex_color(&self.gradient_from) {
                Some((r, g, b)) => format!("0x{r:02X}{g:02X}{b:02X}"),
                None => "black".into(),
            }
        } else {
            "black".into()
        }
    }
}

/// Kích thước (rộng × cao) của visualizer trên khung.
pub fn viz_size(v: &VisualOptions, frame: &FrameCtx) -> (u32, u32) {
    let h = (frame.height as f64 * v.visualizer_height_pct.clamp(6, 45) as f64 / 100.0).round() as u32;
    (frame.width, h.max(16))
}

/// Vùng khung bị visualizer chiếm (tính từ mép trên/dưới, KHÔNG gồm vùng an
/// toàn) để phụ đề/chữ/watermark né. Middle không chiếm mép.
pub fn viz_reserves(v: &VisualOptions, frame: &FrameCtx) -> Reserves {
    if v.visualizer_kind() == VisualizerKind::Off {
        return Reserves::default();
    }
    let (_, h) = viz_size(v, frame);
    let used = h as f64 + (VIZ_GAP_PX * frame.scale_factor()).round();
    match v.visualizer_place() {
        VerticalPlace::Bottom => Reserves { top: 0.0, bottom: used },
        VerticalPlace::Top => Reserves { top: used, bottom: 0.0 },
        VerticalPlace::Middle => Reserves::default(),
    }
}

/// (graph, tham số overlay) của visualizer — `(None, None)` khi tắt.
///
/// `graph` đọc audio input 1 (`[1:a]`) và ghi nhãn `[viz]`; `overlay` là filter
/// `overlay=…` lắp sau `[vN][viz]`. `eof_action=repeat` giữ nguyên khung
/// visualizer cuối khi audio hết trước video.
pub fn build_visualizer(v: &VisualOptions, frame: &FrameCtx) -> (Option<String>, Option<String>) {
    let kind = v.visualizer_kind();
    if kind == VisualizerKind::Off {
        return (None, None);
    }
    let (w, h) = viz_size(v, frame);
    let fps = frame.fps.max(1);
    let alpha = (v.visualizer_opacity_pct.min(100) as f64 / 100.0).clamp(0.0, 1.0);
    let color = drawtext_color(&v.visualizer_color, 1.0);
    let mono = "[1:a]aformat=channel_layouts=mono,";
    let graph = match kind {
        VisualizerKind::Wave => {
            // showwaves vẽ nét dày đúng 1 px — trên khung 1080 gần như vô
            // hình và mất sắc độ khi nén yuv420p. Vẽ ở 1/5 độ phân giải rồi
            // phóng bicubic để có dải sáng dày, mềm (kiểu neon).
            let (sw, sh) = ((w / 5).max(2), (h / 5).max(2));
            format!("{mono}showwaves=s={sw}x{sh}:mode=cline:rate={fps}:colors={color},scale={w}x{h}:flags=bicubic,format=rgba,colorchannelmixer=aa={alpha:.3}[viz]")
        }
        VisualizerKind::Bars => format!(
            "{mono}showfreqs=s={w}x{h}:mode=bar:win_size=2048:ascale=log:fscale=log:rate={fps}:cmode=combined:colors={color},format=rgba,colorchannelmixer=aa={alpha:.3}[viz]"
        ),
        VisualizerKind::Spectrum => format!(
            "{mono}showspectrum=s={w}x{h}:slide=scroll:mode=combined:color=rainbow:scale=log:fps={fps},format=rgba,colorchannelmixer=aa={alpha:.3}[viz]"
        ),
        VisualizerKind::Off => return (None, None),
    };
    let (top, bottom, _, _) = frame.insets();
    let gap = (VIZ_GAP_PX * frame.scale_factor()).round() as u32;
    let y = match v.visualizer_place() {
        VerticalPlace::Bottom => format!("H-overlay_h-{}", bottom.round() as u32 + gap),
        VerticalPlace::Top => (top.round() as u32 + gap).to_string(),
        VerticalPlace::Middle => "(H-overlay_h)/2".to_string(),
    };
    (Some(graph), Some(format!("overlay=x=0:y='{y}':eof_action=repeat")))
}

/// Nguồn nền gradient chuyển động (lavfi) cho lệnh ffmpeg:
/// `-f lavfi -i "<graph>"`.
/// Dành cho bản ảnh bìa (Ken Burns + nền gradient) — chưa nối vào pipeline.
#[allow(dead_code)]
pub fn gradient_source(v: &VisualOptions, width: u32, height: u32, fps: u32) -> String {
    let c0 = drawtext_color(&v.gradient_from, 1.0);
    let c1 = drawtext_color(&v.gradient_to, 1.0);
    format!("gradients=s={width}x{height}:c0={c0}:c1={c1}:n=2:r={}", fps.max(1))
}

/// Bộ supersample cho Ken Burns: 2× với khung ≤ 2.5MP, 1.5× tới 5MP, ngoài ra
/// 1× (khung 4K đã quá lớn, zoompan 8K sẽ rất chậm).
/// Dành cho bản ảnh bìa (Ken Burns + nền gradient) — chưa nối vào pipeline.
#[allow(dead_code)]
fn supersample(width: u32, height: u32) -> f64 {
    let mp = width as f64 * height as f64 / 1_000_000.0;
    if mp <= 2.5 {
        2.0
    } else if mp <= 5.0 {
        1.5
    } else {
        1.0
    }
}

/// Filter đưa ẢNH BÌA vào khung đích (đưa vào `-vf`, ảnh là input
/// `-loop 1 -framerate fps`). Có hiệu ứng thì supersample rồi `zoompan` trong
/// `duration` giây; không thì scale/crop thẳng.
/// Dành cho bản ảnh bìa (Ken Burns + nền gradient) — chưa nối vào pipeline.
#[allow(dead_code)]
pub fn cover_fit_filters(v: &VisualOptions, frame: &FrameCtx, duration: f64) -> Vec<String> {
    let motion = v.motion();
    let (w, h) = (frame.width, frame.height);
    let even = |x: f64| ((x.round() as u64) / 2) * 2;
    let f = supersample(w, h);
    if motion == CoverMotion::Off || f <= 1.0 || !(duration > 0.0) {
        return vec![
            format!("scale={w}:{h}:force_original_aspect_ratio=increase"),
            format!("crop={w}:{h}"),
            "format=yuv420p".to_string(),
        ];
    }
    let (sw, sh) = (even(w as f64 * f), even(h as f64 * f));
    let fps = frame.fps.max(1);
    // Tổng số khung xuất: `on` của zoompan chạy 0..total-1.
    let total = (duration * fps as f64).round().max(1.0) as u32;
    let z = format!("{:.2}", 1.0 + KENBURNS_ZOOM);
    let center = "x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)'";
    let zoompan = match motion {
        CoverMotion::ZoomIn => format!("zoompan=z='1+{KENBURNS_ZOOM:.2}*on/{total}':{center}"),
        CoverMotion::ZoomOut => format!("zoompan=z='{z}-{KENBURNS_ZOOM:.2}*on/{total}':{center}"),
        CoverMotion::PanLeft => {
            format!("zoompan=z={z}:x='(iw-iw/zoom)*on/{total}':y='(ih-ih/zoom)/2'")
        }
        CoverMotion::PanRight => {
            format!("zoompan=z={z}:x='(iw-iw/zoom)*(1-on/{total})':y='(ih-ih/zoom)/2'")
        }
        CoverMotion::Off => unreachable!(),
    };
    vec![
        format!("scale={sw}:{sh}:force_original_aspect_ratio=increase"),
        format!("crop={sw}:{sh}"),
        format!("{zoompan}:d=1:s={w}x{h}:fps={fps}"),
        "format=yuv420p".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::SafeZone;

    fn tiktok() -> FrameCtx {
        FrameCtx::new(1080, 1920, Some(SafeZone { top: 140, bottom: 440, left: 40, right: 180 }))
    }

    fn hd() -> FrameCtx {
        FrameCtx::new(1920, 1080, None)
    }

    #[test]
    fn parse_cac_kieu() {
        assert_eq!(VisualizerKind::parse("wave"), VisualizerKind::Wave);
        assert_eq!(VisualizerKind::parse("BARS"), VisualizerKind::Bars);
        assert_eq!(VisualizerKind::parse("spectrum"), VisualizerKind::Spectrum);
        assert_eq!(VisualizerKind::parse(""), VisualizerKind::Off);
        assert_eq!(VerticalPlace::parse("top"), VerticalPlace::Top);
        assert_eq!(VerticalPlace::parse("center"), VerticalPlace::Middle);
        assert_eq!(VerticalPlace::parse(""), VerticalPlace::Bottom);
        assert_eq!(CoverMotion::parse("zoom_in"), CoverMotion::ZoomIn);
        assert_eq!(CoverMotion::parse("pan-right"), CoverMotion::Off);
        assert_eq!(CoverMotion::parse("pan_right"), CoverMotion::PanRight);
    }

    #[test]
    fn viz_size_va_reserves() {
        let v = VisualOptions { visualizer: "wave".into(), ..Default::default() };
        // 18% của 1920 = 345.6 → 346; khoảng hở 40px @1080.
        assert_eq!(viz_size(&v, &tiktok()), (1080, 346));
        assert_eq!(viz_reserves(&v, &tiktok()), Reserves { top: 0.0, bottom: 386.0 });
        let top = VisualOptions { visualizer_position: "top".into(), ..v.clone() };
        assert_eq!(viz_reserves(&top, &tiktok()), Reserves { top: 386.0, bottom: 0.0 });
        let mid = VisualOptions { visualizer_position: "middle".into(), ..v.clone() };
        assert_eq!(viz_reserves(&mid, &tiktok()), Reserves::default());
        // Tắt → không chiếm chỗ.
        assert_eq!(viz_reserves(&VisualOptions::default(), &tiktok()), Reserves::default());
        // Kẹp % về 6–45; hệ số co theo khung (720p: k = 0.667 → gap 27).
        let huge = VisualOptions { visualizer: "bars".into(), visualizer_height_pct: 90, ..Default::default() };
        let f720 = FrameCtx::new(1280, 720, None);
        assert_eq!(viz_size(&huge, &f720), (1280, 324)); // 45% của 720
        assert_eq!(viz_reserves(&huge, &f720).bottom, 324.0 + 27.0);
    }

    #[test]
    fn viz_graph_wave_bars_spectrum_dung_chuoi_da_do() {
        let v = VisualOptions { visualizer: "wave".into(), ..Default::default() };
        let (g, o) = build_visualizer(&v, &tiktok());
        assert_eq!(
            g,
            Some("[1:a]aformat=channel_layouts=mono,showwaves=s=216x69:mode=cline:rate=30:colors=0x39FF14,scale=1080x346:flags=bicubic,format=rgba,colorchannelmixer=aa=0.900[viz]".into())
        );
        // Đáy: né safe-zone 440 + khoảng hở 40.
        assert_eq!(o, Some("overlay=x=0:y='H-overlay_h-480':eof_action=repeat".into()));

        let v = VisualOptions { visualizer: "bars".into(), visualizer_opacity_pct: 70, ..Default::default() };
        let (g, _) = build_visualizer(&v, &hd());
        assert_eq!(
            g,
            Some("[1:a]aformat=channel_layouts=mono,showfreqs=s=1920x194:mode=bar:win_size=2048:ascale=log:fscale=log:rate=30:cmode=combined:colors=0x39FF14,format=rgba,colorchannelmixer=aa=0.700[viz]".into())
        );

        let v = VisualOptions { visualizer: "spectrum".into(), visualizer_position: "top".into(), ..Default::default() };
        let (g, o) = build_visualizer(&v, &tiktok());
        assert_eq!(
            g,
            Some("[1:a]aformat=channel_layouts=mono,showspectrum=s=1080x346:slide=scroll:mode=combined:color=rainbow:scale=log:fps=30,format=rgba,colorchannelmixer=aa=0.900[viz]".into())
        );
        assert_eq!(o, Some("overlay=x=0:y='180':eof_action=repeat".into())); // 140 + 40

        let (g, o) = build_visualizer(&VisualOptions::default(), &tiktok());
        assert_eq!((g, o), (None, None));
    }

    #[test]
    fn gradient_source_format() {
        let v = VisualOptions::default();
        assert_eq!(gradient_source(&v, 1080, 1920, 30), "gradients=s=1080x1920:c0=0x1E1B4B:c1=0xEC4899:n=2:r=30");
        assert!(v.gradient_background());
        assert_eq!(v.pad_color(), "0x1E1B4B");
        let black = VisualOptions { background: "black".into(), ..Default::default() };
        assert!(!black.gradient_background());
        assert_eq!(black.pad_color(), "black");
    }

    #[test]
    fn cover_motion_ken_burns() {
        let v = VisualOptions { cover_motion: "zoom_in".into(), ..Default::default() };
        assert_eq!(
            cover_fit_filters(&v, &tiktok(), 6.0),
            vec![
                "scale=2160:3840:force_original_aspect_ratio=increase",
                "crop=2160:3840",
                "zoompan=z='1+0.15*on/180':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d=1:s=1080x1920:fps=30",
                "format=yuv420p",
            ]
        );
        let v = VisualOptions { cover_motion: "zoom_out".into(), ..Default::default() };
        assert!(cover_fit_filters(&v, &tiktok(), 6.0)[2].starts_with("zoompan=z='1.15-0.15*on/180'"));
        let v = VisualOptions { cover_motion: "pan_left".into(), ..Default::default() };
        assert!(cover_fit_filters(&v, &tiktok(), 6.0)[2].contains("x='(iw-iw/zoom)*on/180'"));
        let v = VisualOptions { cover_motion: "pan_right".into(), ..Default::default() };
        assert!(cover_fit_filters(&v, &tiktok(), 6.0)[2].contains("x='(iw-iw/zoom)*(1-on/180)'"));

        // Tắt hiệu ứng → scale/crop thẳng.
        assert_eq!(
            cover_fit_filters(&VisualOptions::default(), &hd(), 6.0),
            vec!["scale=1920:1080:force_original_aspect_ratio=increase", "crop=1920:1080", "format=yuv420p"]
        );
        // Khung to (1080×1920 = 2.07MP → 2×; 1920×1080 = 2.07MP → 2×).
        assert!(cover_fit_filters(&v, &hd(), 10.0)[0].contains("scale=3840:2160"));
        // Kích thước lẻ của supersample 1.5× phải ra số chẵn (1441×2003 ≈ 2.9MP → 1.5×).
        let odd = FrameCtx::new(1441, 2003, None);
        let f = cover_fit_filters(&v, &odd, 10.0);
        assert!(f[0].contains("scale=2162:3004"), "{f:?}");
    }
}
