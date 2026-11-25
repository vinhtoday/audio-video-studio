//! Định nghĩa routes và các handler HTTP (giai đoạn 2).

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::jobs::{ChildJob, Job, JobOptions, JobStatus, JobStore, Uploads, job_dir_abs, run_job};
use crate::presets::Registry;
use crate::storage::SettingsStore;
use crate::STATIC_INDEX;

/// Giới hạn upload: 4GB (tổng multipart).
const MAX_BODY_SIZE: usize = 4 * 1024 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub store: JobStore,
    pub registry: Registry,
    /// A3: giới hạn lưu trữ kho video (settings.json + env override).
    pub settings: SettingsStore,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { axum::response::Html(STATIC_INDEX) }))
        .route("/api/presets", get(list_presets))
        .route("/api/fonts", get(list_fonts))
        .route("/api/jobs", post(create_job))
        .route("/api/jobs/{id}", get(get_job).delete(delete_job))
        .route("/api/jobs/{id}/download", get(download_job))
        .route("/api/jobs/{id}/download/{preset}", get(download_child))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        // Stream inline (hỗ trợ Range) — dùng cho player xem trước + thư viện.
        .route("/api/jobs/{id}/stream/{preset}", get(stream_child))
        // A2: xem trước tên file output theo "Tên sản phẩm" + danh sách preset.
        .route("/api/output-names", post(preview_output_names))
        // A3: giới hạn lưu trữ kho video.
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/library/cleanup", post(cleanup_library))
        // Kho lưu trữ output đã xuất.
        .route("/api/library", get(list_library))
        .route("/api/library/{job}/{preset}/stream", get(library_stream))
        .route("/api/library/{job}/{preset}/download", get(library_download))
        .route("/api/library/{job}/{preset}", delete(library_delete))
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}

/// GET /api/presets — danh sách preset cho UI (kèm cảnh báo max_duration).
async fn list_presets(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let presets = state.registry.list();
    Ok(Json(json!({ "presets": presets })))
}

/// GET /api/fonts — family các font hệ thống cho dropdown (fc-list).
async fn list_fonts() -> Json<serde_json::Value> {
    Json(json!({ "fonts": list_system_fonts() }))
}

/// GET /api/settings — A3: giới hạn kho hiện tại + mức sử dụng thực tế.
async fn get_settings(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let s = state.settings.get().await;
    let usage = crate::storage::library_usage().await;
    Ok(Json(json!({
        "library_max_gb": s.max_gb,
        "library_max_files": s.max_files,
        "library_used_bytes": usage.total_bytes,
        "library_used_files": usage.total_files,
    })))
}

/// PUT /api/settings — cập nhật giới hạn kho (body JSON 2 field, validate).
async fn put_settings(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<Json<serde_json::Value>> {
    let max_gb = body
        .get("library_max_gb")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| AppError::bad_request("Thiếu library_max_gb (số nguyên, 1–500)."))?;
    let max_files = body
        .get("library_max_files")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| AppError::bad_request("Thiếu library_max_files (số nguyên, 10–5000)."))?
        as usize;
    crate::storage::validate_settings(max_gb, max_files).map_err(AppError::bad_request)?;
    let s = crate::storage::LibrarySettings { max_gb, max_files };
    state.settings.set(s).await.map_err(AppError::bad_request)?;
    // Giới hạn mới chặt hơn → dọn kho về dưới ngưỡng mới ngay (không đợi job
    // tiếp theo); job đang chạy được bảo vệ.
    let protected = state.store.active_job_ids().await;
    let res = crate::storage::enforce_library_limits(s, protected).await;
    tracing::info!(
        "Cập nhật giới hạn kho: {max_gb} GB / {max_files} file (dọn {} file)",
        res.deleted
    );
    Ok(Json(json!({ "ok": true, "library_max_gb": max_gb, "library_max_files": max_files })))
}

/// POST /api/library/cleanup — A3: nút "Dọn ngay": xoá file cũ nhất cho đến
/// khi kho còn 70% giới hạn. Trả về số file xoá + byte giải phóng.
async fn cleanup_library(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let s = state.settings.get().await;
    let protected = state.store.active_job_ids().await;
    let res = crate::storage::cleanup_library_to_ratio(s, 0.7, protected).await;
    Ok(Json(json!({
        "deleted": res.deleted,
        "freed_bytes": res.freed_bytes,
        "kept_bytes": res.kept_bytes,
        "kept_files": res.kept_files,
        "warnings": res.warnings,
    })))
}

