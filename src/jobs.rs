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

use crate::ffmpeg::{self, build_final_args, build_final_args_with_segment, build_normalize_args, build_concat_args, build_xfade_args, concat_list_content, probe, validate_xfade, AudioSegment, FitMode};
use crate::presets::{Registry, RenderTarget};
use crate::split::{self, Span};
use crate::textover::{FrameCtx, SubtitleOptions, TextOverlayOptions, TextPosition, WatermarkOptions};
use crate::visuals::{self, VisualOptions};

/// Thư mục làm việc: `<data_dir>/workspace/<job_id>/` (M8: data dir theo
/// RVM_DATA_DIR / tương thích CWD / cạnh exe).
pub fn workspace_dir() -> PathBuf {
    crate::datadir::data_dir().join("workspace")
}

/// Thư mục kho output đã xuất (A2/A3) trong thư mục dữ liệu.
pub fn library_dir() -> PathBuf {
    crate::datadir::data_dir().join("library")
}

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
    workspace_dir().join(job_id.to_string())
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
    /// B2: chia phần khi audio dài hơn `max_duration_sec` của preset.
    #[serde(default)]
    pub split: SplitOptions,
}

/// B2: tuỳ chọn chia phần. Chế độ chọn chung cho cả job ("cut" mặc định —
/// hành vi cũ: bản xuất cắt cụt ở giới hạn nền tảng; "split" — chia thành
/// nhiều video, mỗi video một job con).
#[derive(Debug, Clone, Deserialize)]
pub struct SplitOptions {
    /// "cut" | "split".
    #[serde(default = "default_split_mode")]
    pub mode: String,
    /// Gắn nhãn "Phần i/n" lên video (mặc định tắt).
    #[serde(default)]
    pub label_enabled: bool,
    #[serde(default = "default_label_position")]
    pub label_position: TextPosition,
    #[serde(default = "default_label_size")]
    pub label_size_px: u32,
    #[serde(default = "default_label_color")]
    pub label_color: String,
    /// Điểm cắt kéo tay trên UI (giây trên audio gốc), theo preset id. Có
    /// điểm tay thì dùng đúng; không có thì chia tự động (ưu tiên im lặng).
    #[serde(default)]
    pub cuts: HashMap<String, Vec<f64>>,
}

fn default_split_mode() -> String {
    "cut".into()
}
fn default_label_position() -> TextPosition {
    TextPosition::Top
}
fn default_label_size() -> u32 {
    36
}
fn default_label_color() -> String {
    "#FFFFFF".into()
}

impl Default for SplitOptions {
    fn default() -> Self {
        Self {
            mode: default_split_mode(),
            label_enabled: false,
            label_position: default_label_position(),
            label_size_px: default_label_size(),
            label_color: default_label_color(),
            cuts: HashMap::new(),
        }
    }
}

impl SplitOptions {
    pub fn enabled(&self) -> bool {
        self.mode == "split"
    }

    /// Màu nhãn đã kiểm tra: không phải `#RRGGBB` → trắng (không để chuỗi
    /// lạ lọt vào filter drawtext).
    pub fn label_color_safe(&self) -> String {
        if crate::textover::parse_hex_color(&self.label_color).is_some() {
            self.label_color.trim().to_string()
        } else {
            default_label_color()
        }
    }

    /// Cỡ chữ nhãn giới hạn trong khoảng 12–240px (baseline 1080).
    pub fn label_size_clamped(&self) -> u32 {
        self.label_size_px.clamp(12, 240)
    }
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

/// B2: thông tin phần khi preset được chia (mỗi phần = 1 job con).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ChildPart {
    /// 1-based.
    pub index: usize,
    pub total: usize,
    /// `[start, end)` trên audio gốc, giây.
    pub start: f64,
    pub end: f64,
}

/// Trạng thái 1 job con (1 preset, hoặc 1 phần của preset khi chia phần).
#[derive(Debug, Clone, Serialize)]
pub struct ChildJob {
    /// Khoá duy nhất trong job cha: = `preset_id`, hoặc `<preset_id>.phan<N>`
    /// khi preset chia phần (API tải/xem dùng khoá này).
    pub key: String,
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
    /// B2: Some khi job con là một phần của preset được chia.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part: Option<ChildPart>,
}

impl ChildJob {
    /// Job con cho một preset (chưa chia phần) — key = preset_id.
    pub fn for_preset(target: &RenderTarget) -> Self {
        Self {
            key: target.preset_id.clone(),
            preset_id: target.preset_id.clone(),
            label: target.label.clone(),
            status: JobStatus::Queued,
            progress: 0.0,
            error: None,
            filename: None,
            part: None,
        }
    }

    /// Khoá của phần thứ `index` (1-based) của một preset.
    pub fn part_key(preset_id: &str, index: usize) -> String {
        format!("{preset_id}.phan{index}")
    }
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
    /// B2: run_job còn bước cuối (nén zip, dọn file) — mọi job con đã xong
    /// nhưng cha CHƯA chuyển "done", để UI không báo xong khi chưa có zip
    /// (job chia phần có nhiều file, bước nén thấy rõ). `finish()` gỡ cờ.
    pub finalizing: bool,
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
    /// `key` = khoá job con (preset_id, hoặc `<preset>.phan<N>` khi chia phần).
    pub async fn get_child_display_name(&self, id: &Uuid, key: &str) -> Option<String> {
        self.child(id, key).await?.filename
    }

    /// Một job con theo khoá.
    pub async fn child(&self, id: &Uuid, key: &str) -> Option<ChildJob> {
        let jobs = self.jobs.read().await;
        jobs.get(id)?
            .children
            .iter()
            .find(|c| c.key == key)
            .cloned()
    }

    /// B2: khoá mọi job con theo thứ tự (xếp thứ tự entry zip).
    pub async fn child_keys(&self, id: &Uuid) -> Vec<String> {
        self.jobs
            .read()
            .await
            .get(id)
            .map(|j| j.children.iter().map(|c| c.key.clone()).collect())
            .unwrap_or_default()
    }

    /// B2: mở rộng các job con theo kế hoạch chia phần: preset có ≥ 2 phần
    /// được thay bằng bấy nhiêu job con (khoá `<preset>.phan<N>`), các job
    /// con khác giữ nguyên. Gọi MỘT lần trong run_job sau khi biết độ dài
    /// audio (create_job tạo job con trước khi probe).
    pub async fn expand_children(&self, id: &Uuid, plans: &HashMap<String, Vec<Span>>) {
        let mut jobs = self.jobs.write().await;
        let Some(job) = jobs.get_mut(id) else { return };
        let mut expanded: Vec<ChildJob> = Vec::with_capacity(job.children.len());
        for c in job.children.drain(..) {
            match plans.get(&c.preset_id) {
                Some(spans) if spans.len() > 1 => {
                    let total = spans.len();
                    for (i, (start, end)) in spans.iter().enumerate() {
                        let index = i + 1;
                        expanded.push(ChildJob {
                            key: ChildJob::part_key(&c.preset_id, index),
                            preset_id: c.preset_id.clone(),
                            label: format!("{} — Phần {index}/{total}", c.label),
                            status: c.status,
                            progress: c.progress,
                            error: None,
                            filename: None,
                            part: Some(ChildPart { index, total, start: *start, end: *end }),
                        });
                    }
                }
                _ => expanded.push(c),
            }
        }
        job.children = expanded;
        recompute_parent(job);
    }

