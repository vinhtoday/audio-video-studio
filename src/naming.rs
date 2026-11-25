//! Đặt tên file output theo "Tên sản phẩm" người dùng nhập (A2).
//!
//! Toàn bộ hàm ở đây THUẦN (không IO) để unit test dễ; phần quét kho/chỉnh
//! tên file thật nằm ở `jobs.rs` + `routes.rs`. Nguyên tắc an toàn xuyên suốt:
//! **không bao giờ tin tên người dùng làm đường dẫn** — làm sạch về một chuỗi
//! không có ký tự phân tách thư mục (`/`, `\`) nên không thể path traversal.

use unicode_normalization::UnicodeNormalization;

/// Số ký tự tối đa của phần tên (không tính đuôi mở rộng do app tự gắn).
const MAX_NAME_CHARS: usize = 100;

// ---------------------------------------------------------------------------
// Làm sạch tên
// ---------------------------------------------------------------------------

/// Làm sạch tên người dùng thành "stem" an toàn dùng trong tên file.
///
/// - trim + chuẩn hoá Unicode NFC (GIỮ nguyên dấu tiếng Việt);
/// - xoá ký tự cấm của Windows/Linux: `< > : " / \ | ? *` và ký tự điều khiển;
/// - bỏ dấu chấm / khoảng trắng ở hai đầu;
/// - giới hạn [MAX_NAME_CHARS] ký tự (đếm theo ký tự, không phải byte);
/// - né tên dành riêng của Windows (CON, PRN, AUX, NUL, COM1-9, LPT1-9) bằng
///   cách thêm hậu tố `_`;
/// - rỗng sau khi làm sạch → `None` (gọi về tên mặc định).
pub fn sanitize_filename(input: &str) -> Option<String> {
    // 1. NFC + trim: "Kem chống nắng" ở dạng compose/decompose đều ra 1 chuỗi.
    let mut s: String = input.nfc().collect::<String>().trim().to_string();
    // 2. Xoá ký tự cấm + ký tự điều khiển (giữ mọi ký tự khác, kể cả dấu Việt).
    s = s
        .chars()
        .filter(|c| !is_forbidden(*c))
        .collect();
    // 3. Bỏ '.'/' ' ở hai đầu ("..name.." → "name"; ".." → rỗng).
    trim_edge_dots_spaces(&mut s);
    // 4. Giới hạn độ dài, rồi bỏ '.'/' ' cuối lần nữa (truncation có thể để lại).
    if s.chars().count() > MAX_NAME_CHARS {
        s = s.chars().take(MAX_NAME_CHARS).collect();
        trim_edge_dots_spaces(&mut s);
    }
    if s.is_empty() {
        return None;
    }
    // 5. Né tên dành riêng của Windows ("CON.mp4" cũng bị Windows chặn).
    if is_windows_reserved(&s) {
        s.push('_');
    }
    Some(s)
}

/// Ký tự cấm trong tên file trên Windows/Linux + ký tự điều khiển.
fn is_forbidden(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control()
}

/// Bỏ mọi '.' và ' ' ở đầu và cuối (in-place).
fn trim_edge_dots_spaces(s: &mut String) {
    while matches!(s.chars().next(), Some('.') | Some(' ')) {
        s.remove(0);
    }
    while matches!(s.chars().last(), Some('.') | Some(' ')) {
        s.pop();
    }
}

