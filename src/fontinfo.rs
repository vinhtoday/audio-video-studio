//! Đọc tối giản font TrueType/OpenType (SFNT) — không cần dependency.
//!
//! Vì sao cần: drawtext của FFmpeg không có tuỳ chọn letter-spacing, nên để
//! giãn chữ app phải chèn một ký tự "khoảng cách mảnh" giữa các chữ. Cách đó
//! chỉ an toàn khi font CÓ glyph của ký tự ấy — font thiếu U+200A (hair space),
//! ví dụ Noto Serif SC hay nhiều font display, sẽ vẽ ô `.notdef` (□) giữa MỌI
//! chữ. Module này đọc bảng `cmap` (mã → glyph) và `hmtx`/`hhea`/`head` (bề
//! rộng glyph) để chọn ký tự giãn mà font thực sự có, lặp đúng số lần theo bề
//! rộng thật.
//!
//! Hỗ trợ file `.ttf`/`.otf` (một face) và `.ttc` (lấy face đầu); WOFF/EOT
//! nén → `None` (coi như không biết — sẽ không giãn chữ thay vì vẽ ô vuông).

use std::path::Path;

/// Bề rộng (đơn vị em) của các ký tự khoảng cách. `None` = font không có glyph ấy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontMetrics {
    pub units_per_em: u16,
    /// U+200A HAIR SPACE — mảnh nhất, ưa dùng để giãn chữ.
    pub hair_space: Option<f64>,
    /// U+2009 THIN SPACE — dự phòng khi font không có hair space.
    pub thin_space: Option<f64>,
    /// U+0020 SPACE — chỉ dùng khi người dùng muốn giãn RỘNG.
    pub space: Option<f64>,
}

