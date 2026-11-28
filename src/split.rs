//! Chia phần: khi audio dài hơn `max_duration_sec` của preset, thay vì cắt cụt
//! thì chia thành nhiều video riêng, mỗi video là một job con.
//!
//! Phần tính toán là hàm thuần (dễ test); phần gọi ffmpeg chỉ có
//! [`detect_silences`] (đoạn im lặng để chọn điểm cắt "mềm").

use std::path::Path;

use anyhow::Context;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Không phần nào ngắn hơn mức này; phần quá ngắn được gộp vào phần trước
/// (khi gộp vẫn không vượt giới hạn).
pub const MIN_PART_SEC: f64 = 5.0;
/// Chỉ dời điểm cắt tới đoạn im lặng trong khoảng ±15% độ dài phần quanh mốc.
pub const SILENCE_WINDOW_FRAC: f64 = 0.15;
/// Fade-in ở đầu mỗi phần (trừ phần đầu) để không bị "bụp".
pub const CUT_FADE_IN_SEC: f64 = 0.15;
/// Fade-out ở cuối mỗi phần (trừ phần cuối) để không bị "bụp".
pub const CUT_FADE_OUT_SEC: f64 = 0.3;
/// Bộ lọc dò im lặng (theo đề bài: -35dB, tối thiểu 0.4s).
pub const SILENCE_FILTER: &str = "silencedetect=noise=-35dB:d=0.4";

const EPS: f64 = 1e-6;

/// Một phần của audio: `[start, end)` tính bằng giây trên audio gốc.
pub type Span = (f64, f64);

/// Chia `audio_duration` thành các phần, mỗi phần ≤ `max_sec`.
///
/// 1. Số phần tối thiểu `n = ceil(duration / max_sec)`; mốc mục tiêu của điểm
///    cắt thứ `k` là `k * duration / n` (các phần đều nhau, phần cuối không
///    bị ngắn).
/// 2. Mỗi mốc được dời tới đoạn im lặng gần nhất trong ±15% độ dài phần
///    (`silences`: các khoảng `(start, end)`), cắt giữa đoạn im lặng. Không có
///    đoạn im lặng (hoặc dời tới đó sẽ làm phần khác vượt giới hạn / quá
///    ngắn) thì cắt đúng mốc.
/// 3. Không phần nào vượt `max_sec`; phần dưới 5s được gộp vào phần trước khi
///    gộp xong vẫn không vượt `max_sec`.
///
/// Trả về `[(start, end), …]` phủ kín `[0, duration]`; audio không vượt giới
/// hạn (hoặc `max_sec` không hợp lệ) → một phần duy nhất; duration không hợp
/// lệ → rỗng.
pub fn plan_splits(audio_duration: f64, max_sec: f64, silences: &[Span]) -> Vec<Span> {
    // NaN (ffprobe trả duration lạ) cũng coi như không hợp lệ.
    if !audio_duration.is_finite() || audio_duration <= 0.0 {
        return Vec::new();
    }
    if !max_sec.is_finite() || max_sec <= 0.0 || audio_duration <= max_sec + EPS {
        return vec![(0.0, audio_duration)];
    }

    let n = ((audio_duration / max_sec) - 1e-9).ceil().max(1.0) as usize;
    let part = audio_duration / n as f64;
    let window = part * SILENCE_WINDOW_FRAC;

    let mut cuts: Vec<f64> = vec![0.0];
    for k in 1..n {
        let target = part * k as f64;
        cuts.push(pick_cut(
            target,
            cuts[k - 1],
            part * (k + 1) as f64,
            audio_duration,
            max_sec,
            window,
            part,
            silences,
        ));
    }
    cuts.push(audio_duration);
    merge_short_parts(&cuts, max_sec)
}

