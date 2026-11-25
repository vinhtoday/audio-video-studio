#!/usr/bin/env python3
"""E2E A2: "Tên sản phẩm" — xuất thật qua HTTP rồi kiểm từng yêu cầu nghiệm thu.

1. Preview API: tên tiếng Việt, bỏ dấu, rỗng.
2. Job 1 preset "Kem chống nắng" → file "Kem chống nắng.mp4",
   Content-Disposition có filename ASCII + filename* UTF-8.
3. Job 2 preset cùng tên → zip "Kem chống nắng.zip", entry trong zip đúng
   "<tên>_<preset_id>.mp4", zip có 2 entry.
4. Xuất lại đúng tên đó (trùng tên kho) → " (2)" không ghi đè.
5. Kho /api/library trả display_name; stream/download/xoá với tên Unicode.
6. Path traversal qua tham số file= bị chặn 400.
"""
import json
import sys
import time
import urllib.parse
import urllib.request

sys.path.insert(0, "/home/z/my-project/scripts")

BASE = "http://127.0.0.1:3200"
MEDIA = "/home/z/my-project/e2e_media"
NAME = "Kem chống nắng"


def req(method, path, data=None, headers=None, raw=False):
    r = urllib.request.Request(BASE + path, data=data, method=method)
    for k, v in (headers or {}).items():
        r.add_header(k, v)
    try:
        with urllib.request.urlopen(r, timeout=180) as resp:
            body = resp.read()
            # Giữ HTTPMessage (case-insensitive) thay vì dict thường.
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