/// POST /api/output-names — A2: xem trước tên file output sẽ nhận được.
/// Body: `{ "output_name": "…", "strip": bool, "presets": ["tiktok_9x16", …] }`.
/// Trả về `{ "stem": …, "fallback": bool, "files": [{preset_id, name}], "zip": … }`
/// — UI hiển thị trực tiếp, người dùng biết chính xác tên file cuối.
async fn preview_output_names(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<Json<serde_json::Value>> {
    let name = body.get("output_name").and_then(|v| v.as_str()).unwrap_or("");
    let strip = body
        .get("strip")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Danh sách preset id (giới hạn 8 như tạo job thật).
    let presets: Vec<String> = body
        .get("presets")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(|s| s.to_string()))
                .take(8)
                .collect()
        })
        .unwrap_or_default();

    match crate::naming::resolve_stem(name, strip) {
        Some(stem) => {
            let multi = presets.len() > 1;
            let files: Vec<serde_json::Value> = if presets.is_empty() {
                Vec::new()
            } else {
                presets
                    .iter()
                    .map(|p| {
                        json!({
                            "preset_id": p,
                            // Preset lạ/không resolve được vẫn hiện đúng
                            // quy tắc tên (UI đã có id từ /api/presets).
                            "name": crate::naming::preset_filename(&stem, p, multi),
                        })
                    })
                    .collect()
            };
            Ok(Json(json!({
                "stem": stem,
                "fallback": false,
                "files": files,
                "zip": if multi { Some(crate::naming::zip_filename(&stem)) } else { None },
            })))
        }
        // Rỗng sau khi làm sạch → tên mặc định theo preset (fallback=true để
        // UI gợi ý "để trống = tên mặc định").
        None => {
            let _ = state.registry; // registry không cần ở nhánh này
            Ok(Json(json!({
                "stem": serde_json::Value::Null,
                "fallback": true,
                "files": [],
                "zip": serde_json::Value::Null,
            })))
        }
    }
}

/// Liệt kê family font hệ thống — ủy quyền cho module fontconfig
/// (fc-list + fallback Windows, quy tắc Q2c).
fn list_system_fonts() -> Vec<String> {
    crate::fontconfig::list_system_fonts()
}

