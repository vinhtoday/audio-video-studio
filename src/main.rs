mod error;
mod ffmpeg;
mod fontconfig;
mod fontinfo;
mod jobs;
mod naming;
mod storage;
mod presets;
mod routes;
mod split;
mod subtitle;
mod textover;
mod visuals;
mod zipwriter;
mod datadir;

use jobs::JobStore;
use presets::Registry;
use routes::AppState;
use tokio::signal::ctrl_c;
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

/// Frontend nhúng thẳng vào binary bằng include_str!.
static STATIC_INDEX: &str = include_str!("../static/index.html");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "radio_video_merger=info".into()),
        )
        .init();

    // Resolve ffmpeg + ffprobe (tự tải nếu máy chưa có), thiếu thì in hướng
    // dẫn rồi thoát — làm NGAY lúc khởi động, trước khi nhận request nào.
    if let Err(msg) = ffmpeg::ensure_bins() {
        eprintln!("{msg}");
        std::process::exit(1);
    }

    // M8: thư mục dữ liệu (RVM_DATA_DIR / tương thích CWD / cạnh exe) — in
    // đường dẫn đang dùng + cảnh báo nếu rơi vào nhánh tương thích ngược.
    let data_dir = datadir::data_dir();
    std::fs::create_dir_all(&data_dir).ok();

    // Chuẩn bị thư mục workspace (trong thư mục dữ liệu).
    if let Err(e) = std::fs::create_dir_all(jobs::workspace_dir()) {
        tracing::error!("Không tạo được thư mục workspace ({}): {e}", jobs::workspace_dir().display());
        return;
    }
    // Dọn thư mục job mồ côi từ lần chạy trước bị kill đột ngột.
    jobs::cleanup_orphan_job_dirs();

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
    // A3: giới hạn kho video (settings.json + env override) — gắn vào store
    // để run_job dọn kho sau mỗi job con.
    let settings = storage::SettingsStore::load();
    let store = store.with_settings(settings.clone());
    let state = AppState { store: store.clone(), registry, settings: settings.clone() };
    let app = routes::router(state);

    // A3: dọn kho về dưới giới hạn ngay lúc khởi động (kho có thể phình to
    // từ lần chạy trước); lúc này chưa có job nào chạy nên không cần bảo vệ.
    {
        let s = settings.get().await;
        let res = storage::enforce_library_limits(s, Vec::new()).await;
        if res.deleted > 0 {
            tracing::info!(
                "Dọn kho lúc khởi động: xoá {} file, giải phóng {:.2} GB",
                res.deleted,
                res.freed_bytes as f64 / 1e9
            );
        }
        for w in &res.warnings {
            tracing::warn!("Kho video: {w}");
        }
    }

    // Bind mặc định loopback (an toàn: app local, không lộ ra LAN). Muốn mở
    // ra ngoài (dùng từ máy khác/điện thoại trong mạng), đặt RVM_HOST=0.0.0.0
    // (hoặc IP cụ thể) trước khi chạy.
    let port: u16 = std::env::var("RVM_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let host: std::net::IpAddr = std::env::var("RVM_HOST")
        .ok()
        .and_then(|h| h.parse().ok())
        .unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
    let addr = std::net::SocketAddr::from((host, port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Không bind được {addr}: {e}");
            return;
        }
    };
    // URL in ra console + tự mở trình duyệt luôn dùng 127.0.0.1 (hoặc giá trị
    // RVM_HOST nếu người dùng tự đặt) — Chrome/Edge trên Windows chặn
    // http://0.0.0.0 với ERR_ADDRESS_INVALID.
    let display_host = if host.is_loopback() {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    };
    println!("Ứng dụng đang chạy tại: http://{display_host}:{port}");
    println!("Nhấn Ctrl+C để tắt.");

    // Tự mở trình duyệt (không chết nếu thất bại, ví dụ server không có GUI).
    if let Err(e) = open::that(format!("http://{display_host}:{port}")) {
        tracing::warn!("Không mở được trình duyệt: {e} — hãy tự mở http://{display_host}:{port}");
    }

    // Graceful shutdown: Ctrl+C HOẶC SIGTERM (kill, systemd stop, docker
    // stop) → kill mọi ffmpeg, dọn workspace.
    let shutdown_store = store.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        if shutdown_signal().await.is_some() {
            tracing::info!("Nhận tín hiệu dừng, đang tắt...");
            shutdown_store.kill_all().await;
            let ws = jobs::workspace_dir();
            if let Err(e) = tokio::fs::remove_dir_all(&ws).await {
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

/// Chờ tín hiệu dừng: Ctrl+C luôn; SIGTERM (kill/systemd/docker stop) trên
/// unix. Trả về Some(()) khi nhận được tín hiệu, None nếu cài handler lỗi
/// (thì cứ mặc định tiếp tục chạy).
async fn shutdown_signal() -> Option<()> {
    #[cfg(unix)]
    {
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Không cài được handler SIGTERM: {e}");
                return ctrl_c().await.ok();
            }
        };
        tokio::select! {
            _ = ctrl_c() => Some(()),
            _ = sigterm.recv() => Some(()),
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c().await.ok()
    }
}