/// Chọn điểm cắt thứ `k` quanh `target` (điểm trước là `prev`, mốc kế tiếp là
/// `next_target`): im lặng gần nhất vẫn giữ mọi phần còn lại hợp lệ, không có
/// thì đúng `target`.
#[allow(clippy::too_many_arguments)]
fn pick_cut(
    target: f64,
    prev: f64,
    next_target: f64,
    duration: f64,
    max_sec: f64,
    window: f64,
    part: f64,
    silences: &[Span],
) -> f64 {
    // Khi cả phần đều đà dưới 5s thì không thể đòi hỏi 5s được — lấy độ dài phần.
    let min_len = MIN_PART_SEC.min(part);
    let feasible = |c: f64| {
        c - prev <= max_sec + EPS // phần vừa kết thúc không vượt giới hạn
            && next_target - c <= max_sec + EPS // mốc kế tiếp vẫn với tới
            && c - prev >= min_len - EPS // phần vừa kết thúc không quá ngắn
            && duration - c >= min_len - EPS // phần cuối không quá ngắn
    };

    let mut best: Option<(f64, f64)> = None; // (khoảng cách tới mốc, điểm cắt)
    for (s, e) in silences {
        let (s, e) = (s.max(0.0), e.min(duration));
        if !(e > s) {
            continue;
        }
        // Mốc nằm trong im lặng thì cắt đúng mốc, không thì cắt giữa im lặng.
        let point = if s <= target && target <= e { target } else { (s + e) / 2.0 };
        let dist = (point - target).abs();
        if dist > window + EPS || !feasible(point) {
            continue;
        }
        if best.map_or(true, |(bd, _)| dist < bd - EPS) {
            best = Some((dist, point));
        }
    }
    best.map(|(_, p)| p).unwrap_or(target)
}

/// Gộp phần ngắn hơn [`MIN_PART_SEC`] vào phần trước (khi gộp xong vẫn ≤
/// `max_sec`); `bounds` là các biên `[0, c1, c2, …, duration]`.
pub fn merge_short_parts(bounds: &[f64], max_sec: f64) -> Vec<Span> {
    let mut parts: Vec<Span> = Vec::new();
    for w in bounds.windows(2) {
        let (s, e) = (w[0], w[1]);
        if e - s + EPS < MIN_PART_SEC {
            if let Some(last) = parts.last_mut() {
                if e - last.0 <= max_sec + EPS {
                    last.1 = e;
                    continue;
                }
            }
        }
        parts.push((s, e));
    }
    parts
}

/// Điểm cắt kéo tay trên UI (thời điểm tuyệt đối trên audio) → các phần.
///
/// Sắp xếp, bỏ điểm trùng/ngoài audio; có phần vượt `max_sec` → `Err` (kèm lý
/// do) để caller quay về chia tự động; phần < 5s được gộp như hàm tự động.
pub fn plan_from_cuts(audio_duration: f64, max_sec: f64, cuts: &[f64]) -> Result<Vec<Span>, String> {
    if !(audio_duration > 0.0) || !audio_duration.is_finite() {
        return Err("Audio không có thời lượng để chia".into());
    }
    let mut pts: Vec<f64> = cuts
        .iter()
        .copied()
        .filter(|c| c.is_finite() && *c > EPS && *c < audio_duration - EPS)
        .collect();
    pts.sort_by(|a, b| a.total_cmp(b));
    pts.dedup_by(|a, b| (*b - *a).abs() < 0.05);
    if pts.is_empty() {
        return Ok(vec![(0.0, audio_duration)]);
    }
    if !max_sec.is_finite() || max_sec <= 0.0 {
        return Err("Preset này không giới hạn thời lượng".into());
    }
    let mut bounds: Vec<f64> = Vec::with_capacity(pts.len() + 2);
    bounds.push(0.0);
    bounds.extend(pts);
    bounds.push(audio_duration);
    for w in bounds.windows(2) {
        if w[1] - w[0] > max_sec + EPS {
            return Err(format!(
                "Có phần dài {} vượt giới hạn {}",
                fmt_secs(w[1] - w[0]),
                fmt_secs(max_sec)
            ));
        }
    }
    Ok(merge_short_parts(&bounds, max_sec))
}