/// POST /api/jobs — multipart: `videos` (n lần), `audio` (n lần), `options` (JSON).
/// Stream file xuống đĩa theo từng chunk, không load cả file vào RAM.
async fn create_job(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> AppResult<Json<serde_json::Value>> {
    let job_id = Uuid::new_v4();
    let job_dir = job_dir_abs(&job_id);
    tokio::fs::create_dir_all(&job_dir)
        .await
        .map_err(|e| AppError::internal(format!("Không tạo được thư mục job: {e}")))?;

    let mut video_paths: Vec<PathBuf> = Vec::new();
    let mut audio_paths: Vec<PathBuf> = Vec::new();
    let mut options: Option<JobOptions> = None;
    let mut uploads = Uploads::default();

    let result = read_multipart(
        &mut multipart,
        &job_dir,
        &mut video_paths,
        &mut audio_paths,
        &mut options,
        &mut uploads,
    )
    .await;

    let validated = async {
        if let Err(e) = result {
            return Err(e);
        }
        if video_paths.is_empty() {
            return Err(AppError::bad_request("Thiếu file video (cần ít nhất 1)."));
        }
        if audio_paths.is_empty() {
            return Err(AppError::bad_request("Thiếu file audio (cần ít nhất 1)."));
        }
        if audio_paths.len() > 16 {
            return Err(AppError::bad_request("Tối đa 16 file audio."));
        }
        let options = options.take().ok_or_else(|| {
            AppError::bad_request("Thiếu options JSON (presets, fit_mode, ...).")
        })?;

        // Gộp audio (dạng lồng đè dạng flat) NGAY để validate đúng giá trị
        // client thực sự gửi — tránh âm lượng ngoài khoảng bị bỏ qua im lặng.
        let mut options = options;
        options.audio = Some(options.audio());
        // audio() luôn trả về giá trị hợp lệ (fallback default) nên as_ref
        // không bao giờ None — nhưng lấy lỗi rõ ràng thay vì panic nếu có sửa
        // đổi trong tương lai làm hỏng bất biến này.
        let Some(audio) = options.audio.as_ref() else {
            return Err(AppError::bad_request("Tuỳ chọn audio không hợp lệ."));
        };
        if !(0.0..=200.0).contains(&audio.volume) {
            return Err(AppError::bad_request("Âm lượng phải từ 0 đến 200%."));
        }
        if !(0.0..=5.0).contains(&audio.fade_out) {
            return Err(AppError::bad_request("Fade-out phải từ 0 đến 5 giây."));
        }
        if !(0.0..=1.0).contains(&options.xfade_duration) {
            return Err(AppError::bad_request("Thời lượng chuyển cảnh phải từ 0 đến 1 giây."));
        }
        if let Err(e) = crate::ffmpeg::validate_xfade_transition(&options.xfade_transition) {
            return Err(AppError::bad_request(e));
        }
        if options.presets.is_empty() {
            return Err(AppError::bad_request("Chọn ít nhất 1 preset."));
        }
        if options.presets.len() > 8 {
            return Err(AppError::bad_request("Tối đa 8 preset mỗi lần xuất."));
        }

        // Bật overlay mà bật fast_mode → tự tắt copy và báo lại cho UI.
        let overlay_on = options.text.active()
            || options.subtitle.active()
            || options.watermark.active_text()
            || options.watermark.active_logo();
        let fast_mode_adjusted = if audio.fast_mode && overlay_on {
            false
        } else {
            audio.fast_mode
        };

        // Phụ đề bật: hoặc file .srt upload, hoặc chế độ Tự động (jobs sẽ
        // dùng ASR để sinh) — nếu không có gì thì lỗi rõ ràng.
        if options.subtitle.active()
            && uploads.subtitle.is_none()
            && !options.subtitle.auto
        {
            return Err(AppError::bad_request(
                "Bật phụ đề nhưng chưa chọn Tự động hoặc upload file .srt.",
            ));
        }
        // Watermark logo bật nhưng thiếu file → lỗi rõ ràng.
        if options.watermark.active_logo() && uploads.logo.is_none() {
            return Err(AppError::bad_request(
                "Chọn watermark logo nhưng chưa upload file ảnh (PNG).",
            ));
        }
        // Font upload nhưng thiếu file → fallback về mặc định an toàn.
        for fc in [&options.text.font, &options.watermark.font, &options.subtitle.font] {
            if let crate::textover::FontChoice::Upload { id } = fc {
                if !uploads.fonts.contains_key(id) {
                    return Err(AppError::bad_request(format!(
                        "Chọn font upload \"{id}\" nhưng không có file font tương ứng."
                    )));
                }
            }
        }

        // A3: kiểm tra đĩa trước khi render — cần ước tính tổng input × 3
        // (chuẩn hoá + ghép + output). Thiếu chỗ → từ chối ngay, báo số GB
        // còn/cần để người dùng tự dọn hoặc chọn preset gọn hơn.
        {
            let mut input_bytes: u64 = 0;
            for p in video_paths.iter().chain(audio_paths.iter()) {
                if let Ok(m) = tokio::fs::metadata(p).await {
                    input_bytes += m.len();
                }
            }
            if let Some(p) = uploads.subtitle.as_ref() {
                if let Ok(m) = tokio::fs::metadata(p).await {
                    input_bytes += m.len();
                }
            }
            if let Some(p) = uploads.logo.as_ref() {
                if let Ok(m) = tokio::fs::metadata(p).await {
                    input_bytes += m.len();
                }
            }
            for p in uploads.fonts.values() {
                if let Ok(m) = tokio::fs::metadata(p).await {
                    input_bytes += m.len();
                }
            }
            let need_bytes = input_bytes.saturating_mul(3);
            match crate::storage::disk_free_bytes(std::path::Path::new(".")) {
                Ok(free) if free < need_bytes => {
                    let free_gb = free as f64 / 1e9;
                    let need_gb = need_bytes as f64 / 1e9;
                    return Err(AppError::bad_request(format!(
                        "Ổ đĩa không đủ chỗ: cần khoảng {need_gb:.1} GB (tổng input × 3 cho \
                         file trung gian + output) nhưng chỉ còn {free_gb:.1} GB trống. Hãy xoá \
                         bớt file ở Kho video hoặc chọn preset nhỏ hơn."
                    )));
                }
                Ok(_) => {}
                // Không đọc được dung lượng đĩa (filesystem lạ) → không chặn
                // job vì một phép đo hỏng; log để debug.
                Err(e) => {
                    tracing::warn!("Không đọc được dung lượng đĩa: {e}");
                }
            }
        }

        // Resolve trước để lỗi preset hiện ngay trong response.
        let mut children: Vec<ChildJob> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for choice in &options.presets {
            let target = state
                .registry
                .resolve_target(&choice.id, choice.custom_width, choice.custom_height)
                .map_err(AppError::bad_request)?;
            if !seen.insert(target.preset_id.clone()) {
                return Err(AppError::bad_request(format!(
                    "Preset \"{}\" được chọn 2 lần.",
                    target.preset_id
                )));
            }
            children.push(ChildJob {
                preset_id: target.preset_id,
                label: target.label,
                status: JobStatus::Queued,
                progress: 0.0,
                error: None,
                filename: None,
            });
        }
        let mut options = options;
        if let Some(a) = options.audio.as_mut() {
            a.fast_mode = fast_mode_adjusted;
        }
        let fast_mode_note = if overlay_on && !fast_mode_adjusted {
            Some("Đã tự tắt chế độ nhanh vì có chữ/phụ đề/watermark (cần re-encode).")
        } else {
            None
        };
        Ok((options, children, fast_mode_note))
    }
    .await;

    let (options, children, fast_mode_note) = match validated {
        Ok(v) => v,
        Err(e) => {
            cleanup_dir(&job_dir).await;
            return Err(e);
        }
    };

    let total = children.len();
    // A2: làm sạch "Tên sản phẩm" NGAY lúc tạo job (giá trị hiển thị preview
    // và tên file zip cần thống nhất từ đầu).
    let output_stem = options
        .output_name
        .as_deref()
        .and_then(|n| crate::naming::resolve_stem(n, options.output_name_strip));
    let job = Job {
        id: job_id,
        status: JobStatus::Queued,
        progress: 0.0,
        error: None,
        zip: None,
        done_count: 0,
        total_count: total,
        children,
        cancel: None,
        output_stem,
    };
    state.store.insert(job).await;

    // Chạy nền, không block response.
    let store = state.store.clone();
    let registry = state.registry.clone();
    tokio::spawn(run_job(
        store,
        registry,
        job_id,
        options,
        uploads,
        video_paths,
        audio_paths,
    ));

    let mut resp = json!({ "job_id": job_id.to_string() });
    if let Some(note) = fast_mode_note {
        resp["fast_mode_note"] = json!(note);
    }
    Ok(Json(resp))
}

#[allow(clippy::too_many_arguments)]
async fn read_multipart(
    multipart: &mut Multipart,
    job_dir: &std::path::Path,
    video_paths: &mut Vec<PathBuf>,
    audio_paths: &mut Vec<PathBuf>,
    options: &mut Option<JobOptions>,
    uploads: &mut Uploads,
) -> AppResult<()> {
    let mut video_index = 0usize;
    let mut audio_index = 0usize;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(format!("Multipart không hợp lệ: {e}")))?
    {
        match field.name().unwrap_or_default() {
            // Giai đoạn 1 dùng tên "video" — vẫn chấp nhận để tương thích ngược.
            "videos" | "video" => {
                let path = job_dir.join(format!("input_video_{video_index:03}"));
                stream_to_file(&mut field, &path).await?;
                video_paths.push(path);
                video_index += 1;
            }
            "audio" => {
                // Nhiều file audio: giữ đúng thứ tự upload (UI sắp xếp trước),
                // jobs sẽ ghép nối tiếp thành 1 file chung.
                let path = job_dir.join(format!("input_audio_{audio_index:03}"));
                stream_to_file(&mut field, &path).await?;
                audio_paths.push(path);
                audio_index += 1;
            }
            // Giai đoạn 3: file .srt phụ đề (1 file duy nhất).
            "subtitle" => {
                let path = job_dir.join("input_subtitle.srt");
                stream_to_file(&mut field, &path).await?;
                uploads.subtitle = Some(path);
            }
            // Giai đoạn 3: logo ảnh (PNG/JPG/WEBP — giữ đuôi gốc, ffmpeg sniff).
            "logo" => {
                let orig = field
                    .file_name()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "logo.png".to_string());
                let ext = orig
                    .rsplit('.')
                    .next()
                    .map(|e| e.to_ascii_lowercase())
                    .filter(|e| ["png", "jpg", "jpeg", "webp", "gif", "bmp"].contains(&e.as_str()))
                    .unwrap_or_else(|| "png".to_string());
                let path = job_dir.join(format!("input_logo.{ext}"));
                stream_to_file(&mut field, &path).await?;
                uploads.logo = Some(path);
            }
            // Giai đoạn 3: font upload — field "font" lặp lại, id lấy từ
            // filename gốc (UI đặt filename = id để đối chiếu FontChoice::Upload).
            "font" => {
                let id = field
                    .file_name()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| format!("font_{video_index}"));
                let path = job_dir.join(format!("font_{}", sanitize_filename(&id)));
                stream_to_file(&mut field, &path).await?;
                uploads.fonts.insert(id, path);
            }
            "options" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| AppError::bad_request(format!("Không đọc được options: {e}")))?;
                *options = Some(serde_json::from_str(&text).map_err(|e| {
                    AppError::bad_request(format!("options JSON không hợp lệ: {e}"))
                })?);
            }
            other => {
                // Field lạ: drain để parser multipart không lỗi.
                tracing::debug!("Bỏ qua field multipart lạ: {other}");
                while field
                    .chunk()
                    .await
                    .map_err(|e| AppError::bad_request(format!("Đọc body bị lỗi: {e}")))?
                    .is_some()
                {}
            }
        }
    }
    Ok(())
}

