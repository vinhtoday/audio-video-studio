//! Fontconfig cho FFmpeg đa nền tảng (Windows / Linux / macOS).
//!
//! Bản dựng FFmpeg trên Windows thường bật `--enable-fontconfig`; mặc định nó
//! tra `/etc/fonts/fonts.conf` (không tồn tại) và chết với
//! "Fontconfig error: Cannot load default config file" (exit 1).
//!
//! Chiến lược 3 lớp (quy tắc bất biến Q2):
//! 1. `ensure_fonts_conf()` — tạo file `fonts.conf` chỉ định thư mục font hệ
//!    thống của từng nền tảng, và mọi tiến trình ffmpeg/ffprobe do app spawn
//!    đều được bơm biến môi trường `FONTCONFIG_FILE` trỏ tới file này
//!    (xem `run_ffmpeg` trong `ffmpeg.rs` và `probe` trong `ffmpeg.rs`).
//! 2. `default_font_path()` — khi người dùng chọn "Font mặc định", ưu tiên
//!    trỏ **trực tiếp** `fontfile=` vào font hệ thống (Arial trên Windows)
//!    để FreeType nạp thẳng, không phụ thuộc tra cứu fontconfig → tiếng
//!    Việt UTF-8 render chuẩn 100%.
//! 3. `list_system_fonts()` — `fc-list` nếu có; nếu không có (Windows không
//!    ship fc-list) fallback về danh sách font cơ bản của Windows.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Danh sách font cơ bản của Windows dùng khi `fc-list` không tồn tại.
pub const WINDOWS_FALLBACK_FONTS: &[&str] = &[
    "Arial",
    "Calibri",
    "Segoe UI",
    "Tahoma",
    "Times New Roman",
    "Verdana",
];

/// Thư mục font hệ thống theo nền tảng.
fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "windows") {
        // %WINDIR%\Fonts (thường là C:\Windows\Fonts) + biến WINDOWSFONTDIR
        // mà một số bản dựng FFmpeg dùng.
        if let Some(windir) = std::env::var_os("WINDIR") {
            dirs.push(PathBuf::from(&windir).join("Fonts"));
        } else {
            dirs.push(PathBuf::from(r"C:\Windows\Fonts"));
        }
        dirs.push(PathBuf::from("WINDOWSFONTDIR"));
    } else if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(&home).join("Library/Fonts"));
        }
    } else {
        // Linux/BSD: các thư mục fontconfig tiêu chuẩn.
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(&home).join(".fonts"));
            dirs.push(PathBuf::from(&home).join(".local/share/fonts"));
        }
    }
    dirs
}

/// Đường dẫn file `fonts.conf` app quản lý (tạo trong workspace/fonts).
/// Workspace gốc lấy từ `RVM_WORKSPACE` hoặc `./workspace` — trùng logic jobs.
fn fonts_conf_path() -> PathBuf {
    let ws = std::env::var_os("RVM_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("workspace"));
    ws.join("fonts.conf")
}

/// Tạo (nếu chưa có) file fonts.conf trỏ vào thư mục font hệ thống và trả về
/// đường dẫn tới nó. File rất nhỏ, tạo idempotent mỗi lần gọi đầu tiên.
pub fn ensure_fonts_conf() -> std::io::Result<PathBuf> {
    let conf = fonts_conf_path();
    // File do bản cũ (0.3) tạo chưa có <cachedir> → ghi lại, nếu không
    // fontconfig báo "No writable cache directories" và tra font chậm mỗi lần.
    if conf.exists()
        && std::fs::read_to_string(&conf)
            .map(|c| c.contains("<cachedir>"))
            .unwrap_or(false)
    {
        return Ok(conf);
    }
    if let Some(parent) = conf.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut xml = String::from("<?xml version=\"1.0\"?>\n<fontconfig>\n");
    for d in font_dirs() {
        let d = d.to_string_lossy().replace('\\', "/");
        xml.push_str(&format!("  <dir>{}</dir>\n", xml_escape(&d)));
    }
    // Thư mục cache RIÊNG của app: khi app chạy từ thư mục không ghi được
    // cache hệ thống (~/.cache, /var/cache) fontconfig báo "No writable cache
    // directories" và tra cứu font chậm lại mỗi lần spawn ffmpeg.
    let cache = conf
        .parent()
        .unwrap_or(Path::new("workspace"))
        .join("fontconfig-cache")
        .to_string_lossy()
        .replace('\\', "/");
    xml.push_str(&format!("  <cachedir>{}</cachedir>\n", xml_escape(&cache)));
    xml.push_str("</fontconfig>\n");
    std::fs::write(&conf, xml)?;
    tracing::info!("Đã tạo fonts.conf cho fontconfig: {}", conf.display());
    Ok(conf)
}

/// Bơm `FONTCONFIG_FILE` (nếu chưa có trong môi trường) vào Command sắp spawn
/// ffmpeg/ffprobe (tokio::process). Được gọi bởi mọi nơi spawn ffmpeg/ffprobe.
pub fn apply_env_tokio(cmd: &mut tokio::process::Command) {
    apply_env_inner(|k, v| {
        cmd.env(k, v);
    });
}

fn apply_env_inner(mut set_env: impl FnMut(&str, &std::path::Path)) {
    if std::env::var_os("FONTCONFIG_FILE").is_some() {
        tracing::debug!("FONTCONFIG_FILE đã có sẵn — không đụng.");
        return; // người dùng tự cấu hình — tôn trọng.
    }
    match ensure_fonts_conf() {
        Ok(conf) => {
            tracing::debug!("Bơm FONTCONFIG_FILE={} cho ffmpeg", conf.display());
            set_env("FONTCONFIG_FILE", &conf);
        }
        Err(e) => {
            // Không chết vì fonts.conf — ffmpeg vẫn chạy nếu filter không
            // cần fontconfig (fontfile trực tiếp). Ghi log để chẩn đoán.
            tracing::warn!("Không tạo được fonts.conf: {e}");
        }
    }
}

/// Font mặc định trỏ trực tiếp bằng `fontfile=` (Q2b): chọn file font phổ
/// nhất của nền tảng, ưu tiên font có glyph tiếng Việt đầy đủ.
///
/// Trả về `None` nếu không tìm thấy file nào (thường chỉ trên Linux trần trụi
/// — khi đó filter không kèm `fontfile` và ffmpeg tự dùng fontconfig mặc định).
pub fn default_font_path() -> Option<PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE.get_or_init(default_font_path_uncached).clone()
}

