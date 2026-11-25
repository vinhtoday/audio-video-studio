mod error;
mod ffmpeg;
mod fontconfig;
mod fontinfo;
mod jobs;
mod presets;
mod routes;
mod subtitle;
mod textover;
mod visuals;
mod zipwriter;

use jobs::JobStore;
use presets::Registry;
use routes::AppState;
use tokio::signal::ctrl_c;

/// Frontend nhúng thẳng vào binary bằng include_str!.
static STATIC_INDEX: &str = include_str!("../static/index.html");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "radio_video_merger=info,tower_http=info".into()),
        )
        .init();

    // Resolve ffmpeg + ffprobe (tự tải nếu máy chưa có), thiếu thì in hướng
    // dẫn rồi thoát — làm NGAY lúc khởi động, trước khi nhận request nào.
    if let Err(msg) = ffmpeg::ensure_bins() {
        eprintln!("{msg}");
        std::process::exit(1);
    }

    // Chuẩn bị thư mục workspace.
    if let Err(e) = std::fs::create_dir_all(jobs::WORKSPACE_DIR) {
        tracing::error!("Không tạo được thư mục workspace: {e}");
        return;
    }

    // Preset: builtin + override từ presets_config.json (nếu có) cạnh file chạy.
    let config_path = std::env::current_dir()
        .ok()
        .map(|d| d.join("presets_config.json"));
    let registry = Registry::load(config_path.as_deref());
    tracing::info!(
        "Đã nạp {} preset (config: {})",
        registry.list().len(),
        config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "không".into())
    );

    let store = JobStore::new();
    let state = AppState { store: store.clone(), registry };
    let app = routes::router(state);

    // Bind mọi interface để preview proxy chạm được từ ngoài (loopback chỉ dùng khi dev).
    let port: u16 = std::env::var("RVM_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Không bind được {addr}: {e}");
            return;
        }
    };
    // Q3: bind 0.0.0.0 để mọi interface lắng nghe được, nhưng URL in ra
    // console + tự mở trình duyệt PHẢI dùng 127.0.0.1 — Chrome/Edge trên
    // Windows chặn http://0.0.0.0 với ERR_ADDRESS_INVALID.
    println!("Ứng dụng đang chạy tại: http://127.0.0.1:{port}");
    println!("Nhấn Ctrl+C để tắt.");

    // Tự mở trình duyệt (không chết nếu thất bại, ví dụ server không có GUI).
    if let Err(e) = open::that(format!("http://127.0.0.1:{port}")) {
        tracing::warn!("Không mở được trình duyệt: {e} — hãy tự mở http://127.0.0.1:{port}");
    }

    // Graceful shutdown: Ctrl+C → kill mọi ffmpeg, dọn workspace.
    let shutdown_store = store.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        if ctrl_c().await.is_ok() {
            tracing::info!("Nhận tín hiệu Ctrl+C, đang tắt...");
            shutdown_store.kill_all().await;
            if let Err(e) = tokio::fs::remove_dir_all(jobs::WORKSPACE_DIR).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("Không dọn được workspace: {e}");
                }
            }
        }
    });

    if let Err(e) = server.await {
        tracing::error!("Lỗi server: {e}");
    }
}

/// Kiểm tra ffmpeg và ffprobe trong PATH.
fn check_media_tools() -> Result<(), String> {
    let missing: Vec<&str> = ["ffmpeg", "ffprobe"]
        .into_iter()
        .filter(|tool| which_tool(tool).is_none())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        let list = missing.join(", ");
        Err(format!(
            "\n==================== THIẾU CÔNG CỤ: {list} ====================\n\
             Ứng dụng cần ffmpeg và ffprobe để xử lý media.\n\
             Hãy cài rồi chạy lại:\n\n\
             Windows (PowerShell):\n  winget install ffmpeg\n\n\
             macOS:\n  brew install ffmpeg\n\n\
             Linux (Debian/Ubuntu):\n  sudo apt install ffmpeg\n\n\
             Sau khi cài, mở terminal/cửa sổ mới để PATH được cập nhật.\n\
             ==============================================================="
        ))
    }
}

/// `which` tối giản không dependency: quét từng thư mục trong PATH.
fn which_tool(tool: &str) -> Option<std::path::PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(tool);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        // Windows: thử cả đuôi .exe.
        let candidate_exe = dir.join(format!("{tool}.exe"));
        if is_executable(&candidate_exe) {
            return Some(candidate_exe);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}