/// Stream 1 field multipart xuống file theo từng chunk.
async fn stream_to_file(
    field: &mut axum::extract::multipart::Field<'_>,
    path: &std::path::Path,
) -> Result<(), AppError> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| AppError::internal(format!("Không tạo được file tạm: {e}")))?;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| AppError::bad_request(format!("Đọc upload bị lỗi: {e}")))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|e| AppError::internal(format!("Ghi file tạm bị lỗi: {e}")))?;
    }
    file.flush()
        .await
        .map_err(|e| AppError::internal(format!("Flush file tạm bị lỗi: {e}")))?;
    Ok(())
}

/// GET /api/jobs/:id — `{status, progress, children[], ...}`.
async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let uuid = parse_uuid(&id)?;
    match state.store.get_status(&uuid).await {
        Some(resp) => Ok(Json(
            serde_json::to_value(&resp).unwrap_or_else(|_| json!({ "status": "error" })),
        )),
        None => Err(AppError::not_found("Không tìm thấy job.")),
    }
}

/// GET /api/jobs/:id/download — zip (nếu có) hoặc file duy nhất.
async fn download_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let uuid = parse_uuid(&id)?;

    // Ưu tiên zip khi có nhiều output. Tên tải về: "Tên sản phẩm".zip nếu
    // người dùng đặt tên (A2), không thì giữ tên cũ.
    if let Some(zip_path) = state.store.get_zip(&uuid).await {
        let zip_name = state
            .store
            .get_output_zip_name(&uuid)
            .await
            .unwrap_or_else(|| "radio_outputs.zip".to_string());
        return serve_file(&zip_path, &zip_name, "application/zip").await;
    }

    // Fallback: file output đầu tiên (nếu chỉ 1 preset).
    let status = state
        .store
        .get_status(&uuid)
        .await
        .ok_or_else(|| AppError::not_found("Job chưa hoàn tất hoặc không tồn tại."))?;
    let Some(child) = status.children.iter().find(|c| c.filename.is_some()) else {
        return Err(AppError::not_found("Job chưa có file output nào."));
    };
    // find() ở trên đã lọc c.filename.is_some() nên luôn lấy được tên file;
    // clone() không panic được nhưng vẫn xử lý None đề phòng bất biến bị phá.
    let Some(filename) = child.filename.clone() else {
        return Err(AppError::not_found("Job chưa có file output nào."));
    };
    // Workspace truoc, library sau (job cu da don workspace).
    let mut path = job_dir_abs(&uuid).join(&filename);
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        path = std::path::PathBuf::from("library")
            .join(uuid.to_string())
            .join(format!("{}__{filename}", child.preset_id));
    }
    serve_file(&path, &filename, "video/mp4").await
}