/// Quét thư mục font hệ thống (CHỈ chạy 1 lần — kết quả được
/// [`default_font_path`] cache lại cho cả tiến trình).
fn default_font_path_uncached() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[
            "arial.ttf",
            "segoeui.ttf",
            "tahoma.ttf",
            "calibri.ttf",
            "times.ttf",
            "verdana.ttf",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "SFNS.ttf",           // San Francisco (hệ thống)
            "Helvetica.ttc",
            "Arial.ttf",
            "Georgia.ttf",
        ]
    } else {
        // Linux: DejaVu Sans có glyph tiếng Việt đầy đủ, Noto Sans tiếp theo.
        &[
            "DejaVuSans.ttf",
            "NotoSans-Regular.ttf",
            "NotoSansSC-Regular.ttf",
            "LiberationSans-Regular.ttf",
            "FreeSans.ttf",
        ]
    };

    // Windows/macOS: font nằm ngay trong thư mục hệ thống.
    for dir in font_dirs() {
        if dir.as_os_str() == "WINDOWSFONTDIR" {
            continue; // literal, không phải path thật.
        }
        for name in candidates {
            let p = dir.join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    // Linux/BSD: font nằm sâu tuỳ distro (/usr/share/fonts/truetype/dejavu/,
    // …/fonts-noto/) — quét đệ quy tối đa 3 tầng theo thứ tự ưu tiên ứng viên.
    if cfg!(not(target_os = "windows")) && cfg!(not(target_os = "macos")) {
        fn find_rec(dir: &Path, name: &str, depth: u8) -> Option<PathBuf> {
            if depth == 0 {
                return None;
            }
            let rd = std::fs::read_dir(dir).ok()?;
            for sub in rd.flatten() {
                let p = sub.path();
                if p.is_dir() {
                    if let Some(hit) = find_rec(&p, name, depth - 1) {
                        return Some(hit);
                    }
                } else if p.file_name().map(|f| f == name).unwrap_or(false) {
                    return Some(p);
                }
            }
            None
        }
        for root in ["/usr/share/fonts", "/usr/local/share/fonts"] {
            for name in candidates {
                if let Some(p) = find_rec(Path::new(root), name, 3) {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Escape ký tự đặc biệt XML cho file fonts.conf (`&`, `<`, `>`).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Tên family (tiếng Anh) của một file font — đọc name table thật bằng fontdb.
/// Dùng để ghi `Fontname` vào ASS cho font người dùng upload (libass chỉ tra
/// font theo family + `fontsdir`, không nhận `fontfile`).
///
/// Trả `None` nếu file không phải font hợp lệ (TTC/OTF/TTF/WOFF đều đọc được;
/// WOFF2 thì không).
pub fn font_family_name(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let mut db = fontdb::Database::new();
    db.load_font_data(data);
    let face = db.faces().next()?;
    // fontdb đặt tên family tiếng Anh (en-US) lên ĐẦU danh sách, tên bản địa
    // (tiếng Việt, tiếng Trung…) xếp sau — libass/VSFilter quen tên tiếng Anh.
    face.families.first().map(|(name, _)| name.clone())
}

/// Liệt kê family font hệ thống cho dropdown (Q2c): `fc-list` nếu có,
/// fallback quét thật bằng crate open-source `fontdb`
/// (https://github.com/RazrFalcon/fontdb — cùng nền mà resvg/usvg dùng).
pub fn list_system_fonts() -> Vec<String> {
    let out = std::process::Command::new("fc-list").arg(":").arg("family").output();
    if let Ok(o) = out {
        if o.status.success() {
            let mut families: Vec<String> = String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            families.sort();
            families.dedup();
            if !families.is_empty() {
                return families;
            }
        }
    }
    // Fallback: fc-list không có (Windows không ship) → fontdb quét toàn bộ
    // font hệ thống THẬT (registry + %WINDIR%\Fonts trên Windows) thay cho
    // danh sách tĩnh 5 font trước đây.
    let mut families = list_fonts_fontdb();
    if families.is_empty() {
        // Không quét được gì (máy lạ) — vẫn trả fallback tĩnh để dropdown
        // không trống.
        families = WINDOWS_FALLBACK_FONTS.iter().map(|s| s.to_string()).collect();
        if !cfg!(target_os = "windows") {
            families.push("DejaVu Sans".into());
            families.push("Noto Sans".into());
        }
    }
    families.sort();
    families.dedup();
    families
}

/// CSDL font hệ thống (fontdb) — quét MỘT lần rồi cache cho cả tiến trình.
fn system_db() -> &'static fontdb::Database {
    static DB: OnceLock<fontdb::Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        for d in font_dirs() {
            // "WINDOWSFONTDIR" là sentinel của fonts.conf, không phải thư mục thật.
            if d.as_os_str() != "WINDOWSFONTDIR" && d.is_dir() {
                db.load_fonts_dir(&d);
            }
        }
        db
    })
}

/// File font của một family hệ thống — dùng để đọc metric khi giãn chữ (xem
/// `fontinfo`). `None` nếu máy không có family ấy.
///
/// Tự quét thay vì `Database::query`: query của fontdb phân biệt hoa/thường
/// theo tên family, còn người dùng gõ tên trong API một cách tự do. Trong các
/// face cùng family, ưu tiên face "thường" (không đậm/nghiêng/co giãn).
pub fn system_font_file(family: &str) -> Option<PathBuf> {
    let target = family.trim();
    if target.is_empty() {
        return None;
    }
    let mut best: Option<(u8, &fontdb::FaceInfo)> = None;
    for face in system_db().faces() {
        if !face.families.iter().any(|(name, _)| name.eq_ignore_ascii_case(target)) {
            continue;
        }
        let score = u8::from(face.weight == fontdb::Weight::NORMAL)
            + u8::from(face.style == fontdb::Style::Normal)
            + u8::from(face.stretch == fontdb::Stretch::Normal);
        if best.is_none_or(|(s, f)| score > s || (score == s && face.index < f.index)) {
            best = Some((score, face));
        }
    }
    let face = best?.1;
    // Font trên đĩa: `File` hoặc `SharedFile` (khi bật feature memmap — mặc định).
    match &face.source {
        fontdb::Source::File(p) | fontdb::Source::SharedFile(p, _) => Some(p.clone()),
        _ => None,
    }
}

/// Quét font hệ thống bằng fontdb: trả về mọi tên family (bản tiếng Anh +
/// bản địa của từng face).
fn list_fonts_fontdb() -> Vec<String> {
    let mut families: Vec<String> = system_db()
        .faces()
        .flat_map(|f| f.families.iter().map(|(name, _)| name.clone()))
        .collect();
    families.sort();
    families.dedup();
    families
}

/// Đường dẫn font phục vụ test: chấp nhận override để unit test không phụ
/// thuộc máy thật. (`#[cfg(test)]` không dùng được vì hàm này nằm trong cây
/// runtime duy nhất.)
#[allow(dead_code)]
pub fn font_at(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Các test dưới đây đổi biến môi trường `RVM_WORKSPACE` (toàn tiến
    /// trình) → khoá để không chạy chồng lên nhau khi test chạy song song.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn fonts_conf_tao_duoc_va_ton_tai() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Chạy trong workspace test riêng để không đụng workspace thật.
        let tmp = std::env::temp_dir().join(format!("rvm-fontconf-{}", std::process::id()));
        std::env::set_var("RVM_WORKSPACE", &tmp);
        let conf = ensure_fonts_conf().expect("tạo fonts.conf");
        assert!(conf.exists(), "file fonts.conf phải tồn tại: {}", conf.display());
        let xml = std::fs::read_to_string(&conf).unwrap();
        assert!(xml.starts_with("<?xml"), "phải là XML hợp lệ: {xml}");
        assert!(xml.contains("<fontconfig>"), "{xml}");
        assert!(xml.contains("<dir>"), "phải có ít nhất 1 thư mục font: {xml}");
        std::env::remove_var("RVM_WORKSPACE");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fallback_fonts_khi_khong_co_fc_list() {
        // fc-list có thể có hoặc không trên máy chạy test — kiểm danh sách
        // fallback luôn hợp lệ (không rỗng, chứa Arial).
        let list = WINDOWS_FALLBACK_FONTS.to_vec();
        assert!(list.contains(&"Arial"));
        assert!(list.len() >= 5);
    }

    fn find_test_font(name: &str) -> Option<PathBuf> {
        for root in ["/usr/share/fonts", "/usr/local/share/fonts"] {
            if let Ok(Some(p)) = find_rec(Path::new(root), name, 4) {
                return Some(p);
            }
        }
        None
    }

    fn find_rec(dir: &Path, name: &str, depth: u8) -> std::io::Result<Option<PathBuf>> {
        if depth == 0 {
            return Ok(None);
        }
        for sub in std::fs::read_dir(dir)?.flatten() {
            let p = sub.path();
            if p.is_dir() {
                if let Some(hit) = find_rec(&p, name, depth - 1)? {
                    return Ok(Some(hit));
                }
            } else if p.file_name().map(|f| f == name).unwrap_or(false) {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }

    #[test]
    fn font_family_name_doc_file_that() {
        // DejaVu Sans là font tiêu chuẩn của mọi distro — đọc name table thật.
        let Some(p) = find_test_font("DejaVuSans.ttf") else { return };
        assert_eq!(font_family_name(&p).as_deref(), Some("DejaVu Sans"));
    }

    #[test]
    fn font_family_name_tu_choi_file_khong_phai_font() {
        let tmp = std::env::temp_dir().join(format!("rvm-notfont-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let f = tmp.join("fake.ttf");
        std::fs::write(&f, b"day khong phai font").unwrap();
        assert!(font_family_name(&f).is_none());
        assert!(font_family_name(&tmp.join("khong-ton-tai.ttf")).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn default_font_duoc_cache_va_on_dinh() {
        // Gọi 2 lần phải ra cùng 1 đường dẫn (cache OnceLock).
        assert_eq!(default_font_path(), default_font_path());
    }

    #[test]
    fn fonts_conf_co_cachedir_trong_workspace() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("rvm-fontconf2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("RVM_WORKSPACE", &tmp);
        let conf = ensure_fonts_conf().expect("tao fonts.conf");
        let xml = std::fs::read_to_string(&conf).unwrap();
        assert!(xml.contains("<cachedir>"), "phai co cachedir: {xml}");
        assert!(xml.contains("fontconfig-cache"), "{xml}");
        std::env::remove_var("RVM_WORKSPACE");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fonts_conf_ban_cu_thieu_cachedir_duoc_ghi_lai() {
        // Bản 0.3 để lại fonts.conf không có <cachedir> → phải được nâng cấp tại chỗ.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("rvm-fontconf3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("fonts.conf"), "<?xml version=\"1.0\"?>\n<fontconfig>\n</fontconfig>\n").unwrap();
        std::env::set_var("RVM_WORKSPACE", &tmp);
        let conf = ensure_fonts_conf().expect("tạo fonts.conf");
        let xml = std::fs::read_to_string(&conf).unwrap();
        assert!(xml.contains("<cachedir>"), "phai duoc ghi lai co cachedir: {xml}");
        // Lần gọi sau: file đã chuẩn → giữ nguyên, không ghi lại.
        std::fs::write(&conf, &xml).unwrap();
        assert_eq!(ensure_fonts_conf().unwrap(), conf);
        std::env::remove_var("RVM_WORKSPACE");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn system_font_file_kiem_lai_ten_family() {
        // Máy có DejaVu Sans → trả đường dẫn thật, file tồn tại.
        if find_test_font("DejaVuSans.ttf").is_some() {
            let p = system_font_file("DejaVu Sans").expect("phải tìm thấy DejaVu Sans");
            assert!(p.is_file(), "{}", p.display());
            // Không phân biệt hoa/thường.
            assert_eq!(system_font_file("dejavu sans"), Some(p));
        }
        // Family không tồn tại → None (fontdb sẽ rơi về font đầu tiên, phải chặn).
        assert_eq!(system_font_file("RVM Khong Co Font Nay XYZ"), None);
        assert_eq!(system_font_file("   "), None);
    }

    #[test]
    fn default_font_tren_may_nay() {
        // Máy CI Linux phải tìm thấy DejaVu/Noto; máy Windows phải có arial.
        let p = default_font_path();
        if let Some(p) = p {
            assert!(p.exists(), "font mặc định phải tồn tại: {}", p.display());
        }
        // None chấp nhận được (không có font nào) — không fail cứng.
    }
}
