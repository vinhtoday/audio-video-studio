//! Preset nền tảng (giai đoạn 2): tỷ lệ / độ phân giải / fps / safe-zone /
//! giới hạn thời lượng khi đăng bài.
//!
//! - Danh sách gốc nằm trong hằng số [`PRESET_DEFS`] để dễ sửa.
//! - Khi khởi động, [`Registry::load`] gộp thêm override từ file
//!   `presets_config.json` (nếu có) trong thư mục chạy — nhờ vậy chỉnh
//!   `max_duration_sec` hay safe-zone mà không cần build lại.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Vùng an toàn (safe-zone): khoảng mép có thể bị UI của app che, tính theo
/// pixel TẠI độ phân giải của preset. Là ước lượng — chỉnh tại [`PRESET_DEFS`]
/// hoặc trong `presets_config.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeZone {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

/// Bản định nghĩa const của 1 preset (dữ liệu tĩnh, dễ sửa một chỗ).
#[derive(Debug, Clone, Copy)]
pub struct PresetDef {
    pub id: &'static str,
    pub name: &'static str,
    /// Tỷ lệ khung hiển thị trên UI, ví dụ "9:16"; "-" nếu không cố định.
    pub ratio: &'static str,
    /// 0 = không cố định (giữ nguyên gốc / tuỳ chỉnh).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub note: &'static str,
    pub safe_zone: Option<SafeZone>,
    /// 0 = không giới hạn.
    pub max_duration_sec: u32,
}

/// Bảng preset gốc. Giữ nguyên thứ tự này trên UI.
pub const PRESET_DEFS: &[PresetDef] = &[
    PresetDef {
        id: "tiktok_9x16",
        name: "TikTok / Reels / Shorts",
        ratio: "9:16",
        width: 1080,
        height: 1920,
        fps: 30,
        note: "Video dọc toàn màn hình, feed TikTok / Instagram Reels / YouTube Shorts.",
        // Ước lượng theo UI TikTok: cột nút bên phải, caption + nhạc dưới đáy,
        // thanh tài khoản trên đỉnh.
        safe_zone: Some(SafeZone { top: 140, bottom: 440, left: 40, right: 180 }),
        max_duration_sec: 600,
    },
    PresetDef {
        id: "fb_feed_4x5",
        name: "Facebook Feed",
        ratio: "4:5",
        width: 1080,
        height: 1350,
        fps: 30,
        note: "Chiếm nhiều diện tích nhất trên newsfeed Facebook.",
        safe_zone: Some(SafeZone { top: 60, bottom: 160, left: 60, right: 60 }),
        max_duration_sec: 0,
    },
    PresetDef {
        id: "fb_yt_16x9",
        name: "Facebook / YouTube",
        ratio: "16:9",
        width: 1920,
        height: 1080,
        fps: 30,
        note: "Khung ngang chuẩn cho YouTube và link video Facebook.",
        safe_zone: None,
        max_duration_sec: 0,
    },
    PresetDef {
        id: "ig_feed_1x1",
        name: "Instagram Feed",
        ratio: "1:1",
        width: 1080,
        height: 1080,
        fps: 30,
        note: "Ảnh vuông trên lưới profile Instagram.",
        safe_zone: Some(SafeZone { top: 40, bottom: 140, left: 40, right: 40 }),
        max_duration_sec: 90,
    },
    PresetDef {
        id: "threads_4x5",
        name: "Threads",
        ratio: "4:5",
        width: 1080,
        height: 1350,
        fps: 30,
        note: "Bài video trên Threads.",
        safe_zone: Some(SafeZone { top: 60, bottom: 140, left: 60, right: 60 }),
        max_duration_sec: 300,
    },
    PresetDef {
        id: "x_16x9",
        name: "X (Twitter)",
        ratio: "16:9",
        width: 1280,
        height: 720,
        fps: 30,
        note: "Video trên X dùng khung 16:9 nhẹ hơn để tải nhanh.",
        safe_zone: Some(SafeZone { top: 40, bottom: 80, left: 0, right: 0 }),
        max_duration_sec: 140,
    },
    PresetDef {
        id: "original",
        name: "Giữ nguyên gốc",
        ratio: "-",
        width: 0,
        height: 0,
        fps: 30,
        note: "Không co giãn: giữ nguyên độ phân giải video nguồn.",
        safe_zone: None,
        max_duration_sec: 0,
    },
    PresetDef {
        id: "custom",
        name: "Tuỳ chỉnh",
        ratio: "-",
        width: 0,
        height: 0,
        fps: 30,
        note: "Nhập kích thước riêng (W × H, chẵn).",
        safe_zone: None,
        max_duration_sec: 0,
    },
    PresetDef {
        id: "audio_only",
        name: "Tách âm thanh (MP3)",
        ratio: "audio",
        width: 0,
        height: 0,
        fps: 0,
        note: "Xuất MP3 320 kbps từ audio đã ghép (áp dụng âm lượng + fade-out), không kèm video.",
        safe_zone: None,
        max_duration_sec: 0,
    },
];

