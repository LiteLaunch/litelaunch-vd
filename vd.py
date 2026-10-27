"""vd — video downloader for LiteLaunch (yt-dlp).

LiteLaunch checks the link and keeps the results; this script only talks to yt-dlp:
    vd.py <request-json>   query / download / update (JSON-RPC v1, like Flow plugins)
    vd.py --wait           prewarm: import yt-dlp now, read the request from stdin later
The query saves yt-dlp's info to a temp file, so the download does not extract it again.
"""

import ctypes
import hashlib
import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.request
from ctypes import wintypes
from pathlib import Path

HERE = Path(__file__).resolve().parent
LIB = HERE / "lib"
sys.path.insert(0, str(LIB))

TEMP = Path(os.environ.get("TEMP", HERE)) / "LiteLaunch-vd"
ICON = str(HERE / "icon.png")
UPDATE_MARKER = HERE / "last_update"
UPDATE_DAYS = 7
WAIT_SECONDS = 30
MAX_FORMATS = 40


# === Notifications through the LiteLaunch tray icon ===

class COPYDATASTRUCT(ctypes.Structure):
    _fields_ = [("dwData", ctypes.c_size_t), ("cbData", wintypes.DWORD), ("lpData", ctypes.c_void_p)]


user32 = ctypes.WinDLL("user32", use_last_error=True)
user32.FindWindowW.restype = wintypes.HWND
user32.FindWindowW.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR]
user32.SendMessageTimeoutW.restype = ctypes.c_size_t
user32.SendMessageTimeoutW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, ctypes.c_void_p,
                                       wintypes.UINT, wintypes.UINT, ctypes.POINTER(ctypes.c_size_t)]
WM_COPYDATA = 0x004A
CMD_NOTIFY = 3  # main.rs


def notify(title: str, text: str, folder: str = ""):
    hwnd = user32.FindWindowW("LiteLaunchWindow", None)
    if not hwnd:
        return
    payload = json.dumps({"title": title, "text": text, "folder": folder, "icon": "vd"})
    buf = ctypes.create_unicode_buffer(payload)
    cds = COPYDATASTRUCT(CMD_NOTIFY, len(payload.encode("utf-16-le")), ctypes.cast(buf, ctypes.c_void_p))
    result = ctypes.c_size_t()
    user32.SendMessageTimeoutW(hwnd, WM_COPYDATA, 0, ctypes.addressof(cds), 0x2, 3000, ctypes.byref(result))


# === Helpers ===

def expand(path: str) -> str:
    return os.path.abspath(os.path.expanduser(os.path.expandvars(path or "")))


def which(name: str):
    for d in os.environ.get("PATH", "").split(os.pathsep):
        p = Path(d) / name
        if p.is_file():
            return p
    return None


def ffmpeg_dir(settings) -> str:
    """Folder with ffmpeg.exe: from the settings, next to this script, or on PATH."""
    for d in (settings.get("ffmpeg_dir"), str(HERE)):
        if d and (Path(expand(d)) / "ffmpeg.exe").is_file():
            return expand(d)
    exe = which("ffmpeg.exe")
    return str(exe.parent) if exe else ""


def base_opts(settings) -> dict:
    opts = {
        "quiet": True,
        "no_warnings": True,
        "noprogress": True,
        "socket_timeout": 30,
        "noplaylist": True,
        "ignore_no_formats_error": True,
    }
    cookies = expand(settings.get("cookie_file", "")) if settings.get("cookie_file") else ""
    if cookies and os.path.isfile(cookies):
        opts["cookiefile"] = cookies
    if which("node.exe"):
        opts["js_runtimes"] = {"node": {}}
    return opts


def num(v, default=0.0):
    try:
        return float(v) if v is not None else default
    except (TypeError, ValueError):
        return default


def size_text(b) -> str:
    if not b:
        return ""
    mb = b / 1024 / 1024
    return f"{mb / 1024:.2f} ГБ" if mb >= 1024 else f"{mb:.1f} МБ" if mb < 100 else f"{mb:.0f} МБ"


