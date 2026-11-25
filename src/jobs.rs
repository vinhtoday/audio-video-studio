//! Quản lý trạng thái job (giai đoạn 2): 1 **job cha** = 1 lần upload
//! (n video + 1 audio + danh sách preset), chứa N **job con** — mỗi job con
//! render 1 preset qua pipeline 3 bước:
//!
//! 1. Chuẩn hoá từng clip (bỏ audio, đồng nhất WxH/fps/pix_fmt).
//! 2. Ghép chuỗi (concat hoặc xfade).
//! 3. Lặp chuỗi cho hết audio + ghép audio radio + cắt `-t`.
//!
//! Job cha chạy nền bằng `tokio::spawn`, các job con render SONG SONG với
//! giới hạn đồng thời (Semaphore — mặc định ncpu/2, tối đa 4; ép bằng biến
//! môi trường `RVM_MAX_PARALLEL`) để các ffmpeg không giành CPU lẫn nhau.
//! Cancel job cha huỷ token cha → mọi ffmpeg con đang chạy bị kill đồng loạt.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::ffmpeg::{self, build_final_args, build_normalize_args, build_concat_args, build_xfade_args, concat_list_content, probe, validate_xfade, FitMode};
use crate::presets::{Registry, RenderTarget};
use crate::textover::{FrameCtx, SubtitleOptions, TextOverlayOptions, WatermarkOptions};
use crate::visuals::{self, VisualOptions};

/// Thư mục làm việc: `./workspace/<job_id>/`.
pub const WORKSPACE_DIR: &str = "./workspace";

/// Thư mục kho output đã xuất (A2/A3).
pub const LIBRARY_DIR: &str = "library";

/// Các file đính kèm giai đoạn 3 (đã lưu xuống job_dir khi parse multipart).
#[derive(Debug, Default)]
pub struct Uploads {
    pub subtitle: Option<PathBuf>,
    pub logo: Option<PathBuf>,
    /// Font upload: id (tên file gốc) → đường dẫn đã lưu.
    pub fonts: HashMap<String, PathBuf>,
}

/// Đường dẫn tuyệt đối tới thư mục job. Concat demuxer giải quy đường dẫn
/// trong list.txt theo thư mục chứa list.txt nên mọi path phải absolute.
pub fn job_dir_abs(job_id: &Uuid) -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(WORKSPACE_DIR)
        .join(job_id.to_string())
}

// ---------------------------------------------------------------------------
// Tuỳ chọn
// ---------------------------------------------------------------------------

/// Tuỳ chọn audio (giữ đúng nghĩa từ giai đoạn 1).
#[derive(Debug, Clone, Deserialize)]
pub struct AudioOptions {
    /// Âm lượng audio radio, % (0–200). Mặc định 100.
    #[serde(default = "default_volume")]
    pub volume: f64,
    /// Fade-out cuối bài, giây (0–5). Mặc định 0.
    #[serde(default = "default_fade")]
    pub fade_out: f64,
    /// Chế độ nhanh ở pass cuối: `-c:v copy`. Mặc định tắt.
    #[serde(default)]
    pub fast_mode: bool,
    /// Mức chất lượng encode: "fast" | "balanced" | "high". Mặc định balanced.
    #[serde(default = "default_quality")]
    pub quality: String,
}

fn default_quality() -> String {
    "balanced".into()
}

fn default_volume() -> f64 {
    100.0
}
fn default_fade() -> f64 {
    0.0
}
fn default_transition() -> String {
    "fade".to_string()
}
fn default_xfade() -> f64 {
    0.0
}
fn default_fit() -> FitMode {
    FitMode::Cover
}

impl Default for AudioOptions {
    fn default() -> Self {
        Self {
            volume: default_volume(),
            fade_out: default_fade(),
            fast_mode: false,
            quality: default_quality(),
        }
    }
}

/// Lựa chọn preset từ UI: id (hoặc "custom" + kích thước).
#[derive(Debug, Clone, Deserialize)]
pub struct PresetChoice {
    pub id: String,
    #[serde(default)]
    pub custom_width: Option<u32>,
    #[serde(default)]
    pub custom_height: Option<u32>,
}

/// Tuỳ chọn đầy đủ của 1 job (field `options` trong multipart).
///
/// Audio nhận 2 dạng: flat (volume/fade_out/fast_mode ở gốc — UI hiện tại)
/// hoặc lồng `audio:{...}`. Cả hai cùng tồn tại thì dạng lồng thắng.
#[derive(Debug, Clone, Deserialize)]
pub struct JobOptions {
    #[serde(default)]
    pub audio: Option<AudioOptions>,
    #[serde(default = "default_volume", alias = "volume")]
    volume_flat: f64,
    #[serde(default = "default_fade", alias = "fade_out")]
    fade_flat: f64,
    #[serde(default, alias = "fast_mode")]
    fast_mode_flat: bool,
    #[serde(default = "default_quality", alias = "quality")]
    quality_flat: String,
    /// Chế độ khớp khung cho các preset cố định kích thước.
    #[serde(default = "default_fit")]
    pub fit_mode: FitMode,
    /// Chuyển cảnh xfade giữa các clip: 0 = tắt.
    #[serde(default = "default_xfade")]
    pub xfade_duration: f64,
    /// Hiệu ứng chuyển cảnh (tên xfade của ffmpeg).
    #[serde(default = "default_transition")]
    pub xfade_transition: String,
    /// Danh sách preset cần xuất (1 phần tử = 1 job con).
    pub presets: Vec<PresetChoice>,
    /// Giai đoạn 3: chữ overlay.
    #[serde(default)]
    pub text: TextOverlayOptions,
    /// Giai đoạn 3: phụ đề burn-in.
    #[serde(default)]
    pub subtitle: SubtitleOptions,
    /// Giai đoạn 3: watermark text/logo.
    #[serde(default)]
    pub watermark: WatermarkOptions,
    /// Hiệu ứng hình ảnh: visualizer theo audio, thanh tiến độ.
    #[serde(default)]
    pub visuals: VisualOptions,
    /// A2: "Tên sản phẩm" — tên file output người dùng nhập (tuỳ chọn).
    /// Trống/không gửi → giữ cách đặt tên mặc định (`radio_<preset>…`).
    #[serde(default)]
    pub output_name: Option<String>,
    /// A2: bỏ dấu tiếng Việt trong tên file (đ→d, khoảng trắng→`-`).
    #[serde(default)]
    pub output_name_strip: bool,
}

impl JobOptions {
    /// Audio đã gộp: dạng lồng `audio:{...}` (nếu có) đè lên giá trị flat.
    pub fn audio(&self) -> AudioOptions {
        self.audio.clone().unwrap_or(AudioOptions {
            volume: self.volume_flat,
            fade_out: self.fade_flat,
            fast_mode: self.fast_mode_flat,
            quality: self.quality_flat.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Trạng thái job cha / job con
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Done,
    Error,
    Cancelled,
}

/// Trạng thái 1 job con (1 preset).
#[derive(Debug, Clone, Serialize)]
pub struct ChildJob {
    pub preset_id: String,
    pub label: String,
    pub status: JobStatus,
    /// 0–100.
    pub progress: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Tên file output (chỉ Some khi done) — dùng cho `/download` và zip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Debug)]
pub struct Job {
    pub id: Uuid,
    pub status: JobStatus,
    /// Tiến độ tổng 0–100 (trung bình các job con).
    pub progress: f64,
    pub error: Option<String>,
    /// Đường dẫn file zip (chỉ Some khi done và có nhiều job con).
    pub zip: Option<PathBuf>,
    /// Số file output đã xong (cho label download).
    pub done_count: usize,
    pub total_count: usize,
    pub children: Vec<ChildJob>,
    /// Token kill ffmpeg của job con đang chạy.
    pub cancel: Option<CancellationToken>,
    /// A2: "Tên sản phẩm" đã làm sạch (None → đặt tên mặc định). Dùng để
    /// sinh tên file zip tải về và cho API preview tên file.
    pub output_stem: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct JobStatusResponse {
    pub status: JobStatus,
    pub progress: f64,
    pub done_count: usize,
    pub total_count: usize,
    pub children: Vec<ChildJob>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Có zip sẵn sàng để tải (>= 2 file done).
    pub has_zip: bool,
}

impl From<&Job> for JobStatusResponse {
    fn from(j: &Job) -> Self {
        Self {
            status: j.status,
            progress: j.progress,
            done_count: j.done_count,
            total_count: j.total_count,
            children: j.children.clone(),
            error: j.error.clone(),
            has_zip: j.zip.is_some(),
        }
    }
}

/// Store chia sẻ giữa các route và các task chạy nền.
#[derive(Clone)]
pub struct JobStore {
    jobs: Arc<RwLock<HashMap<Uuid, Job>>>,
    /// A3: giới hạn kho — run_job cần dọn sau mỗi job con mà không nhận
    /// AppState; giữ tham chiếu SettingsStore (None trong unit test).
    settings: Option<crate::storage::SettingsStore>,
}

impl JobStore {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(HashMap::new())),
            settings: None,
        }
    }

    /// A3: gắn settings vào store (main gọi sau khi load settings.json).
    pub fn with_settings(mut self, s: crate::storage::SettingsStore) -> Self {
        self.settings = Some(s);
        self
    }

    /// A3: snapshot giới hạn hiện tại (None khi không cấu hình — bỏ qua dọn).
    fn settings_snapshot(&self) -> Option<crate::storage::LibrarySettings> {
        self.settings.as_ref().map(|s| s.inner_blocking_get())
    }

    pub async fn insert(&self, job: Job) {
        self.jobs.write().await.insert(job.id, job);
    }

    pub async fn get_status(&self, id: &Uuid) -> Option<JobStatusResponse> {
        let jobs = self.jobs.read().await;
        jobs.get(id).map(|j| j.into())
    }

    pub async fn update<F>(&self, id: &Uuid, f: F)
    where
        F: FnOnce(&mut Job),
    {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(id) {
            f(job);
        }
    }

    /// Lấy đường dẫn zip nếu job xong và có zip.
    pub async fn get_zip(&self, id: &Uuid) -> Option<PathBuf> {
        let jobs = self.jobs.read().await;
        jobs.get(id).and_then(|j| j.zip.clone())
    }

    /// A2: tên file zip tải về ("<Tên sản phẩm>.zip"; None khi không đặt tên).
    pub async fn get_output_zip_name(&self, id: &Uuid) -> Option<String> {
        let jobs = self.jobs.read().await;
        jobs.get(id)
            .and_then(|j| j.output_stem.clone())
            .map(|stem| crate::naming::zip_filename(&stem))
    }

