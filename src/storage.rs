//! A3: giới hạn lưu trữ cho kho video đã xuất (library/).
//!
//! Gồm 3 phần tách bạch:
//! 1. **SettingsStore** — đọc/ghi `settings.json` cạnh file chạy; biến môi
//!    trường `RVM_LIBRARY_MAX_GB` / `RVM_LIBRARY_MAX_FILES` ghi đè giá trị
//!    file (ưu tiên cho ops); validate 1–500 GB và 10–5000 file.
//! 2. **Hàm thuần `plan_cleanup`** — quyết định file nào cần xoá để kho về
//!    dưới ngưỡng: cũ nhất trước (mtime), không bao giờ đụng file của job
//!    đang chạy hoặc vừa xuất; file đơn lớn hơn cả giới hạn thì giữ lại và
//!    trả cảnh báo. Thuần để unit test đầy đủ.
//! 3. **Thao tác fs thật** (quét library, xoá) — bọc `spawn_blocking` để
//!    không chặn tokio runtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Thư mục kho output: dùng jobs::library_dir() (M8: theo thư mục dữ liệu).
/// Giới hạn mặc định của kho (đề mục A3).
pub const DEFAULT_MAX_GB: u64 = 5;
pub const DEFAULT_MAX_FILES: usize = 200;

pub const MIN_GB: u64 = 1;
pub const MAX_GB: u64 = 500;
pub const MIN_FILES: usize = 10;
pub const MAX_FILES: usize = 5000;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct LibrarySettings {
    pub max_gb: u64,
    pub max_files: usize,
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self { max_gb: DEFAULT_MAX_GB, max_files: DEFAULT_MAX_FILES }
    }
}

/// Validate một cặp giới hạn (dùng cho cả PUT API và env override).
/// Trả về Err(thông báo lỗi tiếng Việt) khi sai khoảng.
pub fn validate_settings(max_gb: u64, max_files: usize) -> Result<(), String> {
    if !(MIN_GB..=MAX_GB).contains(&max_gb) {
        return Err(format!("Giới hạn dung lượng phải từ {MIN_GB} đến {MAX_GB} GB."));
    }
    if !(MIN_FILES..=MAX_FILES).contains(&max_files) {
        return Err(format!("Giới hạn số file phải từ {MIN_FILES} đến {MAX_FILES}."));
    }
    Ok(())
}

/// Nơi giữ setting hiện tại — dùng RwLock vì đọc xảy ra mỗi lần render /
/// dọn kho, ghi chỉ khi người dùng sửa qua API.
#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<tokio::sync::RwLock<LibrarySettings>>,
}

impl SettingsStore {
    /// Nạp settings.json + áp env override. File hỏng/thiếu → dùng mặc định
    /// (ghi log, không crash — app phải chạy được).
    pub fn load() -> Self {
        let s = Self::load_from_disk();
        tracing::info!(
            "Giới hạn kho video: {} GB, {} file (mặc định {DEFAULT_MAX_GB} GB / {DEFAULT_MAX_FILES} file)",
            s.max_gb,
            s.max_files
        );
        Self { inner: Arc::new(tokio::sync::RwLock::new(s)) }
    }

    /// Bản có đường dẫn để unit test dùng thư mục tạm.
    fn load_from_disk() -> LibrarySettings {
        let mut s = Self::read_file(&crate::datadir::settings_path()).unwrap_or_default();
        // Env override: chỉ áp khi hợp lệ, sai khoảng thì bỏ qua + log.
        if let Ok(v) = std::env::var("RVM_LIBRARY_MAX_GB") {
            match v.parse::<u64>() {
                Ok(gb) if validate_settings(gb, s.max_files).is_ok() => s.max_gb = gb,
                _ => tracing::warn!("RVM_LIBRARY_MAX_GB={v} không hợp lệ (1–500), bỏ qua."),
            }
        }
        if let Ok(v) = std::env::var("RVM_LIBRARY_MAX_FILES") {
            match v.parse::<usize>() {
                Ok(n) if validate_settings(s.max_gb, n).is_ok() => s.max_files = n,
                _ => tracing::warn!("RVM_LIBRARY_MAX_FILES={v} không hợp lệ (10–5000), bỏ qua."),
            }
        }
        s
    }

