//! Phụ đề: đọc SRT → cue → sinh file ASS cho libass (burn-in ở pass cuối).
//!
//! Vì sao app tự sinh ASS thay vì `subtitles=…:force_style=…` trên file SRT:
//! libass mặc định dựng SRT trong "sân khấu" 384×288 rồi co giãn theo chiều
//! CAO khung — cùng một `Fontsize` cho chữ ~90px trên 16:9 nhưng ~160px trên
//! 9:16 (đo thật trên ffmpeg 7.1). File ASS do app sinh đặt `PlayResX/Y`
//! đúng bằng khung đích nên cỡ chữ, lề, viền đều tính bằng px thật và ĐỒNG
//! NHẤT giữa mọi tỉ lệ khung (có unit test + E2E đo pixel).
//!
//! Mọi hàm ở đây là thuần (không I/O) để unit test được.

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Cue + parse SRT
// ---------------------------------------------------------------------------

/// Một dòng phụ đề. `text` là văn bản SRT đã làm sạch thẻ HTML (`<i>`… bị bỏ)
/// và đã escape `{`/`}`; sau [`cues_to_ass_text`] thì ở dạng ASS (thẻ đổi thành
/// override tag, xuống dòng thành `\N`).
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// "12.345" → "00:00:12,345" (SRT).
/// Chỉ unit test dùng hiện tại — giữ làm tài liệu hành vi + dự kiến nối vào UI bản sau.
#[allow(dead_code)]
pub fn srt_time(sec: f64) -> String {
    let ms = (sec.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// "12.345" → "0:00:12.35" (ASS — phần trăm giây, 2 chữ số).
pub fn ass_time(sec: f64) -> String {
    let cs = (sec.max(0.0) * 100.0).round() as u64;
    format!(
        "{}:{:02}:{:02}.{:02}",
        cs / 360_000,
        cs / 6_000 % 60,
        cs / 100 % 60,
        cs % 100
    )
}

/// "#RRGGBB" → 6 hex theo thứ tự BGR của ASS (`BBGGRR`); màu lạ → trắng.
fn ass_bgr(color: &str) -> String {
    match crate::textover::parse_hex_color(color) {
        Some((r, g, b)) => format!("{b:02X}{g:02X}{r:02X}"),
        None => "FFFFFF".to_string(),
    }
}

/// Màu cho dòng `Style:` của ASS: `&H00BBGGRR` (alpha 00 = đục).
pub fn ass_style_color(color: &str) -> String {
    format!("&H00{}", ass_bgr(color))
}

/// Màu cho override tag inline: `\c&HBBGGRR&`.
fn ass_inline_color(color: &str) -> String {
    format!("&H{}&", ass_bgr(color))
}

/// "HH:MM:SS,mmm" / "HH:MM:SS.mmm" / "MM:SS.mmm" → giây.
fn parse_stamp(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    let mut parts = s.split(':');
    let count = s.split(':').count();
    if count == 3 {
        let h: f64 = parts.next()?.trim().parse().ok()?;
        let m: f64 = parts.next()?.trim().parse().ok()?;
        let sec: f64 = parts.next()?.trim().parse().ok()?;
        if !(0.0..60.0).contains(&m) || !(0.0..60.0).contains(&sec) {
            return None;
        }
        Some(h * 3600.0 + m * 60.0 + sec)
    } else if count == 2 {
        let m: f64 = parts.next()?.trim().parse().ok()?;
        let sec: f64 = parts.next()?.trim().parse().ok()?;
        if !(0.0..60.0).contains(&m) || !(0.0..60.0).contains(&sec) {
            return None;
        }
        Some(m * 60.0 + sec)
    } else {
        None
    }
}

/// Lấy giá trị `color="…"`/`color=…` trong thẻ `<font>`; trả `#RRGGBB`
/// (tên màu cơ bản đổi sang hex) hoặc `None` nếu không nhận dạng được.
fn tag_color(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let pos = lower.find("color")?;
    let rest = lower[pos + 5..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let value: String = if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next()?.to_string()
    } else {
        rest.split_whitespace().next()?.to_string()
    };
    let v = value.trim();
    if let Some(hex) = v.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(format!("#{hex}"));
        }
        if hex.len() == 3 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let full: String = hex.chars().flat_map(|c| [c, c]).collect();
            return Some(format!("#{full}"));
        }
        return None;
    }
    let named = match v {
        "red" => "#FF0000",
        "green" => "#008000",
        "blue" => "#0000FF",
        "yellow" => "#FFFF00",
        "white" => "#FFFFFF",
        "black" => "#000000",
        "orange" => "#FFA500",
        "purple" => "#800080",
        "cyan" => "#00FFFF",
        "magenta" => "#FF00FF",
        "gray" | "grey" => "#808080",
        _ => return None,
    };
    Some(named.to_string())
}