fn be16(d: &[u8], i: usize) -> Option<u16> {
    let b = d.get(i..i + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn be32(d: &[u8], i: usize) -> Option<u32> {
    let b = d.get(i..i + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Đọc metric của font từ bytes của file.
pub fn probe(data: &[u8]) -> Option<FontMetrics> {
    // Collection (.ttc): 'ttcf' | version u32 | numFonts u32 | offsets… — lấy face đầu.
    let base = if data.get(0..4)? == b"ttcf" { be32(data, 12)? as usize } else { 0 };
    let num_tables = be16(data, base + 4)? as usize;
    let (mut cmap, mut hhea, mut hmtx, mut head) = (None, None, None, None);
    for t in 0..num_tables {
        let rec = base + 12 + t * 16;
        let tag = data.get(rec..rec + 4)?;
        let off = be32(data, rec + 8)? as usize;
        let len = be32(data, rec + 12)? as usize;
        if off.checked_add(len).is_some_and(|end| end > data.len()) {
            continue; // bảng bị cắt cụt — bỏ qua, không panic.
        }
        match tag {
            b"cmap" => cmap = Some(off),
            b"hhea" => hhea = Some(off),
            b"hmtx" => hmtx = Some(off),
            b"head" => head = Some(off),
            _ => {}
        }
    }
    let (cmap, hhea, hmtx, head) = (cmap?, hhea?, hmtx?, head?);
    let upem = match be16(data, head + 18)? {
        0 => return None,
        u => u,
    };
    let num_hmetrics = (be16(data, hhea + 34)? as usize).max(1);
    let adv = |gid: u16| -> Option<f64> {
        // Glyph sau `numberOfHMetrics` dùng chung bề rộng của glyph cuối.
        let idx = (gid as usize).min(num_hmetrics - 1);
        Some(be16(data, hmtx + idx * 4)? as f64 / upem as f64)
    };
    let width_of = |cp: u32| -> Option<f64> {
        match cmap_lookup(data, cmap, cp) {
            Some(g) if g != 0 => adv(g),
            _ => None, // glyph 0 = .notdef → font không có ký tự này.
        }
    };
    Some(FontMetrics {
        units_per_em: upem,
        hair_space: width_of(0x200A),
        thin_space: width_of(0x2009),
        space: width_of(0x20),
    })
}

/// Đọc metric từ file font.
pub fn from_file(path: &Path) -> Option<FontMetrics> {
    probe(&std::fs::read(path).ok()?)
}

/// Tra `cmap`: mã Unicode → glyph id (0 = không có). Chọn subtable tốt nhất
/// (UCS-4 format 12 > BMP format 4) rồi tra theo segment.
fn cmap_lookup(data: &[u8], cmap: usize, cp: u32) -> Option<u16> {
    let n = be16(data, cmap + 2)? as usize;
    let mut best: Option<(u8, usize)> = None;
    for i in 0..n {
        let rec = cmap + 4 + i * 8;
        let (pid, eid) = (be16(data, rec)?, be16(data, rec + 2)?);
        let sub = cmap + be32(data, rec + 4)? as usize;
        let score = match (pid, eid, be16(data, sub)?) {
            (3, 10, 12) => 5,
            (0, _, 12) => 4,
            (3, 1, 4) => 3,
            (0, _, 4) => 2,
            (_, _, 4 | 12) => 1,
            _ => 0, // subtable symbol/legacy (Mac Roman…) — bỏ qua.
        };
        if score > 0 && best.map_or(true, |(s, _)| score > s) {
            best = Some((score, sub));
        }
    }
    let sub = best?.1;
    match be16(data, sub)? {
        4 => {
            let seg = be16(data, sub + 6)? as usize / 2;
            let end_off = sub + 14;
            let start_off = end_off + seg * 2 + 2;
            let delta_off = start_off + seg * 2;
            let range_off = delta_off + seg * 2;
            for i in 0..seg {
                let end = be16(data, end_off + i * 2)? as u32;
                if cp > end {
                    continue;
                }
                let start = be16(data, start_off + i * 2)? as u32;
                if cp < start {
                    return None;
                }
                let delta = be16(data, delta_off + i * 2)?;
                let range = be16(data, range_off + i * 2)? as usize;
                if range == 0 {
                    return Some((cp as u16).wrapping_add(delta));
                }
                let at = range_off + i * 2 + range + (cp - start) as usize * 2;
                let g = be16(data, at)?;
                return Some(if g == 0 { 0 } else { g.wrapping_add(delta) });
            }
            None
        }
        12 => {
            let groups = be32(data, sub + 12)? as usize;
            for g in 0..groups {
                let rec = sub + 16 + g * 12;
                let (s, e, gs) = (be32(data, rec)?, be32(data, rec + 4)?, be32(data, rec + 8)?);
                if cp >= s && cp <= e {
                    return Some((gs + (cp - s)) as u16);
                }
            }
            None
        }
        _ => None,
    }
}

impl FontMetrics {
    /// Ký tự giãn + số lần lặp để thêm ~`spacing_px` giữa hai chữ với cỡ
    /// `font_px`. Thử hair → thin → space, lấy ký tự HẸP nhất có bề rộng không
    /// vượt 1.5× mức giãn; không có ký tự nào phù hợp → `(None, 0)` (không
    /// giãn — an toàn hơn là vẽ ô vuông).
    pub fn spacer(&self, spacing_px: f64, font_px: f64) -> (Option<char>, usize) {
        if !(spacing_px > 0.0) || !(font_px > 0.0) {
            return (None, 0);
        }
        let candidates: [(Option<f64>, char); 3] = [
            (self.hair_space, '\u{200A}'),
            (self.thin_space, '\u{2009}'),
            (self.space, ' '),
        ];
        for (adv, ch) in candidates {
            let Some(w) = adv.map(|a| a * font_px).filter(|w| *w > 0.0) else { continue };
            if w > spacing_px * 1.5 {
                continue;
            }
            let reps = ((spacing_px / w).round() as usize).clamp(0, 8);
            return (if reps == 0 { None } else { Some(ch) }, reps);
        }
        (None, 0)
    }
}

#[cfg(test)]
#[allow(clippy::approx_constant)] // 0.318 = số đo font tổng hợp trong test, không phải hằng số toán
mod tests {
    use super::*;

    fn find_font(name: &str) -> Option<std::path::PathBuf> {
        fn rec(dir: &Path, name: &str, depth: u8) -> Option<std::path::PathBuf> {
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
        ["/usr/share/fonts", "/usr/local/share/fonts"]
            .iter()
            .find_map(|r| rec(Path::new(r), name, 4))
    }

    #[test]
    fn doc_metric_dejavu_dung_voi_fonttools() {
        // Đối chiếu số liệu đo bằng fontTools trên cùng máy (advance / unitsPerEm).
        let Some(p) = find_font("DejaVuSans.ttf") else { return };
        let m = from_file(&p).expect("đọc được DejaVu Sans");
        assert_eq!(m.units_per_em, 2048);
        assert!((m.space.unwrap() - 0.318).abs() < 0.002, "{m:?}");
        assert!((m.thin_space.unwrap() - 0.2).abs() < 0.002, "{m:?}");
        assert!((m.hair_space.unwrap() - 0.1).abs() < 0.002, "{m:?}");
    }

    #[test]
    fn font_thieu_hair_space_tra_none() {
        // Noto Serif SC không có U+2000–U+200B → hair/thin = None, không panic.
        let Some(p) = find_font("NotoSerifSC-Regular.ttf") else { return };
        let m = from_file(&p).expect("đọc được Noto Serif SC");
        assert_eq!(m.hair_space, None, "{m:?}");
        assert_eq!(m.thin_space, None, "{m:?}");
        assert!(m.space.unwrap() > 0.2, "{m:?}");
    }

    #[test]
    fn file_khong_phai_font() {
        assert_eq!(probe(b"khong phai font"), None);
        assert_eq!(probe(&[]), None);
        assert_eq!(probe(b"\0\x01\0\0"), None);
        assert_eq!(from_file(Path::new("/khong/ton/tai/font.ttf")), None);
    }

    #[test]
    fn chon_ky_tu_gian() {
        let full = FontMetrics { units_per_em: 2048, hair_space: Some(0.1), thin_space: Some(0.2), space: Some(0.318) };
        // 72px: hair = 7.2px. Giãn 7.2 → 1 hair; giãn 30 → 4 hair (28.8px).
        assert_eq!(full.spacer(7.2, 72.0), (Some('\u{200A}'), 1));
        assert_eq!(full.spacer(30.0, 72.0), (Some('\u{200A}'), 4));
        // Mức giãn nhỏ hơn nửa hair → không giãn (thà thiếu còn hơn vẽ ô vuông).
        assert_eq!(full.spacer(2.0, 72.0), (None, 0));
        assert_eq!(full.spacer(0.0, 72.0), (None, 0));
        assert_eq!(full.spacer(8.0, 0.0), (None, 0));
        // Giãn rất rộng → tối đa 8 ký tự.
        assert_eq!(full.spacer(500.0, 72.0).1, 8);
    }

    #[test]
    fn font_khong_hair_thi_dung_thin_hoac_space() {
        let thin_only = FontMetrics { units_per_em: 1000, hair_space: None, thin_space: Some(0.2), space: Some(0.256) };
        assert_eq!(thin_only.spacer(30.0, 72.0), (Some('\u{2009}'), 2));
        let space_only = FontMetrics { units_per_em: 1000, hair_space: None, thin_space: None, space: Some(0.25) };
        assert_eq!(space_only.spacer(30.0, 72.0), (Some(' '), 2));
        assert_eq!(space_only.spacer(8.0, 72.0), (None, 0));
        let none = FontMetrics { units_per_em: 1000, hair_space: None, thin_space: None, space: None };
        assert_eq!(none.spacer(30.0, 72.0), (None, 0));
    }

    /// SFNT tối giản: `cmap` format 4 (1 segment A–Z, glyph = cp + delta) +
    /// head/hhea/hmtx — để test nhánh BMP, đường đi phổ biến nhất của font Latin.
    fn synthetic_font() -> Vec<u8> {
        fn put16(v: &mut Vec<u8>, x: u16) {
            v.extend_from_slice(&x.to_be_bytes());
        }
        let mut sub = Vec::new();
        put16(&mut sub, 4); // format
        put16(&mut sub, 30); // length
        put16(&mut sub, 0); // language
        put16(&mut sub, 4); // segCountX2 (2 segment)
        put16(&mut sub, 4); // searchRange
        put16(&mut sub, 1); // entrySelector
        put16(&mut sub, 0); // rangeShift
        put16(&mut sub, 0x005A); // endCode[0]
        put16(&mut sub, 0xFFFF); // endCode[1]
        put16(&mut sub, 0); // reservedPad
        put16(&mut sub, 0x0041); // startCode[0]
        put16(&mut sub, 0xFFFF); // startCode[1]
        put16(&mut sub, 1u16.wrapping_sub(0x41)); // idDelta[0] → glyph của 'A' = 1
        put16(&mut sub, 1); // idDelta[1]
        put16(&mut sub, 0); // idRangeOffset[0]
        put16(&mut sub, 0); // idRangeOffset[1]
        let mut cmap = Vec::new();
        put16(&mut cmap, 0); // version
        put16(&mut cmap, 1); // numTables
        put16(&mut cmap, 3); // platformID = Windows
        put16(&mut cmap, 1); // encodingID = Unicode BMP
        cmap.extend_from_slice(&12u32.to_be_bytes()); // offset tới subtable
        cmap.extend_from_slice(&sub);

        let mut head = vec![0u8; 54];
        head[18..20].copy_from_slice(&1000u16.to_be_bytes()); // unitsPerEm
        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes()); // numberOfHMetrics
        let mut hmtx = Vec::new(); // 2 glyph: advance 500 / 300
        for adv in [500u16, 300u16] {
            hmtx.extend_from_slice(&adv.to_be_bytes());
            hmtx.extend_from_slice(&0i16.to_be_bytes());
        }

        let tables: [(&[u8; 4], &[u8]); 4] = [(b"cmap", &cmap), (b"head", &head), (b"hhea", &hhea), (b"hmtx", &hmtx)];
        let mut out = Vec::new();
        out.extend_from_slice(&0x00010000u32.to_be_bytes()); // sfntVersion
        put16(&mut out, tables.len() as u16);
        put16(&mut out, 16);
        put16(&mut out, 2);
        put16(&mut out, 0);
        let mut offset = 12 + 16 * tables.len();
        for (tag, body) in tables {
            out.extend_from_slice(tag);
            out.extend_from_slice(&[0; 4]); // checksum
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            offset += body.len();
        }
        for (_, body) in tables {
            out.extend_from_slice(body);
        }
        out
    }

    /// Lấy offset của một bảng trong table directory của file font tổng hợp.
    fn table_offset(data: &[u8], tag: &[u8; 4]) -> usize {
        let n = be16(data, 4).unwrap() as usize;
        (0..n)
            .map(|t| 12 + t * 16)
            .find(|&rec| &data[rec..rec + 4] == tag)
            .map(|rec| be32(data, rec + 8).unwrap() as usize)
            .unwrap()
    }

    #[test]
    fn cmap_format_4_va_bang_hmtx() {
        let data = synthetic_font();
        let cmap = table_offset(&data, b"cmap");
        assert_eq!(cmap_lookup(&data, cmap, b'A' as u32), Some(1));
        assert_eq!(cmap_lookup(&data, cmap, b'Z' as u32), Some(26));
        assert_eq!(cmap_lookup(&data, cmap, 0x200A), None);
        let m = probe(&data).expect("parse được font tổng hợp");
        assert_eq!(m.units_per_em, 1000);
        assert_eq!(m.hair_space, None, "font tổng hợp không có hair space");
        assert_eq!(m.space, None);
    }

    #[test]
    fn bang_bi_cut_cuc_khong_panic() {
        let data = synthetic_font();
        for cut in 1..data.len() {
            let _ = probe(&data[..cut]);
        }
        let mut broken = data.clone();
        broken.truncate(40); // thiếu bảng
        assert!(probe(&broken).is_none());
    }
}