    /// Đọc settings.json; lỗi parse/thiếu file → None (caller về mặc định).
    fn read_file(path: &Path) -> Option<LibrarySettings> {
        let text = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let max_gb = v.get("library_max_gb")?.as_u64()?;
        let max_files = v.get("library_max_files")?.as_u64()? as usize;
        validate_settings(max_gb, max_files).ok()?;
        Some(LibrarySettings { max_gb, max_files })
    }

    pub async fn get(&self) -> LibrarySettings {
        *self.inner.read().await
    }

    /// Đọc nhanh từ ngữ cảnh sync (JobStore dùng khi dọn kho). Khóa đọc của
    /// tokio RwLock không chặn lâu (ghi hiếm khi xảy ra) nhưng nếu lock đang
    /// bận ở nhánh write thì fallback về mặc định an toàn.
    pub fn inner_blocking_get(&self) -> LibrarySettings {
        match self.inner.try_read() {
            Ok(g) => *g,
            Err(_) => LibrarySettings::default(),
        }
    }

    /// Ghi giá trị mới (đã validate trước) vào settings.json + bộ nhớ.
    /// Ghi file hỏng (đĩa đầy, quyền) → vẫn cập nhật bộ nhớ nhưng trả Err
    /// để API báo rõ cho người dùng.
    pub async fn set(&self, s: LibrarySettings) -> Result<(), String> {
        let json = serde_json::json!({
            "library_max_gb": s.max_gb,
            "library_max_files": s.max_files,
        });
        let text = serde_json::to_string_pretty(&json)
            .map_err(|e| format!("Không tạo JSON settings: {e}"))?;
        std::fs::write(crate::datadir::settings_path(), text + "\n")
            .map_err(|e| format!("Không ghi được settings.json: {e}"))?;
        *self.inner.write().await = s;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hàm thuần: chọn file cần xoá
// ---------------------------------------------------------------------------

/// 1 file trong kho, dạng dữ liệu thô cho hàm thuần.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibFile {
    /// Đường dẫn tuyệt đối/relative đến file (để xoá sau này).
    pub path: PathBuf,
    /// Dung lượng byte.
    pub size: u64,
    /// Thời gian sửa đổi (epoch mili-giây) — nhỏ hơn = cũ hơn.
    pub modified_ms: u64,
    /// ID job sở hữu (tên thư mục cha) — để bảo vệ job đang chạy.
    pub job_id: String,
}

/// Kết quả lập kế hoạch dọn kho.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupPlan {
    /// Danh sách file cần xoá (đã sắp theo thứ tự xoá: cũ nhất trước).
    pub to_delete: Vec<LibFile>,
    /// Dung lượng giữ lại sau khi xoá (byte).
    pub kept_bytes: u64,
    /// Số file giữ lại sau khi xoá.
    pub kept_files: usize,
    /// Cảnh báo (file đơn lớn hơn giới hạn…), rỗng nếu không có.
    pub warnings: Vec<String>,
}