    /// Lay duong dan 1 file output cua job con (theo preset id): uu tien
    /// workspace (dang render / xong gan day), fallback library/<job_id>/
    /// <preset_id>__<filename> (job cu sau khi workspace da don).
    /// A2: tên hiển thị (tên tải về) của 1 job con — chính là `child.filename`
    /// (đã là "Tên sản phẩm" nếu người dùng đặt), không lẫn tiền tố kho.
    pub async fn get_child_display_name(&self, id: &Uuid, preset_id: &str) -> Option<String> {
        let jobs = self.jobs.read().await;
        jobs.get(id)?
            .children
            .iter()
            .find(|c| c.preset_id == preset_id)?
            .filename
            .clone()
    }

    /// A3: ID (dạng string, trùng tên thư mục trong library/) của mọi job
    /// đang chờ/đang chạy — dọn kho phải bảo vệ output các job này.
    pub async fn active_job_ids(&self) -> Vec<String> {
        let jobs = self.jobs.read().await;
        jobs.iter()
            .filter(|(_, j)| matches!(j.status, JobStatus::Queued | JobStatus::Running))
            .map(|(id, _)| id.to_string())
            .collect()
    }

    pub async fn get_child_output(&self, id: &Uuid, preset_id: &str) -> Option<PathBuf> {
        let jobs = self.jobs.read().await;
        let job = jobs.get(id)?;
        let child = job.children.iter().find(|c| c.preset_id == preset_id)?;
        let filename = child.filename.as_ref()?.clone();
        let ws_path = job_dir_abs(&job.id).join(&filename);
        if tokio::fs::try_exists(&ws_path).await.unwrap_or(false) {
            return Some(ws_path);
        }
        // Fallback: kho library (output da copy o drain_preset_results).
        let lib = std::path::PathBuf::from(LIBRARY_DIR)
            .join(job.id.to_string())
            .join(format!("{preset_id}__{filename}"));
        if tokio::fs::try_exists(&lib).await.unwrap_or(false) {
            return Some(lib);
        }
        None
    }

    /// Đánh dấu cancelled và kill ffmpeg. Trả về true nếu job đang chạy.
    pub async fn cancel(&self, id: &Uuid) -> bool {
        let mut jobs = self.jobs.write().await;
        match jobs.get_mut(id) {
            Some(job) if matches!(job.status, JobStatus::Queued | JobStatus::Running) => {
                job.status = JobStatus::Cancelled;
                // Các job con chưa kịp chạy cũng đánh dấu cancelled cho nhất quán.
                for c in job.children.iter_mut() {
                    if c.status == JobStatus::Queued {
                        c.status = JobStatus::Cancelled;
                    }
                }
                if let Some(token) = job.cancel.take() {
                    token.cancel();
                }
                true
            }
            _ => false,
        }
    }

    /// Xoá job khỏi store, trả về đường dẫn thư mục để caller dọn file.
    pub async fn remove(&self, id: &Uuid) -> Option<PathBuf> {
        let mut job = self.jobs.write().await.remove(id)?;
        if let Some(token) = job.cancel.take() {
            token.cancel();
        }
        Some(job_dir_abs(&job.id))
    }

    /// Kill mọi ffmpeg còn sống (khi tắt app).
    pub async fn kill_all(&self) {
        let mut jobs = self.jobs.write().await;
        for (_, job) in jobs.iter_mut() {
            if let Some(token) = job.cancel.take() {
                token.cancel();
            }
        }
    }

    async fn get_cancel_token(&self, id: &Uuid) -> Option<CancellationToken> {
        let jobs = self.jobs.read().await;
        jobs.get(id).and_then(|j| j.cancel.clone())
    }

    /// Cập nhật 1 job con theo preset id rồi tính lại tổng trạng thái job cha.
    async fn update_child(&self, id: &Uuid, preset_id: &str, f: impl FnOnce(&mut ChildJob)) {
        let mut jobs = self.jobs.write().await;
        let Some(job) = jobs.get_mut(id) else { return };
        if let Some(child) = job.children.iter_mut().find(|c| c.preset_id == preset_id) {
            f(child);
        }
        recompute_parent(job);
    }

    /// Đánh dấu 1 job con bắt đầu chạy thật (đã có slot) rồi tính lại cha.
    async fn mark_child_running(&self, id: &Uuid, preset_id: &str) {
        self.update_child(id, preset_id, |c| {
            if c.status == JobStatus::Queued {
                c.status = JobStatus::Running;
            }
        })
        .await;
    }

    /// Cập nhật tiến độ job con (được gọi từ callback ffmpeg).
    async fn set_child_progress(&self, id: &Uuid, preset_id: &str, pct: f64) {
        self.update_child(id, preset_id, |c| {
            // Progress chỉ có nghĩa khi con đang chạy — con đã done/error thì
            // giữ nguyên mốc của nó.
            if c.status == JobStatus::Queued || c.status == JobStatus::Running {
                c.progress = pct;
            }
        })
        .await;
    }
}

/// Tính lại progress/status/done_count của job cha từ các job con.
fn recompute_parent(job: &mut Job) {
    let total = job.children.len();
    if total == 0 {
        return;
    }
    let done = job.children.iter().filter(|c| c.status == JobStatus::Done).count();
    let error = job.children.iter().filter(|c| c.status == JobStatus::Error).count();
    let cancelled = job.children.iter().filter(|c| c.status == JobStatus::Cancelled).count();
    job.done_count = done;
    job.total_count = total;
    job.progress = job.children.iter().map(|c| c.progress).sum::<f64>() / total as f64;

    if done + error + cancelled < total {
        // Vẫn còn job con đang chạy/đợi.
        if job.status == JobStatus::Queued {
            job.status = JobStatus::Running;
        }
        return;
    }
    // Tất cả job con đã kết thúc.
    if cancelled > 0 && done + error < total {
        job.status = JobStatus::Cancelled;
        return;
    }
    if error > 0 {
        if done > 0 {
            // Một phần thành công: vẫn done nhưng kèm thông báo lỗi.
            job.status = JobStatus::Done;
            job.error = Some(format!(
                "{done}/{total} preset thành công; {error} preset lỗi."
            ));
        } else {
            job.status = JobStatus::Error;
        }
        return;
    }
    job.status = JobStatus::Done;
    job.progress = 100.0;
    // Zip được tạo ở cuối run_job (cần file thật), ở đây chỉ đánh dấu trạng thái.
}

// ---------------------------------------------------------------------------
// Chạy job cha
// ---------------------------------------------------------------------------

/// Thông tin 1 clip sau khi validate (đường dẫn + metadata cần cho pipeline).
struct ClipInfo {
    path: PathBuf,
    duration: f64,
    width: Option<u32>,
    height: Option<u32>,
    rotation: i32,
}