/// Đổi 1 dòng SRT sang text ASS: `<i>/<b>/<u>/<font color>` thành override
/// tag, thẻ HTML khác bị bỏ, `{` `}` `\` trong văn bản được escape để không bị
/// hiểu nhầm là tag. `primary` = màu chính của style (dùng khi đóng `</font>`).
pub fn srt_line_to_ass(line: &str, primary: &str) -> String {
    // 1. Tách theo thẻ HTML trước (trên văn bản gốc), 2. escape từng mảnh text.
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len() + 8);
    let mut i = 0usize;
    let push_text = |out: &mut String, raw: &[u8]| {
        for c in String::from_utf8_lossy(raw).chars() {
            match c {
                '{' => out.push_str("\\{"),
                '}' => out.push_str("\\}"),
                '\\' => out.push_str("\\\\"),
                _ => out.push(c),
            }
        }
    };
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Tìm `>` đóng (không tính `>` nằm trong giá trị giữa nháy kép).
            let mut j = i + 1;
            let mut in_quote = false;
            while j < bytes.len() && (bytes[j] != b'>' || in_quote) {
                if bytes[j] == b'"' {
                    in_quote = !in_quote;
                }
                j += 1;
            }
            if j < bytes.len() {
                let tag = String::from_utf8_lossy(&bytes[i + 1..j]).trim().to_string();
                let lower = tag.to_ascii_lowercase();
                let replacement: String = if lower == "i" {
                    "{\\i1}".into()
                } else if lower == "/i" {
                    "{\\i0}".into()
                } else if lower == "b" {
                    "{\\b1}".into()
                } else if lower == "/b" {
                    "{\\b0}".into()
                } else if lower == "u" {
                    "{\\u1}".into()
                } else if lower == "/u" {
                    "{\\u0}".into()
                } else if lower == "/font" {
                    format!("{{\\c{}}}", ass_inline_color(primary))
                } else if lower.starts_with("font") {
                    match tag_color(&tag) {
                        Some(c) => format!("{{\\c{}}}", ass_inline_color(&c)),
                        None => String::new(),
                    }
                } else {
                    String::new()
                };
                out.push_str(&replacement);
                i = j + 1;
                continue;
            }
        }
        // Ký tự thường: gom tới khi gặp `<` kế tiếp.
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'<' {
            j += 1;
        }
        push_text(&mut out, &bytes[i..j]);
        i = j;
    }
    out
}

/// Đọc nội dung file SRT → danh sách cue (text thô đã làm sạch thẻ HTML).
/// Chấp nhận BOM, CRLF, dấu phẩy HOẶC dấu chấm ngăn mili-giây, block dính
/// nhau (thiếu dòng trống giữa hai phụ đề). Cue có end ≤ start bị bỏ.
/// Không có cue nào hợp lệ → Err.
pub fn parse_srt(content: &str) -> Result<Vec<Cue>, String> {
    let content = content.trim_start_matches('\u{feff}');
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut cues: Vec<Cue> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i].trim();
        let Some(arrow) = line.find("-->") else {
            // Dòng số thứ tự / dòng trống / rác đứng trước mốc thời gian.
            i += 1;
            continue;
        };
        let (a, b) = line.split_at(arrow);
        // Sau mốc kết thúc có thể có toạ độ "X1:.. X2:.." → chỉ lấy token đầu.
        let end_tok = b[3..].trim().split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (parse_stamp(a), parse_stamp(end_tok)) else {
            i += 1;
            continue;
        };
        i += 1;
        // Text = các dòng tiếp theo đến dòng trống HOẶC mốc thời gian kế tiếp.
        let mut text: Vec<String> = Vec::new();
        while i < lines.len() {
            let t = lines[i].trim();
            if t.is_empty() || t.contains("-->") {
                break;
            }
            // Dòng chỉ toàn số đứng ngay trước mốc thời gian = chỉ số của cue SAU.
            let is_next_index = t.chars().all(|c| c.is_ascii_digit())
                && lines[i + 1..]
                    .iter()
                    .find(|l| !l.trim().is_empty())
                    .is_some_and(|l| l.contains("-->"));
            if is_next_index {
                break;
            }
            text.push(srt_line_to_ass(t, "#FFFFFF"));
            i += 1;
        }
        let joined = text.join("\n");
        if end > start && !joined.trim().is_empty() {
            cues.push(Cue { start, end, text: joined });
        }
    }
    if cues.is_empty() {
        return Err(
            "File SRT không có dòng phụ đề hợp lệ (cần dòng thời gian dạng \
             00:00:01,000 --> 00:00:03,000)."
                .into(),
        );
    }
    Ok(cues)
}