/// Chọn file cần xoá để kho về dưới BOTH hai ngưỡng.
///
/// Nguyên tắc:
/// - chỉ xét file KHÔNG thuộc `protected_jobs` (job đang chạy / vừa xuất);
/// - xoá CŨ NHẤT trước (mtime nhỏ nhất); các file được bảo vệ vẫn tính vào
///   tổng để quyết định có cần xoá hay không;
/// - nếu 1 file đơn lẻ (sau khi đã bỏ mọi file có thể xoá) vẫn vượt giới
///   hạn dung lượng → GIỮ lại + cảnh báo (không bao giờ tự xoá hết kho);
/// - kho rỗng / đã dưới ngưỡng → không xoá gì.
pub fn plan_cleanup(
    files: &[LibFile],
    max_bytes: u64,
    max_files: usize,
    protected_jobs: &[String],
) -> CleanupPlan {
    let mut plan = CleanupPlan::default();
    if files.is_empty() {
        return plan;
    }

    let mut deletable: Vec<&LibFile> = files
        .iter()
        .filter(|f| !protected_jobs.contains(&f.job_id))
        .collect();
    // Cũ nhất trước: mtime nhỏ nhất được xoá đầu tiên.
    deletable.sort_by_key(|a| a.modified_ms);

    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let total_files = files.len();
    let mut cur_bytes = total_bytes;
    let mut cur_files = total_files;

    for f in deletable {
        let over_bytes = cur_bytes > max_bytes;
        let over_files = cur_files > max_files;
        if !over_bytes && !over_files {
            break; // đã dưới cả 2 ngưỡng
        }
        // File đơn lẻ lớn hơn cả giới hạn dung lượng → KHÔNG tự xoá: giữ lại
        // và cảnh báo người dùng (xoá output lớn vì dọn dẹp là mất dữ liệu
        // thật). Ưu tiên xoá các file nhỏ cũ hơn phía sau.
        if f.size > max_bytes {
            continue;
        }
        plan.to_delete.push(f.clone());
        cur_bytes = cur_bytes.saturating_sub(f.size);
        cur_files -= 1;
    }

    // Còn vượt dung lượng → chắc chắn có file đơn quá lớn (hoặc toàn bộ bị
    // bảo vệ). Giữ lại + cảnh báo người dùng tự xử lý.
    if cur_bytes > max_bytes {
        let over_gb = (cur_bytes - max_bytes) as f64 / 1e9;
        plan.warnings.push(format!(
            "Kho còn vượt {over_gb:.2} GB nhưng không thể tự xoá: các file còn lại \
             thuộc job đang chạy hoặc có file đơn lẻ lớn hơn giới hạn. Hãy tăng \
             giới hạn hoặc xoá thủ công."
        ));
    }

    plan.kept_bytes = cur_bytes;
    plan.kept_files = cur_files;
    plan
}

/// Kế hoạch "dọn về X% giới hạn" cho nút Dọn ngay (A3: 70%).
/// Cùng logic chọn file với [plan_cleanup], chỉ khác ngưỡng mục tiêu.
pub fn plan_cleanup_to_ratio(
    files: &[LibFile],
    max_bytes: u64,
    max_files: usize,
    ratio: f64,
    protected_jobs: &[String],
) -> CleanupPlan {
    let target_bytes = ((max_bytes as f64) * ratio) as u64;
    let target_files = ((max_files as f64) * ratio) as usize;
    plan_cleanup(files, target_bytes, target_files, protected_jobs)
}

// ---------------------------------------------------------------------------
// Thao tác fs thật — chạy trong spawn_blocking
// ---------------------------------------------------------------------------

/// Quét library/ thành danh sách [LibFile]. Chạy trong blocking thread.
pub fn scan_library_sync(lib_dir: &Path) -> Vec<LibFile> {
    let mut out = Vec::new();
    let Ok(job_dirs) = std::fs::read_dir(lib_dir) else {
        return out;
    };
    for jd in job_dirs.flatten() {
        if !jd.path().is_dir() {
            continue;
        }
        let job_id = jd.file_name().to_string_lossy().into_owned();
        let Ok(files) = std::fs::read_dir(jd.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            let Ok(meta) = std::fs::metadata(&p) else { continue };
            if !meta.is_file() {
                continue;
            }
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            out.push(LibFile {
                path: p,
                size: meta.len(),
                modified_ms,
                job_id: job_id.clone(),
            });
        }
    }
    out
}

