# Bản 0.4 — Rà soát mã nguồn + Visualizer

Bản này tập trung **kiểm tra lại toàn bộ src**, sửa bug thật (mỗi bug có test
hồi quy), và thêm nhóm tính năng "sóng âm + thanh tiến độ + chất lượng encode".

## Bug đã sửa (có test hồi quy)

| # | Bug | Sửa |
|---|-----|-----|
| 1 | **Fade-out sai mốc** khi `volume ≠ 100` hoặc khi video bị cắt theo `max_duration_sec` (afade tính trên thời lượng audio gốc) | Tính lại `out_duration = min(max_duration, audio_duration)` rồi mới đặt `afade=t=out:st=…` |
| 2 | **Upload font bị trỏ nhầm sang file text** (chữ đè dùng font hệ thống dù người dùng đã upload font) | `FontChoice::resolve(&uploads)` chọn đúng file font theo thứ tự ưu tiên |
| 3 | **Cỡ phụ đề không theo px thật** (PlayRes ≠ kích thước khung → chữ to/nhẹ sai tỉ lệ) | ASS sinh với `PlayResX/Y = kích thước khung`, `font_size` là px thật; mặc định 52 px @1080 |
| 4 | **Letter-spacing bị "ô vuông" (tofu)** với font thiếu U+200A | Chèn ký tự giãn theo metric thật của font (hair → thin → space; bỏ nếu không có glyph) |
| 5 | **Letter-spacing không giãn khoảng trắng giữa các từ** | `space_out` chèn gap sau mọi ký tự thường, bỏ sau khoảng trắng/XUỐNG DÒNG/ZWJ/variation selector (giữ nguyên emoji ghép) |
| 6 | **Đồ thị lọc chỉ có logo** dựa vào auto-connect của ffmpeg | Gán nhãn tường minh `[v0]`, `[viz]`, `[pbar]`… |
| 7 | **Thoát ngay lúc khởi động** nếu ffmpeg chưa có (sidecar auto-download chưa kịp chạy) | `ffmpeg::ensure_bins()` — resolve + `-version` + hint cài đặt |
| 8 | **UI gửi `fast_mode`/`quality`** mà backend không đọc | Thêm alias serde `fast_mode`, `quality` trên `JobOptions` |
| 9 | **Phụ đề/watermark đè lên visualizer** ở preset có vùng an toàn | Reserve visualizer = `viz + inset` an toàn của preset |
| 10 | **Link GitHub giữ chỗ** (`https://github.com/`) | Trỏ về repo thật `vinhtoday/audio-video-studio` |
| 11 | Comment lỗi ký tự (garbled) trong ffmpeg.rs | Viết lại tiếng Việt chuẩn UTF-8 |

## Tính năng mới

- **Visualizer sóng âm** (wave/bars/spectrum) chạy theo audio thật, màu + độ
  cao + độ mờ tuỳ chỉnh, đặt trên/dưới/giữa. Sóng vẽ ở 1/5 độ phân giải rồi
  phóng bicubic — dải sáng dày kiểu neon thay vì nét 1 px vô hình.
- **Thanh tiến độ** chạy theo thời lượng audio (E2E: 864 px @0.6 s → 7776 px @5.4 s).
- **Chất lượng encode**: Nhanh (`veryfast`/CRF 26/128k) — Cân bằng (CRF 23/192k)
  — Cao (`slow`/CRF 18/256k).
- **Kịch bản cảnh chữ** (`[00:05-00:20] Nội dung`): backend + unit test sẵn,
  chưa có nút trên UI.
- Khung `FrameCtx` chuẩn hoá mọi tính toán toạ độ theo tỉ lệ 1080.

## Kiểm chứng

- `cargo test --release`: **156/156 pass** (trong đó 40+ test hồi quy cho các bug trên).
- E2E render thật qua HTTP (preset TikTok 1080×1920, video 6 s + audio sine):
  **15/15 PASS** — text overlay, phụ đề đúng vị trí, thanh tiến độ, fade-out
  RMS 0.084 → 0.0090, visualizer sóng ≈ 2.940–4.076 px màu neon trong vùng đo
  (trước bản sửa: 0 px).

## Chưa làm (hậu bị cho bản 0.5)

- Chuẩn hoá âm lượng loudnorm 2-pass; ảnh bìa + Ken Burns (đã có hàm
  `cover_fit_filters`, chưa nối vào pipeline); chia phần tại đoạn im lặng.
- CORS đang permissive → nên kiểm tra Origin; giới hạn job song song;
  SIGTERM; `spawn_blocking` cho các gọi fs đồng bộ; zip > 4 GB guard.
- UI: nút kịch bản cảnh chữ, kéo-thả, tiến độ upload, khôi phục job sau reload,
  xem trước vùng an toàn; bars/spectrum chưa render thử thật.

## Đẩy lên GitHub

```bash
git clone download/radio-video-studio.gitbundle audio-video-studio
cd audio-video-studio
git checkout release-0.4
git remote add origin https://github.com/vinhtoday/audio-video-studio.git
git push -u origin release-0.4:main
```