/// GET /api/jobs/:id/download/:preset — file mp4 của 1 preset cụ thể.
async fn download_child(
    State(state): State<AppState>,
    Path((id, preset)): Path<(String, String)>,
) -> AppResult<Response> {
    let uuid = parse_uuid(&id)?;
    let path = state
        .store
        .get_child_output(&uuid, &preset)
        .await
        .ok_or_else(|| AppError::not_found("Không tìm thấy output của preset này."))?;
    // A2: tên tải về dùng tên hiển thị người dùng đặt (child.filename), KHÔNG
    // dùng tên vật lý trong kho `<preset_id>__<tên>` khi rơi về fallback.
    let display = state
        .store
        .get_child_display_name(&uuid, &preset)
        .await
        .unwrap_or_else(|| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    serve_file(&path, &display, "video/mp4").await
}

async fn serve_file(path: &std::path::Path, filename: &str, mime: &str) -> AppResult<Response> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| AppError::internal(format!("Không mở được file output: {e}")))?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(match mime {
            "application/zip" => "application/zip",
            _ => "video/mp4",
        }),
    );
    // A2: filename ASCII fallback + filename* UTF-8 percent-encode → trình
    // duyệt hiện đúng tên tiếng Việt khi tải về.
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&crate::naming::content_disposition(true, filename))
            .map_err(|_| AppError::internal("Tên file không hợp lệ"))?,
    );

    Ok((headers, body).into_response())
}