/// Tên dành riêng của Windows (đối chiếu không phân biệt hoa/thường):
/// CON, PRN, AUX, NUL, COM1-9, LPT1-9. (COM10/LPT10 không nằm trong danh sách.)
fn is_windows_reserved(s: &str) -> bool {
    let up = s.to_ascii_uppercase();
    if matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(rest) = up.strip_prefix(prefix) {
            if rest.len() == 1 && rest.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Bỏ dấu tiếng Việt
// ---------------------------------------------------------------------------

/// Bỏ dấu tiếng Việt: `đ→d`, `Đ→D`, mọi nguyên âm có dấu/thanh về chữ cái
/// gốc, khoảng trắng thành `-`. Ký tự khác giữ nguyên.
pub fn remove_diacritics(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ả' | 'ã' | 'ạ' | 'ă' | 'ắ' | 'ằ' | 'ẳ' | 'ẵ' | 'ặ' | 'â' | 'ấ'
            | 'ầ' | 'ẩ' | 'ẫ' | 'ậ' => 'a',
            'Á' | 'À' | 'Ả' | 'Ã' | 'Ạ' | 'Ă' | 'Ắ' | 'Ằ' | 'Ẳ' | 'Ẵ' | 'Ặ' | 'Â' | 'Ấ'
            | 'Ầ' | 'Ẩ' | 'Ẫ' | 'Ậ' => 'A',
            'é' | 'è' | 'ẻ' | 'ẽ' | 'ẹ' | 'ê' | 'ế' | 'ề' | 'ể' | 'ễ' | 'ệ' => 'e',
            'É' | 'È' | 'Ẻ' | 'Ẽ' | 'Ẹ' | 'Ê' | 'Ế' | 'Ề' | 'Ể' | 'Ễ' | 'Ệ' => 'E',
            'í' | 'ì' | 'ỉ' | 'ĩ' | 'ị' => 'i',
            'Í' | 'Ì' | 'Ỉ' | 'Ĩ' | 'Ị' => 'I',
            'ó' | 'ò' | 'ỏ' | 'õ' | 'ọ' | 'ô' | 'ố' | 'ồ' | 'ổ' | 'ỗ' | 'ộ' | 'ơ' | 'ớ'
            | 'ờ' | 'ở' | 'ỡ' | 'ợ' => 'o',
            'Ó' | 'Ò' | 'Ỏ' | 'Õ' | 'Ọ' | 'Ô' | 'Ố' | 'Ồ' | 'Ổ' | 'Ỗ' | 'Ộ' | 'Ơ' | 'Ớ'
            | 'Ờ' | 'Ở' | 'Ỡ' | 'Ợ' => 'O',
            'ú' | 'ù' | 'ủ' | 'ũ' | 'ụ' | 'ư' | 'ứ' | 'ừ' | 'ử' | 'ữ' | 'ự' => 'u',
            'Ú' | 'Ù' | 'Ủ' | 'Ũ' | 'Ụ' | 'Ư' | 'Ứ' | 'Ừ' | 'Ử' | 'Ữ' | 'Ự' => 'U',
            'ý' | 'ỳ' | 'ỷ' | 'ỹ' | 'ỵ' => 'y',
            'Ý' | 'Ỳ' | 'Ỷ' | 'Ỹ' | 'Ỵ' => 'Y',
            'đ' => 'd',
            'Đ' => 'D',
            ' ' => '-',
            other => other,
        })
        .collect()
}

/// Tên stem cuối cùng theo tuỳ chọn: làm sạch rồi bỏ dấu (nếu bật).
/// Bỏ dấu có thể tạo ra tên Windows dành riêng ("CÔN" → "CON") nên kiểm tra
/// lại sau khi bỏ dấu.
pub fn resolve_stem(input: &str, strip: bool) -> Option<String> {
    let s = sanitize_filename(input)?;
    if !strip {
        return Some(s);
    }
    let s = remove_diacritics(&s);
    // Bỏ dấu giữ nguyên độ dài và không tạo '.'/' ' cuối, chỉ cần né tên reserved.
    if s.is_empty() {
        return None;
    }
    if is_windows_reserved(&s) {
        return Some(format!("{s}_"));
    }
    Some(s)
}

// ---------------------------------------------------------------------------
// Tên file cuối (mp4 / zip)
// ---------------------------------------------------------------------------