/// Chạy job cha: validate → render song song các preset (bounded) → zip.
pub async fn run_job(
    store: JobStore,
    registry: Registry,
    job_id: Uuid,
    options: JobOptions,
    uploads: Uploads,
    video_paths: Vec<PathBuf>,
    audio_paths: Vec<PathBuf>,
) {
    // Chia sẻ an toàn giữa các task song song: chỉ đọc trong lúc render.
    let mut uploads = Arc::new(uploads);
    // Absolute hoá ngay từ đầu (xem job_dir_abs).
    let job_dir = job_dir_abs(&job_id);

    // Danh sách file trung gian — dọn hết cuối job.
    let mut intermediates: Vec<PathBuf> = Vec::new();

    // Giai đoạn 3: ghi nội dung chữ overlay + watermark text vào file text
    // sạch (đường dẫn do app sinh) để drawtext dùng `textfile=`.
    let overlay_text_path = job_dir.join("overlay_text.txt");
    intermediates.push(overlay_text_path.clone());
    if options.text.active() {
        if let Err(e) = std::fs::write(&overlay_text_path, &options.text.content) {
            fail(&store, &job_id, &format!("Không ghi được file text overlay: {e}")).await;
            return;
        }
    }
    let wm_text_path = job_dir.join("watermark_text.txt");
    intermediates.push(wm_text_path.clone());
    if options.watermark.active_text() {
        let name = options.watermark.channel_name.trim();
        if let Err(e) = std::fs::write(&wm_text_path, name) {
            fail(&store, &job_id, &format!("Không ghi được file text watermark: {e}")).await;
            return;
        }
    }

    // 1. Validate TỪNG file audio bằng ffprobe (trước khi ghép).
    let mut audio_total = 0.0f64;
    for (i, p) in audio_paths.iter().enumerate() {
        match probe(p).await {
            Ok(info) if info.has_audio && info.duration > 0.0 => audio_total += info.duration,
            Ok(_) => {
                fail(
                    &store,
                    &job_id,
                    &format!("File audio thứ {} không chứa stream audio hợp lệ.", i + 1),
                )
                .await;
                return;
            }
            Err(e) => {
                fail(&store, &job_id, &format!("Không đọc được file audio thứ {i}: {e:#}")).await;
                return;
            }
        }
    }

    // Bị cancel trước khi kịp chạy.
    if let Some(token) = store.get_cancel_token(&job_id).await {
        if token.is_cancelled() {
            tracing::info!(job = %job_id, "Job bị huỷ trước khi chạy");
            return;
        }
    }

    // Token huỷ của job cha: đăng ký MỘT lần ở đây. Mỗi bước ffmpeg (trong
    // run_step) lấy token này từ store; các job con song song cùng dùng nó —
    // huỷ job cha = kill đồng loạt mọi ffmpeg đang chạy.
    let parent_token = CancellationToken::new();
    store
        .update(&job_id, |j| j.cancel = Some(parent_token.clone()))
        .await;

    // 1b. Nhiều file audio → ghép nối tiếp thành MỘT file chung bằng concat
    //     filter (chuẩn hoá sample-rate/channel để không lệch nhịp), rồi mọi
    //     pipeline phía sau dùng file chung này như audio duy nhất.
    let audio_path: PathBuf = if audio_paths.len() > 1 {
        tracing::info!(job = %job_id, count = audio_paths.len(), "Ghép {} file audio", audio_paths.len());
        match concat_audios(&store, &job_id, &audio_paths, &job_dir, &parent_token).await {
            Ok(p) => {
                intermediates.push(p.clone());
                p
            }
            Err(PresetError::Cancelled) => return,
            Err(e) => {
                let msg = match e {
                    PresetError::Fatal(m) | PresetError::Preset(m) => m,
                    PresetError::Cancelled => String::new(),
                };
                fail(&store, &job_id, &msg).await;
                return;
            }
        }
    } else {
        audio_paths[0].clone()
    };

    // Duration chuẩn của audio cuối (file ghép hoặc file đơn).
    let audio_info = match probe(&audio_path).await {
        Ok(info) => info,
        Err(e) => {
            fail(&store, &job_id, &format!("Không đọc được file audio: {e:#}")).await;
            return;
        }
    };
    if !audio_info.has_audio || audio_info.duration <= 0.0 {
        fail(
            &store,
            &job_id,
            "File audio không chứa stream audio hợp lệ (duration <= 0).",
        )
        .await;
        return;
    }
    let _ = audio_total;

    // 1c. Phụ đề Tự động: sinh .srt bằng ASR từ audio (viết vào uploads.subtitle
    //     để mọi pass cuối dùng như file upload). Lỗi không làm chết job —
    //     chỉ bỏ phụ đề và báo qua store.
    // uploads đang là Arc — giai đoạn này (trước khi spawn job con) vẫn độc
    // quyền nên get_mut an toàn; mượn mutable rồi trả lại Arc.
    if options.subtitle.active() && options.subtitle.auto
        && uploads.subtitle.is_none()
    {
        if let Some(u) = Arc::get_mut(&mut uploads) {
        tracing::info!(job = %job_id, "Sinh phụ đề tự động bằng ASR…");
        match generate_srt_auto(&audio_path, audio_info.duration).await {
            Ok(srt_path) => {
                tracing::info!(job = %job_id, "ASR sinh phụ đề: {}", srt_path.display());
                intermediates.push(srt_path.clone());
                u.subtitle = Some(srt_path);
            }
            Err(msg) => {
                tracing::warn!(job = %job_id, "ASR phụ đề lỗi: {msg}");
                store.update(&job_id, |j| {
                    j.error = Some(format!(
                        "Phụ đề tự động lỗi ({msg}) — đã xuất không kèm phụ đề."
                    ));
                })
                .await;
            }
            }
        }
    }

    // 2. Giải quyết các preset thành đích render (sớm để lỗi cú pháp hiện ngay).
    let mut targets: Vec<RenderTarget> = Vec::new();
    for choice in &options.presets {
        match registry.resolve_target(&choice.id, choice.custom_width, choice.custom_height) {
            Ok(t) => {
                // Tránh trùng preset_id (2 lựa chọn giống nhau).
                if targets.iter().any(|t2| t2.preset_id == t.preset_id) {
                    fail(&store, &job_id, &format!("Preset \"{}\" được chọn 2 lần.", t.preset_id)).await;
                    return;
                }
                targets.push(t);
            }
            Err(e) => {
                fail(&store, &job_id, &e).await;
                return;
            }
        }
    }

    store.update(&job_id, |j| j.status = JobStatus::Running).await;

    // A2: "Tên sản phẩm" → stem dùng chung cho mọi tên file output của job.
    // Rỗng sau khi làm sạch → None → mọi chỗ rơi về tên mặc định cũ.
    let output_stem = options
        .output_name
        .as_deref()
        .and_then(|n| crate::naming::resolve_stem(n, options.output_name_strip));

    // 3. Validate + probe từng video clip MỘT LẦN (dùng chung cho mọi preset).
    //    (Job chỉ-audio không gửi video — routes đã cho phép; khối này bỏ qua.)
    let mut clips: Vec<ClipInfo> = Vec::new();
    for (i, path) in video_paths.iter().enumerate() {
        match probe(path).await {
            Ok(info) if info.has_video => clips.push(ClipInfo {
                path: path.clone(),
                duration: info.duration,
                width: info.width,
                height: info.height,
                rotation: info.rotation,
            }),
            Ok(_) => {
                fail(&store, &job_id, &format!("File video thứ {} không chứa stream video.", i + 1)).await;
                return;
            }
            Err(e) => {
                fail(&store, &job_id, &format!("Không đọc được file video thứ {i}: {e:#}")).await;
                return;
            }
        }
    }
    // Với preset "giữ nguyên gốc", các clip phải cùng kích thước thì concat
    // mới hoạt động — báo lỗi rõ thay vì để ffmpeg lỗi mơ hồ.
    // (audio_only cũng width=None nhưng không cần clip, loại khỏi kiểm tra.)
    let has_original = targets
        .iter()
        .any(|t| t.width.is_none() && !crate::presets::is_audio_only(&t.preset_id));
    if has_original && clips.len() >= 2 {
        let first = (clips[0].width, clips[0].height);
        let mismatch = clips.iter().enumerate().find(|(_, c)| {
            c.width.is_none() || c.height.is_none() || (c.width, c.height) != first
        });
        if let Some((idx, _)) = mismatch {
            let (w0, h0) = (first.0.unwrap_or(0), first.1.unwrap_or(0));
            let c = &clips[idx];
            fail(
                &store,
                &job_id,
                &format!(
                    "Preset \"Giữ nguyên gốc\" yêu cầu các clip cùng độ phân giải, \
                     nhưng clip thứ {idx} là {}×{} trong khi clip đầu là {w0}×{h0}. \
                     Hãy chọn một preset có kích thước cố định hoặc bỏ clip lệch.",
                    c.width.unwrap_or(0),
                    c.height.unwrap_or(0)
                ),
            )
            .await;
            return;
        }
    }

    let clips = Arc::new(clips);

    // 4. Tách preset: kích thước cố định (dùng chung chuỗi ghép) vs giữ nguyên gốc.
    //    A1: preset "audio_only" đi nhánh riêng (xuất MP3, không đụng video).
    let mut ok_outputs: Vec<(String, PathBuf)> = Vec::new();
    let audio_only_targets: Vec<RenderTarget> = targets
        .iter()
        .filter(|t| crate::presets::is_audio_only(&t.preset_id))
        .cloned()
        .collect();
    let fixed: Vec<RenderTarget> = targets
        .iter()
        .filter(|t| {
            t.width.is_some() && t.height.is_some() && !crate::presets::is_audio_only(&t.preset_id)
        })
        .cloned()
        .collect();
    let originals: Vec<RenderTarget> = targets
        .iter()
        .filter(|t| {
            (t.width.is_none() || t.height.is_none())
                && !crate::presets::is_audio_only(&t.preset_id)
        })
        .cloned()
        .collect();
    let fixed_ids: Vec<String> = fixed.iter().map(|t| t.preset_id.clone()).collect();

    // (intermediates đã khai báo ở Giai đoạn 3.)

    // 4a. Chuỗi ghép chung cho mọi preset cố định: chuẩn hoá + ghép MỘT lần
    //     ở khung lớn nhất của nhóm (thay vì lặp N lần cho từng preset).
    let chain_common: Option<PathBuf> = if fixed.is_empty() {
        None
    } else {
        let (gw, gh) = common_frame(&fixed, &clips);
        let fps = fixed.iter().map(|t| t.fps).max().unwrap_or(30);
        match build_common_chain(
            &store, &job_id, &clips, (gw, gh), fps, options.fit_mode, &options,
            &job_dir, &fixed_ids, &mut intermediates,
        )
        .await
        {
            Ok(p) => Some(p),
            Err(PresetError::Cancelled) => return,
            Err(PresetError::Fatal(msg)) => {
                fail(&store, &job_id, &msg).await;
                return;
            }
            Err(PresetError::Preset(msg)) => {
                // Lỗi chuỗi chung đánh chết mọi preset cố định → fail cả job.
                fail(&store, &job_id, &msg).await;
                return;
            }
        }
    };

    // 4b. Render từng preset cố định SONG SONG từ chuỗi dùng chung — giới
    //     hạn đồng thời bằng Semaphore (mặc định ncpu/2, tối đa 4; ép bằng
    //     biến môi trường RVM_MAX_PARALLEL). Mỗi ffmpeg x264 tự dùng nhiều
    //     luồng CPU nên K quá lớn chỉ giành CPU nhau, không nhanh hơn.
    {
        let sem = Arc::new(tokio::sync::Semaphore::new(max_parallel_presets()));
        let mut set: tokio::task::JoinSet<(String, Result<PathBuf, PresetError>)> =
            tokio::task::JoinSet::new();
        for target in &fixed {
            // Ngữ cảnh khung cho overlay: kích thước preset + safe-zone của preset.
            let frame = FrameCtx::new(
                target.width.unwrap_or(1080),
                target.height.unwrap_or(1920),
                registry_safe(&registry, &target.preset_id),
            );
            let store = store.clone();
            let job_id = job_id;
            let target = target.clone();
            let chain = chain_common.clone().unwrap_or_else(|| clips[0].path.clone());
            let audio_path = audio_path.clone();
            let uploads = Arc::clone(&uploads);
            let overlay_text_path = overlay_text_path.clone();
            let wm_text_path = wm_text_path.clone();
            let options = options.clone();
            let job_dir = job_dir.clone();
            let sem = Arc::clone(&sem);
            let out_stem = output_stem.clone();
            let multi = targets.len() > 1;
            set.spawn(async move {
                // Chưa tới lượt hoặc job đã huỷ → thoát sớm, không chiếm slot.
                if is_cancelled(&store, &job_id).await {
                    return (target.preset_id.clone(), Err(PresetError::Cancelled));
                }
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return (target.preset_id.clone(), Err(PresetError::Cancelled)),
                };
                // Có slot → preset này bắt đầu chạy thật (queued → running).
                store.mark_child_running(&job_id, &target.preset_id).await;
                let res = render_preset_from_chain(
                    &store, &job_id, &target, &chain, &audio_path, &uploads, &frame,
                    &overlay_text_path, &wm_text_path, audio_info.duration, &options,
                    &job_dir, out_stem.as_deref(), multi,
                )
                .await;
                (target.preset_id.clone(), res)
            });
        }
        match drain_preset_results(&store, &job_id, &parent_token, set).await {
            Ok(Some(outputs)) => ok_outputs.extend(outputs),
            Ok(None) => {
                store.update(&job_id, |j| j.status = JobStatus::Cancelled).await;
                return;
            }
            Err(msg) => {
                fail(&store, &job_id, &msg).await;
                return;
            }
        }
    }



    // 4c. Render preset "giữ nguyên gốc" — pipeline riêng (không scale),
    //     song song như 4b.
    {
        let sem = Arc::new(tokio::sync::Semaphore::new(max_parallel_presets()));
        let mut set: tokio::task::JoinSet<(String, Result<PathBuf, PresetError>)> =
            tokio::task::JoinSet::new();
        for target in &originals {
            let (w0, h0) = (clips[0].width.unwrap_or(1080), clips[0].height.unwrap_or(1920));
            let rotated = matches!(clips[0].rotation.rem_euclid(360), 90 | 270);
            let frame = FrameCtx::new(if rotated { h0 } else { w0 }, if rotated { w0 } else { h0 }, None);
            let store = store.clone();
            let job_id = job_id;
            let target = target.clone();
            let clips = Arc::clone(&clips);
            let audio_path = audio_path.clone();
            let uploads = Arc::clone(&uploads);
            let overlay_text_path = overlay_text_path.clone();
            let wm_text_path = wm_text_path.clone();
            let options = options.clone();
            let job_dir = job_dir.clone();
            let sem = Arc::clone(&sem);
            let out_stem = output_stem.clone();
            let multi = targets.len() > 1;
            set.spawn(async move {
                if is_cancelled(&store, &job_id).await {
                    return (target.preset_id.clone(), Err(PresetError::Cancelled));
                }
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return (target.preset_id.clone(), Err(PresetError::Cancelled)),
                };
                // Có slot → preset này bắt đầu chạy thật (queued → running).
                store.mark_child_running(&job_id, &target.preset_id).await;
                let res = render_preset_original(
                    &store, &job_id, &target, &clips, &audio_path, &uploads, &frame,
                    &overlay_text_path, &wm_text_path, audio_info.duration, &options,
                    &job_dir, out_stem.as_deref(), multi,
                )
                .await;
                (target.preset_id.clone(), res)
            });
        }
        match drain_preset_results(&store, &job_id, &parent_token, set).await {
            Ok(Some(outputs)) => ok_outputs.extend(outputs),
            Ok(None) => {
                store.update(&job_id, |j| j.status = JobStatus::Cancelled).await;
                return;
            }
            Err(msg) => {
                fail(&store, &job_id, &msg).await;
                return;
            }
        }
    }



    // 4c. A1 — preset "Tách âm thanh": xuất MP3 trực tiếp từ audio đã ghép.
    //     Chạy song song với các preset video ở trên (chỉ 1 ffmpeg nhẹ, không
    //     qua semaphore vì gần như không ăn CPU/IO như encode x264).
    if !audio_only_targets.is_empty() {
        let mut set: tokio::task::JoinSet<(String, Result<PathBuf, PresetError>)> =
            tokio::task::JoinSet::new();
        for target in &audio_only_targets {
            let store = store.clone();
            let job_id = job_id;
            let target = target.clone();
            let audio_path = audio_path.clone();
            let options = options.clone();
            let job_dir = job_dir.clone();
            let out_stem = output_stem.clone();
            let multi = targets.len() > 1;
            let audio_duration = audio_info.duration;
            set.spawn(async move {
                if is_cancelled(&store, &job_id).await {
                    return (target.preset_id.clone(), Err(PresetError::Cancelled));
                }
                store.mark_child_running(&job_id, &target.preset_id).await;
                let res = render_audio_only(
                    &store, &job_id, &target, &audio_path, audio_duration, &options,
                    &job_dir, out_stem.as_deref(), multi,
                )
                .await;
                (target.preset_id.clone(), res)
            });
        }
        match drain_preset_results(&store, &job_id, &parent_token, set).await {
            Ok(Some(outputs)) => ok_outputs.extend(outputs),
            Ok(None) => {
                store.update(&job_id, |j| j.status = JobStatus::Cancelled).await;
                return;
            }
            Err(msg) => {
                fail(&store, &job_id, &msg).await;
                return;
            }
        }
    }

    // 5. Zip nếu >= 2 file done. Tên file zip tải về: "Tên sản phẩm".zip khi
    //    người dùng đặt tên (job.zip tên vật lý trên đĩa giữ nguyên — route
    //    download sẽ gắn Content-Disposition đúng).
    //    Entry trong zip đặt THẲNG tên file output (đã chứa preset khi nhiều
    //    preset) — không bọc thêm thư mục <preset>/ vì tên đã mang preset.
    if ok_outputs.len() >= 2 {
        let zip_path = job_dir.join("outputs.zip");
        let entries: Vec<(String, PathBuf)> = ok_outputs
            .iter()
            .map(|(_preset_id, p)| {
                let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                (name, p.clone())
            })
            .collect();
        let mut writer = crate::zipwriter::ZipWriter::new(std::io::BufWriter::new(
            match std::fs::File::create(&zip_path) {
                Ok(f) => f,
                Err(e) => {
                    fail(&store, &job_id, &format!("Không tạo được file zip: {e}")).await;
                    return;
                }
            },
        ));
        for (name, path) in &entries {
            if let Err(e) = writer.add_file(name, path) {
                fail(&store, &job_id, &format!("Không nén được {name}: {e}")).await;
                return;
            }
        }
        if let Err(e) = writer.finish() {
            fail(&store, &job_id, &format!("Không hoàn tất zip: {e}")).await;
            return;
        }
        store.update(&job_id, |j| j.zip = Some(zip_path)).await;
    }

    // 6. Dọn đúng các file trung gian đã tạo (clip chuẩn hoá, chuỗi ghép, list).
    for p in &intermediates {
        let _ = tokio::fs::remove_file(p).await;
    }

    // 6b. Job thanh cong: output da copy vao library (drain_preset_results),
    //     moi input/trung gian khong con tac dung - don ca thu muc, CHI GIU
    //     outputs.zip (route /download zip dang phuc vu tu day). Khong don
    //     thi workspace phinh to vo han khi app lau dai.
    if !ok_outputs.is_empty() {
        let dir = job_dir_abs(&job_id);
        let keep_zip = dir.join("outputs.zip");
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(_) => {
                tracing::info!(job = %job_id, "Job cha hon tat: {} output", ok_outputs.len());
                return;
            }
        };
        while let Ok(Some(e)) = entries.next_entry().await {
            let p = e.path();
            if p == keep_zip {
                continue;
            }
            if p.is_dir() {
                let _ = tokio::fs::remove_dir_all(&p).await;
            } else {
                let _ = tokio::fs::remove_file(&p).await;
            }
        }
    }
    tracing::info!(job = %job_id, "Job cha hoan tat: {} output", ok_outputs.len());
}