/// Kết quả thực thi dọn kho (để API/UI hiển thị).
#[derive(Debug, Default, serde::Serialize)]
pub struct CleanupResult {
    /// Số file đã xoá.
    pub deleted: usize,
    /// Số byte đã giải phóng.
    pub freed_bytes: u64,
    /// Cảnh báo từ kế hoạch (file đơn quá lớn…).
    pub warnings: Vec<String>,
    /// Dung lượng + số file trong kho SAU khi dọn.
    pub kept_bytes: u64,
    pub kept_files: usize,
}

/// Thực thi 1 kế hoạch dọn kho: xoá file, thu thập kết quả. Blocking.
fn execute_plan_sync(plan: &CleanupPlan) -> CleanupResult {
    let mut res = CleanupResult {
        warnings: plan.warnings.clone(),
        kept_bytes: plan.kept_bytes,
        kept_files: plan.kept_files,
        ..Default::default()
    };
    for f in &plan.to_delete {
        match std::fs::remove_file(&f.path) {
            Ok(()) => {
                res.deleted += 1;
                res.freed_bytes += f.size;
                // Thư mục job rỗng → xoá luôn cho gọn (đồng bộ với library_delete).
                if let Some(dir) = f.path.parent() {
                    if std::fs::read_dir(dir).map(|mut rd| rd.next().is_none()).unwrap_or(false) {
                        let _ = std::fs::remove_dir(dir);
                    }
                }
            }
            Err(e) => {
                // File bị khoá/xoá bởi request khác → tiếp tục, không chết.
                tracing::warn!("Không xoá được {}: {e}", f.path.display());
                res.kept_bytes += f.size;
                res.kept_files += 1;
            }
        }
    }
    res
}

/// Dọn kho về dưới giới hạn (dùng sau mỗi job xong + lúc khởi động).
/// Toàn bộ fs sync chạy trong `spawn_blocking`.
pub async fn enforce_library_limits(
    settings: LibrarySettings,
    protected_jobs: Vec<String>,
) -> CleanupResult {
    let max_bytes = settings.max_gb * 1_000_000_000;
    let max_files = settings.max_files;
    let result = tokio::task::spawn_blocking(move || {
        let files = scan_library_sync(&crate::jobs::library_dir());
        let plan = plan_cleanup(&files, max_bytes, max_files, &protected_jobs);
        execute_plan_sync(&plan)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!("Task dọn kho panic: {e}");
        CleanupResult::default()
    });
    if result.deleted > 0 {
        tracing::info!(
            "Dọn kho: xoá {} file, giải phóng {:.2} GB",
            result.deleted,
            result.freed_bytes as f64 / 1e9
        );
    }
    result
}

/// Dọn kho về `ratio` (0–1) của giới hạn — cho nút "Dọn ngay" (70%).
pub async fn cleanup_library_to_ratio(
    settings: LibrarySettings,
    ratio: f64,
    protected_jobs: Vec<String>,
) -> CleanupResult {
    let max_bytes = settings.max_gb * 1_000_000_000;
    let max_files = settings.max_files;
    tokio::task::spawn_blocking(move || {
        let files = scan_library_sync(&crate::jobs::library_dir());
        let plan = plan_cleanup_to_ratio(&files, max_bytes, max_files, ratio, &protected_jobs);
        execute_plan_sync(&plan)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!("Task dọn kho panic: {e}");
        CleanupResult::default()
    })
}

/// Thống kê kho hiện tại (cho API/UI). Blocking chạy trong spawn_blocking.
#[derive(Debug, Default, serde::Serialize)]
pub struct LibraryUsage {
    pub total_bytes: u64,
    pub total_files: usize,
}

pub async fn library_usage() -> LibraryUsage {
    tokio::task::spawn_blocking(|| {
        let files = scan_library_sync(&crate::jobs::library_dir());
        LibraryUsage {
            total_bytes: files.iter().map(|f| f.size).sum(),
            total_files: files.len(),
        }
    })
    .await
    .unwrap_or_default()
}