def result(title, sub="", icon=ICON, action=None):
    r = {"Title": title, "SubTitle": sub, "IcoPath": icon}
    if action:
        r["JsonRPCAction"] = {"method": "download", "parameters": action}
    return r


def error(title, sub=""):
    return {"result": [result(title, sub)]}


# === Query ===

def media_formats(info):
    """yt-dlp formats worth offering: no storyboards, no format without audio and video."""
    raw = info.get("formats") or ([info] if info.get("format_id") and info.get("url") else [])
    out, seen = [], set()
    for f in raw:
        if not isinstance(f, dict) or not f.get("format_id"):
            continue
        note = str(f.get("format_note") or "").lower()
        if f.get("ext") == "mhtml" or "storyboard" in note:
            continue
        audio_only = f.get("vcodec") == "none"
        if audio_only and f.get("acodec") == "none":
            continue
        width, height = num(f.get("width")), num(f.get("height"))
        size = num(f.get("filesize") or f.get("filesize_approx"))
        tbr = num(f.get("tbr"))
        key = (f["format_id"], width, height, tbr, size)
        if key in seen:
            continue
        seen.add(key)
        out.append({
            "id": f["format_id"], "audio": audio_only, "width": width, "height": height,
            "fps": num(f.get("fps")), "tbr": tbr, "size": size, "ext": f.get("ext") or "",
        })
    return out


def sort_formats(formats, order):
    keys = {
        "File Size": lambda f: f["size"],
        "Total Bitrate": lambda f: f["tbr"],
        "FPS": lambda f: f["fps"],
    }
    key = keys.get(order, lambda f: (f["width"] * f["height"], f["tbr"]))
    return sorted(formats, key=key, reverse=True)


YOUTUBE_ID = re.compile(r"(?:youtube\.com/(?:watch\?(?:.*&)?v=|shorts/|live/|embed/)|youtu\.be/)([\w-]{11})")


def fetch(url: str, path: Path) -> bool:
    try:
        req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
        with urllib.request.urlopen(req, timeout=3) as r:
            path.write_bytes(r.read())
        return True
    except Exception:
        return False


def prefetch_thumbnail(url: str, vid: str):
    """YouTube thumbnails have a known address: fetch it while yt-dlp extracts."""
    m = YOUTUBE_ID.search(url)
    if not m:
        return None
    path = TEMP / f"{vid}.jpg"
    t = threading.Thread(target=fetch, args=(f"https://i.ytimg.com/vi/{m.group(1)}/mqdefault.jpg", path), daemon=True)
    t.start()
    return t


def thumbnail(info, vid: str, prefetch=None) -> str:
    """A small JPEG thumbnail saved locally (LiteLaunch shows local files only)."""
    path = TEMP / f"{vid}.jpg"
    if prefetch:
        prefetch.join(3)
        if path.exists() and path.stat().st_size > 0:
            return str(path)
    thumbs = [t for t in info.get("thumbnails") or [] if str(t.get("url", "")).split("?")[0].endswith(".jpg")]
    thumbs.sort(key=lambda t: num(t.get("width"), 9999) if num(t.get("width"), 9999) >= 100 else 99999)
    url = thumbs[0]["url"] if thumbs else info.get("thumbnail")
    if not url:
        return ICON
    return str(path) if fetch(url, path) else ICON


def cleanup():
    """Info files and thumbnails older than a day."""
    cutoff = time.time() - 86400
    for p in TEMP.glob("*"):
        try:
            if p.stat().st_mtime < cutoff:
                p.unlink()
        except OSError:
            pass