/// Xoá các thư mục job mồ côi trong `ws_dir` (mặc định `WORKSPACE_DIR`) —
/// tàn dư của lần chạy trước bị kill đột ngột (mất điện, kill -9, crash).
/// Được gọi NGAY lúc khởi động, trước khi nhận job mới: lúc này chưa có job
/// nào đang chạy nên mọi thư mục con đều là rác an toàn để xoá (output tốt
/// đã nằm trong library/). Trả về số thư mục đã xoá.
pub fn cleanup_orphan_job_dirs() -> usize {
    cleanup_orphans_in(std::path::Path::new(WORKSPACE_DIR))
}

/// Bản có tham số của [cleanup_orphan_job_dirs] — unit test dùng thư mục tạm.
fn cleanup_orphans_in(ws_dir: &std::path::Path) -> usize {
    let rd = match std::fs::read_dir(ws_dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!("Không đọc được workspace để dọn mồ côi: {e}");
            return 0;
        }
    };
    let mut removed = 0usize;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            match std::fs::remove_dir_all(&p) {
                Ok(()) => removed += 1,
                Err(err) => {
                    tracing::warn!("Không dọn được thư mục mồ côi {}: {err}", p.display());
                }
            }
        }
    }
    if removed > 0 {
        tracing::info!("Đã dọn {removed} thư mục job mồ côi từ lần chạy trước.");
    }
    removed
}

/// Safe-zone của preset theo id (original/custom → None).
fn registry_safe(registry: &Registry, preset_id: &str) -> Option<crate::presets::SafeZone> {
    // preset_id của custom là "custom_WxH" — không có trong registry.
    registry.get(preset_id).and_then(|p| p.safe_zone)
}

/// Số preset render song song tối đa. Mỗi ffmpeg x264 tự dùng nhiều luồng CPU
/// nên K quá lớn chỉ giành CPU nhau: mặc định `ncpu/2`, chặn trong 1..=4.
/// Ép số cụ thể bằng biến môi trường `RVM_MAX_PARALLEL` (1–8).
fn max_parallel_presets() -> usize {
    if let Ok(v) = std::env::var("RVM_MAX_PARALLEL") {
        if let Ok(n) = v.trim().parse::<usize>() {
            if (1..=8).contains(&n) {
                return n;
            }
        }
    }
    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (ncpu / 2).clamp(1, 4)
}

/// Thu kết quả các task preset đã spawn (4b/4c): cập nhật status từng job con
/// ngay khi nó xong. Trả về:
/// - `Ok(Some(outputs))` — loạt chạy xong bình thường;
/// - `Ok(None)` — có job con bị huỷ (đã huỷ token cha dừng phần còn lại);
/// - `Err(msg)` — lỗi fatal (đĩa/thư mục), đã huỷ token cha.
async fn drain_preset_results(
    store: &JobStore,
    job_id: &Uuid,
    parent_token: &CancellationToken,
    mut set: tokio::task::JoinSet<(String, Result<PathBuf, PresetError>)>,
) -> Result<Option<Vec<(String, PathBuf)>>, String> {
    let mut ok_outputs: Vec<(String, PathBuf)> = Vec::new();
    let mut batch_cancelled = false;
    // A2: danh sách tên hiển thị đã chiếm chỗ trong kho (toàn bộ library/) —
    // nạp MỘT lần rồi thêm dần mỗi output, để " (2)", " (3)" không bao giờ
    // ghi đè file cũ.
    let mut taken_names: Vec<String> = library_taken_display_names();
    while let Some(joined) = set.join_next().await {
        let (preset_id, res) = match joined {
            Ok(v) => v,
            Err(e) => {
                // Task panic — lỗi nghiêm trọng, dừng cả job.
                parent_token.cancel();
                return Err(format!("Task render preset lỗi: {e}"));
            }
        };
        match res {
            Ok(mut out_path) => {
                let mut filename =
                    out_path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                // Trùng tên trong kho → đổi thành "tên (2).mp4"… (đổi cả file
                // trong workspace để child.filename / zip entry khớp tên tải về).
                filename = crate::naming::unique_display_name(&filename, &taken_names);
                taken_names.push(filename.clone());
                if out_path.file_name().map(|n| n.to_string_lossy() != filename).unwrap_or(false) {
                    let renamed = out_path
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .join(&filename);
                    if let Err(e) = tokio::fs::rename(&out_path, &renamed).await {
                        tracing::warn!(job = %job_id, "Không đổi tên trùng: {e}");
                    } else {
                        out_path = renamed;
                    }
                }
                // Lưu vào kho: library/<job_id>/<preset_id>__<filename>.
                if let Err(e) = library_save(&job_id, &preset_id, &out_path) {
                    tracing::warn!(job = %job_id, "Không lưu vào kho: {e}");
                }
                ok_outputs.push((preset_id.clone(), out_path));
                store
                    .update_child(&job_id, &preset_id, |c| {
                        c.status = JobStatus::Done;
                        c.progress = 100.0;
                        c.filename = Some(filename);
                    })
                    .await;
                // A3: sau mỗi job con hoàn tất → dọn kho nếu vượt giới hạn.
                // Bảo vệ: job cha này (và mọi job khác đang chạy) không bị xoá.
                if let Some(settings) = store.settings_snapshot() {
                    let mut protected = store.active_job_ids().await;
                    if !protected.contains(&job_id.to_string()) {
                        protected.push(job_id.to_string());
                    }
                    let res = crate::storage::enforce_library_limits(settings, protected).await;
                    for w in &res.warnings {
                        tracing::warn!("Dọn kho: {w}");
                    }
                }
            }
            Err(PresetError::Cancelled) => {
                store
                    .update_child(&job_id, &preset_id, |c| {
                        c.status = JobStatus::Cancelled;
                    })
                    .await;
                if !batch_cancelled {
                    batch_cancelled = true;
                    // Dừng các preset còn lại (cả đang chạy lẫn đang chờ slot).
                    parent_token.cancel();
                }
            }
            Err(PresetError::Fatal(msg)) => {
                // Lỗi dùng chung (đĩa, thư mục): dừng cả job.
                parent_token.cancel();
                return Err(msg);
            }
            Err(PresetError::Preset(msg)) => {
                store
                    .update_child(&job_id, &preset_id, |c| {
                        c.status = JobStatus::Error;
                        c.error = Some(msg);
                    })
                    .await;
                // Tiếp tục các preset khác.
            }
        }
    }
    if batch_cancelled {
        return Ok(None);
    }
    Ok(Some(ok_outputs))
}