/// Dời phụ đề về timeline của một phần (B2): trừ `start`, cắt lại trong
/// `[0, len]`, bỏ câu nằm hoàn toàn ngoài phần. Câu vượt qua điểm cắt bị
/// chụt lại ở biên (không hiện nửa câu ở phần khác).
pub fn shift_cues(cues: Vec<crate::subtitle::Cue>, start: f64, len: f64) -> Vec<crate::subtitle::Cue> {
    cues.into_iter()
        .filter_map(|c| {
            let (s, e) = (c.start - start, c.end - start);
            if e <= 0.0 || s >= len {
                return None;
            }
            let mut c = c;
            c.start = s.max(0.0);
            c.end = e.min(len);
            Some(c)
        })
        .collect()
}

/// `185.4` → `"3:05"`; `3725.0` → `"1:02:05"` (dùng trong thông báo lỗi / log).
pub fn fmt_secs(secs: f64) -> String {
    let total = secs.round().max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Parse stderr của `silencedetect` thành các khoảng im lặng `(start, end)`.
pub fn parse_silencedetect(stderr: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut start: Option<f64> = None;
    for line in stderr.lines() {
        if let Some(rest) = line.split("silence_start:").nth(1) {
            if let Some(v) = first_number(rest) {
                start = Some(v);
            }
        } else if let Some(rest) = line.split("silence_end:").nth(1) {
            if let Some(v) = first_number(rest) {
                if let Some(s) = start.take() {
                    if v > s {
                        out.push((s, v));
                    }
                }
            }
        }
    }
    out
}

fn first_number(s: &str) -> Option<f64> {
    s.trim()
        .split(|c: char| c.is_whitespace() || c == '|')
        .next()
        .filter(|t| !t.is_empty())
        .and_then(|t| t.parse::<f64>().ok())
}

/// Tham số ffmpeg dò im lặng: `ffmpeg -i audio -af <filter> -f null -`.
/// `filter` ví dụ [`SILENCE_FILTER`] (chia phần) hoặc `noise=-32dB:d=0.35` (ASR).
pub fn build_silence_detect_args(audio: &Path, filter: &str) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "info".into(),
        "-nostats".into(),
        "-i".into(),
        audio.to_string_lossy().into_owned(),
        "-af".into(),
        filter.into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]
}

/// Chạy ffmpeg dò im lặng của cả audio. Lỗi (hoặc bị huỷ) → `Err`; caller coi
/// như không có im lặng và cắt đúng mốc.
pub async fn detect_silences(audio: &Path, cancel: &CancellationToken) -> anyhow::Result<Vec<Span>> {
    detect_silences_with(audio, SILENCE_FILTER, cancel).await
}