/// Tên file mp4 của 1 preset theo quy tắc:
/// - 1 preset: `<tên>.mp4`;
/// - nhiều preset: `<tên>_<preset_id>.mp4`.
pub fn preset_filename(stem: &str, preset_id: &str, multi: bool) -> String {
    if multi {
        format!("{stem}_{preset_id}.mp4")
    } else {
        format!("{stem}.mp4")
    }
}

/// Tên file zip gộp nhiều preset: `<tên>.zip`.
pub fn zip_filename(stem: &str) -> String {
    format!("{stem}.zip")
}

// ---------------------------------------------------------------------------
// Trùng tên trong kho
// ---------------------------------------------------------------------------

/// Tìm tên chưa bị chiếm: nếu `desired` đã có trong `existing` thì thử
/// `thân (2).đuôi`, `thân (3).đuôi`… — không bao giờ trả về tên đã có.
pub fn unique_display_name(desired: &str, existing: &[String]) -> String {
    if !existing.iter().any(|e| e == desired) {
        return desired.to_string();
    }
    // Tách đuôi mở rộng ở dấu chấm CUỐI (giữ ".tar.gz" nguyên vẹn).
    let (stem, ext) = match desired.rfind('.') {
        Some(i) => (&desired[..i], &desired[i..]),
        None => (desired, ""),
    };
    for n in 2..1000u32 {
        let cand = format!("{stem} ({n}){ext}");
        if !existing.iter().any(|e| e == &cand) {
            return cand;
        }
    }
    // Về mặt thực tế không bao giờ tới đây; phòng khi ai đó tạo 998 bản trùng.
    format!("{stem}__{ext}")
}

// ---------------------------------------------------------------------------
// HTTP header + tra cứu an toàn
// ---------------------------------------------------------------------------