/// Đổi cue từ dạng SRT (text đã escape) sang ASS: xuống dòng mềm → `\N`.
/// (Thẻ HTML đã được đổi ngay khi parse.)
/// Chỉ unit test dùng hiện tại — giữ làm tài liệu hành vi + dự kiến nối vào UI bản sau.
#[allow(dead_code)]
pub fn cues_to_ass_text(cues: &mut [Cue]) {
    for c in cues.iter_mut() {
        c.text = c.text.replace('\n', "\\N");
    }
}

/// Dời cue về timeline của một PHẦN video [start, end) (chế độ chia phần):
/// cue nằm ngoài phần bị bỏ, cue vắt lề bị cắt gọn, mốc thời gian trừ đi
/// `start`. Giữ nguyên thứ tự.
/// Chỉ unit test dùng hiện tại — giữ làm tài liệu hành vi + dự kiến nối vào UI bản sau.
#[allow(dead_code)]
pub fn shift_and_clip(cues: &[Cue], start: f64, end: f64) -> Vec<Cue> {
    cues.iter()
        .filter_map(|c| {
            let s = c.start.max(start);
            let e = c.end.min(end);
            (e - s > 0.05).then(|| Cue { start: s - start, end: e - start, text: c.text.clone() })
        })
        .collect()
}

/// Chia cue DÀI thành các cue ngắn dễ đọc (dành cho phụ đề tự động — ASR trả
/// cả đoạn nói dài). Ngắt theo câu (`. ! ? …`), không đủ thì theo dấu phẩy,
/// không đủ thì theo khoảng trắng; thời gian chia TƯƠNG ĐỐI theo số ký tự.
pub fn split_long_cues(cues: Vec<Cue>, max_chars: usize, max_dur: f64) -> Vec<Cue> {
    let max_chars = max_chars.max(8);
    let mut out: Vec<Cue> = Vec::with_capacity(cues.len());
    for c in cues {
        let chars = c.text.chars().count();
        let dur = c.end - c.start;
        if chars <= max_chars && dur <= max_dur {
            out.push(c);
            continue;
        }
        let pieces = split_text_balanced(&c.text, max_chars);
        if pieces.is_empty() {
            out.push(c);
            continue;
        }
        let weights: Vec<usize> = pieces.iter().map(|p| p.text_units().max(1)).collect();
        let total: usize = weights.iter().sum();
        let span = c.end - c.start;
        let mut cursor = c.start;
        for (idx, p) in pieces.iter().enumerate() {
            let is_last = idx + 1 == pieces.len();
            // Phần cuối nhận phần dư để tổng khít đúng cue gốc.
            let len = if is_last { c.end - cursor } else { span * weights[idx] as f64 / total as f64 };
            out.push(Cue { start: cursor, end: cursor + len, text: p.clone() });
            cursor += len;
        }
    }
    out
}

/// Số "đơn vị ký tự" đã tính chữ rộng (CJK ≈ 2) — dùng để chia thời gian.
trait TextUnits {
    fn text_units(&self) -> usize;
}

impl TextUnits for str {
    fn text_units(&self) -> usize {
        self.chars().map(|c| if char_width_em(c) >= 0.9 { 2 } else { 1 }).sum()
    }
}