/// A1: preset đặc biệt "chỉ audio" — output là MP3, không phải video.
/// Mọi nơi rẽ nhánh theo preset đều hỏi hàm này trước.
pub fn is_audio_only(preset_id: &str) -> bool {
    preset_id == "audio_only"
}

/// Preset tại runtime (đã gộp override từ config, dữ liệu owned để chỉnh sửa).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub ratio: String,
    /// None = không cố định (giữ nguyên gốc / tuỳ chỉnh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub fps: u32,
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_zone: Option<SafeZone>,
    /// None = không giới hạn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_sec: Option<u64>,
}

impl Preset {
    fn from_def(def: &PresetDef) -> Self {
        Self {
            id: def.id.to_string(),
            name: def.name.to_string(),
            ratio: def.ratio.to_string(),
            width: (def.width > 0).then_some(def.width),
            height: (def.height > 0).then_some(def.height),
            fps: def.fps,
            note: def.note.to_string(),
            safe_zone: def.safe_zone,
            max_duration_sec: (def.max_duration_sec > 0).then_some(def.max_duration_sec as u64),
        }
    }
}

/// Đích render đã chọn của 1 job con (1 preset cụ thể hoá).
#[derive(Debug, Clone, PartialEq)]
pub struct RenderTarget {
    pub preset_id: String,
    /// Tên hiển thị trên UI.
    pub label: String,
    /// None = giữ nguyên kích thước nguồn.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: u32,
    /// Giới hạn thời lượng output của nền tảng (TikTok 3:00, IG 1:30…).
    /// None = không giới hạn. Pass cuối cắt `-t min(audio, max)`.
    pub max_duration_sec: Option<u64>,
}

/// Gói các preset builtin + override từ file config, chia sẻ cho mọi request.
#[derive(Clone)]
pub struct Registry {
    presets: Arc<Vec<Preset>>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    presets: std::collections::BTreeMap<String, PresetOverride>,
}

/// Các field `Some` sẽ đè lên preset builtin cùng id; id chưa có thì thêm mới.
#[derive(Debug, Default, Deserialize)]
struct PresetOverride {
    name: Option<String>,
    ratio: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    note: Option<String>,
    safe_zone: Option<SafeZone>,
    /// None = không đổi; Some(None) = bỏ giới hạn; Some(Some(n)) = đặt n.
    max_duration_sec: Option<Option<u64>>,
}