/// Giá trị header `Content-Disposition` chuẩn RFC 6266: `filename` là fallback
/// ASCII (ký tự non-ASCII → `_`) và `filename*=UTF-8''…` percent-encode đúng
/// tên gốc — trình duyệt hiện đúng tên tiếng Việt khi tải xuống.
pub fn content_disposition(download: bool, filename: &str) -> String {
    if !download {
        return "inline".to_string();
    }
    let fallback: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut out = format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''");
    for b in filename.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Kiểm tra tên file tra cứu kho (tham số `file` trên URL): giữ nguyên
/// Unicode để khớp tên trên đĩa, chỉ chặn mọi khả năng path traversal —
/// từ chối nếu có `/`, `\`, ký tự điều khiển, dài quá 255, bắt đầu bằng `.`,
/// hoặc chính là `.`/`..`.
pub fn safe_lookup_name(input: &str) -> Option<String> {
    if input.is_empty() || input.len() > 255 {
        return None;
    }
    if input.contains(['/', '\\']) || input.starts_with('.') {
        return None;
    }
    if input == "." || input == ".." {
        return None;
    }
    if input.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(input.to_string())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- sanitize_filename -------------------------------------------------

    #[test]
    fn sanitize_giu_dau_tieng_viet() {
        assert_eq!(sanitize_filename("Kem chống nắng"), Some("Kem chống nắng".into()));
        // Dạng NFD (tách dấu) cũng phải ra NFC chuẩn.
        let nfd: String = "Kem chống nắng".nfd().collect();
        assert_eq!(sanitize_filename(&nfd), Some("Kem chống nắng".into()));
    }

    #[test]
    fn sanitize_xoa_ky_tu_cam() {
        assert_eq!(sanitize_filename("a<b>c:\"d/e\\f|g?h*i"), Some("abcdefghi".into()));
        // Ký tự điều khiển (tab, NUL) bị xoá; khoảng trắng thường GIỮ nguyên
        // (chỉ bị chuyển thành '-' khi bật "bỏ dấu").
        assert_eq!(sanitize_filename("ke\tm\t ch\u{0}ống"), Some("kem chống".into()));
    }

    #[test]
    fn sanitize_ten_windows_danh_rieng() {
        assert_eq!(sanitize_filename("CON"), Some("CON_".into()));
        assert_eq!(sanitize_filename("con"), Some("con_".into()));
        assert_eq!(sanitize_filename("Com3"), Some("Com3_".into()));
        assert_eq!(sanitize_filename("LPT9"), Some("LPT9_".into()));
        // Không phải tên dành riêng thì giữ nguyên.
        assert_eq!(sanitize_filename("CONAN"), Some("CONAN".into()));
        assert_eq!(sanitize_filename("COM10"), Some("COM10".into()));
        assert_eq!(sanitize_filename("LPT"), Some("LPT".into()));
    }

    #[test]
    fn sanitize_gioi_han_100_ky_tu() {
        let long = "ă".repeat(150);
        let out = sanitize_filename(&long).unwrap();
        assert_eq!(out.chars().count(), 100);
        // Truncate xong không được để dấu chấm/khoảng trắng cuối.
        let long2 = format!("{}{}", "a".repeat(99), "..");
        assert_eq!(sanitize_filename(&long2), Some("a".repeat(99)));
    }

    #[test]
    fn sanitize_rong_hoac_toan_ky_tu_cam() {
        assert_eq!(sanitize_filename(""), None);
        assert_eq!(sanitize_filename("   "), None);
        assert_eq!(sanitize_filename("???"), None);
        assert_eq!(sanitize_filename("<>:\"/\\|?*"), None);
    }

    #[test]
    fn sanitize_bo_cham_va_khoang_trang_cuoi() {
        assert_eq!(sanitize_filename("Kem chống nắng... "), Some("Kem chống nắng".into()));
        assert_eq!(sanitize_filename("  .name. "), Some("name".into()));
        // Chấm GIỮA tên giữ nguyên.
        assert_eq!(sanitize_filename("Sữa.chống.nắng"), Some("Sữa.chống.nắng".into()));
    }

    #[test]
    fn sanitize_path_traversal_vo_hieu() {
        // Không còn ký tự phân tách thư mục → không thể trỏ ra ngoài.
        assert_eq!(sanitize_filename("../../x"), Some("x".into()));
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("/etc/passwd"), Some("etcpasswd".into()));
        assert_eq!(sanitize_filename("C:\\Windows\\system32"), Some("CWindowssystem32".into()));
    }

    // ---- remove_diacritics -------------------------------------------------

    #[test]
    fn bo_dau_tieng_viet() {
        assert_eq!(remove_diacritics("Kem chống nắng"), "Kem-chong-nang");
        assert_eq!(remove_diacritics("Đà Nẵng"), "Da-Nang");
        assert_eq!(remove_diacritics("Bạn Đã Biết"), "Ban-Da-Biet");
        assert_eq!(remove_diacritics("Ủy ban Thường trực"), "Uy-ban-Thuong-truc");
        // Ký tự không phải tiếng Việt giữ nguyên.
        assert_eq!(remove_diacritics("Sản phẩm #1!"), "San-pham-#1!");
    }

    #[test]
    fn bo_dau_khong_tao_ten_windows_reserved() {
        // "CÔN" → "CON" là tên reserved → resolve_stem phải né.
        assert_eq!(resolve_stem("CÔN", true), Some("CON_".into()));
        assert_eq!(resolve_stem("CÔN", false), Some("CÔN".into()));
        // resolve_stem không bỏ dấu khi flag tắt.
        assert_eq!(resolve_stem("Kem chống nắng", false), Some("Kem chống nắng".into()));
        assert_eq!(resolve_stem("Kem chống nắng", true), Some("Kem-chong-nang".into()));
        // Rỗng → None ở cả 2 chế độ.
        assert_eq!(resolve_stem("???", true), None);
    }

    // ---- tên file cuối -----------------------------------------------------

    #[test]
    fn ten_theo_so_preset() {
        assert_eq!(preset_filename("Kem chống nắng", "tiktok_9x16", false), "Kem chống nắng.mp4");
        assert_eq!(
            preset_filename("Kem chống nắng", "tiktok_9x16", true),
            "Kem chống nắng_tiktok_9x16.mp4"
        );
        assert_eq!(zip_filename("Kem chống nắng"), "Kem chống nắng.zip");
    }

    // ---- trùng tên ----------------------------------------------------------

    #[test]
    fn trung_ten_them_so_thu_tu() {
        let existing: Vec<String> = vec!["Kem chống nắng.mp4".into()];
        assert_eq!(
            unique_display_name("Kem chống nắng.mp4", &existing),
            "Kem chống nắng (2).mp4"
        );
        // (2) đã có → nhảy (3).
        let existing2: Vec<String> = vec!["n.mp4".into(), "n (2).mp4".into()];
        assert_eq!(unique_display_name("n.mp4", &existing2), "n (3).mp4");
        // Không trùng → giữ nguyên.
        assert_eq!(unique_display_name("khac.mp4", &existing2), "khac.mp4");
        // Không có đuôi mở rộng vẫn hoạt động.
        assert_eq!(unique_display_name("name", &["name".to_string()][..]), "name (2)");
        // Tách đuôi theo dấu chấm CUỐI (quy tắc generic): "clip.tar.gz" →
        // stem "clip.tar" + ".gz".
        assert_eq!(
            unique_display_name("clip.tar.gz", &["clip.tar.gz".to_string()][..]),
            "clip.tar (2).gz"
        );
    }

    // ---- Content-Disposition -------------------------------------------------

    #[test]
    fn content_disposition_ten_tieng_viet() {
        let cd = content_disposition(true, "Kem chống nắng.mp4");
        // Fallback ASCII: mọi ký tự non-ASCII thành '_', ASCII giữ nguyên.
        assert!(cd.contains("filename=\"Kem ch_ng n_ng.mp4\""), "{cd}");
        // filename* percent-encode UTF-8: ố = E1 BB 91, ắ = E1 BA AF.
        assert!(cd.contains(
            "filename*=UTF-8''Kem%20ch%E1%BB%91ng%20n%E1%BA%AFng.mp4"
        ), "{cd}");
    }

    #[test]
    fn content_disposition_ascii_va_inline() {
        // Tên ASCII thuần: cả 2 dạng đều có, filename* chính là tên gốc.
        let cd = content_disposition(true, "radio_tiktok_9x16.mp4");
        assert!(cd.contains("filename=\"radio_tiktok_9x16.mp4\""), "{cd}");
        assert!(cd.contains("filename*=UTF-8''radio_tiktok_9x16.mp4"), "{cd}");
        assert_eq!(content_disposition(false, "x.mp4"), "inline");
        // Ký tự cấm trong header value (dấu nháy) được thay bằng '_'.
        let cd = content_disposition(true, "a\"b.mp4");
        assert!(cd.contains("filename=\"a_b.mp4\""), "{cd}");
    }

    // ---- tra cứu an toàn ------------------------------------------------------

    #[test]
    fn lookup_chan_path_traversal() {
        assert_eq!(safe_lookup_name("../../etc/passwd"), None);
        assert_eq!(safe_lookup_name("a/b.mp4"), None);
        assert_eq!(safe_lookup_name("a\\b.mp4"), None);
        assert_eq!(safe_lookup_name(".."), None);
        assert_eq!(safe_lookup_name(".hidden"), None);
        assert_eq!(safe_lookup_name(""), None);
        // Tên tiếng Việt + " (2)" hợp lệ, chấm giữa tên hợp lệ.
        assert_eq!(safe_lookup_name("Kem chống nắng (2).mp4"), Some("Kem chống nắng (2).mp4".into()));
        assert_eq!(safe_lookup_name("a..b.mp4"), Some("a..b.mp4".into()));
    }
}