    /// B2: run_job đã xong mọi bước cuối (zip, dọn) → gỡ cờ `finalizing`
    /// và tính lại trạng thái cha (lúc này mới chuyển "done").
    pub async fn finish(&self, id: &Uuid) {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(id) {
            job.finalizing = false;
            recompute_parent(job);
        }
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

    /// `key` = khoá job con; file kho lưu theo preset THẬT (`<preset_id>__<filename>`).
    pub async fn get_child_output(&self, id: &Uuid, key: &str) -> Option<PathBuf> {
        let jobs = self.jobs.read().await;
        let job = jobs.get(id)?;
        let child = job.children.iter().find(|c| c.key == key)?;
        let filename = child.filename.as_ref()?.clone();
        let ws_path = job_dir_abs(&job.id).join(&filename);
        if tokio::fs::try_exists(&ws_path).await.unwrap_or(false) {
            return Some(ws_path);
        }
        // Fallback: kho library (output da copy o drain_preset_results).
        let lib = library_dir()
            .join(job.id.to_string())
            .join(format!("{}__{filename}", child.preset_id));
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
                if let Some(token) = job.cancel.as_ref() {
                    token.cancel();
                }
                // KHÔNG `take()` token: pipeline tra token theo job_id cho từng
                // bước còn lại — token đã huỷ giữ lại thì các bước sau dừng
                // ngay, không chạy nốt ffmpeg trên file đã bị dọn.
                true
            }
            _ => false,
        }
    }

    /// Job đã bị huỷ (trạng thái "cancelled")? Mọi lỗi phát sinh SAU khi huỷ
    /// chỉ là hệ quả của việc huỷ — xem [`fail`] và `recompute_parent`.
    pub async fn was_cancelled(&self, id: &Uuid) -> bool {
        let jobs = self.jobs.read().await;
        jobs.get(id).map(|j| j.status == JobStatus::Cancelled).unwrap_or(false)
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
        for job in jobs.values_mut() {
            if let Some(token) = job.cancel.take() {
                token.cancel();
            }
        }
    }

    async fn get_cancel_token(&self, id: &Uuid) -> Option<CancellationToken> {
        let jobs = self.jobs.read().await;
        jobs.get(id).and_then(|j| j.cancel.clone())
    }

    /// Cập nhật 1 job con theo khoá rồi tính lại tổng trạng thái job cha.
    async fn update_child(&self, id: &Uuid, key: &str, f: impl FnOnce(&mut ChildJob)) {
        let mut jobs = self.jobs.write().await;
        let Some(job) = jobs.get_mut(id) else { return };
        if let Some(child) = job.children.iter_mut().find(|c| c.key == key) {
            f(child);
        }
        recompute_parent(job);
    }

    /// Đánh dấu 1 job con bắt đầu chạy thật (đã có slot) rồi tính lại cha.
    async fn mark_child_running(&self, id: &Uuid, key: &str) {
        self.update_child(id, key, |c| {
            if c.status == JobStatus::Queued {
                c.status = JobStatus::Running;
            }
        })
        .await;
    }

    /// Cập nhật tiến độ job con (được gọi từ callback ffmpeg).
    async fn set_child_progress(&self, id: &Uuid, key: &str, pct: f64) {
        self.update_child(id, key, |c| {
            // Progress chỉ có nghĩa khi con đang chạy — con đã done/error thì
            // giữ nguyên mốc của nó.
            if c.status == JobStatus::Queued || c.status == JobStatus::Running {
                c.progress = pct;
            }
        })
        .await;
    }
}