// ---------------------------------------------------------------------------
// Ghép nhiều audio — dùng concat filter của ffmpeg (an toàn hơn demuxer vì
// ép cùng sample rate/channel/pix). File ra: audio_merged.m4a (AAC).
// ---------------------------------------------------------------------------
#[allow(clippy::too_many_arguments)]
async fn concat_audios(
    store: &JobStore,
    job_id: &Uuid,
    audio_paths: &[PathBuf],
    job_dir: &std::path::Path,
    parent_token: &CancellationToken,
) -> Result<PathBuf, PresetError> {
    use crate::ffmpeg::run_ffmpeg;

    let out = job_dir.join("audio_merged.m4a");
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
    ];
    for p in audio_paths {
        args.push("-i".into());
        args.push(p.to_string_lossy().into_owned());
    }
    // concat filter: n input → 1 chuỗi; mỗi input chuẩn hoá 44.1kHz stereo.
    let n = audio_paths.len();
    let mut filter = String::new();
    for i in 0..n {
        filter.push_str(&format!("[{i}:a]aresample=44100,aformat=channel_layouts=stereo[a{i}];"));
    }
    for i in 0..n {
        filter.push_str(&format!("[a{i}]"));
    }
    filter.push_str(&format!("concat=n={n}:v=0:a=1[aout]"));
    args.push("-filter_complex".into());
    args.push(filter);
    args.push("-map".into());
    args.push("[aout]".into());
    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-b:a".into());
    args.push("192k".into());
    args.push("-y".into());
    args.push(out.to_string_lossy().into_owned());

    let cancel = CancellationToken::new();
    store.update(job_id, |j| j.cancel = Some(cancel.clone())).await;
    let outcome = run_ffmpeg(&args, cancel, |_| {}).await;
    // Khôi phục token cha cho các bước sau.
    store.update(job_id, |j| j.cancel = Some(parent_token.clone())).await;
    match outcome {
        Ok(o) if o.success => Ok(out),
        Ok(o) if o.cancelled => Err(PresetError::Cancelled),
        Ok(o) => Err(PresetError::Fatal(format!(
            "Ghép audio lỗi: {}",
            o.stderr_tail.join("\n")
        ))),
        Err(e) => Err(PresetError::Fatal(format!(
            "Không chạy được ffmpeg ghép audio: {e:#}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Phụ đề tự động — chiến lược 2 bước (không cần timestamps từ ASR):
// 1. ffmpeg silencedetect chia audio thành các đoạn có tiếng (≥ 0.8s, ngắt
//    khi im ≥ 0.35s) → khoảng [start, end] cho mỗi câu.
// 2. Mỗi đoạn cắt ra WAV 16k mono → gọi `z-ai asr` → text.
// Ghép thành SRT chuẩn (số thứ tự + HH:MM:SS,mmm).
// ---------------------------------------------------------------------------
async fn generate_srt_auto(
    audio_path: &std::path::Path,
    total_duration: f64,
) -> Result<PathBuf, String> {
    let dir = audio_path
        .parent()
        .ok_or("Không xác định được thư mục job")?
        .to_path_buf();
    let segments = detect_speech_segments(audio_path.to_path_buf(), total_duration).await
        .map_err(|e| format!("silencedetect lỗi: {e}"))?;
    if segments.is_empty() {
        return Err("Audio không có đoạn tiếng nào (toàn im lặng hoặc nhạc thuần)".into());
    }
    tracing::info!(segs = segments.len(), "ASR: {} đoạn tiếng", segments.len());

    let mut srt = String::new();
    let mut idx = 1usize;
    let mut i = 0usize;
    while i < segments.len() {
        // Gom tối đa 3 đoạn liền nhau ≤ 25s thành 1 câu (giảm số lần gọi API).
        let mut group_end = segments[i].1;
        let mut count = 1;
        while count < 3
            && i + count < segments.len()
            && segments[i + count].1 - segments[i].0 <= 25.0
        {
            group_end = segments[i + count].1;
            count += 1;
        }
        let (start, end) = (segments[i].0, group_end);
        let wav = dir.join(format!("asr_seg_{i:03}.wav"));
        if cut_to_wav16k(audio_path, start, end, &wav).is_ok() {
            match asr_transcribe(&wav).await {
                Ok(text) => {
                    let text = text.trim();
                    if !text.is_empty() && text != "#" {
                        srt.push_str(&format!("{idx}\n{} --> {}\n{}\n\n", srt_time(start), srt_time(end), text));
                        idx += 1;
                    }
                }
                Err(e) => tracing::warn!(seg = i, "ASR đoạn lỗi: {e}"),
            }
        }
        let _ = tokio::fs::remove_file(&wav).await;
        i += count;
    }
    if srt.is_empty() {
        return Err("ASR không nhận diện được lời nói nào".into());
    }
    let srt_path = dir.join("auto_subtitle.srt");
    tokio::fs::write(&srt_path, srt)
        .await
        .map_err(|e| format!("Không ghi được SRT: {e}"))?;
    Ok(srt_path)
}

/// Chạy ffmpeg silencedetect, parse stderr ra các đoạn CÓ tiếng.
async fn detect_speech_segments(
    audio_path: std::path::PathBuf,
    total_duration: f64,
) -> Result<Vec<(f64, f64)>, String> {
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(crate::ffmpeg::ffmpeg_bin())
            .args([
                "-hide_banner",
                "-i",
            ])
            .arg(audio_path)
            .args([
                "-af",
                "silencedetect=noise=-32dB:d=0.35",
                "-f",
                "null",
                "-",
            ])
            .output()
    })
    .await
    .map_err(|e| format!("join lỗi: {e}"))?
    .map_err(|e| format!("không chạy được ffmpeg: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut speech: Vec<(f64, f64)> = Vec::new();
    let mut cursor = 0.0f64;
    let mut silence_start: Option<f64> = None;
    for line in stderr.lines() {
        if let Some(v) = parse_kv(&line, "silence_start") {
            let v = v as f64;
            if v > cursor + 0.15 {
                speech.push((cursor, v));
            }
            silence_start = Some(v);
        } else if let Some(v) = parse_kv(&line, "silence_end") {
            let v = v as f64;
            if silence_start.is_some() && v > cursor {
                cursor = v;
                silence_start = None;
            }
        }
    }
    if cursor < total_duration - 0.1 {
        speech.push((cursor, total_duration));
    }
    // Lọc quá ngắn (< 0.5s thường là tiếng ồn) và đoạn lệch (end <= start).
    Ok(speech
        .into_iter()
        .filter(|(s, e)| e - s >= 0.5 && e > s)
        .collect())
}

/// "…silence_start: 1.234…" → 1.234
fn parse_kv(line: &str, key: &str) -> Option<f64> {
    let pat = format!("{key}: ");
    let pos = line.find(&pat)?;
    let rest = &line[pos + pat.len()..];
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    num.parse().ok()
}

/// Cắt [start, end] sang WAV 16kHz mono (định dạng ASR yêu cầu).
fn cut_to_wav16k(audio_path: &std::path::Path, start: f64, end: f64, out: &std::path::Path) -> Result<(), String> {
    let status = std::process::Command::new(crate::ffmpeg::ffmpeg_bin())
        .args(["-hide_banner", "-loglevel", "error", "-ss"])
        .arg(format!("{start:.3}"))
        .arg("-to")
        .arg(format!("{end:.3}"))
        .arg("-i")
        .arg(audio_path)
        .args(["-vn", "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", "-y"])
        .arg(out)
        .status()
        .map_err(|e| format!("không chạy được ffmpeg: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("ffmpeg cắt đoạn lỗi".into())
    }
}

/// Gọi `z-ai asr --file <wav>` (CLI của sandbox SDK), đọc text từ JSON ra.
async fn asr_transcribe(wav: &std::path::Path) -> Result<String, String> {
    // Timeout 120s/đoạn: z-ai bị kẹt (mạng, service) không được treo cả job.
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new("z-ai")
            .args(["asr", "--file"])
            .arg(wav)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "z-ai asr vượt 120 giây (timeout)".to_string())?
    .map_err(|e| format!("không chạy được z-ai: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "z-ai asr lỗi: {}",
            String::from_utf8_lossy(&out.stderr).chars().take(300).collect::<String>()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // CLI in "✅ File saved to: …" rồi JSON có thể nằm ở output file hoặc stdout.
    let json_str = extract_json_object(&stdout)
        .ok_or_else(|| "không parse được JSON ASR".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| format!("JSON ASR lỗi: {e}"))?;
    Ok(v
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string())
}

/// Tìm object JSON "{…}" bằng brace-matching (bỏ qua chuỗi trong ngoặc
/// kép) — lấy object ĐẦU TIÊN hợp lệ chứ không phải '{' đầu…'}' cuối.
fn extract_json_object(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &c) in b.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// 12.345s → "00:00:12,345"
fn srt_time(sec: f64) -> String {
    let ms = (sec * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// A2: mọi tên hiển thị (phần sau `__`) đang có trong kho library/ — dùng để
/// né ghi đè khi output mới trùng tên. File vật lý dạng `<preset_id>__<tên>`;
/// tên hiển thị mới do `unique_display_name` sinh cũng là tên tải về.
fn library_taken_display_names() -> Vec<String> {
    let mut names = Vec::new();
    let lib = std::path::Path::new(LIBRARY_DIR);
    let Ok(job_dirs) = std::fs::read_dir(lib) else {
        return names;
    };
    for jd in job_dirs.flatten() {
        let Ok(files) = std::fs::read_dir(jd.path()) else {
            continue;
        };
        for f in files.flatten() {
            if f.path().is_file() {
                let name = f.file_name().to_string_lossy().into_owned();
                names.push(match name.split_once("__") {
                    Some((_, display)) => display.to_string(),
                    None => name,
                });
            }
        }
    }
    names
}

/// Copy output vào kho lưu trữ: ./library/<job_id>/<preset_id>__<filename>.
fn library_save(job_id: &Uuid, preset_id: &str, out_path: &std::path::Path) -> Result<(), String> {
    let dir = std::path::PathBuf::from(LIBRARY_DIR).join(job_id.to_string());
    std::fs::create_dir_all(&dir).map_err(|e| format!("tạo thư mục kho: {e}"))?;
    let filename = out_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let dest = dir.join(format!("{preset_id}__{filename}"));
    std::fs::copy(out_path, dest).map(|_| ()).map_err(|e| format!("copy: {e}"))
}

async fn is_cancelled(store: &JobStore, id: &Uuid) -> bool {
    store
        .get_cancel_token(id)
        .await
        .map(|t| t.is_cancelled())
        .unwrap_or(false)
}

/// Lỗi khi render 1 preset.
enum PresetError {
    /// Người dùng huỷ.
    Cancelled,
    /// Lỗi hệ thống nặng — dừng cả job cha.
    Fatal(String),
    /// Lỗi riêng preset này — preset khác vẫn chạy.
    Preset(String),
}

/// Khung chung cho chuỗi ghép dùng chung mọi preset cố định: cạnh ngắn =
/// cạnh ngắn lớn nhất trong nhóm; cạnh dài = theo tỉ lệ clip đầu, chặn 2.4×.
/// Làm chẵn 2 để yuv420p hợp lệ.
fn common_frame(fixed: &[RenderTarget], clips: &[ClipInfo]) -> (u32, u32) {
    let short = fixed
        .iter()
        .filter_map(|t| t.width.zip(t.height))
        .map(|(w, h)| w.min(h))
        .max()
        .unwrap_or(1080);
    // Kích thước HIỂN THỊ của clip đầu (đã áp rotation 90/270 → đảo canh).
    let rotated = matches!(clips[0].rotation.rem_euclid(360), 90 | 270);
    let (cw0, ch0) = (clips[0].width.unwrap_or(1080), clips[0].height.unwrap_or(1920));
    let (cw, ch) = if rotated { (ch0, cw0) } else { (cw0, ch0) };
    let (cw, ch) = (cw as f64, ch as f64);
    let ratio = ch / cw.max(1.0);
    let long = ((short as f64 * ratio).round() as u32)
        .clamp(short, ((short as f64) * 2.4) as u32)
        & !1;
    if ratio >= 1.0 {
        (short, long.max(short))
    } else {
        (long.max(short), short)
    }
}

/// Chuỗi ghép dùng chung: chuẩn hoá từng clip ở khung chung → ghép (xfade hoặc
/// concat) MỘT lần → `chain_common.mp4`. Progress gán cho mọi preset cố định.
/// Lỗi trả về là Fatal cho job (mọi preset cố định đều phụ thuộc chuỗi này).
#[allow(clippy::too_many_arguments)]
async fn build_common_chain(
    store: &JobStore,
    job_id: &Uuid,
    clips: &[ClipInfo],
    frame: (u32, u32),
    fps: u32,
    fit_mode: FitMode,
    options: &JobOptions,
    job_dir: &std::path::Path,
    preset_ids: &[String],
    intermediates: &mut Vec<PathBuf>,
) -> Result<PathBuf, PresetError> {
    // --- Chuẩn hoá từng clip ở khung chung ---
    let mut norm_paths: Vec<PathBuf> = Vec::new();
    for (i, clip) in clips.iter().enumerate() {
        let out = job_dir.join(format!("norm_common_{i:03}.mp4"));
        intermediates.push(out.clone());
        let args = build_normalize_args(&clip.path, &out, Some(frame.0), Some(frame.1), fps, fit_mode, clip.rotation);
        run_step(store, job_id, preset_ids, &format!("chuẩn hoá chung clip {i}"), args,
                 |frac| 2.0 + frac * 18.0, // 2–20%
                 clip.duration,
        )
        .await?;
        norm_paths.push(out);
    }

    // Probe độ dài sau chuẩn hoá (fps làm tròn số frame → lệch nhẹ).
    let mut norm_durations: Vec<f64> = Vec::new();
    for p in &norm_paths {
        match probe(p).await {
            Ok(info) if info.duration > 0.0 => norm_durations.push(info.duration),
            _ => return Err(PresetError::Fatal("Không đo được clip sau chuẩn hoá.".into())),
        }
    }

    // --- Ghép chuỗi MỘT lần ---
    let chain_path = job_dir.join("chain_common.mp4");
    intermediates.push(chain_path.clone());
    if options.xfade_duration > 0.0 && norm_paths.len() >= 2 {
        if let Err(msg) = validate_xfade(options.xfade_duration, norm_paths.len(), &norm_durations) {
            return Err(PresetError::Preset(msg));
        }
        let args = build_xfade_args(&norm_paths, &chain_path, &options.xfade_transition, options.xfade_duration, &norm_durations);
        run_step(store, job_id, preset_ids, "ghép xfade chung", args,
                 |frac| 20.0 + frac * 25.0, // 20–45%
                 norm_durations.iter().sum::<f64>() - options.xfade_duration * (norm_paths.len() - 1) as f64,
        )
        .await?;
    } else if norm_paths.len() >= 2 {
        let list_txt = job_dir.join("list_common.txt");
        intermediates.push(list_txt.clone());
        let content = concat_list_content(&norm_paths);
        if let Err(e) = tokio::fs::write(&list_txt, content).await {
            return Err(PresetError::Fatal(format!("Không ghi được list.txt: {e}")));
        }
        let args = build_concat_args(&list_txt, &chain_path);
        run_step(store, job_id, preset_ids, "ghép concat chung", args,
                 |frac| 20.0 + frac * 25.0,
                 norm_durations.iter().sum::<f64>(),
        )
        .await?;
    } else {
        // 1 clip duy nhất: chính clip chuẩn hoá là chuỗi (cả file này là
        // trung gian — cleanup cuối job xoá).
        return Ok(norm_paths[0].clone());
    }
    Ok(chain_path)
}

/// Pass cuối cho 1 preset cố định từ chuỗi dùng chung: khớp khung đích
/// (chèn ĐẦU filter_complex) + overlay + audio — decode/encode đúng 1 lần.
#[allow(clippy::too_many_arguments)]
async fn render_preset_from_chain(
    store: &JobStore,
    job_id: &Uuid,
    target: &RenderTarget,
    chain: &std::path::Path,
    audio_path: &std::path::Path,
    uploads: &Uploads,
    frame: &FrameCtx,
    _overlay_text_path: &std::path::Path,
    wm_text_path: &std::path::Path,
    audio_duration: f64,
    options: &JobOptions,
    job_dir: &std::path::Path,
    // A2: "Tên sản phẩm" đã làm sạch (None → tên mặc định).
    out_stem: Option<&str>,
    // Job có nhiều preset → tên file kèm hậu tố preset_id.
    multi: bool,
) -> Result<PathBuf, PresetError> {
    let sub = format!("preset {}", target.preset_id);
    let tid = target.preset_id.clone();

    // --- Overlay spec theo FrameCtx của preset + filter khớp khung đích ---
    let base = format!("fit_{}", tid.replace(['-', '.'], "_"));
    let out_duration = out_duration_for(audio_duration, target.max_duration_sec);
    let mut overlay =
        build_overlay_spec(options, uploads, frame, wm_text_path, job_dir, &base, out_duration)
            .await
            .map_err(PresetError::Fatal)?;
    overlay.pre_scale = Some(crate::ffmpeg::build_fit_filter(
        target.width.unwrap_or(1080),
        target.height.unwrap_or(1920),
        options.fit_mode,
        &base,
    ));

    // --- Pass cuối ---
    let out_final = job_dir.join(output_filename(target, out_stem, multi));
    let args = build_final_args(
        chain,
        audio_path,
        &out_final,
        audio_duration,
        target.max_duration_sec,
        &options.audio(),
        &overlay,
        uploads.logo.as_deref(),
    );
    run_step(store, job_id, &[tid.clone()], &format!("{sub}: hoàn thiện"), args,
             |frac| 45.0 + frac * 55.0, // 45–100%
             audio_duration,
    )
    .await
    .map(|_| out_final)
}

/// Pipeline riêng cho preset "giữ nguyên gốc": chuẩn hoá fps-only từng clip
/// (kích thước nguồn) → ghép → pass cuối. File trung gian tự dọn khi xong.
#[allow(clippy::too_many_arguments)]
async fn render_preset_original(
    store: &JobStore,
    job_id: &Uuid,
    target: &RenderTarget,
    clips: &[ClipInfo],
    audio_path: &std::path::Path,
    uploads: &Uploads,
    frame: &FrameCtx,
    _overlay_text_path: &std::path::Path,
    wm_text_path: &std::path::Path,
    audio_duration: f64,
    options: &JobOptions,
    job_dir: &std::path::Path,
    // A2: "Tên sản phẩm" đã làm sạch (None → tên mặc định).
    out_stem: Option<&str>,
    // Job có nhiều preset → tên file kèm hậu tố preset_id.
    multi: bool,
) -> Result<PathBuf, PresetError> {
    let sub = format!("preset {}", target.preset_id);
    let tid = target.preset_id.clone();

    // --- Bước 1: chuẩn hoá từng clip (giữ kích thước nguồn) ---
    let mut norm_paths: Vec<PathBuf> = Vec::new();
    for (i, clip) in clips.iter().enumerate() {
        let out = job_dir.join(format!("norm_{tid}_{i:03}.mp4"));
        let args = build_normalize_args(&clip.path, &out, target.width, target.height, target.fps, options.fit_mode, clip.rotation);
        run_step(store, job_id, &[tid.clone()], &format!("{sub}: chuẩn hoá clip {i}"), args,
                 |frac| 5.0 + frac * 15.0, // bước này chiếm 5–20%
                 clip.duration,
        )
        .await?;
        norm_paths.push(out);
    }

    // Probe lại độ dài clip sau chuẩn hoá (fps mới làm tròn số frame → lệch nhẹ).
    let mut norm_durations: Vec<f64> = Vec::new();
    for p in &norm_paths {
        match probe(p).await {
            Ok(info) if info.duration > 0.0 => norm_durations.push(info.duration),
            _ => return Err(PresetError::Preset("Không đo được clip sau chuẩn hoá.".into())),
        }
    }

    // --- Bước 2: ghép chuỗi ---
    let chain_path = job_dir.join(format!("chain_{tid}.mp4"));
    let single_source: Option<PathBuf> = if options.xfade_duration > 0.0 && norm_paths.len() >= 2 {
        if let Err(msg) = validate_xfade(options.xfade_duration, norm_paths.len(), &norm_durations) {
            return Err(PresetError::Preset(msg));
        }
        let args = build_xfade_args(&norm_paths, &chain_path, &options.xfade_transition, options.xfade_duration, &norm_durations);
        run_step(store, job_id, &[tid.clone()], &format!("{sub}: ghép xfade"), args,
                 |frac| 20.0 + frac * 25.0, // 20–45%
                 norm_durations.iter().sum::<f64>() - options.xfade_duration * (norm_paths.len() - 1) as f64,
        )
        .await?;
        None
    } else if norm_paths.len() >= 2 {
        let list_txt = job_dir.join(format!("list_{tid}.txt"));
        let content = concat_list_content(&norm_paths);
        if let Err(e) = tokio::fs::write(&list_txt, content).await {
            return Err(PresetError::Fatal(format!("Không ghi được list.txt: {e}")));
        }
        let args = build_concat_args(&list_txt, &chain_path);
        run_step(store, job_id, &[tid.clone()], &format!("{sub}: ghép concat"), args,
                 |frac| 20.0 + frac * 25.0,
                 norm_durations.iter().sum::<f64>(),
        )
        .await?;
        None
    } else {
        // 1 clip duy nhất: chính nó là chuỗi.
        Some(norm_paths[0].clone())
    };

    // --- Bước 3: dựng overlay spec (theo FrameCtx của preset này) ---
    let out_duration = out_duration_for(audio_duration, target.max_duration_sec);
    let overlay = build_overlay_spec(
        options,
        uploads,
        frame,
        wm_text_path,
        job_dir,
        &tid.replace(['-', '.'], "_"),
        out_duration,
    )
    .await
    .map_err(PresetError::Fatal)?;

    // --- Bước 4: pass cuối ---
    let chain = single_source.as_deref().unwrap_or(&chain_path);
    let out_final = job_dir.join(output_filename(target, out_stem, multi));
    let args = build_final_args(
        chain,
        audio_path,
        &out_final,
        audio_duration,
        target.max_duration_sec,
        &options.audio(),
        &overlay,
        uploads.logo.as_deref(),
    );
    let result = run_step(store, job_id, &[tid.clone()], &format!("{sub}: hoàn thiện"), args,
             |frac| 45.0 + frac * 55.0, // 45–100%
             audio_duration,
    )
    .await
    .map(|_| out_final);

    // Dọn file trung gian của preset này (kể cả khi lỗi, tốt nhất là gì còn).
    for p in &norm_paths {
        let _ = tokio::fs::remove_file(p).await;
    }
    let _ = tokio::fs::remove_file(&chain_path).await;
    let _ = tokio::fs::remove_file(job_dir.join(format!("list_{tid}.txt"))).await;
    result
}

/// Dựng [`OverlaySpec`] từ options + uploads cho 1 khung `frame`. Mỗi lớp chữ
/// ghi vào file riêng trong `job_dir` (drawtext đọc `textfile=` nên nội dung
/// không bao giờ lọt vào filtergraph); `tag` = id preset để các preset render
/// song song không ghi đè file của nhau. `out_duration` = độ dài thật của file
/// ra sau giới hạn nền tảng — thanh tiến độ chạy 0→100% theo đúng nó.
///
/// `Err` chỉ đến từ việc đọc/ghi file (SRT hỏng, đĩa đầy) — mang tính hệ thống.
#[allow(clippy::too_many_arguments)]
async fn build_overlay_spec(
    options: &JobOptions,
    uploads: &Uploads,
    frame: &FrameCtx,
    wm_text_path: &std::path::Path,
    job_dir: &std::path::Path,
    tag: &str,
    out_duration: f64,
) -> Result<crate::textover::OverlaySpec, String> {
    use crate::textover::{
        build_subtitle_filter, build_text_filter, build_watermark_logo,
        build_watermark_text_filter, OverlaySpec, ProgressSpec, TextLayout,
    };
    use crate::subtitle;

    let text_font = options.text.font.resolve(&uploads.fonts);
    let sub_font = options.subtitle.font.resolve(&uploads.fonts);
    let wm_font = options.watermark.font.resolve(&uploads.fonts);

    // Visualizer dùng chính audio của output (`[1:a]`); vùng nó chiếm làm
    // phụ đề/chữ/watermark phải né.
    let viz = visuals::viz_reserves(&options.visuals, frame);
    let (inset_top, inset_bottom, _, _) = frame.insets();
    // Visualizer nằm trên mép vùng an toàn, nên vùng nó chiếm (tính từ mép
    // khung) = phần lùi an toàn + khoảng hở + chiều cao. Phụ đề/chữ/watermark
    // phải né hết phần đó, nếu không sẽ đè lên visualizer ở preset có safe-zone.
    let viz_reserve = crate::textover::Reserves { top: viz.top + inset_top, bottom: viz.bottom + inset_bottom };
    let (viz_graph, viz_overlay) = visuals::build_visualizer(&options.visuals, frame);
    let mut spec = OverlaySpec { viz_graph, viz_overlay, ..Default::default() };
    if options.visuals.progress_bar {
        spec.progress =
            Some(ProgressSpec::new(frame, out_duration, &options.visuals.progress_color));
    }

    // 1. Phụ đề (chạy trước để chữ overlay/watermark vẽ ĐÈ lên phụ đề):
    //    SRT → ASS do app sinh, cỡ chữ tính theo px THẬT của khung (PlayRes =
    //    khung). Bản 0.3 giao libass scale theo PlayRes mặc định 384×288 nên
    //    cùng giá trị "28" lúc thành 105px, lúc 187px tuỳ preset.
    if options.subtitle.active() {
        if let Some(srt) = uploads.subtitle.as_deref() {
            let raw = tokio::fs::read_to_string(srt)
                .await
                .map_err(|e| format!("Không đọc được file phụ đề: {e}"))?;
            let cues =
                subtitle::parse_srt(&raw).map_err(|e| format!("File SRT không hợp lệ: {e}"))?;
            let style =
                crate::textover::subtitle_style(&options.subtitle, &sub_font, frame, viz_reserve);
            // Mỗi câu tối đa ~2 hàng chữ (rộng ký tự ≈ 0.52 × cỡ chữ); câu dài
            // hơn bị tách theo câu/dấu câu, mỗi mảnh ≤ 7s.
            let per_line = ((frame.width.saturating_sub(style.margin_l + style.margin_r) as f64)
                / (style.fontsize.max(1) as f64 * 0.52))
                .floor()
                .max(8.0) as usize;
            let cues = subtitle::split_long_cues(cues, per_line * 2, 7.0);
            let ass_path = job_dir.join(format!("sub_{tag}.ass"));
            tokio::fs::write(&ass_path, subtitle::render_ass(&cues, &style))
                .await
                .map_err(|e| format!("Không ghi được file phụ đề ASS: {e}"))?;
            spec.chain.push(build_subtitle_filter(&ass_path, sub_font.fonts_dir.as_deref()));
        }
    }

    // 2. Chữ overlay: kịch bản nhiều cảnh → nhiều lớp, mỗi lớp 1 file text.
    let layout = TextLayout { offset: 0.0, duration: Some(out_duration), reserve: viz_reserve };
    if options.text.active() {
        for (i, item) in
            crate::textover::text_items(&options.text, &text_font, frame).into_iter().enumerate()
        {
            let path = job_dir.join(format!("text_{tag}_{i:02}.txt"));
            tokio::fs::write(&path, &item.content)
                .await
                .map_err(|e| format!("Không ghi được file chữ overlay: {e}"))?;
            if let Some(f) =
                build_text_filter(&item, &options.text, &path, &text_font, frame, &layout)
            {
                spec.chain.push(f);
            }
        }
    }

    // 3. Watermark chữ.
    if options.watermark.active_text() {
        if let Some(f) =
            build_watermark_text_filter(&options.watermark, wm_text_path, &wm_font, frame, viz_reserve)
        {
            spec.chain.push(f);
        }
    }

    // 4. Watermark logo — cần input logo (index 2: chain, audio, logo).
    if options.watermark.active_logo() && uploads.logo.is_some() {
        let logo = build_watermark_logo(&options.watermark, frame, 2, viz_reserve);
        spec.logo_prep = Some(logo.prep);
        spec.logo_xy = Some((logo.x, logo.y));
    }

    Ok(spec)
}

/// Độ dài thật của file ra: min(audio, giới hạn nền tảng).
fn out_duration_for(audio_duration: f64, max_duration_sec: Option<u64>) -> f64 {
    match max_duration_sec {
        Some(max) if (max as f64) < audio_duration => max as f64,
        _ => audio_duration,
    }
}

/// Tên file output cho 1 preset: `radio_<preset>_<WxH>.mp4`.
/// Tên file output của 1 preset. Có "Tên sản phẩm" (`out_stem`) thì đặt theo
/// quy tắc A2: `<tên>.mp4` (1 preset) / `<tên>_<preset_id>.mp4` (nhiều
/// preset); không có thì giữ cách đặt tên mặc định cũ.
/// A1: preset audio_only dùng đuôi `.mp3`.
fn output_filename(target: &RenderTarget, out_stem: Option<&str>, multi: bool) -> String {
    let ext = if crate::presets::is_audio_only(&target.preset_id) { "mp3" } else { "mp4" };
    match out_stem {
        Some(stem) => crate::naming::preset_filename_ext(stem, &target.preset_id, multi, ext),
        None => match (target.width, target.height) {
            (Some(w), Some(h)) => format!("radio_{}_{}x{}.{ext}", target.preset_id, w, h),
            _ => format!("radio_{}.{ext}", target.preset_id),
        },
    }
}

/// A1 — render preset "Tách âm thanh": 1 ffmpeg duy nhất, input là audio
/// đã ghép (nhiều file đã concat từ bước 1), output MP3 320 kbps. Tiến độ
/// 0→100 theo out_time / audio_duration. Không đụng clip video, không
/// overlay/watermark (không có khung hình để đặt lên).
#[allow(clippy::too_many_arguments)]
async fn render_audio_only(
    store: &JobStore,
    job_id: &Uuid,
    target: &RenderTarget,
    audio_path: &std::path::Path,
    audio_duration: f64,
    options: &JobOptions,
    job_dir: &std::path::Path,
    out_stem: Option<&str>,
    multi: bool,
) -> Result<PathBuf, PresetError> {
    let tid = target.preset_id.clone();
    let target_label = target.preset_id.clone();
    let out_final = job_dir.join(output_filename(target, out_stem, multi));
    let args = crate::ffmpeg::build_mp3_args(
        audio_path,
        &out_final,
        audio_duration,
        target.max_duration_sec,
        &options.audio(),
    );
    run_step(
        store,
        job_id,
        &[tid],
        &format!("preset {target_label}: xuất MP3"),
        args,
        |frac| frac * 100.0,
        audio_duration,
    )
    .await?;
    Ok(out_final)
}

/// Chạy 1 bước ffmpeg của job con, cập nhật tiến độ theo trọng số.
async fn run_step(
    store: &JobStore,
    job_id: &Uuid,
    preset_ids: &[String],
    _label: &str,
    args: Vec<String>,
    weight: impl Fn(f64) -> f64 + Send + Sync + 'static,
    expected_duration: f64,
) -> Result<(), PresetError> {
    // Token huỷ do job cha đăng ký (một lần, xem run_job) — các bước và các
    // job con song song đều dùng chung, huỷ một nơi kill mọi ffmpeg.
    let cancel = store
        .get_cancel_token(job_id)
        .await
        .unwrap_or_default();

    let progress_store = store.clone();
    let progress_job = *job_id;
    let progress_presets: Vec<String> = preset_ids.to_vec();
    let on_progress = move |out_time_secs: f64| {
        let frac = if expected_duration > 0.0 {
            (out_time_secs / expected_duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let pct = weight(frac);
        let store = progress_store.clone();
        for preset in &progress_presets {
            let store = store.clone();
            let preset = preset.clone();
            tokio::spawn(async move {
                store.set_child_progress(&progress_job, &preset, pct).await;
            });
        }
    };

    let outcome = ffmpeg::run_ffmpeg(&args, cancel, on_progress).await;

    match outcome {
        Ok(o) if o.success => Ok(()),
        Ok(o) if o.cancelled => Err(PresetError::Cancelled),
        Ok(o) => {
            let tail = o.stderr_tail.join("\n");
            let msg = if tail.is_empty() {
                "ffmpeg lỗi (không có thông tin chi tiết).".to_string()
            } else {
                format!("ffmpeg lỗi:\n{tail}")
            };
            Err(PresetError::Preset(msg))
        }
        Err(e) => Err(PresetError::Fatal(format!("Không chạy được ffmpeg: {e:#}"))),
    }
}

async fn fail(store: &JobStore, id: &Uuid, msg: &str) {
    tracing::warn!(job = %id, "Job lỗi: {msg}");
    store
        .update(id, |j| {
            j.status = JobStatus::Error;
            j.error = Some(msg.to_string());
            j.cancel = None;
        })
        .await;
    // B6: job fail → dọn sạch file input/trung gian trong workspace để
    // không phình đĩa khi upload nhiều lần hỏng (output tốt đã nằm trong
    // library, không mất).
    let dir = job_dir_abs(id);
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(job = %id, "Không dọn được job dir sau lỗi: {e}");
        }
    }
}

#[cfg(test)]
mod parallel_tests {
    use super::*;

    #[test]
    fn test_max_parallel_presets_trong_gioi_han() {
        let n = max_parallel_presets();
        assert!((1..=8).contains(&n), "max_parallel_presets = {n} ngoài 1..=8");
    }

    #[test]
    fn test_max_parallel_mac_dinh_ncua2() {
        // Không đặt RVM_MAX_PARALLEL (env test sạch) — phải về ncpu/2 chặn 1..=4.
        // Lưu ý: nếu env đã đặt ở môi trường chạy test, kết quả vẫn phải hợp lệ.
        if std::env::var("RVM_MAX_PARALLEL").is_err() {
            let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            assert_eq!(max_parallel_presets(), (ncpu / 2).clamp(1, 4));
        }
    }

    // -----------------------------------------------------------------
    // Hồi quy mục 3 (bản 0.4.1): vòng đời job con + tổng hợp job cha.
    // -----------------------------------------------------------------

    /// Job cha mẫu với 3 job con để tái dùng trong các test vòng đời.
    fn job_with_children() -> Job {
        let mk = |preset: &str| ChildJob {
            preset_id: preset.into(),
            label: preset.into(),
            status: JobStatus::Queued,
            progress: 0.0,
            error: None,
            filename: None,
        };
        Job {
            id: Uuid::new_v4(),
            status: JobStatus::Queued,
            progress: 0.0,
            error: None,
            zip: None,
            done_count: 0,
            total_count: 0,
            children: vec![mk("tiktok"), mk("fb"), mk("yt")],
            cancel: None,
            output_stem: None,
        }
    }

    #[tokio::test]
    async fn child_lifecycle_queued_running_done() {
        // Giả lập đúng trình tự run_job thật: con được slot → running,
        // ffmpeg báo progress riêng → xong → done.
        let store = JobStore::new();
        let job = job_with_children();
        let id = job.id;
        store.insert(job).await;

        // Đầu job: mọi con queued, cha queued, progress 0.
        let st = store.get_status(&id).await.unwrap();
        assert!(st.children.iter().all(|c| c.status == JobStatus::Queued));
        assert_eq!(st.status, JobStatus::Queued);

        // mark_child_running chỉ đúng con được slot; con khác vẫn queued.
        store.mark_child_running(&id, "tiktok").await;
        store.mark_child_running(&id, "fb").await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.children[0].status, JobStatus::Running);
        assert_eq!(st.children[1].status, JobStatus::Running);
        assert_eq!(st.children[2].status, JobStatus::Queued);
        // Cha chuyển running ngay khi có con chạy.
        assert_eq!(st.status, JobStatus::Running);

        // Progress từ ffmpeg chỉ cập nhật con đang chạy/đợi — con queued
        // (chưa có slot) KHÔNG nhận progress của con khác.
        store.set_child_progress(&id, "tiktok", 40.0).await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.children[0].progress, 40.0);
        assert_eq!(st.children[2].progress, 0.0);
        // Progress cha = trung bình các con.
        assert!((st.progress - 40.0 / 3.0).abs() < 1e-9);

        // Con xong: done + progress chốt 100.
        store
            .update_child(&id, "tiktok", |c| {
                c.status = JobStatus::Done;
                c.progress = 100.0;
                c.filename = Some("tiktok.mp4".into());
            })
            .await;
        // Progress đến muộn sau khi done KHÔNG được ghi đè mốc 100.
        store.set_child_progress(&id, "tiktok", 55.0).await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.children[0].status, JobStatus::Done);
        assert_eq!(st.children[0].progress, 100.0);

        // Các con còn lại lần lượt done → cha done, progress 100.
        store.update_child(&id, "fb", |c| c.status = JobStatus::Done).await;
        store.update_child(&id, "yt", |c| c.status = JobStatus::Done).await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.status, JobStatus::Done);
        assert_eq!(st.done_count, 3);
        assert_eq!(st.progress, 100.0);
    }

    #[tokio::test]
    async fn child_error_partial_still_done_parent() {
        // 1 con lỗi, 2 con tốt → cha done với thông báo "x/3 thành công".
        let store = JobStore::new();
        let job = job_with_children();
        let id = job.id;
        store.insert(job).await;
        store.mark_child_running(&id, "tiktok").await;
        store.mark_child_running(&id, "fb").await;
        store.update_child(&id, "tiktok", |c| c.status = JobStatus::Done).await;
        store
            .update_child(&id, "fb", |c| {
                c.status = JobStatus::Error;
                c.error = Some("ffmpeg lỗi".into());
            })
            .await;
        store.update_child(&id, "yt", |c| c.status = JobStatus::Done).await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.status, JobStatus::Done);
        assert_eq!(st.done_count, 2);
        assert!(st.error.as_deref().unwrap_or("").contains("2/3"));
    }

    #[tokio::test]
    async fn child_cancelled_marks_parent_cancelled() {
        // Huỷ giữa chừng: con chưa chạy → cancelled, cha cancelled.
        let store = JobStore::new();
        let job = job_with_children();
        let id = job.id;
        store.insert(job).await;
        store.mark_child_running(&id, "tiktok").await;
        store.update_child(&id, "tiktok", |c| c.status = JobStatus::Done).await;
        // cancel(): con queued → cancelled, cha → cancelled.
        assert!(store.cancel(&id).await);
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.status, JobStatus::Cancelled);
        assert_eq!(st.children[1].status, JobStatus::Cancelled);
        assert_eq!(st.children[2].status, JobStatus::Cancelled);
        // Con đã done vẫn giữ done.
        assert_eq!(st.children[0].status, JobStatus::Done);
    }

    #[tokio::test]
    async fn running_child_not_demoted_by_late_progress() {
        // Bất biến: progress callback trễ không được làm hỏng trạng thái con.
        let store = JobStore::new();
        let job = job_with_children();
        let id = job.id;
        store.insert(job).await;
        store.mark_child_running(&id, "tiktok").await;
        store.set_child_progress(&id, "tiktok", 99.0).await;
        // mark_child_running lần 2 (idempotent) không đổi gì.
        store.mark_child_running(&id, "tiktok").await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.children[0].status, JobStatus::Running);
        assert_eq!(st.children[0].progress, 99.0);
    }

    // -----------------------------------------------------------------
    // Hồi quy mục 4 (bản 0.4.1): dọn workspace mồ côi lúc khởi động.
    // -----------------------------------------------------------------

    #[test]
    fn cleanup_orphans_removes_dirs_keeps_files() {
        // Thư mục tạm mô phỏng workspace sau lần chạy bị kill: 2 thư mục job
        // mồ côi + 1 file rác (file thường KHÔNG bị xoá vì không phải dir).
        let ws = std::env::temp_dir().join(format!("rvm_test_ws_{}", Uuid::new_v4()));
        std::fs::create_dir_all(ws.join("11111111-1111-1111-1111-111111111111/sub")).unwrap();
        std::fs::create_dir_all(ws.join("22222222-2222-2222-2222-222222222222")).unwrap();
        std::fs::write(ws.join("stray.txt"), "rác").unwrap();

        let removed = cleanup_orphans_in(&ws);

        assert_eq!(removed, 2, "phải xoá đúng 2 thư mục job mồ côi");
        assert!(!ws.join("11111111-1111-1111-1111-111111111111").exists());
        assert!(!ws.join("22222222-2222-2222-2222-222222222222").exists());
        assert!(ws.join("stray.txt").exists(), "file thường nằm ngoài workspace/<job>/ không bị xoá");
        assert!(ws.exists(), "bản thân workspace/ phải còn");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn cleanup_orphans_empty_or_missing_dir() {
        // Workspace trống → 0; workspace chưa tồn tại → 0, không panic.
        let ws = std::env::temp_dir().join(format!("rvm_test_empty_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        assert_eq!(cleanup_orphans_in(&ws), 0);
        std::fs::remove_dir_all(&ws).unwrap();
        let missing = std::env::temp_dir().join(format!("rvm_test_missing_{}", Uuid::new_v4()));
        assert_eq!(cleanup_orphans_in(&missing), 0);
    }
}

/// Chỉ để unit test gọi hàm private extract_json_object.
#[cfg(test)]
pub fn extract_json_object_for_test(s: &str) -> Option<&str> {
    extract_json_object(s)
}
