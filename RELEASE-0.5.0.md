# Bản 0.5.0 — A1 · A2 · A3

Ba tính năng lớn theo yêu cầu, mỗi tính năng có **unit test + E2E thật** (chạy
server, tạo job qua HTTP, kiểm file output bằng ffprobe).

> Ghi chú minh bạch: commit `2c46e75` (đầu bản 0.5.0) nhắc "A1 tách audio"
> trong thông điệp nhưng mã nguồn lúc đó chưa kèm phần A1. Bản này là nơi A1
> được hiện thực hoá đầy đủ và kiểm chứng — kèm 16 kiểm thức E2E riêng.

## A1 — Tách âm thanh (MP3)

Preset **"Tách âm thanh (MP3)"** cuối danh sách preset: xuất MP3 từ audio đã
ghép, không kèm video.

- Encode `libmp3lame` **320 kbps, 44,1 kHz, stereo**.
- Áp dụng đúng **âm lượng + fade-out** như bản video (cùng bộ lọc `-af`).
- Chọn **riêng preset này thì không cần upload video** — xuất podcast/audiobook
  từ nhiều file audio nối tiếp không cần video nền.
- Kết hợp với preset video: zip chứa cả `.mp3` + `.mp4`; tên file
  `<Tên sản phẩm>_audio_only.mp3` theo quy tắc đặt tên chung.
- Tải về với `Content-Type: audio/mpeg` đúng chuẩn, tên tiếng Việt giữ nguyên
  dấu qua `filename*` RFC 6266.
- Xem trước tên file (preview) hiện đúng đuôi `.mp3`.

**E2E:** 16/16 pass (`scripts/e2e_a1_audio_only.py`) — job chỉ-audio, mime,
bitrate, 44.1kHz stereo, không stream video, độ dài khớp nguồn, zip phối hợp,
preview tên, chặn job thiếu audio.

## A2 — Tên sản phẩm

File xuất ra mang đúng tên người dùng nhập (đã có từ commit đầu bản 0.5.0):

- 1 preset → `<tên>.mp4`; nhiều preset → `<tên>_<preset>.mp4` + zip `<tên>.zip`.
- Làm sạch an toàn: NFC, ≤100 ký tự, xoá ký tự cấm Windows/macOS/Linux, né tên
  đặt biệt `CON`/`PRN`/`NUL`…
- Tuỳ chọn **bỏ dấu tiếng Việt**: `Kem chống nắng` → `Kem-chong-nang`.
- Trùng tên trong kho → tự thành ` (2)`, ` (3)` — không bao giờ ghi đè.
- Header tải về `filename` (ASCII fallback) + `filename*` (UTF-8 RFC 6266).

## A3 — Giới hạn lưu trữ kho video

Kho output không phình to vô hạn:

- Mặc định **5 GB / 200 file**; sửa trong UI (thanh dung lượng) hoặc
  `settings.json`; env `RVM_LIBRARY_MAX_GB` / `RVM_LIBRARY_MAX_FILES` ghi đè.
- Vượt ngưỡng → **tự xoá file cũ nhất** (sau mỗi job + lúc khởi động), không
  đụng file job đang chạy; file đơn lớn hơn giới hạn được giữ lại + cảnh báo.
- **Kiểm tra đĩa trước render**: cần trống `tổng input × 3` — thiếu thì từ chối
  job với số GB còn thiếu (statvfs trên Linux/macOS, API Windows).
- Thanh dung lượng trong UI: cảnh báo màu ≥ 80%, nút **Dọn ngay** xoá về 70%
  có xác nhận.
- API `GET/PUT /api/settings` (validate 1–500 GB / 10–5000 file).

## Kiểm thử

| Bộ | Kết quả |
| --- | --- |
| `cargo test --release` | **191/191 pass**, 0 warning (thêm 3 unit test A1: resolve preset, args MP3, max duration) |
| E2E A1 (`e2e_a1_audio_only.py`) | **16/16 pass** |
| E2E A2 (`e2e_a2_output_name.py`) | pass (từ commit đầu bản) |
| E2E A3 (`e2e_a3_storage.py`) | pass (từ commit đầu bản) |

## Thay đổi kỹ thuật

- `presets.rs`: preset `audio_only` + `is_audio_only()`; `RenderTarget` fps=0.
- `ffmpeg.rs`: `build_mp3_args()` — `-vn -map 0:a:0 -c:a libmp3lame -b:a 320k`.
- `jobs.rs`: `render_audio_only()` (1 ffmpeg, tiến độ theo out_time); pipeline
  tách 3 nhóm fixed/originals/audio-only; `has_original` loại A1 khỏi kiểm tra
  kích thước clip.
- `routes.rs`: cho job chỉ-audio không cần video; `mime_for()` theo đuôi file
  cho mọi route tải/xem; preview tên dùng `preset_filename_ext`.
- `naming.rs`: `preset_filename_ext()` (đuôi tuỳ chọn; A1 → `.mp3`).
- `static/index.html`: chip preset "Tách âm thanh (MP3) — MP3 · 320 kbps";
  `canExport()` bỏ yêu cầu video khi chỉ chọn A1; gợi ý "cần thêm" cập nhật.
- `docs/`: 3 screenshot mới (dark/light/mobile) nén JPEG (~100 KB/ảnh, giảm
  ~6 MB so với bản 0.4).

## Hạn chế đã biết

- MP3 cố định 320 kbps / 44,1 kHz (chưa có tuỳ chọn bitrate).
- Watermark/chữ overlay không áp dụng cho MP3 (không có khung hình).
- ASR phụ đề tự động vẫn là dịch vụ online duy nhất (không đổi từ 0.4).
