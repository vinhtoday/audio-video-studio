#!/usr/bin/env python3
"""E2E A1: "Tách âm thanh" — xuất thật qua HTTP rồi kiểm từng yêu cầu nghiệm thu.

Kịch bản (giống e2e_a2/e2e_a3):
1. Chỉ chọn preset audio_only, KHÔNG gửi video → job phải chạy được.
2. File tải về là MP3 320 kbps (ffprobe), đúng độ dài audio, đúng volume/fade.
3. Tải qua /api/jobs/<id>/download/audio_only → Content-Type audio/mpeg,
   tên file đúng "Tên sản phẩm.mp3".
4. Kèm preset video → zip chứa cả .mp3 và .mp4, tên mp3 có hậu tố preset.
5. Preview tên file (/api/output-names) hiện .mp3 cho audio_only.
"""
import json
import os
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

BASE = os.environ.get("RVM_E2E_BASE", "http://127.0.0.1:3201")
MEDIA = os.environ.get("RVM_E2E_MEDIA", "/home/z/my-project/e2e_media")

passed, failed = [], []


def check(name, ok, detail=""):
    (passed if ok else failed).append(name)
    print(("  ✓ " if ok else "  ✗ ") + name + (f"  [{detail}]" if detail and not ok else ""))


def post_job(audio_files, presets, output_name=None, video_files=()):
    """POST /api/jobs multipart — trả (job_id, resp_json)."""
    boundary = uuid.uuid4().hex
    parts = []
    for f in video_files:
        with open(f, "rb") as fh:
            parts.append(f"--{boundary}\r\nContent-Disposition: form-data; name=\"videos\"; "
                         f"filename=\"{f.split('/')[-1]}\"\r\nContent-Type: video/mp4\r\n\r\n".encode()
                         + fh.read() + b"\r\n")
    for f in audio_files:
        with open(f, "rb") as fh:
            parts.append(f"--{boundary}\r\nContent-Disposition: form-data; name=\"audio\"; "
                         f"filename=\"{f.split('/')[-1]}\"\r\nContent-Type: audio/wav\r\n\r\n".encode()
                         + fh.read() + b"\r\n")
    opts = {"presets": [{"id": p} for p in presets], "fit_mode": "cover",
            "audio": {"volume": 100, "fade_out": 0, "fast_mode": False}}
    if output_name is not None:
        opts["output_name"] = output_name
    parts.append(f"--{boundary}\r\nContent-Disposition: form-data; name=\"options\"\r\n"
                 f"Content-Type: application/json\r\n\r\n{json.dumps(opts)}\r\n".encode())
    body = b"".join(parts) + f"--{boundary}--\r\n".encode()
    req = urllib.request.Request(f"{BASE}/api/jobs", data=body, method="POST",
                                 headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)["job_id"]


def wait_done(job_id, timeout=180):
    t0 = time.time()
    while time.time() - t0 < timeout:
        with urllib.request.urlopen(f"{BASE}/api/jobs/{job_id}", timeout=10) as r:
            d = json.load(r)
        if d["status"] in ("done", "failed", "cancelled"):
            return d
        time.sleep(1)
    raise TimeoutError(job_id)


def download(url):
    with urllib.request.urlopen(url, timeout=60) as r:
        # Giữ HTTPMessage (tra cứu case-insensitive) — dict() làm mất tính
        # chất này vì key bị lowercase.
        return r.read(), r.headers


def header_get(hdrs, name):
    """Lấy header case-insensitive (HTTPMessage hoặc dict)."""
    if hasattr(hdrs, "get"):
        try:
            return hdrs.get(name)
        except Exception:
            pass
    return None


def probe(path):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries",
         "format=format_name,duration,bit_rate:stream=codec_name,codec_type,sample_rate,channels",
         "-of", "json", path],
        capture_output=True, text=True, check=True).stdout
    return json.loads(out)


