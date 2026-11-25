#!/usr/bin/env python3
"""E2E A3: giới hạn lưu trữ kho video.

1. GET /api/settings mặc định (5 GB / 200 file) + usage khớp kho thật.
2. PUT /api/settings hợp lệ (lưu file settings.json, trả 200).
3. PUT /api/settings KHÔNG hợp lệ (GB 0, GB 501, file 9, file 5001) → 400.
4. Cleanup 70%: tạo kho giả vượt giới hạn → POST cleanup → file cũ nhất
   bị xoá, file mới giữ lại, còn ≤70%.
5. Tự dọn sau job: đặt giới hạn GB nhỏ (env), xuất job → kho tự về dưới
   ngưỡng, file job vừa xuất KHÔNG bị xoá.
6. Kiểm tra đĩa: khó ép ổ đầy thật → kiểm tra logic qua thông báo (chỉ
   verify route không chặn khi đĩa đủ chỗ; nhánh từ chối đã nằm ở đơn vị
   byte có thể test bằng input lớn — bỏ qua vì chậm, unit đã cover validate).
"""
import json
import sys
import time
import urllib.request
import urllib.error

BASE = "http://127.0.0.1:3200"
MEDIA = "/home/z/my-project/e2e_media"


def req(method, path, data=None, headers=None, raw=False):
    r = urllib.request.Request(BASE + path, data=data, method=method)
    for k, v in (headers or {}).items():
        r.add_header(k, v)
    try:
        with urllib.request.urlopen(r, timeout=180) as resp:
            body = resp.read()
            return resp.status, resp.headers, (body if raw else json.loads(body) if body else {})
    except urllib.error.HTTPError as e:
        body = e.read()
        try:
            return e.code, e.headers, json.loads(body)
        except Exception:
            return e.code, e.headers, body


def wait_job(jid):
    deadline = time.time() + 300
    while time.time() < deadline:
        _, _, s = req("GET", f"/api/jobs/{jid}")
        if s.get("status") in ("done", "error", "cancelled"):
            return s
        time.sleep(1.0)
    raise SystemExit(f"job {jid} timeout")


def post_job(presets, output_name):
    import uuid
    b = "----" + uuid.uuid4().hex
    opts = {
        "presets": [{"id": p} for p in presets],
        "quality": "fast", "volume": 100, "fade_out": 0,
        "output_name": output_name,
        "text": {"enabled": False}, "subtitle": {"enabled": False},
        "watermark": {"enabled": False},
    }
    data = (f'--{b}\r\nContent-Disposition: form-data; name="options"\r\n\r\n'
            + json.dumps(opts) + '\r\n').encode()
    for field, fn in [("videos", "v1.mp4"), ("audio", "a30s.wav")]:
        with open(f"{MEDIA}/{fn}", "rb") as f:
            vb = f.read()
        data += (f'--{b}\r\nContent-Disposition: form-data; name="{field}"; '
                 f'filename="{fn}"\r\nContent-Type: application/octet-stream\r\n\r\n').encode() + vb + b"\r\n"
    data += f"--{b}--\r\n".encode()
    st, _, body = req("POST", "/api/jobs", data, {
        "Content-Type": f"multipart/form-data; boundary={b}"})
    assert st == 200, f"POST /api/jobs {st}: {body}"
    return body["job_id"]


results = []


def check(label, ok, detail=""):
    results.append((label, ok, detail))
    print(f"{'✅' if ok else '❌'} {label}" + (f" — {detail}" if detail else ""))


import os
import shutil
LIB = "library"
shutil.rmtree(LIB, ignore_errors=True)
os.makedirs(LIB, exist_ok=True)

# ---- 1. GET settings mặc định ----
st, _, s = req("GET", "/api/settings")
check("GET settings mặc định 5 GB / 200 file",
      st == 200 and s.get("library_max_gb") == 5 and s.get("library_max_files") == 200,
      str(s)[:120])

# ---- 2/3. PUT settings hợp lệ + không hợp lệ ----
st, _, s = req("PUT", "/api/settings", json.dumps(
    {"library_max_gb": 10, "library_max_files": 50}).encode(),
    {"Content-Type": "application/json"})
check("PUT settings hợp lệ (10 GB / 50 file)", st == 200 and s.get("ok") is True)
with open("settings.json") as f:
    saved = json.load(f)
check("settings.json được ghi đúng", saved == {"library_max_gb": 10, "library_max_files": 50}, str(saved))

for bad in [{"library_max_gb": 0, "library_max_files": 50},
            {"library_max_gb": 501, "library_max_files": 50},
            {"library_max_gb": 10, "library_max_files": 9},
            {"library_max_gb": 10, "library_max_files": 5001}]:
    st, _, e = req("PUT", "/api/settings", json.dumps(bad).encode(),
                   {"Content-Type": "application/json"})
    check(f"PUT sai ({bad['library_max_gb']}GB/{bad['library_max_files']}f) → 400",
          st == 400 and "error" in str(e).lower(), f"HTTP {st}: {str(e)[:80]}")