// ---------------------------------------------------------------------------
// Stream inline (player trong app) — hỗ trợ HTTP Range để seek được.
// ---------------------------------------------------------------------------

/// GET /api/jobs/:id/stream/:preset — stream output của 1 preset (inline).
async fn stream_child(
    State(state): State<AppState>,
    Path((id, preset)): Path<(String, String)>,
    req_headers: HeaderMap,
) -> AppResult<Response> {
    let uuid = parse_uuid(&id)?;
    let path = state
        .store
        .get_child_output(&uuid, &preset)
        .await
        .ok_or_else(|| AppError::not_found("Không tìm thấy output của preset này."))?;
    let filename = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let range = req_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    serve_video_range(&path, &filename, false, range).await
}

/// Serve file video hỗ trợ Range (206 Partial Content) — dùng cho cả stream
/// inline (player) lẫn download attachment.
async fn serve_video_range(
    path: &std::path::Path,
    filename: &str,
    download: bool,
    range: Option<String>,
) -> AppResult<Response> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| AppError::internal(format!("Không đọc được file: {e}")))?;
    let total = metadata.len();
    if total == 0 {
        return Err(AppError::internal("File rỗng."));
    }

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| AppError::internal(format!("Không mở được file: {e}")))?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("video/mp4"));
    headers.insert(header::ACCEPT_RANGES, header::HeaderValue::from_static("bytes"));
    // A2: kèm filename* UTF-8 để tên tải về giữ dấu tiếng Việt.
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&crate::naming::content_disposition(download, filename))
            .map_err(|_| AppError::internal("Tên file lỗi"))?,
    );

    // Range hợp lệ → 206 + đúng đoạn; không có/không hợp lệ → cả file (200).
    let (start, end, partial) = match range.as_deref().and_then(|h| parse_range(h, total)) {
        Some((s, e)) => {
            headers.insert(
                header::CONTENT_RANGE,
                header::HeaderValue::from_str(&format!("bytes {s}-{e}/{total}"))
                    .map_err(|_| AppError::internal("Header lỗi"))?,
            );
            (s, e, true)
        }
        None => (0, total - 1, false),
    };

    file.seek(SeekFrom::Start(start))
        .await
        .map_err(|e| AppError::internal(format!("Seek lỗi: {e}")))?;

    // STREAM THẬT: ReaderStream + take đúng số byte — file to bao nhiêu cũng
    // không phình RAM, client nhận đủ 100% bytes (không còn cắt cụt).
    let stream = tokio_util::io::ReaderStream::with_capacity(
        file.take(end - start + 1),
        256 * 1024,
    );
    let body = axum::body::Body::from_stream(stream);

    headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&(end - start + 1).to_string())
            .map_err(|_| AppError::internal("Header lỗi"))?,
    );

    if partial {
        return Ok((StatusCode::PARTIAL_CONTENT, headers, body).into_response());
    }
    Ok((StatusCode::OK, headers, body).into_response())
}

/// `bytes=0-499` / `bytes=-500` / `bytes=500-` → (start, end). Không hỗ trợ multi-range.
fn parse_range(h: &str, total: u64) -> Option<(u64, u64)> {
    let rest = h.trim().strip_prefix("bytes=")?;
    let (s, e) = rest.split_once('-')?;
    // Hai dạng: "bytes=-N" (N byte cuối) và "bytes=S-E" / "bytes=S-".
    let (start, end) = if s.is_empty() {
        let n: u64 = e.parse().ok()?;
        if n == 0 {
            return None; // bytes=-0 không hợp lệ
        }
        (total.checked_sub(n)?, total - 1)
    } else {
        let start: u64 = s.parse().ok()?;
        let end = if e.is_empty() {
            total - 1
        } else {
            e.parse::<u64>().ok()?.min(total - 1)
        };
        (start, end)
    };
    if start > end {
        return None;
    }
    Some((start, end))
}

// ---------------------------------------------------------------------------
// Kho lưu trữ (library) — ./library/<job_id>/<preset>__<file>.mp4
// ---------------------------------------------------------------------------