/// Progress cha từ trung bình các job con — HÀM THUẦN để test riêng.
///
/// Quy tắc "không bao giờ lùi": kẹp 0–100, rồi lấy max với giá trị cha đã
/// báo trước đó. Job con mới bắt đầu (progress về 0) không kéo cha xuống —
/// ví dụ 2 con, con 1 xong (100) + con 2 mới bắt đầu (0) → cha giữ 50 chứ
/// không rơi về 50→0 hay dao động.
pub fn parent_progress(children: &[f64], previous: f64) -> f64 {
    if children.is_empty() {
        return previous.clamp(0.0, 100.0);
    }
    let avg = children.iter().sum::<f64>() / children.len() as f64;
    avg.clamp(0.0, 100.0).max(previous.clamp(0.0, 100.0))
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
    let child_p: Vec<f64> = job.children.iter().map(|c| c.progress).collect();
    job.progress = parent_progress(&child_p, job.progress);

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
        // Job đã bị huỷ: job con lỗi là hệ quả của việc huỷ (route huỷ dọn
        // thư mục job, các bước còn lại bỏ chạy) → vẫn báo "đã huỷ", không
        // đổi thành "lỗi" / "xong một phần".
        if job.status == JobStatus::Cancelled {
            return;
        }
        if done > 0 {
            // Một phần thành công: vẫn done nhưng kèm thông báo lỗi.
            // (còn `finalizing` → chờ run_job nén zip xong rồi mới done)
            if !job.finalizing {
                job.status = JobStatus::Done;
            }
            // B2: job con là phần chia thì gọi là "file" cho đúng.
            let unit = if job.children.iter().any(|c| c.part.is_some()) { "file" } else { "preset" };
            job.error = Some(format!(
                "{done}/{total} {unit} thành công; {error} {unit} lỗi."
            ));
        } else {
            job.status = JobStatus::Error;
        }
        return;
    }
    // Mọi job con đều xong: zip được tạo ở cuối run_job (cần file thật) —
    // khi còn `finalizing` chỉ nâng tiến độ lên 100, trạng thái "done" chờ
    // `finish()` để UI không thấy "xong" khi chưa có file zip.
    if job.finalizing {
        return;
    }
    job.status = JobStatus::Done;
    job.progress = 100.0;
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

    // Bị cancel trước khi kịp chạy (token chưa đăng ký lúc này → xét cả trạng
    // thái: route huỷ đã dọn thư mục job, chạy tiếp chỉ sinh lỗi oan).
    if store.was_cancelled(&job_id).await {
        tracing::info!(job = %job_id, "Job bị huỷ trước khi chạy");
        return;
    }
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
        match generate_srt_auto(&audio_path, audio_info.duration, &parent_token).await {
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

    // 2b. B2 — chia phần: chế độ "split" + audio dài hơn giới hạn preset →
    //     dò im lặng một lần, tính kế hoạch từng preset, rồi mở rộng job con
    //     (mỗi phần = 1 job con, khoá `<preset>.phan<N>`). create_job đã tạo
    //     job con theo preset từ trước (chưa biết độ dài audio) nên phải
    //     mở rộng ở đây.
    let split_plans = plan_splits_for_job(
        &store,
        &job_id,
        &options,
        &targets,
        &audio_path,
        audio_info.duration,
        &parent_token,
    )
    .await;
    if parent_token.is_cancelled() {
        return;
    }
    if !split_plans.is_empty() {
        store.expand_children(&job_id, &split_plans).await;
    }

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
    // B2: tiến độ bước dựng chuỗi chung cộng dồn cho MỌI job con của các
    // preset cố định — kể cả các phần của preset được chia (khoá `.phanN`).
    let fixed_keys: Vec<String> = fixed
        .iter()
        .flat_map(|t| parts_for(t, &split_plans))
        .map(|p| p.key)
        .collect();

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
            &job_dir, &fixed_keys, &mut intermediates,
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
    //     B2: preset được chia thì MỖI PHẦN là một task riêng (job con riêng,
    //     cùng semaphore) — video lặp từ đầu chuỗi, audio lấy đúng đoạn.
    {
        let sem = Arc::new(tokio::sync::Semaphore::new(max_parallel_presets()));
        let mut set: tokio::task::JoinSet<Vec<PartResult>> = tokio::task::JoinSet::new();
        for target in &fixed {
            // Ngữ cảnh khung cho overlay: kích thước preset + safe-zone của preset.
            let frame = FrameCtx::new(
                target.width.unwrap_or(1080),
                target.height.unwrap_or(1920),
                registry_safe(&registry, &target.preset_id),
            );
            for part in parts_for(target, &split_plans) {
                let store = store.clone();
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
                let key = part.key.clone();
                set.spawn(async move {
                    // Chưa tới lượt hoặc job đã huỷ → thoát sớm, không chiếm slot.
                    if is_cancelled(&store, &job_id).await {
                        return vec![(key, Err(PresetError::Cancelled))];
                    }
                    let _permit = match sem.acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => return vec![(key, Err(PresetError::Cancelled))],
                    };
                    // Có slot → job con này bắt đầu chạy thật (queued → running).
                    store.mark_child_running(&job_id, &key).await;
                    let res = render_preset_from_chain(
                        &store, &job_id, &target, &chain, &audio_path, &uploads, &frame,
                        &overlay_text_path, &wm_text_path, audio_info.duration, &options,
                        &job_dir, out_stem.as_deref(), multi, part.part.as_ref(),
                    )
                    .await;
                    vec![(key, res)]
                });
            }
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
    //     song song như 4b. B2: chuỗi (chuẩn hoá + ghép) dựng MỘT lần cho
    //     preset rồi pass cuối chạy cho từng phần trong cùng task (tránh
    //     chuẩn hoá lại clip cho mỗi phần); mỗi phần vẫn là job con riêng.
    {
        let sem = Arc::new(tokio::sync::Semaphore::new(max_parallel_presets()));
        let mut set: tokio::task::JoinSet<Vec<PartResult>> = tokio::task::JoinSet::new();
        for target in &originals {
            let (w0, h0) = (clips[0].width.unwrap_or(1080), clips[0].height.unwrap_or(1920));
            let rotated = matches!(clips[0].rotation.rem_euclid(360), 90 | 270);
            let frame = FrameCtx::new(if rotated { h0 } else { w0 }, if rotated { w0 } else { h0 }, None);
            let store = store.clone();
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
            let parts = parts_for(&target, &split_plans);
            set.spawn(async move {
                let keys: Vec<String> = parts.iter().map(|p| p.key.clone()).collect();
                if is_cancelled(&store, &job_id).await {
                    return keys.into_iter().map(|k| (k, Err(PresetError::Cancelled))).collect();
                }
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return keys.into_iter().map(|k| (k, Err(PresetError::Cancelled))).collect(),
                };
                let mut results: Vec<PartResult> = Vec::new();
                let mut chain: Option<PreparedOriginal> = None;
                for (i, part) in parts.into_iter().enumerate() {
                    let keys_left: Vec<String> = keys.iter().skip(i + 1).cloned().collect();
                    let key = part.key.clone();
                    // Có slot → job con này bắt đầu chạy thật (queued → running).
                    store.mark_child_running(&job_id, &key).await;
                    let res = render_preset_original(
                        &store, &job_id, &target, &clips, &audio_path, &uploads, &frame,
                        &overlay_text_path, &wm_text_path, audio_info.duration, &options,
                        &job_dir, out_stem.as_deref(), multi, part.part.as_ref(), &mut chain,
                    )
                    .await;
                    let stop = matches!(res, Err(PresetError::Cancelled) | Err(PresetError::Fatal(_)));
                    results.push((key, res));
                    if stop {
                        // Phần còn lại của preset này không chạy nữa → đánh dấu huỷ.
                        results.extend(keys_left.into_iter().map(|k| (k, Err(PresetError::Cancelled))));
                        break;
                    }
                }
                results
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
        let mut set: tokio::task::JoinSet<Vec<PartResult>> = tokio::task::JoinSet::new();
        for target in &audio_only_targets {
            let store = store.clone();
            let target = target.clone();
            let audio_path = audio_path.clone();
            let options = options.clone();
            let job_dir = job_dir.clone();
            let out_stem = output_stem.clone();
            let multi = targets.len() > 1;
            let audio_duration = audio_info.duration;
            let key = target.preset_id.clone();
            set.spawn(async move {
                if is_cancelled(&store, &job_id).await {
                    return vec![(key, Err(PresetError::Cancelled))];
                }
                store.mark_child_running(&job_id, &key).await;
                let res = render_audio_only(
                    &store, &job_id, &target, &audio_path, audio_duration, &options,
                    &job_dir, out_stem.as_deref(), multi,
                )
                .await;
                vec![(key, res)]
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
    //    B2: khi nhiều preset CÓ chia phần → gom vào thư mục con từng preset
    //    (`<preset>/<tên>_phan1.mp4`) cho dễ nhìn; entry xếp theo thứ tự job
    //    con (preset theo thứ tự chọn, phần theo thứ tự số).
    if ok_outputs.len() >= 2 {
        let zip_path = job_dir.join("outputs.zip");
        let has_parts = split_plans.values().any(|p| p.len() > 1);
        let use_folders = has_parts && targets.len() > 1;
        let order = store.child_keys(&job_id).await;
        let mut sorted = ok_outputs.clone();
        sorted.sort_by_key(|(key, _)| order.iter().position(|k| k == key).unwrap_or(usize::MAX));
        let mut entries: Vec<(String, String, PathBuf)> = Vec::with_capacity(sorted.len());
        for (key, p) in &sorted {
            let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if use_folders {
                let preset = store
                    .child(&job_id, key)
                    .await
                    .map(|c| c.preset_id)
                    .unwrap_or_else(|| key.split_once(".phan").map(|(b, _)| b.to_string()).unwrap_or_else(|| key.clone()));
                entries.push((format!("{preset}/{name}"), key.clone(), p.clone()));
            } else {
                entries.push((name, key.clone(), p.clone()));
            }
        }
        let mut writer = crate::zipwriter::ZipWriter::new(std::io::BufWriter::new(
            match std::fs::File::create(&zip_path) {
                Ok(f) => f,
                Err(e) => {
                    fail(&store, &job_id, &format!("Không tạo được file zip: {e}")).await;
                    return;
                }
            },
        ));
        for (name, key, path) in &entries {
            let data = match read_output_bytes(&store, &job_id, key, path).await {
                Ok(d) => d,
                Err(e) => {
                    fail(&store, &job_id, &format!("Không nén được {name}: {e}")).await;
                    return;
                }
            };
            if let Err(e) = writer.add_bytes(name, &data) {
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
                tracing::info!(job = %job_id, "Job cha hoan tat: {} output", ok_outputs.len());
                store.finish(&job_id).await;
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
    // B2: mọi bước cuối đã xong (zip đã có) → mới chuyển job sang "done".
    store.finish(&job_id).await;
}

/// Đọc bytes một output để nén zip. File trong workspace có thể bị chương
/// trình bên ngoài (phần mềm diệt virus, công cụ đồng bộ hoá thư mục) khoá
/// hoặc dời đi trong giây lát → thử lại vài lần; vẫn lỗi thì lấy bản đã copy
/// vào kho khi job con xong (kho là nơi lưu lâu dài, workspace chỉ là tạm).
async fn read_output_bytes(
    store: &JobStore,
    job_id: &Uuid,
    key: &str,
    path: &std::path::Path,
) -> std::io::Result<Vec<u8>> {
    let fallback = store
        .child(job_id, key)
        .await
        .and_then(|c| {
            c.filename
                .map(|f| library_dir().join(job_id.to_string()).join(format!("{}__{f}", c.preset_id)))
        });
    read_with_fallback(path, fallback.as_deref()).await
}

/// Bản thuần của [read_output_bytes]: `fallback` = bản trong kho (nếu có).
async fn read_with_fallback(
    path: &std::path::Path,
    fallback: Option<&std::path::Path>,
) -> std::io::Result<Vec<u8>> {
    const ATTEMPTS: usize = 6;
    let use_fallback = |why: String| async move {
        match fallback {
            Some(f) => match tokio::fs::read(f).await {
                Ok(data) => {
                    tracing::warn!(
                        "File output trong workspace không đọc được ({why}) — nén từ bản trong kho."
                    );
                    Some(data)
                }
                Err(e) => {
                    tracing::warn!("Không đọc được {} để nén thay thế: {e}", f.display());
                    None
                }
            },
            None => None,
        }
    };
    let mut last: Option<std::io::Error> = None;
    for _ in 0..ATTEMPTS {
        match tokio::fs::read(path).await {
            Ok(data) if !data.is_empty() => return Ok(data),
            Ok(_) => {
                // File 0 byte: ffmpeg có thể còn ghi nốt (moov) — chờ thêm.
                last = Some(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("{} rỗng (0 byte)", path.display()),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // File không còn trong workspace mà kho đã có bản → dùng luôn.
                let why = format!("{}: {e}", path.display());
                if let Some(data) = use_fallback(why).await {
                    return Ok(data);
                }
                last = Some(e);
            }
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    let err = last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, format!("không đọc được {}", path.display()))
    });
    if let Some(data) = use_fallback(format!("{}: {err}", path.display())).await {
        return Ok(data);
    }
    Err(err)
}

// ---------------------------------------------------------------------------
// B2 — chia phần
// ---------------------------------------------------------------------------

/// Một đầu việc render: cả preset (không chia) hoặc một phần của preset.
#[derive(Debug, Clone)]
struct PartRender {
    /// Khoá job con tương ứng.
    key: String,
    /// None = toàn audio trong một file; Some = một phần khi chia.
    part: Option<ChildPart>,
}

/// Các phần cần render của một preset theo kế hoạch chia (1 phần khi không chia).
fn parts_for(target: &RenderTarget, plans: &HashMap<String, Vec<Span>>) -> Vec<PartRender> {
    match plans.get(&target.preset_id) {
        Some(spans) if spans.len() > 1 => {
            let total = spans.len();
            spans
                .iter()
                .enumerate()
                .map(|(i, (start, end))| PartRender {
                    key: ChildJob::part_key(&target.preset_id, i + 1),
                    part: Some(ChildPart { index: i + 1, total, start: *start, end: *end }),
                })
                .collect()
        }
        _ => vec![PartRender { key: target.preset_id.clone(), part: None }],
    }
}

/// Preset có cần chia không: có giới hạn, audio dài hơn giới hạn, và không
/// phải preset "chỉ audio" (MP3 không có video để chia theo yêu cầu này).
fn needs_split(target: &RenderTarget, audio_duration: f64) -> bool {
    !crate::presets::is_audio_only(&target.preset_id)
        && target
            .max_duration_sec
            .is_some_and(|max| max > 0 && audio_duration > max as f64 + 1e-6)
}

/// Lập kế hoạch chia cho mọi preset vượt giới hạn (chế độ "split"): trả về
/// `preset_id → [(start, end), …]` (chỉ chứa preset CÓ kế hoạch chia). Dò im
/// lặng đúng MỘT lần cho cả audio, chỉ khi có preset chưa có điểm cắt tay.
/// Điểm cắt tay không hợp lệ (tạo phần vượt giới hạn) → quay về chia tự động
/// và báo qua `job.error` (không làm chết job).
#[allow(clippy::too_many_arguments)]
async fn plan_splits_for_job(
    store: &JobStore,
    job_id: &Uuid,
    options: &JobOptions,
    targets: &[RenderTarget],
    audio_path: &std::path::Path,
    audio_duration: f64,
    parent_token: &CancellationToken,
) -> HashMap<String, Vec<Span>> {
    let mut plans: HashMap<String, Vec<Span>> = HashMap::new();
    if !options.split.enabled() {
        return plans;
    }
    let over: Vec<&RenderTarget> = targets.iter().filter(|t| needs_split(t, audio_duration)).collect();
    if over.is_empty() {
        return plans;
    }
    let needs_silences = over.iter().any(|t| options.split.cuts.get(&t.preset_id).map_or(true, |c| c.is_empty()));
    let silences: Vec<Span> = if needs_silences {
        tracing::info!(job = %job_id, "Chia phần: dò im lặng trên audio…");
        match split::detect_silences(audio_path, parent_token).await {
            Ok(s) => {
                tracing::info!(job = %job_id, count = s.len(), "Dò được {} đoạn im lặng", s.len());
                s
            }
            Err(e) => {
                if !parent_token.is_cancelled() {
                    tracing::warn!(job = %job_id, "Dò im lặng lỗi ({e}) — chia đúng mốc đều.");
                }
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    for t in over {
        let max = t.max_duration_sec.unwrap_or(0) as f64;
        let manual = options.split.cuts.get(&t.preset_id).filter(|c| !c.is_empty());
        let plan = match manual {
            Some(cuts) => match split::plan_from_cuts(audio_duration, max, cuts) {
                Ok(p) if p.len() >= 2 => p,
                Ok(_) => split::plan_splits(audio_duration, max, &silences),
                Err(msg) => {
                    tracing::warn!(job = %job_id, preset = %t.preset_id, "Điểm cắt tay không dùng được: {msg}");
                    store
                        .update(job_id, |j| {
                            j.error = Some(format!(
                                "Điểm cắt tay của \"{}\" không hợp lệ ({msg}) — đã chia tự động.",
                                t.label
                            ));
                        })
                        .await;
                    split::plan_splits(audio_duration, max, &silences)
                }
            },
            None => split::plan_splits(audio_duration, max, &silences),
        };
        if plan.len() > 1 {
            tracing::info!(
                job = %job_id,
                preset = %t.preset_id,
                parts = plan.len(),
                "Chia phần: {} phần",
                plan.iter().map(|(s, e)| format!("{}-{}", split::fmt_secs(*s), split::fmt_secs(*e))).collect::<Vec<_>>().join(", ")
            );
            plans.insert(t.preset_id.clone(), plan);
        }
    }
    plans
}

/// Chuỗi "giữ nguyên gốc" đã dựng sẵn cho preset — dùng chung cho mọi phần của
/// preset đó (B2), dọn sau khi phần cuối xong.
struct PreparedOriginal {
    norm_paths: Vec<PathBuf>,
    chain_path: PathBuf,
    list_path: Option<PathBuf>,
    single_source: Option<PathBuf>,
}

impl PreparedOriginal {
    fn chain(&self) -> &std::path::Path {
        self.single_source.as_deref().unwrap_or(&self.chain_path)
    }
}

/// Xoá các thư mục job mồ côi trong `ws_dir` (mặc định `workspace_dir()`) —
/// tàn dư của lần chạy trước bị kill đột ngột (mất điện, kill -9, crash).
/// Được gọi NGAY lúc khởi động, trước khi nhận job mới: lúc này chưa có job
/// nào đang chạy nên mọi thư mục con đều là rác an toàn để xoá (output tốt
/// đã nằm trong library/). Trả về số thư mục đã xoá.
pub fn cleanup_orphan_job_dirs() -> usize {
    cleanup_orphans_in(&workspace_dir())
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

/// Kết quả một job con trả về từ task render: (khoá job con, output).
type PartResult = (String, Result<PathBuf, PresetError>);

/// Thu kết quả các task preset đã spawn (4b/4c): cập nhật status từng job con
/// ngay khi nó xong. Mỗi task trả về 1 hoặc nhiều job con (B2: task của preset
/// "giữ nguyên gốc" chạy tuần tự các phần). Trả về:
/// - `Ok(Some(outputs))` — loạt chạy xong bình thường (`(khoá, đường dẫn)`);
/// - `Ok(None)` — có job con bị huỷ (đã huỷ token cha dừng phần còn lại);
/// - `Err(msg)` — lỗi fatal (đĩa/thư mục), đã huỷ token cha.
async fn drain_preset_results(
    store: &JobStore,
    job_id: &Uuid,
    parent_token: &CancellationToken,
    mut set: tokio::task::JoinSet<Vec<PartResult>>,
) -> Result<Option<Vec<(String, PathBuf)>>, String> {
    let mut ok_outputs: Vec<(String, PathBuf)> = Vec::new();
    let mut batch_cancelled = false;
    // A2: danh sách tên hiển thị đã chiếm chỗ trong kho (toàn bộ library/) —
    // nạp MỘT lần rồi thêm dần mỗi output, để " (2)", " (3)" không bao giờ
    // ghi đè file cũ.
    let mut taken_names: Vec<String> = library_taken_display_names();
    while let Some(joined) = set.join_next().await {
        let results = match joined {
            Ok(v) => v,
            Err(e) => {
                // Task panic — lỗi nghiêm trọng, dừng cả job.
                parent_token.cancel();
                return Err(format!("Task render preset lỗi: {e}"));
            }
        };
        for (key, res) in results {
        // preset THẬT của job con (khoá `.phanN` → phần của preset đó): kho
        // lưu và zip gom theo preset, không theo khoá phần.
        let preset_id = store
            .child(job_id, &key)
            .await
            .map(|c| c.preset_id)
            .unwrap_or_else(|| key.split_once(".phan").map(|(b, _)| b.to_string()).unwrap_or_else(|| key.clone()));
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
                if let Err(e) = library_save(job_id, &preset_id, &out_path) {
                    tracing::warn!(job = %job_id, "Không lưu vào kho: {e}");
                }
                ok_outputs.push((key.clone(), out_path));
                store
                    .update_child(job_id, &key, |c| {
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
                    .update_child(job_id, &key, |c| {
                        c.status = JobStatus::Cancelled;
                    })
                    .await;
                if !batch_cancelled {
                    batch_cancelled = true;
                    // Dừng các phần/preset còn lại (cả đang chạy lẫn đang chờ slot).
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
                    .update_child(job_id, &key, |c| {
                        c.status = JobStatus::Error;
                        c.error = Some(msg);
                    })
                    .await;
                // Tiếp tục các phần/preset khác.
            }
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
    cancel: &CancellationToken,
) -> Result<PathBuf, String> {
    let dir = audio_path
        .parent()
        .ok_or("Không xác định được thư mục job")?
        .to_path_buf();
    let segments = detect_speech_segments(audio_path, total_duration, cancel).await
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

/// Dò đoạn CÓ tiếng (cho phụ đề tự động) từ các khoảng im lặng mà runner chung
/// trả về: im ≥ 0.35s (-32dB) thì ngắt câu; đoạn cuối tới hết file tính luôn.
async fn detect_speech_segments(
    audio_path: &std::path::Path,
    total_duration: f64,
    cancel: &CancellationToken,
) -> Result<Vec<(f64, f64)>, String> {
    const FILTER: &str = "silencedetect=noise=-32dB:d=0.35";
    let silences = crate::split::detect_silences_with(audio_path, FILTER, cancel)
        .await
        .map_err(|e| format!("silencedetect lỗi: {e}"))?;
    let mut speech: Vec<(f64, f64)> = Vec::new();
    let mut cursor = 0.0f64;
    for (s, e) in silences {
        if s > cursor + 0.15 {
            speech.push((cursor, s));
        }
        cursor = cursor.max(e);
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
    let lib = library_dir();
    let Ok(job_dirs) = std::fs::read_dir(&lib) else {
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
    let dir = library_dir().join(job_id.to_string());
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
            // Huỷ job đúng lúc này: route huỷ đã xoá file vừa chuẩn hoá → không phải lỗi.
            _ if store.was_cancelled(job_id).await => return Err(PresetError::Cancelled),
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
/// B2: `part = Some` → render một phần của audio (đoạn `[start, end)`, fade
/// ở điểm cắt, tên file `_phanN`, nhãn "Phần i/n" nếu bật); video vẫn lặp
/// từ đầu chuỗi.
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
    part: Option<&ChildPart>,
) -> Result<PathBuf, PresetError> {
    let sub = match part {
        Some(p) => format!("preset {} phần {}/{}", target.preset_id, p.index, p.total),
        None => format!("preset {}", target.preset_id),
    };
    let tid = part.map(|p| ChildJob::part_key(&target.preset_id, p.index)).unwrap_or_else(|| target.preset_id.clone());

    // --- Overlay spec theo FrameCtx của preset + filter khớp khung đích ---
    // (tag = khoá job con: các phần render song song không ghi đè file nhau)
    let base = format!("fit_{}", tid.replace(['-', '.'], "_"));
    let out_duration = part
        .map(|p| p.end - p.start)
        .unwrap_or_else(|| crate::ffmpeg::out_duration(audio_duration, target.max_duration_sec));
    let mut overlay =
        build_overlay_spec(options, uploads, frame, wm_text_path, job_dir, &base, out_duration, part)
            .await
            .map_err(PresetError::Fatal)?;
    overlay.pre_scale = Some(crate::ffmpeg::build_fit_filter(
        target.width.unwrap_or(1080),
        target.height.unwrap_or(1920),
        options.fit_mode,
        &base,
    ));

    // --- Pass cuối ---
    let out_final = job_dir.join(output_filename(target, out_stem, multi, part.map(|p| p.index)));
    let segment = part.map(|p| AudioSegment::new(p.start, p.end, p.index == 1, p.index == p.total));
    let args = final_args(
        chain,
        audio_path,
        &out_final,
        audio_duration,
        target.max_duration_sec,
        &options.audio(),
        &overlay,
        uploads.logo.as_deref(),
        segment,
    );
    run_step(store, job_id, std::slice::from_ref(&tid), &format!("{sub}: hoàn thiện"), args,
             |frac| 45.0 + frac * 55.0, // 45–100%
             out_duration,
    )
    .await
    .map(|_| out_final)
}

/// Pipeline riêng cho preset "giữ nguyên gốc": chuẩn hoá fps-only từng clip
/// (kích thước nguồn) → ghép → pass cuối. File trung gian tự dọn khi xong.
/// B2: `prepared` giữ chuỗi đã dựng (dùng chung cho mọi phần của preset —
/// caller truyền `&mut None` cho phần đầu); `part = Some` → pass cuối chỉ cho
/// đoạn đó, dọn chuỗi sau khi phần cuối xong.
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
    part: Option<&ChildPart>,
    prepared: &mut Option<PreparedOriginal>,
) -> Result<PathBuf, PresetError> {
    let tid = part.map(|p| ChildJob::part_key(&target.preset_id, p.index)).unwrap_or_else(|| target.preset_id.clone());
    let sub = match part {
        Some(p) => format!("preset {} phần {}/{}", target.preset_id, p.index, p.total),
        None => format!("preset {}", target.preset_id),
    };

    // --- Bước 1–2 (một lần cho cả preset): chuẩn hoá từng clip + ghép chuỗi.
    // Tiến độ các bước này cộng cho MỌI phần của preset (các phần nối tiếp
    // nhau trên cùng thanh 0–45%).
    let part_keys: Vec<String> = match part {
        Some(p) => (1..=p.total).map(|i| ChildJob::part_key(&target.preset_id, i)).collect(),
        None => vec![tid.clone()],
    };
    if prepared.is_none() {
        let mut norm_paths: Vec<PathBuf> = Vec::new();
        for (i, clip) in clips.iter().enumerate() {
            let out = job_dir.join(format!("norm_{tid}_{i:03}.mp4"));
            let args = build_normalize_args(&clip.path, &out, target.width, target.height, target.fps, options.fit_mode, clip.rotation);
            run_step(store, job_id, &part_keys, &format!("{sub}: chuẩn hoá clip {i}"), args,
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
                // Huỷ job đúng lúc này: route huỷ đã xoá file vừa chuẩn hoá → không phải lỗi.
                _ if store.was_cancelled(job_id).await => return Err(PresetError::Cancelled),
                _ => return Err(PresetError::Preset("Không đo được clip sau chuẩn hoá.".into())),
            }
        }

        // --- Bước 2: ghép chuỗi ---
        let chain_path = job_dir.join(format!("chain_{tid}.mp4"));
        let mut list_path: Option<PathBuf> = None;
        let single_source: Option<PathBuf> = if options.xfade_duration > 0.0 && norm_paths.len() >= 2 {
            if let Err(msg) = validate_xfade(options.xfade_duration, norm_paths.len(), &norm_durations) {
                return Err(PresetError::Preset(msg));
            }
            let args = build_xfade_args(&norm_paths, &chain_path, &options.xfade_transition, options.xfade_duration, &norm_durations);
            run_step(store, job_id, &part_keys, &format!("{sub}: ghép xfade"), args,
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
            list_path = Some(list_txt.clone());
            let args = build_concat_args(&list_txt, &chain_path);
            run_step(store, job_id, &part_keys, &format!("{sub}: ghép concat"), args,
                     |frac| 20.0 + frac * 25.0,
                     norm_durations.iter().sum::<f64>(),
            )
            .await?;
            None
        } else {
            // 1 clip duy nhất: chính nó là chuỗi.
            Some(norm_paths[0].clone())
        };
        *prepared = Some(PreparedOriginal { norm_paths, chain_path, list_path, single_source });
    }
    let prep = prepared.as_ref().expect("chuỗi đã dựng ở trên");

    // --- Bước 3: dựng overlay spec (theo FrameCtx của preset này) ---
    let out_duration = part
        .map(|p| p.end - p.start)
        .unwrap_or_else(|| crate::ffmpeg::out_duration(audio_duration, target.max_duration_sec));
    let tag = tid.replace(['-', '.'], "_");
    let overlay = build_overlay_spec(
        options,
        uploads,
        frame,
        wm_text_path,
        job_dir,
        &tag,
        out_duration,
        part,
    )
    .await
    .map_err(PresetError::Fatal)?;

    // --- Bước 4: pass cuối ---
    let chain = prep.chain();
    let out_final = job_dir.join(output_filename(target, out_stem, multi, part.map(|p| p.index)));
    let segment = part.map(|p| AudioSegment::new(p.start, p.end, p.index == 1, p.index == p.total));
    let args = final_args(
        chain,
        audio_path,
        &out_final,
        audio_duration,
        target.max_duration_sec,
        &options.audio(),
        &overlay,
        uploads.logo.as_deref(),
        segment,
    );
    let result = run_step(store, job_id, std::slice::from_ref(&tid), &format!("{sub}: hoàn thiện"), args,
             |frac| 45.0 + frac * 55.0, // 45–100%
             out_duration,
    )
    .await
    .map(|_| out_final);

    // Dọn file trung gian của preset này khi phần CUỐI cùng xong (kể cả khi
    // lỗi, tốt nhất là gì còn).
    let is_last = part.map_or(true, |p| p.index == p.total);
    if is_last {
        if let Some(prep) = prepared.take() {
            for p in &prep.norm_paths {
                let _ = tokio::fs::remove_file(p).await;
            }
            let _ = tokio::fs::remove_file(&prep.chain_path).await;
            if let Some(l) = &prep.list_path {
                let _ = tokio::fs::remove_file(l).await;
            }
        }
    }
    result
}

/// Dựng [`OverlaySpec`] từ options + uploads cho 1 khung `frame`. Mỗi lớp chữ
/// ghi vào file riêng trong `job_dir` (drawtext đọc `textfile=` nên nội dung
/// không bao giờ lọt vào filtergraph); `tag` = id preset để các preset render
/// song song không ghi đè file của nhau. `out_duration` = độ dài thật của file
/// ra sau giới hạn nền tảng — thanh tiến độ chạy 0→100% theo đúng nó.
///
/// `Err` chỉ đến từ việc đọc/ghi file (SRT hỏng, đĩa đầy) — mang tính hệ thống.
///
/// B2: `part = Some` khi render một phần của audio — phụ đề và chữ overlay
/// có mốc thời gian được dời về timeline audio gốc (trừ `start`), câu nằm
/// ngoài phần bị bỏ; nếu bật `split.label_enabled`, thêm lớp chữ "Phần i/n"
/// (dùng lại đúng cơ chế drawtext của chữ overlay: vị trí/cỡ/màu chọn được).
#[allow(clippy::too_many_arguments)]
async fn build_overlay_spec(
    options: &JobOptions,
    uploads: &Uploads,
    frame: &FrameCtx,
    wm_text_path: &std::path::Path,
    job_dir: &std::path::Path,
    tag: &str,
    out_duration: f64,
    part: Option<&ChildPart>,
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
            // B2: phụ đề dời về timeline của phần (câu ngoài phần bị bỏ).
            let cues = match part {
                Some(p) => crate::split::shift_cues(cues, p.start, out_duration),
                None => cues,
            };
            let ass_path = job_dir.join(format!("sub_{tag}.ass"));
            tokio::fs::write(&ass_path, subtitle::render_ass(&cues, &style))
                .await
                .map_err(|e| format!("Không ghi được file phụ đề ASS: {e}"))?;
            spec.chain.push(build_subtitle_filter(&ass_path, sub_font.fonts_dir.as_deref()));
        }
    }

    // 2. Chữ overlay: kịch bản nhiều cảnh → nhiều lớp, mỗi lớp 1 file text.
    // B2: timeline audio gốc = t + start của phần; "hết phần" tính trên đó.
    let offset = part.map_or(0.0, |p| p.start);
    let layout = TextLayout { offset, duration: Some(out_duration + offset), reserve: viz_reserve };
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

    // 3b. B2 — nhãn "Phần i/n": một lớp drawtext dùng lại cơ chế chữ overlay
    // (font chữ overlay, cỡ/màu/vị trí riêng, viền mặc định để đọc được).
    if let Some(p) = part.filter(|_| options.split.label_enabled) {
        let mut label_opts = TextOverlayOptions::default();
        label_opts.enabled = true;
        label_opts.font = options.text.font.clone();
        label_opts.size_px = options.split.label_size_clamped();
        label_opts.color = options.split.label_color_safe();
        label_opts.position = options.split.label_position;
        let label_item = crate::textover::TextItem {
            content: format!("Phần {}/{}", p.index, p.total),
            start_sec: None,
            end_sec: None,
            position: options.split.label_position,
        };
        let path = job_dir.join(format!("label_{tag}.txt"));
        tokio::fs::write(&path, &label_item.content)
            .await
            .map_err(|e| format!("Không ghi được file nhãn phần: {e}"))?;
        if let Some(f) = build_text_filter(&label_item, &label_opts, &path, &text_font, frame, &layout) {
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

/// Args pass cuối: cả audio (không chia) hoặc một đoạn của audio (B2).
#[allow(clippy::too_many_arguments)]
fn final_args(
    chain: &std::path::Path,
    audio: &std::path::Path,
    output: &std::path::Path,
    audio_duration: f64,
    max_duration_sec: Option<u64>,
    options: &AudioOptions,
    overlay: &crate::textover::OverlaySpec,
    logo: Option<&std::path::Path>,
    segment: Option<AudioSegment>,
) -> Vec<String> {
    match segment {
        Some(seg) => build_final_args_with_segment(
            chain, audio, output, audio_duration, max_duration_sec, options, overlay, logo, Some(seg),
        ),
        None => build_final_args(
            chain, audio, output, audio_duration, max_duration_sec, options, overlay, logo,
        ),
    }
}

/// Tên file output cho 1 preset: `radio_<preset>_<WxH>.mp4`.
/// Tên file output của 1 preset. Có "Tên sản phẩm" (`out_stem`) thì đặt theo
/// quy tắc A2: `<tên>.mp4` (1 preset) / `<tên>_<preset_id>.mp4` (nhiều
/// preset); không có thì giữ cách đặt tên mặc định cũ.
/// A1: preset audio_only dùng đuôi `.mp3`.
/// B2: `part_index = Some(n)` → chèn `_phan<n>` trước đuôi file:
/// `<tên>_phan1.mp4` (1 preset) / `<tên>_<preset>_phan1.mp4` (nhiều preset) /
/// `radio_<preset>_<WxH>_phan1.mp4` (không đặt tên).
fn output_filename(target: &RenderTarget, out_stem: Option<&str>, multi: bool, part_index: Option<usize>) -> String {
    let ext = if crate::presets::is_audio_only(&target.preset_id) { "mp3" } else { "mp4" };
    let name = match out_stem {
        Some(stem) => crate::naming::preset_filename_ext(stem, &target.preset_id, multi, ext),
        None => match (target.width, target.height) {
            (Some(w), Some(h)) => format!("radio_{}_{}x{}.{ext}", target.preset_id, w, h),
            _ => format!("radio_{}.{ext}", target.preset_id),
        },
    };
    match part_index {
        Some(n) => crate::naming::part_filename(&name, n),
        None => name,
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
    let out_final = job_dir.join(output_filename(target, out_stem, multi, None));
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
    // Job đã bị huỷ rồi: route huỷ xoá thư mục job ngay, trong khi pipeline
    // còn đang chạy bước cuối (ffprobe sau chuẩn hoá, ghép chuỗi…) nên bước
    // đó báo lỗi. Đó là hệ quả của việc huỷ — giữ "đã huỷ", không thành "lỗi".
    if store.was_cancelled(id).await {
        tracing::info!(job = %id, "Bỏ qua lỗi phát sinh sau khi huỷ job: {msg}");
        return;
    }
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

/// Chỉ để unit test gọi hàm private extract_json_object.
#[cfg(test)]
pub fn extract_json_object_for_test(s: &str) -> Option<&str> {
    extract_json_object(s)
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
    fn parent_progress_khong_bao_gio_lui() {
        // 2 con, con 1 xong (100), con 2 bắt đầu (0) → cha giữ 50, không rơi.
        let p = parent_progress(&[100.0, 0.0], 50.0);
        assert_eq!(p, 50.0, "con mới bắt đầu không kéo cha xuống");
        // Trung bình tăng → cha theo.
        assert!((parent_progress(&[100.0, 40.0], 50.0) - 70.0).abs() < 1e-9);
        // Chuỗi đầy đủ: 2 con chạy song song, tiến độ chỉ tăng.
        let seq = [
            vec![10.0, 0.0],   // cha = 5
            vec![80.0, 5.0],   // cha = 42.5
            vec![100.0, 30.0], // cha = 65
            vec![100.0, 0.0],  // báo lại 50? → không, giữ 65
            vec![100.0, 100.0],// cha = 100
        ];
        let mut prev = 0.0;
        let mut seen: Vec<f64> = Vec::new();
        for children in seq {
            prev = parent_progress(&children, prev);
            seen.push(prev);
        }
        assert_eq!(seen, vec![5.0, 42.5, 65.0, 65.0, 100.0]);
        // Kẹp ngoài khoảng.
        assert_eq!(parent_progress(&[150.0], 0.0), 100.0);
        assert_eq!(parent_progress(&[-20.0], 0.0), 0.0);
        // Không có con → giữ nguyên giá trị cũ.
        assert_eq!(parent_progress(&[], 42.0), 42.0);
    }

    #[test]
    fn parent_progress_tu_trung_binh_khi_chua_co_gia_tri() {
        // Lần đầu (previous = 0): đúng trung bình.
        assert!((parent_progress(&[30.0, 50.0], 0.0) - 40.0).abs() < 1e-9);
        // 3 con: một con xong giữa chừng, 2 con mới chạy.
        assert!((parent_progress(&[100.0, 20.0, 20.0], 0.0) - 140.0 / 3.0).abs() < 1e-9);
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
            key: preset.into(),
            preset_id: preset.into(),
            label: preset.into(),
            status: JobStatus::Queued,
            progress: 0.0,
            error: None,
            filename: None,
            part: None,
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
            finalizing: false,
        }
    }

    #[tokio::test]
    async fn finalizing_cha_khong_done_truoc_khi_co_zip() {
        // Job mới tạo có finalizing = true (như create_job): mọi con xong mà
        // run_job còn bước nén zip → cha vẫn "running", progress đã 100.
        let store = JobStore::new();
        let mut job = job_with_children();
        job.finalizing = true;
        let id = job.id;
        store.insert(job).await;
        for k in ["tiktok", "fb", "yt"] {
            store
                .update_child(&id, k, |c| {
                    c.status = JobStatus::Done;
                    c.progress = 100.0;
                })
                .await;
        }
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.status, JobStatus::Running, "chưa có zip thì chưa báo xong");
        assert_eq!((st.progress, st.done_count, st.total_count), (100.0, 3, 3));
        assert!(!st.has_zip);
        // Nén xong → finish() mới chuyển done.
        store.finish(&id).await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.status, JobStatus::Done);
        // Huỷ / lỗi tất cả các phần thì không chờ zip (job không có gì để nén).
        let mut job2 = job_with_children();
        job2.finalizing = true;
        let id2 = job2.id;
        store.insert(job2).await;
        for k in ["tiktok", "fb", "yt"] {
            store
                .update_child(&id2, k, |c| c.status = JobStatus::Error)
                .await;
        }
        assert_eq!(store.get_status(&id2).await.unwrap().status, JobStatus::Error);
    }

    #[tokio::test]
    async fn zip_doc_output_mat_trong_workspace_lay_ban_trong_kho() {
        // File workspace còn → đọc workspace; biến mất → dùng bản trong kho;
        // không có bản kho → báo lỗi NotFound cho run_job fail job.
        let tmp = std::env::temp_dir().join(format!("rvm_zip_fb_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let ws = tmp.join("ws").join("clip_phan1.mp4");
        let lib = tmp.join("lib").join("tiktok__clip_phan1.mp4");
        std::fs::create_dir_all(ws.parent().unwrap()).unwrap();
        std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
        std::fs::write(&lib, b"KHO").unwrap();
        std::fs::write(&ws, b"WS").unwrap();
        assert_eq!(read_with_fallback(&ws, Some(&lib)).await.unwrap(), b"WS");
        std::fs::remove_file(&ws).unwrap();
        assert_eq!(read_with_fallback(&ws, Some(&lib)).await.unwrap(), b"KHO");
        let err = read_with_fallback(&ws, None).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&tmp).ok();
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

    // -----------------------------------------------------------------
    // B2: chia phần — mở rộng job con, khoá phần, tên file.
    // -----------------------------------------------------------------

    fn target(id: &str, max: Option<u64>) -> RenderTarget {
        RenderTarget {
            preset_id: id.into(),
            label: id.into(),
            width: Some(1080),
            height: Some(1920),
            fps: 30,
            max_duration_sec: max,
        }
    }

    #[tokio::test]
    async fn expand_children_moi_phan_mot_job_con() {
        let store = JobStore::new();
        let job = job_with_children();
        let id = job.id;
        store.insert(job).await;

        let mut plans: HashMap<String, Vec<Span>> = HashMap::new();
        plans.insert("fb".into(), vec![(0.0, 8.333), (8.333, 16.667), (16.667, 25.0)]);
        store.expand_children(&id, &plans).await;

        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.total_count, 5, "3 preset, 1 preset chia 3 → 5 job con");
        let keys: Vec<&str> = st.children.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["tiktok", "fb.phan1", "fb.phan2", "fb.phan3", "yt"]);
        let p2 = st.children.iter().find(|c| c.key == "fb.phan2").unwrap();
        assert_eq!(p2.preset_id, "fb", "preset THẬT giữ nguyên để lưu kho/zip");
        assert_eq!(p2.label, "fb — Phần 2/3");
        let part = p2.part.unwrap();
        assert_eq!((part.index, part.total, part.start, part.end), (2, 3, 8.333, 16.667));
        assert!(st.children[0].part.is_none(), "preset không chia không có part");
        assert_eq!(store.child_keys(&id).await, keys.iter().map(|k| k.to_string()).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn progress_job_con_phan_cong_vao_cha_khong_lui() {
        let store = JobStore::new();
        let job = job_with_children();
        let id = job.id;
        store.insert(job).await;
        let mut plans: HashMap<String, Vec<Span>> = HashMap::new();
        plans.insert("tiktok".into(), vec![(0.0, 10.0), (10.0, 20.0)]);
        store.expand_children(&id, &plans).await;

        // Chỉ phần 2 của tiktok chạy tới 50% → cha = 50/4 = 12.5.
        store.mark_child_running(&id, "tiktok.phan2").await;
        store.set_child_progress(&id, "tiktok.phan2", 50.0).await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.progress, 12.5);
        let c = |k: &str| st.children.iter().find(|c| c.key == k).unwrap().clone();
        assert_eq!(c("tiktok.phan1").status, JobStatus::Queued);
        assert_eq!(c("tiktok.phan2").status, JobStatus::Running);
        assert_eq!(c("tiktok.phan1").progress, 0.0);
        // Phần 1 bắt đầu chạy (progress về 0) → cha không lùi.
        store.mark_child_running(&id, "tiktok.phan1").await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.progress, 12.5, "cha không lùi khi job con mới bắt đầu");
        // Xong hết → 100, tên file theo khoá phần.
        store.update_child(&id, "tiktok.phan1", |c| {
            c.status = JobStatus::Done;
            c.progress = 100.0;
            c.filename = Some("Ten_phan1.mp4".into());
        })
        .await;
        for k in ["tiktok.phan2", "fb", "yt"] {
            store.update_child(&id, k, |c| {
                c.status = JobStatus::Done;
                c.progress = 100.0;
            })
            .await;
        }
        let st = store.get_status(&id).await.unwrap();
        assert_eq!((st.status, st.progress, st.done_count, st.total_count), (JobStatus::Done, 100.0, 4, 4));
        assert_eq!(store.get_child_display_name(&id, "tiktok.phan1").await.as_deref(), Some("Ten_phan1.mp4"));
        // Khoá cũ của preset không còn tồn tại → không trả gì.
        assert_eq!(store.get_child_display_name(&id, "tiktok").await, None);
    }

    #[tokio::test]
    async fn cancel_danh_dau_ca_phan_chua_chay() {
        let store = JobStore::new();
        let mut job = job_with_children();
        job.cancel = Some(CancellationToken::new());
        job.status = JobStatus::Running;
        let id = job.id;
        store.insert(job).await;
        let mut plans: HashMap<String, Vec<Span>> = HashMap::new();
        plans.insert("fb".into(), vec![(0.0, 10.0), (10.0, 20.0), (20.0, 30.0)]);
        store.expand_children(&id, &plans).await;

        assert!(store.cancel(&id).await);
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.status, JobStatus::Cancelled);
        assert!(st.children.iter().all(|c| c.status == JobStatus::Cancelled), "mọi phần chưa chạy đều bị huỷ");
    }

    /// Huỷ đúng lúc pipeline còn bước cuối (route huỷ xoá thư mục job ngay):
    /// token huỷ phải còn tra được, lỗi phát sinh sau đó KHÔNG đổi "đã huỷ"
    /// thành "lỗi", job con lỗi cũng không đổi trạng thái cha.
    #[tokio::test]
    async fn huy_roi_khong_bi_ghi_de_bang_loi_phat_sinh_sau_do() {
        let store = JobStore::new();
        let mut job = job_with_children();
        job.cancel = Some(CancellationToken::new());
        job.status = JobStatus::Running;
        let id = job.id;
        store.insert(job).await;
        for k in ["tiktok", "fb", "yt"] {
            store.mark_child_running(&id, k).await;
        }

        assert!(store.cancel(&id).await);
        assert!(store.was_cancelled(&id).await);
        // Token vẫn lấy được theo job_id và đã ở trạng thái huỷ → mọi run_step
        // sau đó dừng ngay, không chạy nốt ffmpeg trên file đã bị dọn.
        let token = store.get_cancel_token(&id).await.expect("token huỷ không bị take()");
        assert!(token.is_cancelled());
        assert!(is_cancelled(&store, &id).await);

        // Bước cuối của chuỗi chung báo lỗi (ffprobe không thấy file) → bỏ qua.
        fail(&store, &id, "Không đo được clip sau chuẩn hoá.").await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.status, JobStatus::Cancelled);
        assert!(st.error.is_none(), "job đã huỷ không bị gắn thông báo lỗi");

        // Hai phần kịp xong, phần còn lại lỗi (file trung gian đã bị dọn) → cha
        // vẫn "đã huỷ", không thành "xong 2/3" kèm thông báo lỗi.
        for k in ["tiktok", "fb"] {
            store
                .update_child(&id, k, |c| {
                    c.status = JobStatus::Done;
                    c.progress = 100.0;
                })
                .await;
        }
        store
            .update_child(&id, "yt", |c| {
                c.status = JobStatus::Error;
                c.error = Some("ffmpeg lỗi".into());
            })
            .await;
        let st = store.get_status(&id).await.unwrap();
        assert_eq!(st.status, JobStatus::Cancelled);
        assert!(st.error.is_none());

        // Huỷ quá muộn (mọi job con đều xong) thì vẫn báo xong — không mất kết quả.
        let store2 = JobStore::new();
        let mut job2 = job_with_children();
        job2.cancel = Some(CancellationToken::new());
        job2.status = JobStatus::Running;
        let id2 = job2.id;
        store2.insert(job2).await;
        assert!(store2.cancel(&id2).await);
        for k in ["tiktok", "fb", "yt"] {
            store2
                .update_child(&id2, k, |c| {
                    c.status = JobStatus::Done;
                    c.progress = 100.0;
                })
                .await;
        }
        store2.finish(&id2).await;
        assert_eq!(store2.get_status(&id2).await.unwrap().status, JobStatus::Done);

        // Job thường (chưa huỷ) báo lỗi như cũ.
        let store3 = JobStore::new();
        let job3 = job_with_children();
        let id3 = job3.id;
        store3.insert(job3).await;
        fail(&store3, &id3, "Lỗi thật").await;
        let st = store3.get_status(&id3).await.unwrap();
        assert_eq!(st.status, JobStatus::Error);
        assert_eq!(st.error.as_deref(), Some("Lỗi thật"));
    }

    #[test]
    fn output_filename_phan_theo_quy_tac_a2() {
        let t = target("tiktok_9x16", Some(180));
        assert_eq!(output_filename(&t, Some("Ten"), false, Some(1)), "Ten_phan1.mp4");
        assert_eq!(output_filename(&t, Some("Ten"), true, Some(2)), "Ten_tiktok_9x16_phan2.mp4");
        assert_eq!(output_filename(&t, Some("Ten"), false, None), "Ten.mp4");
        assert_eq!(output_filename(&t, None, false, Some(3)), "radio_tiktok_9x16_1080x1920_phan3.mp4");
    }

    #[test]
    fn needs_split_theo_gioi_han_va_audio_only() {
        assert!(needs_split(&target("reels", Some(90)), 500.0));
        assert!(!needs_split(&target("reels", Some(90)), 90.0), "vừa giới hạn không chia");
        assert!(!needs_split(&target("tiktok", Some(0)), 500.0), "0 = không giới hạn");
        assert!(!needs_split(&target("tiktok", None), 500.0));
        assert!(!needs_split(&target("audio_only", Some(60)), 500.0), "MP3 không chia");
    }


    #[test]
    fn parts_for_mot_phan_khi_khong_chia() {
        let plans: HashMap<String, Vec<Span>> = HashMap::new();
        let one = parts_for(&target("fb", Some(90)), &plans);
        assert_eq!((one.len(), one[0].key.as_str(), one[0].part.is_none()), (1, "fb", true));
        let mut plans = plans;
        plans.insert("fb".into(), vec![(0.0, 3.0), (3.0, 6.0)]);
        let two = parts_for(&target("fb", Some(90)), &plans);
        assert_eq!(two.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(), vec!["fb.phan1", "fb.phan2"]);
        assert_eq!(two[1].part.unwrap().index, 2);
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