/// Ngắt một đoạn text thành các mảnh ≤ `max_chars` đơn vị. Thứ tự ưu tiên
/// ranh giới: xuống dòng cứng `\N` (không bao giờ gộp qua) → cuối câu (`. ! ?
/// …`) → vế (, ; :) → khoảng trắng; từ đơn lẻ vẫn dài hơn giới hạn thì cắt cứng.
/// Ghép các mảnh lại bằng một dấu cách cho lại nguyên văn bản (trừ khoảng
/// trắng thừa ở hai đầu mảnh).
pub fn split_text_balanced(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(8);
    let chars: Vec<char> = text.chars().collect();
    let mut pieces: Vec<String> = Vec::new();
    // `block` = các đoạn nằm giữa hai lần xuống dòng cứng.
    let mut block: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut k = 0usize;
    while k < chars.len() {
        let c = chars[k];
        cur.push(c);
        if c == '\\' && chars.get(k + 1) == Some(&'N') {
            cur.push('N');
            k += 2;
            block.push(std::mem::take(&mut cur));
            pieces.extend(split_block(&block, max_chars));
            block.clear();
            continue;
        }
        // Cuối câu: dấu kết câu KHÔNG nằm trong dấu ngoặc/multi-punctuation.
        let is_end = matches!(c, '.' | '!' | '?' | '…')
            && !matches!(chars.get(k + 1), Some('.') | Some('!') | Some('?') | Some(')') | Some(']'));
        if is_end {
            block.push(std::mem::take(&mut cur));
        }
        k += 1;
    }
    if !cur.is_empty() {
        block.push(cur);
    }
    pieces.extend(split_block(&block, max_chars));
    pieces
}

