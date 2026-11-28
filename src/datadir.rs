//! Thư mục dữ liệu (M8): nơi đặt `settings.json`, `library/`, `workspace/`.
//!
//! Quy tắc chọn (ưu tiên từ trên xuống):
//! 1. `RVM_DATA_DIR` (nếu đặt) — dùng nguyên giá trị.
//! 2. Tương thích ngược: CWD hiện tại đã có `library/` hoặc `workspace/`
//!    mà thư mục exe chưa có → dùng CWD (người dùng cũ chạy bằng double-click
//!    từ thư mục khác không mất kho) + cảnh báo trong log.
//! 3. Mặc định: thư mục chứa file exe (cargo test → thư mục target/…).
//!
//! Toàn bộ là hàm thuần nhận tham số để test được; `data_dir()` là bản bọc
//! đọc env/exe thật, cache kết quả bằng OnceLock (đường dẫn không đổi giữa
//! chừng chương trình).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static CACHE: OnceLock<PathBuf> = OnceLock::new();

/// Bản thuần: chọn thư mục dữ liệu từ các đầu vào cho trước.
///
/// - `env_dir`: giá trị `RVM_DATA_DIR` (None nếu không đặt).
/// - `exe_dir`: thư mục chứa file thực thi (None khi không xác định được).
/// - `cwd`: thư mục hiện tại.
///
/// Trả về (đường dẫn, cảnh báo nếu có) — cảnh báo khi rơi vào nhánh tương
/// thích ngược CWD để caller ghi log.
pub fn resolve_data_dir(
    env_dir: Option<&str>,
    exe_dir: Option<&Path>,
    cwd: &Path,
) -> (PathBuf, Option<String>) {
    // 1. Env ghi đè mọi thứ.
    if let Some(dir) = env_dir.map(str::trim).filter(|s| !s.is_empty()) {
        return (PathBuf::from(dir), None);
    }

    let exe = exe_dir.unwrap_or(cwd);
    let cwd_has_data = dir_has_data(cwd);
    let exe_has_data = dir_has_data(exe);

    // 2. CWD có dữ liệu cũ mà thư mục exe chưa có → giữ CWD (tương thích
    //    ngược), ghi cảnh báo.
    if cwd_has_data && !exe_has_data && exe != cwd {
        return (
            cwd.to_path_buf(),
            Some(format!(
                "Phát hiện dữ liệu (library/workspace) trong thư mục hiện hành {} \
                 nhưng thư mục exe {} chưa có — dùng thư mục hiện hành để không \
                 mất kho cũ. Muốn chuyển hẳn, đặt RVM_DATA_DIR hoặc di chuyển \
                 library/ + workspace/ sang thư mục exe.",
                cwd.display(),
                exe.display()
            )),
        );
    }

    // 3. Mặc định: cạnh file exe.
    (exe.to_path_buf(), None)
}

/// Thư mục có dấu hiệu dữ liệu của app không (library/ hoặc workspace/).
fn dir_has_data(dir: &Path) -> bool {
    dir.join("library").is_dir() || dir.join("workspace").is_dir()
}

/// Thư mục dữ liệu đang dùng (cache sau lần gọi đầu). Khởi động in ra log.
pub fn data_dir() -> PathBuf {
    CACHE
        .get_or_init(|| {
            let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let env_dir = std::env::var("RVM_DATA_DIR").ok();
            let (dir, warning) = resolve_data_dir(env_dir.as_deref(), exe_dir.as_deref(), &cwd);
            tracing::info!("Thư mục dữ liệu (settings.json, library/, workspace/): {}", dir.display());
            if let Some(w) = warning {
                tracing::warn!("{w}");
            }
            dir
        })
        .clone()
}

/// Đường dẫn `settings.json` trong thư mục dữ liệu.
pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rvm_datadir_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn env_override_thang_dau_tien() {
        let (d, w) = resolve_data_dir(Some("/tmp/rvm-data"), Some(Path::new("/opt/app")), Path::new("/home/u"));
        assert_eq!(d, PathBuf::from("/tmp/rvm-data"));
        assert!(w.is_none());
    }

    #[test]
    fn env_rong_bi_bo_qua() {
        let (d, _) = resolve_data_dir(Some("   "), Some(Path::new("/opt/app")), Path::new("/home/u"));
        assert_eq!(d, PathBuf::from("/opt/app"));
    }

    #[test]
    fn mac_dinh_la_thu_muc_exe() {
        let (d, w) = resolve_data_dir(None, Some(Path::new("/opt/app")), Path::new("/home/u"));
        assert_eq!(d, PathBuf::from("/opt/app"));
        assert!(w.is_none());
    }

    #[test]
    fn cwd_co_library_cu_duoc_giu_lai_canh_bao() {
        let exe = tmp("exe1");
        let cwd = tmp("cwd1");
        std::fs::create_dir_all(cwd.join("library")).unwrap();
        let (d, w) = resolve_data_dir(None, Some(&exe), &cwd);
        assert_eq!(d, cwd, "CWD có library cũ → dùng CWD");
        assert!(w.is_some(), "phải có cảnh báo tương thích ngược");
    }

    #[test]
    fn ca_hai_co_du_lieu_thi_exe_thang() {
        let exe = tmp("exe2");
        let cwd = tmp("cwd2");
        std::fs::create_dir_all(exe.join("workspace")).unwrap();
        std::fs::create_dir_all(cwd.join("library")).unwrap();
        let (d, w) = resolve_data_dir(None, Some(&exe), &cwd);
        assert_eq!(d, exe, "exe có dữ liệu → mặc định thắng");
        assert!(w.is_none());
    }

    #[test]
    fn exe_khong_xac_dinh_thi_dung_cwd() {
        let cwd = tmp("cwd3");
        let (d, _) = resolve_data_dir(None, None, &cwd);
        assert_eq!(d, cwd);
    }

    #[test]
    fn workspace_cung_la_dau_hieu_du_lieu() {
        let exe = tmp("exe4");
        let cwd = tmp("cwd4");
        std::fs::create_dir_all(cwd.join("workspace")).unwrap();
        let (d, _) = resolve_data_dir(None, Some(&exe), &cwd);
        assert_eq!(d, cwd, "workspace cũ ở CWD cũng được giữ");
    }
}