/// Dung lượng đĩa còn trống tại `path` (byte) — fs4 dùng statvfs trên
/// Linux/macOS và GetDiskFreeSpaceExW trên Windows.
pub fn disk_free_bytes(path: &Path) -> std::io::Result<u64> {
    fs4::free_space(path)
}

// ---------------------------------------------------------------------------
// Unit tests — hàm thuần
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn f(job: &str, name: &str, size: u64, ms: u64) -> LibFile {
        LibFile {
            path: PathBuf::from(format!("library/{job}/{name}")),
            size,
            modified_ms: ms,
            job_id: job.into(),
        }
    }

    const GB: u64 = 1_000_000_000;

    #[test]
    fn kho_rong_khong_xoa_gi() {
        let plan = plan_cleanup(&[], GB, 10, &[]);
        assert!(plan.to_delete.is_empty());
        assert_eq!(plan.kept_bytes, 0);
        assert_eq!(plan.kept_files, 0);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn duoi_nguong_khong_xoa() {
        let files = vec![f("j1", "a.mp4", 100, 1), f("j2", "b.mp4", 200, 2)];
        let plan = plan_cleanup(&files, GB, 10, &[]);
        assert!(plan.to_delete.is_empty());
        assert_eq!(plan.kept_files, 2);
    }

    #[test]
    fn vuot_dung_luong_xoa_cu_nhat_truoc() {
        let files = vec![
            f("j1", "moi.mp4", 3 * GB, 3000), // mới nhất
            f("j2", "cu.mp4", 2 * GB, 1000),  // cũ nhất
            f("j3", "giua.mp4", 2 * GB, 2000),
        ];
        // Limit 5 GB, tổng 7 GB → cần xoá. Cũ nhất (2GB) ra đi trước → còn 5GB
        // (== ngưỡng, không "vượt") → dừng.
        let plan = plan_cleanup(&files, 5 * GB, 200, &[]);
        assert_eq!(plan.to_delete.len(), 1);
        assert_eq!(plan.to_delete[0].path, PathBuf::from("library/j2/cu.mp4"));
        assert_eq!(plan.kept_bytes, 5 * GB);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn vuot_dung_luong_xoa_den_khi_du() {
        let files = vec![
            f("j1", "a.mp4", 2 * GB, 5000),
            f("j2", "b.mp4", 2 * GB, 100),
            f("j3", "c.mp4", 2 * GB, 200),
            f("j4", "d.mp4", 2 * GB, 300),
        ];
        // Tổng 8 GB, limit 5 GB → xoá cũ nhất đến khi còn ≤5: b, c (còn 4 GB).
        let plan = plan_cleanup(&files, 5 * GB, 200, &[]);
        let names: Vec<_> = plan.to_delete.iter().map(|x| x.path.clone()).collect();
        assert_eq!(
            names,
            vec![PathBuf::from("library/j2/b.mp4"), PathBuf::from("library/j3/c.mp4")]
        );
        assert_eq!(plan.kept_bytes, 4 * GB);
    }

    #[test]
    fn vuot_so_file() {
        // Dung lượng thấp nhưng 5 file > limit 3 file → xoá 2 cũ nhất.
        let files = vec![
            f("j1", "1.mp4", 10, 5),
            f("j2", "2.mp4", 10, 1),
            f("j3", "3.mp4", 10, 4),
            f("j4", "4.mp4", 10, 2),
            f("j5", "5.mp4", 10, 3),
        ];
        let plan = plan_cleanup(&files, GB, 3, &[]);
        assert_eq!(plan.to_delete.len(), 2);
        // Thứ tự xoá theo mtime tăng dần: 2.mp4 (1), 4.mp4 (2).
        assert_eq!(plan.to_delete[0].path, PathBuf::from("library/j2/2.mp4"));
        assert_eq!(plan.to_delete[1].path, PathBuf::from("library/j4/4.mp4"));
        assert_eq!(plan.kept_files, 3);
    }

    #[test]
    fn bao_ve_job_dang_chay() {
        let files = vec![
            f("dang-chay", "vua-xuat.mp4", 4 * GB, 9000), // mới nhất, đang chạy
            f("j2", "cu.mp4", 4 * GB, 100),
        ];
        // Limit 5 GB, tổng 8 GB. File cũ nhất thuộc j2 → xoá được, nhưng sau
        // khi xoá vẫn còn 4 GB ≤ 5 GB → đạt. Dù sao file j2 cũ hơn nên bị xoá
        // trước; job đang chạy KHÔNG bao giờ nằm trong to_delete.
        let plan = plan_cleanup(&files, 5 * GB, 200, &["dang-chay".into()]);
        assert_eq!(plan.to_delete.len(), 1);
        assert_eq!(plan.to_delete[0].path, PathBuf::from("library/j2/cu.mp4"));

        // Ngược lại: chỉ có file của job đang chạy → không xoá gì + cảnh báo.
        let only = vec![f("dang-chay", "vua-xuat.mp4", 8 * GB, 9000)];
        let plan = plan_cleanup(&only, 5 * GB, 200, &["dang-chay".into()]);
        assert!(plan.to_delete.is_empty());
        assert_eq!(plan.kept_bytes, 8 * GB);
        assert_eq!(plan.warnings.len(), 1);
    }

    #[test]
    fn file_don_qua_lon_giu_lai_va_canh_bao() {
        // 1 file 8 GB, limit 5 GB → không file nào khác để xoá → giữ + cảnh báo.
        let files = vec![
            f("j1", "lon.mp4", 8 * GB, 100),
            f("j2", "nho.mp4", GB, 50), // cũ hơn, xoá được
        ];
        let plan = plan_cleanup(&files, 5 * GB, 200, &[]);
        // Xoá file nhỏ cũ nhất trước, nhưng vẫn còn 8 GB > 5 GB → cảnh báo.
        assert_eq!(plan.to_delete.len(), 1);
        assert_eq!(plan.kept_bytes, 8 * GB);
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].contains("không thể tự xoá"));
    }

    #[test]
    fn vuot_ca_hai_ngang_mot_luc() {
        // 4 file × 2 GB (tổng 8 GB > 5 GB) và limit 3 file → phải giải quyết
        // cả hai: xoá cũ nhất 1 file là vừa đủ cả dung lượng (6→? ) — kiểm
        // từng bước: sau 1 file xoá còn 3 file (đúng ngưỡng file) nhưng 6 GB
        // vẫn vượt → xoá tiếp file cũ thứ hai → 4 GB, 2 file.
        let files = vec![
            f("j1", "a.mp4", 2 * GB, 400),
            f("j2", "b.mp4", 2 * GB, 100),
            f("j3", "c.mp4", 2 * GB, 200),
            f("j4", "d.mp4", 2 * GB, 300),
        ];
        let plan = plan_cleanup(&files, 5 * GB, 3, &[]);
        assert_eq!(plan.to_delete.len(), 2);
        assert_eq!(plan.kept_files, 2);
        assert_eq!(plan.kept_bytes, 4 * GB);
    }

    #[test]
    fn don_ve_70_phan_tram() {
        let files = vec![
            f("j1", "a.mp4", 2 * GB, 100),
            f("j2", "b.mp4", 2 * GB, 200),
            f("j3", "c.mp4", 2 * GB, 300),
            f("j4", "d.mp4", 2 * GB, 400),
        ];
        // Limit 5 GB → mục tiêu 70% = 3.5 GB → xoá cũ nhất đến ≤3.5 GB:
        // a (còn 6), b (còn 4)... 4 vẫn > 3.5 → xoá nốt c → còn 2 GB.
        let plan = plan_cleanup_to_ratio(&files, 5 * GB, 200, 0.7, &[]);
        assert_eq!(plan.to_delete.len(), 3);
        assert_eq!(plan.kept_bytes, 2 * GB);
        assert_eq!(plan.kept_files, 1);
        // File MỚI NHẤT (d.mp4, mtime 400) luôn được giữ; 3 file cũ bị xoá
        // theo thứ tự tuổi tăng dần.
        assert!(plan.to_delete.iter().all(|x| !x.path.ends_with("d.mp4")));
        assert_eq!(plan.to_delete[0].path, PathBuf::from("library/j1/a.mp4"));
    }

    // ---- settings -----------------------------------------------------------

    #[test]
    fn validate_khoang_gia_tri() {
        assert!(validate_settings(1, 10).is_ok());
        assert!(validate_settings(500, 5000).is_ok());
        assert!(validate_settings(5, 200).is_ok());
        assert!(validate_settings(0, 200).is_err()); // GB dưới 1
        assert!(validate_settings(501, 200).is_err()); // GB trên 500
        assert!(validate_settings(5, 9).is_err()); // file dưới 10
        assert!(validate_settings(5, 5001).is_err()); // file trên 5000
    }

    #[test]
    fn read_file_settings() {
        let dir = std::env::temp_dir().join(format!("rvm_test_settings_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // Hợp lệ.
        std::fs::write(&path, r#"{"library_max_gb": 10, "library_max_files": 50}"#).unwrap();
        let s = SettingsStore::read_file(&path).unwrap();
        assert_eq!((s.max_gb, s.max_files), (10, 50));

        // Thiếu field / sai kiểu / JSON hỏng / ngoài khoảng → None.
        std::fs::write(&path, r#"{"library_max_gb": 10}"#).unwrap();
        assert!(SettingsStore::read_file(&path).is_none());
        std::fs::write(&path, "not json").unwrap();
        assert!(SettingsStore::read_file(&path).is_none());
        std::fs::write(&path, r#"{"library_max_gb": 9999, "library_max_files": 50}"#).unwrap();
        assert!(SettingsStore::read_file(&path).is_none());
        assert!(SettingsStore::read_file(&dir.join("khong-ton-tai.json")).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_va_execute_trong_thu_muc_tam() {
        // Quét thật file tạm rồi thực thi kế hoạch xoá (integration thuần fs).
        let dir = std::env::temp_dir().join(format!("rvm_test_lib_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("j_old")).unwrap();
        std::fs::create_dir_all(dir.join("j_new")).unwrap();
        // 2 file "cũ" 1.5 KB + 1 file "mới" 1.5 KB (dung lượng nhỏ nhưng dùng
        // ngưỡng byte thấp để ép vượt).
        std::fs::write(dir.join("j_old/a.bin"), vec![0u8; 1500]).unwrap();
        std::fs::write(dir.join("j_old/b.bin"), vec![0u8; 1500]).unwrap();
        std::fs::write(dir.join("j_new/c.bin"), vec![0u8; 1500]).unwrap();
        // j_old cũ hơn j_new 1 giờ.
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::open(dir.join("j_old/a.bin"))
            .unwrap()
            .set_modified(past)
            .ok();
        std::fs::File::open(dir.join("j_old/b.bin"))
            .unwrap()
            .set_modified(past)
            .ok();

        let files = scan_library_sync(&dir);
        assert_eq!(files.len(), 3);
        let plan = plan_cleanup(&files, 2000, 10, &[]); // limit 2000 byte
        let res = execute_plan_sync(&plan);
        // 2 file cũ (3000 byte) bị xoá, còn file mới 1500 ≤ 2000.
        assert_eq!(res.deleted, 2);
        assert!(!dir.join("j_old").exists()); // thư mục rỗng bị dọn
        assert!(dir.join("j_new/c.bin").exists());
        // Sau dọn: chỉ còn 1 file.
        assert_eq!(scan_library_sync(&dir).len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