/// Bản tổng quát: dò im lặng với bộ lọc tuỳ ý (dùng chung cho chia phần và
/// phụ đề tự động — một runner duy nhất, có token huỷ).
pub async fn detect_silences_with(
    audio: &Path,
    filter: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<Vec<Span>> {
    let mut cmd = Command::new(crate::ffmpeg::ffmpeg_bin().clone());
    cmd.args(build_silence_detect_args(audio, filter))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("Không chạy được ffmpeg để dò im lặng")?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg không mở được stderr"))?;
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        use tokio::io::AsyncReadExt;
        let mut reader = stderr;
        let _ = reader.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });

    let status = tokio::select! {
        status = child.wait() => status,
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            return Err(anyhow::anyhow!("Đã huỷ"));
        }
    };
    let stderr = stderr_task.await.unwrap_or_default();
    if cancel.is_cancelled() {
        return Err(anyhow::anyhow!("Đã huỷ"));
    }
    let status = status.context("Không chờ được ffmpeg dò im lặng")?;
    if !status.success() {
        anyhow::bail!("ffmpeg dò im lặng lỗi: {}", stderr.lines().last().unwrap_or("(không có chi tiết)"));
    }
    Ok(parse_silencedetect(&stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    fn assert_valid(parts: &[Span], duration: f64, max_sec: f64) {
        assert!(!parts.is_empty());
        assert!(close(parts[0].0, 0.0), "phần đầu phải bắt đầu từ 0: {parts:?}");
        assert!(close(parts[parts.len() - 1].1, duration), "phần cuối phải kết thúc ở hết audio: {parts:?}");
        for w in parts.windows(2) {
            assert!(close(w[0].1, w[1].0), "các phần phải nối liền: {parts:?}");
        }
        for (s, e) in parts {
            assert!(e - s <= max_sec + 1e-3, "phần {s}-{e} vượt giới hạn {max_sec}");
        }
    }

    // 8:20 / 3:00 → 3 phần đều nhau.
    #[test]
    fn phan_bon_phut_khoanghai_chia_ba_phan_deu() {
        let parts = plan_splits(500.0, 180.0, &[]);
        assert_eq!(parts.len(), 3);
        assert_valid(&parts, 500.0, 180.0);
        let lens: Vec<f64> = parts.iter().map(|(s, e)| e - s).collect();
        for l in &lens {
            assert!(close(*l, 500.0 / 3.0), "phần phải đều nhau: {lens:?}");
        }
    }

    #[test]
    fn khong_co_im_lang_cat_dung_moc() {
        let parts = plan_splits(500.0, 180.0, &[]);
        assert!(close(parts[0].1, 166.667));
        assert!(close(parts[1].1, 333.333));
        // Im lặng quá xa mốc (ngoài ±15% = ±25s) cũng bị bỏ qua.
        let parts_far = plan_splits(500.0, 180.0, &[(120.0, 121.0)]);
        assert!(close(parts_far[0].1, 166.667));
    }

    // Có im lặng gần mốc → cắt tại im lặng (giữa đoạn im lặng).
    #[test]
    fn co_im_lang_gan_moc_cat_tai_im_lang() {
        let parts = plan_splits(500.0, 180.0, &[(175.0, 176.2), (330.0, 331.0)]);
        assert_eq!(parts.len(), 3);
        assert_valid(&parts, 500.0, 180.0);
        assert!(close(parts[0].1, 175.6), "cắt giữa đoạn im lặng: {:?}", parts[0]);
        assert!(close(parts[1].1, 330.5), "mốc nằm ngoài im lặng → cắt giữa im lặng: {:?}", parts[1]);
    }

    // Mốc nằm trong đoạn im lặng → cắt đúng mốc (vẫn là trong im lặng).
    #[test]
    fn moc_nam_trong_im_lang_cat_dung_moc() {
        let parts = plan_splits(500.0, 180.0, &[(160.0, 172.0)]);
        assert!(close(parts[0].1, 500.0 / 3.0));
    }

    // Hai đoạn im lặng cùng trong khoảng → lấy đoạn gần mốc nhất.
    #[test]
    fn hai_doan_im_lang_lay_gan_nhat() {
        let parts = plan_splits(500.0, 180.0, &[(150.0, 151.0), (168.0, 169.0)]);
        assert!(close(parts[0].1, 168.5));
    }

    // Im lặng gần mốc nhưng dời tới đó làm phần cuối quá ngắn → bỏ, cắt đúng mốc.
    #[test]
    fn im_lang_lam_phan_qua_ngan_bi_bo() {
        // 11s / 10s → 2 phần 5.5s; im lặng 6.0–6.6 gần mốc nhưng cắt tại 6.3 để
        // lại phần cuối 4.7s < 5s.
        let parts = plan_splits(11.0, 10.0, &[(6.0, 6.6)]);
        assert_eq!(parts.len(), 2);
        assert!(close(parts[0].1, 5.5));
        assert_valid(&parts, 11.0, 10.0);
    }

    // Audio vừa đúng giới hạn → 1 phần.
    #[test]
    fn audio_vua_dung_gioi_han_mot_phan() {
        let parts = plan_splits(180.0, 180.0, &[(90.0, 91.0)]);
        assert_eq!(parts, vec![(0.0, 180.0)]);
    }

    // Phần cuối quá ngắn → gộp vào phần trước (kéo tay tạo 4 phần, phần cuối
    // 3.5s; gộp xong 8.5s vẫn ≤ 10s).
    #[test]
    fn phan_cuoi_qua_ngan_bi_gop() {
        let parts = plan_from_cuts(25.0, 10.0, &[8.0, 16.5, 21.5]).unwrap();
        assert_eq!(parts, vec![(0.0, 8.0), (8.0, 16.5), (16.5, 25.0)]);
        // Hàm thuần gộp: phần giữa 2s bị gộp vào phần trước.
        let merged = merge_short_parts(&[0.0, 9.0, 11.0, 20.0], 12.0);
        assert_eq!(merged, vec![(0.0, 11.0), (11.0, 20.0)]);
    }

    // Gộp mà vượt giới hạn thì không gộp (giới hạn là cứng).
    #[test]
    fn gop_vuot_gioi_han_thi_khong_gop() {
        let merged = merge_short_parts(&[0.0, 8.0, 10.0], 9.0);
        assert_eq!(merged, vec![(0.0, 8.0), (8.0, 10.0)]);
        // 6s / 5s → 2 phần 3s: quá ngắn nhưng gộp lại thành 6s > 5s → giữ nguyên.
        let parts = plan_splits(6.0, 5.0, &[]);
        assert_eq!(parts, vec![(0.0, 3.0), (3.0, 6.0)]);
    }

    // Audio ngắn hơn giới hạn → không chia (dù có im lặng).
    #[test]
    fn audio_ngan_hon_gioi_han_khong_chia() {
        let parts = plan_splits(100.0, 180.0, &[(50.0, 51.0)]);
        assert_eq!(parts, vec![(0.0, 100.0)]);
        // max_sec = 0 / âm = không giới hạn.
        assert_eq!(plan_splits(100.0, 0.0, &[]), vec![(0.0, 100.0)]);
        assert_eq!(plan_splits(100.0, -1.0, &[]), vec![(0.0, 100.0)]);
    }

    #[test]
    fn thoi_luong_khong_hop_le_tra_rong() {
        assert!(plan_splits(0.0, 180.0, &[]).is_empty());
        assert!(plan_splits(-5.0, 180.0, &[]).is_empty());
        assert!(plan_splits(f64::NAN, 180.0, &[]).is_empty());
    }

    // Vài tỉ lệ thực tế: không phần nào vượt giới hạn, phủ kín audio.
    #[test]
    fn cac_ti_le_thuc_te_deu_hop_le() {
        for (d, max) in [(500.0_f64, 180.0_f64), (185.0, 180.0), (365.0, 180.0), (1000.0, 90.0), (25.0, 10.0), (10800.0, 600.0)] {
            let n = (d / max).ceil() as usize;
            let parts = plan_splits(d, max, &[]);
            assert_eq!(parts.len(), n, "{d}s / {max}s");
            assert_valid(&parts, d, max);
        }
    }

    // Dồn đủ nhiều im lặng để mọi mốc đều dời vẫn phải hợp lệ.
    #[test]
    fn dich_moc_van_giu_dung_gioi_han() {
        let silences: Vec<Span> = (1..40).map(|i| (i as f64 * 25.0 + 7.0, i as f64 * 25.0 + 8.5)).collect();
        for (d, max) in [(1000.0, 90.0), (500.0, 180.0), (95.0, 90.0), (12.0, 10.0)] {
            let parts = plan_splits(d, max, &silences);
            assert_valid(&parts, d, max);
        }
    }

    #[test]
    fn plan_from_cuts_loi_khi_vuot_gioi_han() {
        let err = plan_from_cuts(25.0, 10.0, &[12.0, 22.0]).unwrap_err();
        assert!(err.contains("vượt giới hạn"), "{err}");
        // Điểm ngoài audio / trùng được bỏ qua; không còn điểm → 1 phần.
        assert_eq!(plan_from_cuts(25.0, 10.0, &[-3.0, 30.0]).unwrap(), vec![(0.0, 25.0)]);
        assert_eq!(plan_from_cuts(25.0, 10.0, &[8.0, 8.01, 16.0]).unwrap().len(), 3);
        assert!(plan_from_cuts(0.0, 10.0, &[8.0]).is_err());
    }

    #[test]
    fn parse_silencedetect_doc_dung_dong() {
        let stderr = "ffmpeg version 7.1.5\n\
            [silencedetect @ 0x55d] silence_start: 12.3456\n\
            [silencedetect @ 0x55d] silence_end: 13.5678 | silence_duration: 1.2222\n\
            [silencedetect @ 0x55d] silence_start: 40\n\
            [silencedetect @ 0x55d] silence_end: 41.5 | silence_duration: 1.5\n";
        assert_eq!(parse_silencedetect(stderr), vec![(12.3456, 13.5678), (40.0, 41.5)]);
        // Im lặng chưa kết thúc (chạy tới hết file) → bỏ.
        assert_eq!(parse_silencedetect("silence_start: 5\n"), Vec::<Span>::new());
        assert!(parse_silencedetect("").is_empty());
    }

    #[test]
    fn silence_detect_args_dung_bo_loc() {
        let args = build_silence_detect_args(Path::new("/tmp/a.mp3"), "silencedetect=noise=-32dB:d=0.35");
        assert!(args.join(" ").contains("-af silencedetect=noise=-32dB:d=0.35"));
        let args = build_silence_detect_args(Path::new("/tmp/a.mp3"), SILENCE_FILTER);
        let joined = args.join(" ");
        assert!(joined.contains("-af silencedetect=noise=-35dB:d=0.4"), "{joined}");
        assert!(joined.ends_with("-f null -"), "{joined}");
    }

    #[test]
    fn shift_cues_doi_phu_de_ve_phan() {
        use crate::subtitle::Cue;
        let cue = |s: f64, e: f64, t: &str| Cue { start: s, end: e, text: t.into() };
        let cues = vec![cue(0.0, 3.0, "a"), cue(7.0, 9.5, "b"), cue(9.5, 12.0, "c"), cue(16.0, 20.0, "d")];
        // Phần [8, 16): "a" ngoài, "b" chụt về 0, "c" giữ, "d" ngoài.
        let shifted = shift_cues(cues, 8.0, 8.0);
        assert_eq!(shifted.len(), 2);
        assert_eq!((shifted[0].start, shifted[0].end, shifted[0].text.as_str()), (0.0, 1.5, "b"));
        assert_eq!((shifted[1].start, shifted[1].end, shifted[1].text.as_str()), (1.5, 4.0, "c"));
        // Câu vượt qua cuối phần bị chụt về đúng độ dài phần.
        let shifted = shift_cues(vec![cue(10.0, 20.0, "x")], 8.0, 8.0);
        assert_eq!((shifted[0].start, shifted[0].end), (2.0, 8.0));
        assert!(shift_cues(vec![cue(0.0, 3.0, "a")], 8.0, 8.0).is_empty());
    }

    #[test]
    fn fmt_secs_dinh_dang_phut_gio() {
        assert_eq!(fmt_secs(500.0), "8:20");
        assert_eq!(fmt_secs(180.0), "3:00");
        assert_eq!(fmt_secs(59.6), "1:00");
        assert_eq!(fmt_secs(3725.0), "1:02:05");
    }

}