def main():
    print("== E2E A1: Tách âm thanh (MP3) ==")

    # --- 1. Job chỉ audio, không video ---
    jid = post_job([f"{MEDIA}/a2.wav"], ["audio_only"], output_name="Buổi phát thanh số 1")
    st = wait_done(jid)
    check("job chỉ-audio (không video) chạy xong", st["status"] == "done",
          json.dumps(st.get("error"), ensure_ascii=False))
    check("1 job con audio_only", len(st["children"]) == 1 and st["children"][0]["preset_id"] == "audio_only",
          json.dumps(st["children"], ensure_ascii=False))

    # --- 2. Tải về: đúng mime + tên ---
    data, hdrs = download(f"{BASE}/api/jobs/{jid}/download/audio_only")
    check("Content-Type audio/mpeg", header_get(hdrs, "Content-Type") == "audio/mpeg",
          str(header_get(hdrs, "Content-Type")))
    disp = header_get(hdrs, "Content-Disposition") or ""
    # filename* RFC 6266 percent-encode giữ đúng dấu tiếng Việt.
    check("tên tải về = 'Buổi phát thanh số 1.mp3'",
          "Bu%E1%BB%95i%20ph%C3%A1t%20thanh%20s%E1%BB%91%201.mp3" in disp, disp)
    with open("/tmp/e2e_a1_out.mp3", "wb") as f:
        f.write(data)
    info = probe("/tmp/e2e_a1_out.mp3")
    fmt = info["format"]
    streams = info["streams"]
    check("là MP3 (format mp3, codec libmp3lame)",
          "mp3" in fmt["format_name"] and any(s.get("codec_name") == "mp3" for s in streams),
          fmt["format_name"])
    check("bitrate ~320kbps", int(fmt.get("bit_rate", 0)) > 280000, fmt.get("bit_rate"))
    check("44.1kHz stereo",
          any(s.get("sample_rate") == "44100" and s.get("channels") == 2 for s in streams),
          str([s.get("sample_rate") for s in streams]))
    check("không có stream video", all(s.get("codec_type") != "video" for s in streams))
    dur_src = probe(f"{MEDIA}/a2.wav")["format"]["duration"]
    check(f"độ dài khớp audio nguồn ({float(fmt['duration']):.1f}s ≈ {float(dur_src):.1f}s)",
          abs(float(fmt["duration"]) - float(dur_src)) < 1.5, fmt["duration"])

    # --- 3. Kết hợp: audio_only + preset video → zip có .mp3 + .mp4 ---
    jid2 = post_job([f"{MEDIA}/a2.wav"], ["audio_only", "tiktok_9x16"],
                    output_name="Radio sáng", video_files=[f"{MEDIA}/v1.mp4"])
    st2 = wait_done(jid2)
    check("job phối hợp (mp3 + mp4) chạy xong", st2["status"] == "done",
          json.dumps(st2.get("error"), ensure_ascii=False))
    names = [c.get("filename") for c in st2["children"]]
    check("2 job con: .mp3 + .mp4",
          any(n and n.endswith(".mp3") for n in names) and any(n and n.endswith(".mp4") for n in names),
          str(names))
    check("mp3 có hậu tố _audio_only",
          any(n and n == "Radio sáng_audio_only.mp3" for n in names), str(names))
    zip_data, _ = download(f"{BASE}/api/jobs/{jid2}/download")
    with open("/tmp/e2e_a1_out.zip", "wb") as f:
        f.write(zip_data)
    # Zip local header: đọc tên entry (signature PK\x03\x04).
    entries = []
    raw = zip_data
    i = 0
    while True:
        i = raw.find(b"PK\x03\x04", i)
        if i < 0:
            break
        nlen, elen = struct.unpack_from("<HH", raw, i + 26)
        name = raw[i + 30:i + 30 + nlen].decode("utf-8", "replace")
        entries.append(name)
        i += 4
    check("zip chứa .mp3 + .mp4", any(e.endswith(".mp3") for e in entries)
          and any(e.endswith(".mp4") for e in entries), str(entries))

    # --- 4. Preview tên file ---
    req = urllib.request.Request(
        f"{BASE}/api/output-names", method="POST",
        headers={"Content-Type": "application/json"},
        data=json.dumps({"output_name": "Radio sáng", "strip": False,
                         "presets": ["audio_only", "tiktok_9x16"]}).encode())
    with urllib.request.urlopen(req, timeout=10) as r:
        pv = json.load(r)
    files = {f["preset_id"]: f["name"] for f in pv["files"]}
    check("preview tên: audio_only → .mp3",
          files.get("audio_only", "").endswith(".mp3") and "Radio sáng" in files.get("audio_only", ""),
          str(files))
    check("preview tên: tiktok → .mp4",
          files.get("tiktok_9x16", "").endswith(".mp4"), str(files))

    # --- 5. Không gửi video + không audio → lỗi rõ ---
    try:
        post_job([], ["audio_only"])
        check("job không audio bị chặn", False, "không có lỗi")
    except urllib.error.HTTPError as e:
        msg = json.load(e).get("error", "")
        check("job không audio bị chặn", e.code == 400 and "audio" in msg.lower(), f"{e.code} {msg}")

    print(f"\n== KẾT QUẢ: {len(passed)} pass, {len(failed)} fail ==")
    if failed:
        for f in failed:
            print("  FAIL:", f)
        sys.exit(1)


if __name__ == "__main__":
    main()