/// GET /api/library — liệt kê mọi output đã lưu trong kho (mới nhất trước).
async fn list_library(State(_state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let mut items: Vec<serde_json::Value> = Vec::new();
    let lib = std::path::PathBuf::from("library");
    // Đọc thư mục KHÔNG chặn runtime (tokio::fs, chạy trên blocking pool).
    let mut job_dirs: Vec<_> = match tokio::fs::read_dir(&lib).await {
        Ok(mut rd) => {
            let mut v = Vec::new();
            while let Ok(Some(e)) = rd.next_entry().await {
                if e.path().is_dir() {
                    v.push(e);
                }
            }
            v
        }
        Err(_) => Vec::new(), // chưa có kho → rỗng
    };
    // Mới nhất trước theo mtime thư mục.
    job_dirs.sort_by_key(|e| {
        std::cmp::Reverse(
            std::fs::metadata(e.path()).ok().and_then(|m| m.modified().ok()),
        )
    });
    for jd in job_dirs {
        let job_id = jd.file_name().to_string_lossy().into_owned();
        let mut files = Vec::new();
        if let Ok(mut rd) = tokio::fs::read_dir(jd.path()).await {
            while let Ok(Some(e)) = rd.next_entry().await {
                if e.path().is_file() {
                    files.push(e);
                }
            }
        } else {
            continue;
        }
        files.sort_by_key(|e| e.file_name().to_string_lossy().into_owned());
        for f in files {
            let name = f.file_name().to_string_lossy().into_owned();
            // Tên file: <preset_id>__<filename gốc>.
            let (preset_id, display) = name
                .split_once("__")
                .unwrap_or((name.as_str(), name.as_str()));
            let display = display.to_string();
            // std::fs::metadata (path) — sync nhưng nhẹ, gọi 1 lần/file.
            let meta = std::fs::metadata(f.path()).ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            items.push(json!({
                "job_id": job_id,
                "preset_id": preset_id,
                "file": name,
                // A2: tên hiển thị cho UI (phần sau "__").
                "display_name": display,
                "size": size,
                "modified": modified,
                "url_stream": format!("/api/library/{job_id}/{preset_id}/stream?file={}", urlencode(&name)),
                "url_download": format!("/api/library/{job_id}/{preset_id}/download?file={}", urlencode(&name)),
            }));
        }
    }
    Ok(Json(json!({ "items": items, "total": items.len() })))
}

/// GET /api/library/:job/:preset/stream?file=<tên file trong dir job>
async fn library_stream(
    Path((job, preset)): Path<(String, String)>,
    axum::extract::RawQuery(q): axum::extract::RawQuery,
    req_headers: HeaderMap,
) -> AppResult<Response> {
    serve_library(job, preset, q, true, req_headers).await
}

/// GET /api/library/:job/:preset/download?file=…
async fn library_download(
    Path((job, preset)): Path<(String, String)>,
    axum::extract::RawQuery(q): axum::extract::RawQuery,
    req_headers: HeaderMap,
) -> AppResult<Response> {
    serve_library(job, preset, q, false, req_headers).await
}

async fn serve_library(
    job: String,
    preset: String,
    q: Option<String>,
    inline: bool,
    req_headers: HeaderMap,
) -> AppResult<Response> {
    // A2: chặn path traversal nhưng GIỮ nguyên Unicode (tên file có dấu
    // tiếng Việt phải tra cứu được; sanitize ASCII cũ làm sai tên).
    let (job, preset) = (sanitize_filename(&job), sanitize_filename(&preset));
    let file_param = q
        .as_deref()
        .and_then(|s| s.strip_prefix("file="))
        .map(urldecode)
        .unwrap_or_default();
    let file_name = crate::naming::safe_lookup_name(&file_param)
        .ok_or_else(|| AppError::bad_request("Tên file không hợp lệ."))?;
    let path = std::path::PathBuf::from("library")
        .join(&job)
        .join(&file_name);
    if !path.exists() {
        return Err(AppError::not_found("File không còn trong kho."));
    }
    // Tên hiển thị: phần sau "__" nếu có.
    let display = file_name
        .split_once("__")
        .map(|(_, f)| f.to_string())
        .unwrap_or_else(|| file_name.clone());
    let _ = preset;
    let range = req_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    serve_video_range(&path, &display, !inline, range).await
}

/// DELETE /api/library/:job/:preset?file=… — xoá 1 file khỏi kho.
async fn library_delete(
    Path((job, preset)): Path<(String, String)>,
    axum::extract::RawQuery(q): axum::extract::RawQuery,
) -> AppResult<Json<serde_json::Value>> {
    let job = sanitize_filename(&job);
    let _ = preset;
    let file_param = q
        .as_deref()
        .and_then(|s| s.strip_prefix("file="))
        .map(urldecode)
        .unwrap_or_default();
    // A2: như serve_library — chặn traversal, giữ Unicode.
    let file_name = crate::naming::safe_lookup_name(&file_param)
        .ok_or_else(|| AppError::bad_request("Tên file không hợp lệ."))?;
    let path = std::path::PathBuf::from("library").join(&job).join(&file_name);
    if !path.exists() {
        return Err(AppError::not_found("File không còn trong kho."));
    }
    tokio::fs::remove_file(&path)
        .await
        .map_err(|e| AppError::internal(format!("Không xoá được: {e}")))?;
    // Thư mục job rỗng → xoá luôn cho gọn.
    if let Some(dir) = path.parent() {
        if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
            if rd.next_entry().await.ok().flatten().is_none() {
                let _ = tokio::fs::remove_dir(dir).await;
            }
        }
    }
    Ok(Json(json!({ "deleted": true })))
}