/// Tách token kết thúc tại mỗi ký tự trong `seps` (ký tự ngăn giữ nguyên ở
/// cuối token) — không làm mất ký tự nào.
fn split_at(s: &str, seps: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        cur.push(c);
        if seps.contains(&c) {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Gộp các đoạn của một block thành mảnh ≤ max_chars: tách nhỏ theo vế rồi
/// theo từ, gom các từ liền kề tới khi sắp vượt giới hạn, từ đơn lẻ quá dài
/// thì cắt cứng.
fn split_block(segs: &[String], max_chars: usize) -> Vec<String> {
    let tokens: Vec<String> = segs
        .iter()
        .flat_map(|s| split_at(s, &[',', ';', ':']))
        .flat_map(|s| split_at(&s, &[' ']))
        .collect();
    let mut merged: Vec<String> = Vec::new();
    let mut buf = String::new();
    for tok in tokens {
        // Token giữ nguyên khoảng trắng gốc để ghép lại không sai chính tả.
        let used = buf.trim_end().text_units();
        let add = tok.text_units();
        if !buf.trim().is_empty() && used + add > max_chars {
            merged.push(buf.trim().to_string());
            buf.clear();
        }
        buf.push_str(&tok);
    }
    if !buf.trim().is_empty() {
        merged.push(buf.trim().to_string());
    }
    // Mảnh vẫn quá dài (một từ không có dấu ngăn) → cắt cứng theo đơn vị.
    let mut out = Vec::new();
    for p in merged {
        if p.text_units() <= max_chars {
            out.push(p);
            continue;
        }
        let mut acc = String::new();
        let mut used = 0usize;
        for c in p.chars() {
            let w = if char_width_em(c) >= 0.9 { 2 } else { 1 };
            if used + w > max_chars && !acc.is_empty() {
                out.push(std::mem::take(&mut acc));
                used = 0;
            }
            acc.push(c);
            used += w;
        }
        if !acc.is_empty() {
            out.push(acc);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Ước lượng xuống dòng (để chữ overlay né vùng phụ đề)
// ---------------------------------------------------------------------------

/// Bề rộng ước lượng của một ký tự theo em (đo trên DejaVu Sans: Latin trung
/// bình ≈ 0.56 em; chữ CJK/fullwidth ≈ 1 em; dấu kết hợp = 0).
pub fn char_width_em(c: char) -> f64 {
    let u = c as u32;
    if (0x0300..=0x036F).contains(&u) || u == 0x200D {
        return 0.0;
    }
    let wide = (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE4F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1FAFF).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u);
    if wide {
        return 1.0;
    }
    match c {
        // U+200A hair space = ký tự "mỏng" dùng tạo giãn chữ (≈0.1em trên DejaVu).
        ' ' | '\u{200B}' => 0.32,
        '\u{200A}' => 0.1,
        '\u{2009}' => 0.2,
        'i' | 'l' | 'j' | 't' | 'r' | 'f' | 'I' | '.' | ',' | ';' | ':' | '\'' | '!' | '|' | '(' | ')' => 0.34,
        'm' | 'w' | 'M' | 'W' => 0.86,
        'A'..='Z' => 0.68,
        '0'..='9' => 0.64,
        _ => 0.56,
    }
}

/// Bỏ các override tag `{\…}` của ASS khi đo bề rộng.
/// Chỉ unit test dùng hiện tại — giữ làm tài liệu hành vi + dự kiến nối vào UI bản sau.
#[allow(dead_code)]
fn strip_override_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            escaped = false;
            out.push(c);
            continue;
        }
        if c == '\\' {
            escaped = true;
            out.push(c);
            continue;
        }
        match c {
            '{' if !in_tag => in_tag = true,
            '}' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Ước lượng số dòng cần để hiển thị `text` (ASS) trong `usable_width` px với
/// cỡ `em_px` (px của một em). Mỗi `\N` là một dòng cứng.
/// Chỉ unit test dùng hiện tại — giữ làm tài liệu hành vi + dự kiến nối vào UI bản sau.
#[allow(dead_code)]
pub fn estimate_lines(text: &str, em_px: f64, usable_width: u32) -> usize {
    if em_px <= 0.0 || usable_width == 0 {
        return 1;
    }
    let plain = strip_override_tags(text);
    let mut lines = 0usize;
    for para in plain.split("\\N") {
        let width_px: f64 = para.chars().map(char_width_em).sum::<f64>() * em_px;
        lines += (width_px / usable_width as f64).ceil().max(1.0) as usize;
    }
    lines.max(1)
}

// ---------------------------------------------------------------------------
// Sinh ASS
// ---------------------------------------------------------------------------

/// Kiểu phụ đề — quyết định viền/nền đổ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubtitleStyleKind {
    /// Trắng, viền đen đậm (kiểu YouTube/truyền hình).
    #[default]
    Classic,
    /// Nền đen mờ bao quanh chữ, không viền.
    Box,
    /// Viền màu (màu phụ), chữ trắng — kiểu video ngắn.
    Neon,
    /// Không viền, chỉ bóng đổ nhẹ — hợp video nền sáng.
    Soft,
}

impl SubtitleStyleKind {
    /// Tên gửi từ UI ("classic" | "box" | "neon" | "soft").
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "box" => Self::Box,
            "neon" => Self::Neon,
            "soft" => Self::Soft,
            _ => Self::Classic,
        }
    }
}

/// Tham số 1 style ASS — đơn vị px của khung (PlayRes = khung).
#[derive(Debug, Clone)]
pub struct AssStyle {
    pub fontname: String,
    /// `Fontsize` của ASS (px khung).
    pub fontsize: u32,
    /// `&H00BBGGRR`.
    pub primary: String,
    /// `&H00BBGGRR` — màu viền (kiểu Neon đổi sang màu neon).
    pub outline_color: String,
    pub bold: bool,
    /// 2 = căn dưới, 8 = trên, 5 = giữa.
    pub alignment: u8,
    pub margin_l: u32,
    pub margin_r: u32,
    pub margin_v: u32,
    pub kind: SubtitleStyleKind,
    pub play_res_x: u32,
    pub play_res_y: u32,
}

impl AssStyle {
    /// Độ dày viền (px): ≈ 7% cỡ chữ với Classic/Neon; kiểu Box dùng Outline
    /// làm độ đệm của nền; Soft không viền.
    pub fn outline(&self) -> u32 {
        match self.kind {
            SubtitleStyleKind::Box => ((self.fontsize as f64 * 0.14).round() as u32).max(4),
            SubtitleStyleKind::Soft => 0,
            _ => ((self.fontsize as f64 * 0.075).round() as u32).max(2),
        }
    }

    /// Độ dày bóng đổ (px): chỉ kiểu Soft có bóng.
    pub fn shadow(&self) -> u32 {
        match self.kind {
            SubtitleStyleKind::Soft => ((self.fontsize as f64 * 0.07).round() as u32).max(2),
            _ => 0,
        }
    }

    /// 1 = viền + bóng đổ, 3 = nền đục/mờ sau chữ.
    fn border_style(&self) -> u8 {
        match self.kind {
            SubtitleStyleKind::Box => 3,
            _ => 1,
        }
    }

    /// Màu nền/bóng (`&HAABBGGRR`, alpha 00 = đục, FF = trong suốt).
    fn back_colour(&self) -> String {
        match self.kind {
            SubtitleStyleKind::Box => "&H40000000".into(),
            SubtitleStyleKind::Soft => "&H80000000".into(),
            _ => "&H64000000".into(),
        }
    }
}

/// Tên font không được chứa dấu phẩy/ký tự điều khiển (format của ASS).
fn clean_fontname(name: &str) -> String {
    name.chars().filter(|c| *c != ',' && !c.is_control()).collect::<String>().trim().to_string()
}

/// Sinh nội dung file ASS hoàn chỉnh (PlayRes = khung) cho danh sách cue.
/// Cue đã ở dạng ASS text (qua [`parse_srt`] + [`cues_to_ass_text`]).
pub fn render_ass(cues: &[Cue], style: &AssStyle) -> String {
    let mut s = String::new();
    s.push_str("[Script Info]\n; Radio Video Studio\nScriptType: v4.00+\n");
    let _ = writeln!(s, "PlayResX: {}\nPlayResY: {}", style.play_res_x, style.play_res_y);
    s.push_str("ScaledBorderAndShadow: yes\nWrapStyle: 0\n\n");
    s.push_str(
        "[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, \
         BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, \
         Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n",
    );
    let _ = writeln!(
        s,
        "Style: RVM,{},{},{},&H000000FF,{},{},{},0,0,0,100,100,0,0,{},{},{},{},{},{},{},1",
        clean_fontname(&style.fontname),
        style.fontsize,
        style.primary,
        style.outline_color,
        style.back_colour(),
        if style.bold { -1 } else { 0 },
        style.border_style(),
        style.outline(),
        style.shadow(),
        style.alignment,
        style.margin_l,
        style.margin_r,
        style.margin_v,
    );
    s.push_str("\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
    for c in cues {
        // Text không được chứa ngắt dòng thật (mỗi Dialogue là 1 dòng).
        let text: String = c.text.chars().filter(|ch| *ch != '\n' && *ch != '\r').collect();
        let _ = writeln!(s, "Dialogue: 0,{},{},RVM,,0,0,0,,{}", ass_time(c.start), ass_time(c.end), text);
    }
    s
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formats() {
        assert_eq!(srt_time(12.345), "00:00:12,345");
        assert_eq!(srt_time(3725.0), "01:02:05,000");
        assert_eq!(ass_time(12.345), "0:00:12.35");
        assert_eq!(ass_time(0.0), "0:00:00.00");
        assert_eq!(ass_time(3725.5), "1:02:05.50");
    }

    #[test]
    fn colors_bgr() {
        assert_eq!(ass_style_color("#FF8040"), "&H004080FF");
        assert_eq!(ass_style_color("red"), "&H00FFFFFF");
        assert_eq!(ass_inline_color("#FF8040"), "&H4080FF&");
    }

    #[test]
    fn parse_stamp_variants() {
        assert_eq!(parse_stamp("00:00:01,250"), Some(1.25));
        assert_eq!(parse_stamp("00:00:01.250"), Some(1.25));
        assert_eq!(parse_stamp("01:02:03,000"), Some(3723.0));
        assert_eq!(parse_stamp("01:03.5"), Some(63.5));
        assert_eq!(parse_stamp("abc"), None);
        assert_eq!(parse_stamp("00:99:00,000"), None);
    }

    #[test]
    fn parse_srt_basic() {
        let srt = "1\n00:00:01,000 --> 00:00:03,500\nXin chào\ncác bạn\n\n2\n00:00:04,000 --> 00:00:05,000\nTạm biệt\n";
        let cues = parse_srt(srt).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0], Cue { start: 1.0, end: 3.5, text: "Xin chào\ncác bạn".into() });
        assert_eq!(cues[1].text, "Tạm biệt");
    }

    #[test]
    fn parse_srt_bom_crlf_blocks_dinh_nhau() {
        let srt = "\u{feff}1\r\n00:00:01.000 --> 00:00:02.000\r\nMột\r\n2\r\n00:00:03.000 --> 00:00:04.000\r\nHai";
        let cues = parse_srt(srt).unwrap();
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert_eq!(cues[0].text, "Một");
        assert_eq!(cues[1].start, 3.0);
    }

    #[test]
    fn parse_srt_bo_cue_lech_toa_do_va_cue_sai() {
        let srt = "1\n00:00:05,000 --> 00:00:03,000\nNgược thời gian\n\n2\n00:00:04,000 --> 00:00:06,000 X1:10 X2:99\nCó toạ độ\n";
        let cues = parse_srt(srt).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Có toạ độ");
    }

    #[test]
    fn parse_srt_khong_cue_loi() {
        assert!(parse_srt("").is_err());
        assert!(parse_srt("chỉ có chữ, không có mốc thời gian").is_err());
    }

    #[test]
    fn tags_html_sang_ass() {
        assert_eq!(srt_line_to_ass("<i>nghiêng</i>", "#FFFFFF"), "{\\i1}nghiêng{\\i0}");
        assert_eq!(srt_line_to_ass("<b>đậm</b>", "#FFFFFF"), "{\\b1}đậm{\\b0}");
        assert_eq!(srt_line_to_ass("<font color=\"#FF0000\">đỏ</font>", "#FFFFFF"), "{\\c&H0000FF&}đỏ{\\c&HFFFFFF&}");
        assert_eq!(srt_line_to_ass("<font color=\"yellow\">vàng</font>", "#00FF00"), "{\\c&H00FFFF&}vàng{\\c&H00FF00&}");
        // Thẻ lạ bị bỏ, ngoặc và backslash được escape.
        assert_eq!(srt_line_to_ass("<sub>{a}\\b</sub>", "#FFFFFF"), "\\{a\\}\\\\b");
    }

    #[test]
    fn shift_and_clip_cat_theo_phan() {
        let cues = vec![
            Cue { start: 0.0, end: 2.0, text: "A".into() },
            Cue { start: 8.0, end: 12.0, text: "B".into() },
            Cue { start: 15.0, end: 18.0, text: "C".into() },
        ];
        let part = shift_and_clip(&cues, 10.0, 16.0);
        assert_eq!(part.len(), 2);
        assert_eq!((part[0].start, part[0].end), (0.0, 2.0)); // B cắt còn 10..12
        assert_eq!((part[1].start, part[1].end), (5.0, 6.0)); // C cắt còn 15..16
    }

    #[test]
    fn split_long_cues_theo_cau_va_phan_thoi_gian() {
        let long = "Câu thứ nhất khá là dài đó nha. Câu thứ hai cũng dài không kém đâu. Câu ba.";
        let cues = vec![Cue { start: 10.0, end: 20.0, text: long.into() }];
        let out = split_long_cues(cues, 32, 7.0);
        assert!(out.len() >= 2, "{out:?}");
        assert_eq!(out[0].start, 10.0);
        assert_eq!(out.last().unwrap().end, 20.0);
        // Thời gian liền mạch, không chồng lấn.
        for w in out.windows(2) {
            assert!((w[0].end - w[1].start).abs() < 1e-9, "{out:?}");
        }
        // Mọi mảnh đều vừa max_chars (đơn vị) — hoặc là một từ dài không tách được.
        for c in &out {
            assert!(c.text.chars().count() <= 34, "{c:?}");
        }
        // Không mất ký tự nào.
        let joined: String = out.iter().map(|c| c.text.trim()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined, long);
    }

    #[test]
    fn split_long_cues_giu_ngan_cue_ngan() {
        let cues = vec![Cue { start: 0.0, end: 2.0, text: "Ngắn".into() }];
        let out = split_long_cues(cues, 42, 7.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Ngắn");
    }

    #[test]
    fn split_text_balanced_tu_dai_khong_khoang_trang() {
        let pieces = split_text_balanced("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 12);
        assert!(pieces.iter().all(|p| p.chars().count() <= 12), "{pieces:?}");
        assert_eq!(pieces.concat().chars().count(), 47);
    }

    #[test]
    fn estimate_lines_co_ban() {
        // 1 dòng ngắn trong 972px, chữ 45px em: "Xin chào" ≈ 4.5em × 45 ≈ 200px → 1 dòng.
        assert_eq!(estimate_lines("Xin chào", 45.0, 972), 1);
        // Xuống dòng cứng → đúng số dòng.
        assert_eq!(estimate_lines("a\\N\\ib", 45.0, 972), 2);
        // Cue rất dài (~60 ký tự ≈ 33em × 45 = 1500px > 972) → 2 dòng.
        let long = "Đây là một câu phụ đề khá dài để buộc libass phải ngắt thành nhiều dòng hiển thị";
        assert!(estimate_lines(long, 45.0, 972) >= 2);
        // Tag override không tính bề rộng.
        assert_eq!(estimate_lines("{\\i1\\c&HFF0000&}ab", 45.0, 972), estimate_lines("ab", 45.0, 972));
    }

    #[test]
    fn render_ass_style_line_dung_format() {
        let style = AssStyle {
            fontname: "DejaVu, Sans".into(),
            fontsize: 48,
            primary: ass_style_color("#FFFF00"),
            outline_color: ass_style_color("#000000"),
            bold: true,
            alignment: 2,
            margin_l: 54,
            margin_r: 54,
            margin_v: 115,
            kind: SubtitleStyleKind::Classic,
            play_res_x: 1080,
            play_res_y: 1920,
        };
        let cues = vec![Cue { start: 1.0, end: 2.5, text: "{\\i1}Xin chào{\\i0}\\Nbạn".into() }];
        let ass = render_ass(&cues, &style);
        assert!(ass.contains("PlayResX: 1080\nPlayResY: 1920"), "{ass}");
        // Dấu phẩy trong tên font bị lọc (không làm lệch cột).
        assert!(ass.contains("Style: RVM,DejaVu Sans,48,&H0000FFFF,&H000000FF,&H00000000,&H64000000,-1,0,0,0,100,100,0,0,1,4,0,2,54,54,115,1"), "{ass}");
        assert!(ass.contains("Dialogue: 0,0:00:01.00,0:00:02.50,RVM,,0,0,0,,{\\i1}Xin chào{\\i0}\\Nbạn"), "{ass}");
        // Mỗi Dialogue đúng 1 dòng.
        assert_eq!(ass.matches("Dialogue:").count(), 1);
    }

    #[test]
    fn ass_style_kinds() {
        let mk = |kind| AssStyle {
            fontname: "Arial".into(),
            fontsize: 48,
            primary: "&H00FFFFFF".into(),
            outline_color: "&H00000000".into(),
            bold: true,
            alignment: 2,
            margin_l: 0,
            margin_r: 0,
            margin_v: 0,
            kind,
            play_res_x: 1080,
            play_res_y: 1920,
        };
        assert_eq!(mk(SubtitleStyleKind::Classic).border_style(), 1);
        assert_eq!(mk(SubtitleStyleKind::Classic).outline(), 4);
        assert_eq!(mk(SubtitleStyleKind::Neon).border_style(), 1);
        assert_eq!(mk(SubtitleStyleKind::Box).border_style(), 3);
        assert_eq!(mk(SubtitleStyleKind::Box).outline(), 7);
        assert_eq!(mk(SubtitleStyleKind::Soft).outline(), 0);
        assert!(mk(SubtitleStyleKind::Soft).shadow() >= 2);
    }

    #[test]
    fn style_kind_parse() {
        assert_eq!(SubtitleStyleKind::parse("box"), SubtitleStyleKind::Box);
        assert_eq!(SubtitleStyleKind::parse("NEON"), SubtitleStyleKind::Neon);
        assert_eq!(SubtitleStyleKind::parse("soft"), SubtitleStyleKind::Soft);
        assert_eq!(SubtitleStyleKind::parse("gì-đó"), SubtitleStyleKind::Classic);
    }
}