def query(url: str, settings):
    from yt_dlp import YoutubeDL

    TEMP.mkdir(parents=True, exist_ok=True)
    vid = hashlib.sha1(url.encode()).hexdigest()[:16]
    prefetch = prefetch_thumbnail(url, vid)
    errors = []

    class Quiet(YoutubeDL):
        def report_error(self, message, *args, **kwargs):
            errors.append(str(message))

    def extract(opts):
        try:
            with Quiet(opts) as ydl:
                # Unprocessed: the processed info's YouTube https links get 403 when downloaded later.
                info = ydl.extract_info(url, download=False, process=False)
                if info and not info.get("formats") and info.get("_type") in ("url", "url_transparent", "playlist"):
                    info = ydl.process_ie_result(info, download=False)
                return ydl.sanitize_info(info) if info else None
        except Exception as e:
            errors.append(str(e))
            return None

    # YouTube: the visionos client alone gives the same formats ~2.5 s faster than the
    # default set (whose tv client downloads the player and solves its JS in node).
    # Videos it can't serve (age-restricted...) fall back to the defaults.
    info = None
    if YOUTUBE_ID.search(url):
        info = extract({**base_opts(settings), "extractor_args": {"youtube": {"player_client": ["visionos"]}}})
        if info and not info.get("formats"):
            info = None
    if not info:
        info = extract(base_opts(settings))
    if not info:
        msg = errors[-1] if errors else "yt-dlp не вернул данных"
        return error("Не удалось получить видео", msg.replace("ERROR: ", "")[:200])
    if info.get("_type") == "playlist":
        return error("Это плейлист", "Нужна ссылка на одно видео")

    info_path = TEMP / f"{vid}.info.json"
    info_path.write_text(json.dumps(info, ensure_ascii=False), encoding="utf-8")
    icon = thumbnail(info, vid, prefetch)
    title = str(info.get("title") or "Без названия")

    formats = media_formats(info)
    if not formats:
        return error("Нет доступных форматов", title)
    formats = sort_formats(formats, settings.get("sorting_order", "Resolution"))[:MAX_FORMATS]
    path = str(info_path)

    results = []
    videos = [f for f in formats if not f["audio"] and f["height"]]
    if videos:
        best = max(videos, key=lambda f: (f["width"] * f["height"], f["tbr"]))
        results.append(result(f"★ Лучшее видео · {best['height']:.0f}p", title, icon, [path, best["id"], False]))
    audios = [f for f in formats if f["audio"]]
    if audios:
        best = max(audios, key=lambda f: f["tbr"])
        kbps = f" · {best['tbr']:.0f} kbps" if best["tbr"] else ""
        results.append(result(f"★ Только звук{kbps}", title, icon, [path, best["id"], True]))
    # The same format often comes over several protocols: one row each, the best bitrate.
    rows = {}
    for f in formats:
        if f["audio"]:
            name = "Звук"
        elif f["height"]:
            name = f"{f['width']:.0f}×{f['height']:.0f}"
            if f["fps"]:
                name += f" · {f['fps']:.0f} fps"
        else:
            name = "Видео"
        key = (name, f["ext"])
        if key not in rows or f["tbr"] > rows[key][1]["tbr"]:
            rows[key] = (name, f)
    for name, f in rows.values():
        sub = " · ".join(s for s in (f["ext"], f"{f['tbr']:.0f} kbps" if f["tbr"] else "", size_text(f["size"])) if s)
        results.append(result(name, sub, icon, [path, f["id"], f["audio"]]))
    return {"result": results}


# === Download ===