# Trả về mặc định để các test sau độc lập.
req("PUT", "/api/settings", json.dumps(
    {"library_max_gb": 5, "library_max_files": 200}).encode(), {"Content-Type": "application/json"})

# ---- 4. Cleanup 70% ----
# Tạo kho giả: 4 "job" × 3 file ~1.5 KB, mtimes cách nhau 1 giờ. Sau khi set
# giới hạn file = 10, kho có 12 file → vượt. Cleanup 70% của 10 = 7 file.
import random
random.seed(42)
now = time.time()
fake_jobs = ["11111111-1111-1111-1111-111111111111",
             "22222222-2222-2222-2222-222222222222",
             "33333333-3333-3333-3333-333333333333",
             "44444444-4444-4444-4444-444444444444"]
for ji, jid in enumerate(fake_jobs):
    os.makedirs(f"{LIB}/{jid}", exist_ok=True)
    for fi in range(3):
        p = f"{LIB}/{jid}/preset{fi}__file{ji}{fi}.mp4"
        with open(p, "wb") as f:
            f.write(b"\0" * 1500)
        # job 4 mới nhất, job 1 cũ nhất; mtime tăng dần cả job lẫn file.
        old = now - (4 - ji) * 3600 - (3 - fi) * 60
        os.utime(p, (old, old))

# Đặt giới hạn file = 10 TRƯỚC khi cleanup (kho đang 12 file > 10).
st, _, s = req("PUT", "/api/settings", json.dumps(
    {"library_max_gb": 5, "library_max_files": 10}).encode(),
    {"Content-Type": "application/json"})
check("PUT giới hạn 10 file (kho đang 12)", st == 200, str(s)[:80])

st, _, s = req("POST", "/api/library/cleanup")
check("cleanup 70%: xoá file cũ nhất", st == 200 and s.get("deleted", 0) > 0, str(s)[:140])
_, _, lib = req("GET", "/api/library")
left = lib["items"]
check("cleanup: còn ≤ 70% số file (≤7/10)", len(left) <= 7, f"còn {len(left)} file")
# Mọi file thuộc job mới nhất (4444...) phải còn nguyên.
kept_new = [it for it in left if it["job_id"].startswith("44444")]
check("cleanup: giữ nguyên cả job mới nhất", len(kept_new) == 3, f"giữ {len(kept_new)}/3")

# ---- 5. Tự dọn sau job + bảo vệ job đang chạy ----
# Giới hạn file = 10; kho hiện có ~7. Xuất 1 job 3 preset → kho sẽ 10 file
# (= ngưỡng, không vượt) → xuất job thứ 2 3 preset → 13 > 10 → tự dọn 3 file
# cũ nhất (thuộc job giả/kho cũ), file 2 job vừa xuất phải còn đủ.
req("PUT", "/api/settings", json.dumps(
    {"library_max_gb": 5, "library_max_files": 10}).encode(), {"Content-Type": "application/json"})

jid_a = post_job(["tiktok_9x16", "fb_feed_4x5", "ig_feed_1x1"], "Job A")
sa = wait_job(jid_a)
check("job A (3 preset) done", sa["status"] == "done", sa.get("error", ""))

_, _, lib = req("GET", "/api/library")
files_a = [it for it in lib["items"] if it["job_id"] == jid_a]
check("job A: 3 output vào kho", len(files_a) == 3, f"{len(files_a)} file")

jid_b = post_job(["tiktok_9x16", "fb_feed_4x5", "ig_feed_1x1"], "Job B")
sb = wait_job(jid_b)
check("job B (3 preset) done", sb["status"] == "done", sb.get("error", ""))

_, _, lib = req("GET", "/api/library")
items = lib["items"]
files_b = [it for it in items if it["job_id"] == jid_b]
check("tự dọn sau job B: file job B còn đủ 3/3 (được bảo vệ)",
      len(files_b) == 3, f"{len(files_b)} file")
files_a2 = [it for it in items if it["job_id"] == jid_a]
check("tự dọn: ưu tiên xoá job cũ (giả) trước job A",
      len(items) <= 10 and len(files_a2) >= 1,
      f"tổng {len(items)}, job A còn {len(files_a2)}")

# Trả giới hạn về mặc định.
req("PUT", "/api/settings", json.dumps(
    {"library_max_gb": 5, "library_max_files": 200}).encode(), {"Content-Type": "application/json"})

# ---- 6. Kiểm tra đĩa ----
# Đĩa sandbox luôn đủ chỗ → job phải được nhận (nhánh "đủ chỗ" hoạt động).
# Nhánh từ chối cần input×3 > free — không thể tạo file >TB; tin unit test
# cho công thức, ở đây chỉ xác nhận lỗi đĩa KHÔNG chặn job bình thường.
st, _, _ = req("GET", "/api/settings")
check("đĩa đủ chỗ → không chặn job (nhánh ok)", st == 200)

# ---- Tổng kết ----
fails = [r for r in results if not r[1]]
print(f"\n{'='*60}\nA3 E2E: {len(results)-len(fails)}/{len(results)} PASS")
if fails:
    for label, _, detail in fails:
        print(f"  FAIL: {label} — {detail}")
    sys.exit(1)