impl Registry {
    /// Danh sách builtin + gộp `presets_config.json` nếu file tồn tại.
    /// File lỗi (đọc/parse) không làm app chết: log cảnh báo rồi dùng builtin.
    pub fn load(config_path: Option<&Path>) -> Self {
        let mut presets: Vec<Preset> = PRESET_DEFS.iter().map(Preset::from_def).collect();

        if let Some(path) = config_path {
            match std::fs::read_to_string(path) {
                Ok(text) => match serde_json::from_str::<ConfigFile>(&text) {
                    Ok(cfg) => {
                        for (id, ov) in cfg.presets {
                            apply_override(&mut presets, &id, ov);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("presets_config.json không hợp lệ, dùng preset mặc định: {e}");
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!("Không đọc được {}: {e}", path.display());
                }
            }
        }

        Self { presets: Arc::new(presets) }
    }

    /// Danh sách preset cho UI (giữ thứ tự khai báo).
    pub fn list(&self) -> &[Preset] {
        &self.presets
    }

    pub fn get(&self, id: &str) -> Option<&Preset> {
        self.presets.iter().find(|p| p.id == id)
    }

    /// Hoá 1 lựa chọn của người dùng thành đích render.
    /// - "original" → giữ nguyên kích thước nguồn.
    /// - "custom" → cần `custom_width`/`custom_height` (16–7680); số lẻ bị làm
    ///   tròn xuống chẵn (trả về trong warning qua label đã sẵn sàng).
    /// - id khác → lấy đúng độ phân giải của preset.
    pub fn resolve_target(
        &self,
        id: &str,
        custom_width: Option<u32>,
        custom_height: Option<u32>,
    ) -> Result<RenderTarget, String> {
        let preset = self
            .get(id)
            .ok_or_else(|| format!("Không biết preset \"{id}\"."))?;

        match id {
            "original" => Ok(RenderTarget {
                preset_id: preset.id.clone(),
                label: preset.name.clone(),
                width: None,
                height: None,
                fps: preset.fps,
                max_duration_sec: preset.max_duration_sec,
            }),
            // A1: tách audio — không có khung hình, đi nhánh render MP3 riêng.
            "audio_only" => Ok(RenderTarget {
                preset_id: preset.id.clone(),
                label: preset.name.clone(),
                width: None,
                height: None,
                fps: 0,
                max_duration_sec: preset.max_duration_sec,
            }),
            "custom" => {
                let w = custom_width
                    .filter(|v| (16..=7680).contains(v))
                    .ok_or("Preset tuỳ chỉnh cần width từ 16 đến 7680.")?;
                let h = custom_height
                    .filter(|v| (16..=7680).contains(v))
                    .ok_or("Preset tuỳ chỉnh cần height từ 16 đến 7680.")?;
                // yuv420p cần kích thước chẵn.
                let (w, h) = (w & !1, h & !1);
                if w < 16 || h < 16 {
                    return Err("Kích thước tuỳ chỉnh quá nhỏ (sau khi làm tròn chẵn).".into());
                }
                Ok(RenderTarget {
                    preset_id: format!("custom_{w}x{h}"),
                    label: format!("Tuỳ chỉnh {w}×{h}"),
                    width: Some(w),
                    height: Some(h),
                    fps: preset.fps,
                    max_duration_sec: preset.max_duration_sec,
                })
            }
            _ => {
                let (w, h) = (preset.width, preset.height);
                let (Some(w), Some(h)) = (w, h) else {
                    return Err(format!("Preset \"{}\" không cố định độ phân giải.", preset.id));
                };
                Ok(RenderTarget {
                    preset_id: preset.id.clone(),
                    label: preset.name.clone(),
                    width: Some(w),
                    height: Some(h),
                    fps: preset.fps,
                    max_duration_sec: preset.max_duration_sec,
                })
            }
        }
    }
}

fn apply_override(presets: &mut Vec<Preset>, id: &str, ov: PresetOverride) {
    if let Some(p) = presets.iter_mut().find(|p| p.id == id) {
        if let Some(v) = ov.name {
            p.name = v;
        }
        if let Some(v) = ov.ratio {
            p.ratio = v;
        }
        if let Some(v) = ov.width {
            p.width = Some(v);
        }
        if let Some(v) = ov.height {
            p.height = Some(v);
        }
        if let Some(v) = ov.fps {
            p.fps = v;
        }
        if let Some(v) = ov.note {
            p.note = v;
        }
        if let Some(v) = ov.safe_zone {
            p.safe_zone = Some(v);
        }
        if let Some(v) = ov.max_duration_sec {
            p.max_duration_sec = v;
        }
    } else {
        // Preset mới: bắt buộc có kích thước.
        match (ov.width, ov.height) {
            (Some(w), Some(h)) => presets.push(Preset {
                id: id.to_string(),
                name: ov.name.unwrap_or_else(|| id.to_string()),
                ratio: ov.ratio.unwrap_or_else(|| "-".to_string()),
                width: Some(w),
                height: Some(h),
                fps: ov.fps.unwrap_or(30),
                note: ov.note.unwrap_or_default(),
                safe_zone: ov.safe_zone,
                max_duration_sec: ov.max_duration_sec.flatten(),
            }),
            _ => tracing::warn!("Bỏ qua preset mới \"{id}\" trong config: thiếu width/height."),
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_matches_spec_table() {
        // Bảng yêu cầu giai đoạn 2 — kiểm tra id, tỷ lệ, độ phân giải.
        let expect = [
            ("tiktok_9x16", "9:16", (1080, 1920)),
            ("fb_feed_4x5", "4:5", (1080, 1350)),
            ("fb_yt_16x9", "16:9", (1920, 1080)),
            ("ig_feed_1x1", "1:1", (1080, 1080)),
            ("threads_4x5", "4:5", (1080, 1350)),
            ("x_16x9", "16:9", (1280, 720)),
        ];
        let reg = Registry::load(None);
        for (id, ratio, (w, h)) in expect {
            let p = reg.get(id).unwrap_or_else(|| panic!("thiếu preset {id}"));
            assert_eq!(p.ratio, ratio, "preset {id}");
            assert_eq!((p.width, p.height), (Some(w), Some(h)), "preset {id}");
        }
        assert!(reg.get("original").is_some());
        assert!(reg.get("custom").is_some());
        assert!(reg.get("audio_only").is_some());
    }

    #[test]
    fn audio_only_preset_resolves_without_frame() {
        // A1: "Tách âm thanh" resolve thành target không kích thước, không fps.
        let reg = Registry::load(None);
        let t = reg.resolve_target("audio_only", None, None).unwrap();
        assert_eq!(t.preset_id, "audio_only");
        assert_eq!((t.width, t.height), (None, None));
        assert_eq!(t.fps, 0);
        assert!(is_audio_only(&t.preset_id));
        assert!(!is_audio_only("tiktok_9x16"));
        // Vẫn override được qua config như preset thường.
        assert!(reg.get("audio_only").unwrap().note.contains("MP3"));
    }

    #[test]
    fn ids_unique_and_order_stable() {
        let ids: Vec<&str> = PRESET_DEFS.iter().map(|p| p.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "id preset bị trùng");
        // "original" và "custom" luôn có mặt, thứ tự khai báo giữ nguyên.
        assert_eq!(PRESET_DEFS[0].id, "tiktok_9x16");
        assert!(ids.contains(&"original"));
        assert!(ids.contains(&"custom"));
    }

    #[test]
    fn preset_dims_even_for_yuv420p() {
        for p in PRESET_DEFS {
            if p.width > 0 {
                assert_eq!(p.width % 2, 0, "{} width lẻ", p.id);
                assert_eq!(p.height % 2, 0, "{} height lẻ", p.id);
            }
        }
    }

    #[test]
    fn resolve_original_keeps_source_size() {
        let reg = Registry::load(None);
        let t = reg.resolve_target("original", None, None).unwrap();
        assert_eq!((t.width, t.height), (None, None));
        assert_eq!(t.preset_id, "original");
    }

    #[test]
    fn resolve_custom_rounds_down_to_even() {
        let reg = Registry::load(None);
        let t = reg.resolve_target("custom", Some(1081), Some(1921)).unwrap();
        assert_eq!((t.width, t.height), (Some(1080), Some(1920)));
        assert_eq!(t.preset_id, "custom_1080x1920");
    }

    #[test]
    fn resolve_custom_rejects_out_of_range() {
        let reg = Registry::load(None);
        assert!(reg.resolve_target("custom", None, Some(1080)).is_err());
        assert!(reg.resolve_target("custom", Some(10), Some(1080)).is_err());
        assert!(reg.resolve_target("custom", Some(9000), Some(1080)).is_err());
    }

    #[test]
    fn resolve_unknown_preset_fails() {
        let reg = Registry::load(None);
        assert!(reg.resolve_target("khong_ton_tai", None, None).is_err());
    }

    #[test]
    fn config_override_max_duration_and_safe_zone() {
        let dir = std::env::temp_dir().join("rvm_preset_test");
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("presets_config.json");
        std::fs::write(
            &cfg,
            r#"{
              "presets": {
                "tiktok_9x16": { "max_duration_sec": 180, "safe_zone": {"top": 100, "bottom": 400, "left": 30, "right": 200} },
                "kenh_rieng_1x1": { "name": "Kênh riêng", "width": 720, "height": 720, "max_duration_sec": 60 }
              }
            }"#,
        )
        .unwrap();

        let reg = Registry::load(Some(&cfg));
        let tiktok = reg.get("tiktok_9x16").unwrap();
        assert_eq!(tiktok.max_duration_sec, Some(180));
        assert_eq!(
            tiktok.safe_zone,
            Some(SafeZone { top: 100, bottom: 400, left: 30, right: 200 })
        );
        // Không đè field khác.
        assert_eq!(tiktok.width, Some(1080));

        let moi = reg.get("kenh_rieng_1x1").unwrap();
        assert_eq!((moi.width, moi.height), (Some(720), Some(720)));
        assert_eq!(moi.max_duration_sec, Some(60));

        let _ = std::fs::remove_file(&cfg);
    }

    #[test]
    fn config_invalid_json_falls_back_to_builtin() {
        let dir = std::env::temp_dir().join("rvm_preset_test_bad");
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("presets_config.json");
        std::fs::write(&cfg, "{ khong hop le").unwrap();

        let reg = Registry::load(Some(&cfg));
        assert_eq!(reg.list().len(), PRESET_DEFS.len());
        let _ = std::fs::remove_file(&cfg);
    }
}