def download(info_path: str, format_id: str, is_audio: bool, settings):
    from yt_dlp import YoutubeDL, parse_options
    from yt_dlp.utils import DownloadError

    info = json.loads(Path(info_path).read_text(encoding="utf-8"))
    title = str(info.get("title") or "видео")
    folder = expand(settings.get("download_dir") or "%USERPROFILE%\\Downloads")
    os.makedirs(folder, exist_ok=True)

    if is_audio:
        fmt = f"{format_id}/bestaudio/best"
        args = ["-x", "--audio-format", settings.get("audio_format") or "mp3", "--audio-quality", "0"]
    else:
        fmt = f"{format_id}+bestaudio/{format_id}/bestvideo+bestaudio/best"
        args = ["--remux-video", settings.get("video_format") or "mp4"]
    args += [
        "-f", fmt, "-P", folder, "-o", "%(title).100s.%(ext)s",
        "--windows-filenames", "--trim-filenames", "120",
        "--no-mtime", "--no-playlist", "--no-part", "--retries", "3", "--retry-sleep", "2",
        "--quiet", "--no-warnings", "--no-progress",
    ]
    if settings.get("overwrite", True):
        args.append("--force-overwrites")
    ff = ffmpeg_dir(settings)
    if ff:
        args += ["--ffmpeg-location", ff]
    opts = parse_options(args).ydl_opts
    opts.update({k: v for k, v in base_opts(settings).items() if k in ("cookiefile", "js_runtimes")})
    # The CLI default swallows download errors; we need them to report and to retry.
    opts["ignoreerrors"] = False

    notify("Скачивание", title)
    try:
        with YoutubeDL(opts) as ydl:
            try:
                res = ydl.process_ie_result(info, download=True)
            except DownloadError:
                # Links in the saved info expire after a few hours: extract again.
                res = ydl.extract_info(info.get("webpage_url") or info["original_url"], download=True)
        files = [d.get("filepath") for d in (res or {}).get("requested_downloads") or [] if d.get("filepath")]
        name = Path(files[-1]).name if files else title
        notify("Готово", name, folder)
        if settings.get("open_folder_after"):
            os.startfile(folder)
    except Exception as e:
        notify("Ошибка скачивания", f"{title}: {str(e).replace('ERROR: ', '')[:150]}")
    maybe_update()


# === yt-dlp updates ===

def maybe_update():
    """Sites change and yt-dlp follows: update it in the background once a week."""
    try:
        if time.time() - UPDATE_MARKER.stat().st_mtime < UPDATE_DAYS * 86400:
            return
    except OSError:
        pass
    UPDATE_MARKER.touch()
    python = sys.executable.replace("pythonw.exe", "python.exe")
    req = json.dumps({"method": "update", "parameters": []})
    subprocess.Popen([python, "-B", __file__, req], cwd=HERE, creationflags=0x08000000 | 0x00000008,
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def update():
    """pip into lib.new, then swap folders, so a running query never sees half a package."""
    import shutil
    new, old = HERE / "lib.new", HERE / "lib.old"
    shutil.rmtree(new, ignore_errors=True)
    python = sys.executable.replace("pythonw.exe", "python.exe")
    r = subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check",
                        "--target", str(new), "yt-dlp[default]"],
                       creationflags=0x08000000, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if r.returncode != 0 or not (new / "yt_dlp").is_dir():
        shutil.rmtree(new, ignore_errors=True)
        return
    shutil.rmtree(old, ignore_errors=True)
    # A running query or download keeps lib's .pyd files open: wait for it to finish.
    for _ in range(60):
        try:
            if LIB.exists():
                LIB.rename(old)
            break
        except OSError:
            time.sleep(5)
    else:
        shutil.rmtree(new, ignore_errors=True)
        UPDATE_MARKER.unlink(missing_ok=True)
        return
    new.rename(LIB)
    shutil.rmtree(old, ignore_errors=True)


# === Entry ===

def handle(req):
    method = req.get("method")
    params = req.get("parameters") or []
    settings = req.get("settings") or {}
    if method == "query":
        marker = TEMP / ".cleaned"
        if not marker.exists() or time.time() - marker.stat().st_mtime > 3600:
            TEMP.mkdir(parents=True, exist_ok=True)
            cleanup()
            marker.touch()
        print(json.dumps(query(params[0], settings), ensure_ascii=False))
        sys.stdout.flush()
        maybe_update()
    elif method == "download":
        download(params[0], params[1], bool(params[2]), settings)
    elif method == "update":
        update()


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "--wait":
        # Prewarm: pay for the import while the user is still pasting the link.
        timer = threading.Timer(WAIT_SECONDS, lambda: os._exit(0))
        timer.start()
        import yt_dlp  # noqa: F401
        line = sys.stdin.readline()
        timer.cancel()
        if not line.strip():
            os._exit(0)
        handle(json.loads(line))
        os._exit(0)
    handle(json.loads(sys.argv[1]))


if __name__ == "__main__":
    main()