def post_job(presets, output_name, strip=False):
    import uuid
    b = "----" + uuid.uuid4().hex
    opts = {
        "presets": [{"id": p} for p in presets],
        "quality": "fast", "volume": 100, "fade_out": 0,
        "output_name": output_name, "output_name_strip": strip,
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


# Làm sạch kho để test trùng tên đo được chính xác.
import shutil
shutil.rmtree("library", ignore_errors=True)

# ---- 1. Preview API ----
st, _, p = req("POST", "/api/output-names", json.dumps({
    "output_name": NAME, "strip": False,
    "presets": ["tiktok_9x16", "fb_feed_4x5"]}).encode(),
    {"Content-Type": "application/json"})
check("preview 2 preset: stem + 2 file + zip",
      st == 200 and p["stem"] == NAME
      and p["files"][0]["name"] == f"{NAME}_tiktok_9x16.mp4"
      and p["files"][1]["name"] == f"{NAME}_fb_feed_4x5.mp4"
      and p["zip"] == f"{NAME}.zip", str(p)[:150])

st, _, p = req("POST", "/api/output-names", json.dumps({
    "output_name": NAME, "strip": True, "presets": ["tiktok_9x16"]}).encode(),
    {"Content-Type": "application/json"})
check("preview bỏ dấu: Kem-chong-nang.mp4",
      st == 200 and p["files"][0]["name"] == "Kem-chong-nang.mp4", str(p)[:120])

st, _, p = req("POST", "/api/output-names", json.dumps({
    "output_name": "   ", "strip": False, "presets": ["tiktok_9x16"]}).encode(),
    {"Content-Type": "application/json"})
check("preview rỗng → fallback=true", st == 200 and p["fallback"] is True)

st, _, p = req("POST", "/api/output-names", json.dumps({
    "output_name": "CON <đặc> biệt: ***", "strip": False,
    "presets": ["tiktok_9x16"]}).encode(), {"Content-Type": "application/json"})
# "CON" có space theo sau → không phải tên reserved; ký tự cấm bị xoá.
check("preview làm sạch: CON đặc biệt",
      st == 200 and p["files"][0]["name"] == "CON đặc biệt.mp4", str(p)[:120])

st, _, p = req("POST", "/api/output-names", json.dumps({
    "output_name": "CON", "strip": False,
    "presets": ["tiktok_9x16"]}).encode(), {"Content-Type": "application/json"})
check("preview tên reserved CON → CON_",
      st == 200 and p["files"][0]["name"] == "CON_.mp4", str(p)[:120])

# ---- 2. Job 1 preset ----
jid1 = post_job(["tiktok_9x16"], NAME)
s1 = wait_job(jid1)
check("job 1 preset done", s1["status"] == "done", s1.get("error", ""))
fname1 = s1["children"][0]["filename"]
check("tên file con = Kem chống nắng.mp4", fname1 == f"{NAME}.mp4", fname1)

st, hdrs, body = req("GET", f"/api/jobs/{jid1}/download/tiktok_9x16", raw=True)
cd = hdrs.get("Content-Disposition", "")
check("CD 1 preset: filename ASCII + filename* UTF-8",
      st == 200 and 'filename="Kem ch_ng n_ng.mp4"' in cd
      and "filename*=UTF-8''Kem%20ch%E1%BB%91ng%20n%E1%BA%AFng.mp4" in cd, cd)

# ---- 3. Job 2 preset → zip ----
jid2 = post_job(["tiktok_9x16", "fb_feed_4x5"], NAME)
s2 = wait_job(jid2)
check("job 2 preset done + has_zip",
      s2["status"] == "done" and s2["has_zip"], s2.get("error", ""))
names2 = sorted(c["filename"] for c in s2["children"])
check("2 con: <tên>_<preset>.mp4",
      names2 == [f"{NAME}_fb_feed_4x5.mp4", f"{NAME}_tiktok_9x16.mp4"], str(names2))

st, hdrs, zbody = req("GET", f"/api/jobs/{jid2}/download", raw=True)
cd2 = hdrs.get("Content-Disposition", "")
check("CD zip: Kem chống nắng.zip",
      st == 200 and "filename*=UTF-8''Kem%20ch%E1%BB%91ng%20n%E1%BA%AFng.zip" in cd2, cd2)

import io
import zipfile
zf = zipfile.ZipFile(io.BytesIO(zbody))
entries = sorted(zf.namelist())
check("zip entry đúng tên (không thư mục bọc)",
      entries == [f"{NAME}_fb_feed_4x5.mp4", f"{NAME}_tiktok_9x16.mp4"], str(entries))
# ZipWriter tự viết: kiểm tra zipfile đọc được cờ UTF-8 (tên decode đúng).
check("zip mở/nội dung OK", all(zf.getinfo(e).file_size > 1000 for e in entries))

# ---- 4. Trùng tên: xuất lại đúng tên cũ ----
jid3 = post_job(["tiktok_9x16"], NAME)
s3 = wait_job(jid3)
check("job trùng tên done", s3["status"] == "done", s3.get("error", ""))
f3 = s3["children"][0]["filename"]
check("trùng tên → ' (2)'", f3 == f"{NAME} (2).mp4", f3)

# Job thứ 4 lại trùng → (3).
jid4 = post_job(["tiktok_9x16"], NAME, strip=True)
s4 = wait_job(jid4)
f4 = s4["children"][0]["filename"]
check("bỏ dấu trùng 'Kem-chong-nang' lần 1 → không hậu số (khác tên)",
      f4 == "Kem-chong-nang.mp4", f4)

# ---- 5. Kho: display_name + stream/download/xoá ----
st, _, lib = req("GET", "/api/library")
items = lib.get("items", [])
displays = [it["display_name"] for it in items]
check("kho trả display_name đúng",
      f"{NAME}.mp4" in displays and f"{NAME} (2).mp4" in displays,
      str(displays)[:150])
it1 = next(it for it in items if it["display_name"] == f"{NAME}.mp4")
st, hdrs, _ = req("GET", it1["url_download"].replace("/api", "/api"), raw=True)
check("kho download CD filename* UTF-8",
      st == 200 and "filename*=UTF-8''Kem%20ch%E1%BB%91ng%20n%E1%BA%AFng.mp4"
      in hdrs.get("Content-Disposition", ""),
      hdrs.get("Content-Disposition", "")[:100])
st, hdrs, _ = req("GET", it1["url_stream"], raw=True)
check("kho stream 200 (inline, Range)", st == 200)

# Xoá file tên Unicode qua query param.
st, _, d = req("DELETE", f"/api/library/{it1['job_id']}/{it1['preset_id']}"
               f"?file={urllib.parse.quote(it1['file'])}")
check("kho xoá file Unicode OK", st == 200 and d.get("deleted") is True, str(d)[:80])

# ---- 6. Path traversal (qua route stream có query file=) ----
st, _, _ = req("GET", f"/api/library/{it1['job_id']}/{it1['preset_id']}/stream"
               "?file=..%2F..%2Fetc%2Fpasswd")
check("traversal qua file= bị chặn (400)", st == 400, f"HTTP {st}")
st, _, _ = req("GET", f"/api/library/{it1['job_id']}/{it1['preset_id']}/stream"
               "?file=..%5C..%5Csecret.mp4")
check("traversal backslash bị chặn (400)", st == 400, f"HTTP {st}")

# ---- Tổng kết ----
fails = [r for r in results if not r[1]]
print(f"\n{'='*60}\nA2 E2E: {len(results)-len(fails)}/{len(results)} PASS")
if fails:
    for label, _, detail in fails:
        print(f"  FAIL: {label} — {detail}")
    sys.exit(1)