/// Percent-encode tối thiểu cho query string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode %XX và '+' trong query string.
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => match b
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(v) => {
                    out.push(v);
                    i += 3;
                }
                None => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// POST /api/jobs/:id/cancel — kill ffmpeg đang chạy + đánh dấu cancelled.
async fn cancel_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let uuid = parse_uuid(&id)?;
    if state.store.cancel(&uuid).await {
        // A3: huỷ xong → xoá ngay thư mục input/trung gian của job (không
        // đợi tắt app) — fail() đã dọn cho job lỗi, đây là nhánh huỷ.
        let dir = job_dir_abs(&uuid);
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(job = %uuid, "Không dọn được job dir sau huỷ: {e}");
            }
        }
        Ok(Json(json!({ "cancelled": true })))
    } else {
        Ok(Json(json!({
            "cancelled": false,
            "note": "Job không còn đang chạy (đã xong, lỗi hoặc bị huỷ trước đó)."
        })))
    }
}

/// DELETE /api/jobs/:id — kill ffmpeg (nếu còn) + xoá thư mục tạm.
async fn delete_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let uuid = parse_uuid(&id)?;
    let dir = state
        .store
        .remove(&uuid)
        .await
        .ok_or_else(|| AppError::not_found("Không tìm thấy job."))?;
    cleanup_dir(&dir).await;
    Ok(Json(json!({ "deleted": true })))
}

async fn cleanup_dir(dir: &std::path::Path) {
    if let Err(e) = tokio::fs::remove_dir_all(dir).await {
        tracing::warn!("Không xoá được thư mục {dir:?}: {e}");
    }
}

fn parse_uuid(id: &str) -> AppResult<Uuid> {
    id.parse::<Uuid>()
        .map_err(|_| AppError::bad_request("job_id không hợp lệ."))
}

/// Làm sạch tên file upload để làm tên file trên đĩa: chỉ giữ chữ/số/dấu gạch.
fn sanitize_filename(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    // Giới hạn 64 ký tự (giữ đuôi mở rộng nếu có).
    if clean.len() > 64 {
        let ext = clean.rfind('.').filter(|i| *i > 48);
        match ext {
            Some(i) => format!("{}{}", &clean[..40], &clean[i..]),
            None => clean[..64].to_string(),
        }
    } else {
        clean
    }
}

#[cfg(test)]
mod range_tests {
    use super::parse_range;

    #[test]
    fn range_binh_thuong() {
        assert_eq!(parse_range("bytes=0-499", 1000), Some((0, 499)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=0-0", 1000), Some((0, 0)));
    }

    #[test]
    fn range_suffix() {
        // 500 byte cuối của file 1000: [500, 999].
        assert_eq!(parse_range("bytes=-500", 1000), Some((500, 999)));
        // suffix lớn hơn file → cả file.
        assert_eq!(parse_range("bytes=-2000", 1000), None);
        assert_eq!(parse_range("bytes=-0", 1000), None);
        // suffix đúng bằng file.
        assert_eq!(parse_range("bytes=-1000", 1000), Some((0, 999)));
    }

    #[test]
    fn range_bien_gioi() {
        // end vượt total → kẹp về total-1.
        assert_eq!(parse_range("bytes=0-99999", 1000), Some((0, 999)));
        // start vượt total → None (416).
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=2000-3000", 1000), None);
        // start > end.
        assert_eq!(parse_range("bytes=600-500", 1000), None);
        // thiếu prefix / rác.
        assert_eq!(parse_range("items=0-5", 1000), None);
        assert_eq!(parse_range("bytes=abc", 1000), None);
        // khoảng trắng quanh (curl/ffmpeg hay gửi).
        assert_eq!(parse_range(" bytes=0-499 ", 1000), Some((0, 499)));
    }
}
