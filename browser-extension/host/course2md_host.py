"""course2md 浏览器扩展的 Native Messaging 桥接程序（只用标准库）。

由 Chrome 经 install.sh 生成的启动脚本拉起。stdin/stdout 是 Chrome 的消息通道：
每条消息 = 4 字节本机字节序长度 + UTF-8 JSON。stdout 只能写这种消息，
所以 course2md 的输出一律接管道解析后转发，诊断信息只写 stderr。

收到的消息（一次连接只做一件事）：
  {"type": "convert", "source", "title"}      把网址交给 course2md
  {"type": "begin", "title"}                  开始接收视频，随后：
    {"type": "chunk", "index", "data": <base64>} ...  视频分块，index 区分画面 / 声音等不同文件
    {"type": "end", "ok": [index...]} / {"type": "abort"}  收完则拼成一个视频再转换 / 放弃并删掉半成品
  {"type": "open", "path"}                    打开笔记网页版
发出的消息：
  {"type": "stage", "stage"} / {"type": "done", ...} / {"type": "error", "message"}
"""
import base64
import collections
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import threading
from pathlib import Path

APP = Path("/Applications/course2md.app")
CLI = APP / "Contents/MacOS/course2md"
HOME = Path.home()
CONFIG_DIR = Path(os.environ.get("XDG_CONFIG_HOME") or HOME / ".config") / "course2md"
# 本地文件笔记靠这个视频回放「从此处观看」，所以保留，不删（与 video-to-notes 一致）。
VIDEO_DIR = HOME / "Movies/course2md-videos"
# 与 video-to-notes 的默认 AI 设置一致；要改就改这里。
LLM_ARGS = ["--llm", "--llm-provider", "claude-code", "--llm-model", "sonnet", "--llm-vision", "--summarize"]
OK_STATUSES = {"succeeded", "not_requested", "skipped"}

_send_lock = threading.Lock()
_connected = True


LOG_FILE = HOME / "Library/Logs/course2md-host.log"


def log(message):
    # Chrome 不展示桥接程序的 stderr，同时写一份日志文件方便排查。
    line = f"[course2md-host {os.getpid()}] {message}"
    print(line, file=sys.stderr, flush=True)
    try:
        with open(LOG_FILE, "a") as f:
            f.write(line + "\n")
    except OSError:
        pass


def send(obj):
    """发一条消息给扩展；扩展断开后静默丢弃，让转换继续跑完。"""
    global _connected
    if not _connected:
        return
    data = json.dumps(obj, ensure_ascii=False).encode("utf-8")
    with _send_lock:
        try:
            sys.stdout.buffer.write(struct.pack("=I", len(data)) + data)
            sys.stdout.buffer.flush()
        except (BrokenPipeError, OSError):
            _connected = False
            log("扩展已断开，继续完成转换")


def read_message():
    raw = sys.stdin.buffer.read(4)
    if len(raw) < 4:
        return None
    (length,) = struct.unpack("=I", raw)
    body = sys.stdin.buffer.read(length)
    if len(body) < length:
        return None
    return json.loads(body.decode("utf-8"))


def library_root():
    """与 video-to-notes 的 v2n.py 相同：取 default_library 的 root，读不到退回默认库。"""
    ws = CONFIG_DIR / "desktop-workspace.json"
    try:
        data = json.loads(ws.read_text())
        for lib in data.get("libraries", []):
            if lib.get("id") == data.get("default_library") and lib.get("root"):
                return lib["root"]
    except (OSError, ValueError):
        pass
    return str(CONFIG_DIR / "desktop-local-library")


def safe_name(title):
    name = re.sub(r'[\\/:*?"<>|\x00-\x1f\x7f]', " ", title or "")
    name = re.sub(r"\s+", " ", name).strip(" .")[:60].strip()
    return name or "video"


def tool(name):
    """Chrome 拉起的进程 PATH 很短，ffprobe / ffmpeg 要去 Homebrew 目录找。"""
    for d in ("/opt/homebrew/bin", "/usr/local/bin"):
        if os.path.isfile(os.path.join(d, name)):
            return os.path.join(d, name)
    return shutil.which(name)


def probe(path):
    """返回 (有画面, 有声音, 时长秒)；没有 ffprobe 或读失败时返回 None。"""
    ffprobe = tool("ffprobe")
    if not ffprobe:
        return None
    r = subprocess.run([ffprobe, "-v", "error", "-show_entries", "stream=codec_type:format=duration",
                        "-of", "json", str(path)], capture_output=True, text=True)
    try:
        info = json.loads(r.stdout)
    except ValueError:
        return None
    kinds = {st.get("codec_type") for st in info.get("streams", [])}
    try:
        duration = float(info.get("format", {}).get("duration") or 0)
    except ValueError:
        duration = 0.0
    return ("video" in kinds, "audio" in kinds, duration)


def assemble(files, dest):
    """从收到的文件里拼出一个有画面有声音的视频，写到 dest。返回 (路径, 是否只有声音)。

    - 有文件同时有画面和声音：直接用它
    - 一个只有画面、一个只有声音，且时长相近：ffmpeg 只合并不重新编码
    - 否则退而求其次：优先用有声音的（文字稿要靠它），没有截图
    """
    infos = [(f, probe(f)) for f in files]
    if any(info is None for _, info in infos):
        best = max(files, key=lambda f: f.stat().st_size)
        best.rename(dest)
        return dest, False
    both = [f for f, (v, a, _) in infos if v and a]
    if both:
        max(both, key=lambda f: f.stat().st_size).rename(dest)
        return dest, False
    video = next(((f, d) for f, (v, a, d) in infos if v), None)
    audio = next(((f, d) for f, (v, a, d) in infos if a), None)
    if video and audio and abs(video[1] - audio[1]) <= 2:
        r = subprocess.run([tool("ffmpeg") or "ffmpeg", "-v", "error", "-y", "-i", str(video[0]), "-i", str(audio[0]),
                            "-map", "0:v:0", "-map", "1:a:0", "-c", "copy", str(dest)],
                           capture_output=True, text=True)
        if r.returncode == 0 and dest.is_file():
            log(f"已合并画面与声音 → {dest}")
            return dest, False
        log(f"合并失败：{r.stderr.strip()[-300:]}")
    if audio:
        audio[0].rename(dest)
        return dest, True
    video[0].rename(dest)
    return dest, False


def receive_video(title):
    """接收扩展转来的视频分块（可能有多个文件，按 index 区分），在 VIDEO_DIR 里拼成一个视频，
    按页面标题命名（course2md 用文件名当笔记标题）。

    分块先写 .part，收到 end 才处理；abort / 断开 / 出错则删掉半成品。返回 (路径, 是否只有声音) 或 None。
    """
    VIDEO_DIR.mkdir(parents=True, exist_ok=True)
    stem = safe_name(title)
    parts, handles, sizes = {}, {}, {}

    def cleanup():
        for h in handles.values():
            h.close()
        for f in parts.values():
            f.unlink(missing_ok=True)

    try:
        while True:
            msg = read_message()
            kind = msg and msg.get("type")
            if kind == "chunk":
                i = int(msg.get("index") or 0)
                if i not in handles:
                    parts[i] = VIDEO_DIR / f".{stem}.{os.getpid()}.{i}.part"
                    handles[i] = open(parts[i], "wb")
                    sizes[i] = 0
                data = base64.b64decode(msg.get("data") or "")
                handles[i].write(data)
                sizes[i] += len(data)
            elif kind == "end":
                break
            else:
                log(f"接收中止：{kind or '扩展断开'}，已收 {sizes}")
                cleanup()
                return None
        for h in handles.values():
            h.close()
        handles.clear()
        ok = {int(i) for i in (msg.get("ok") or [])}
        files = [parts[i] for i in sorted(parts) if i in ok and sizes[i] > 0]
        for i in parts:
            if parts[i] not in files:
                parts[i].unlink(missing_ok=True)
        if not files:
            send({"type": "error", "message": "收到的视频是空的"})
            return None
        log(f"已接收 {len(files)} 个文件，字节数 {[sizes[i] for i in sorted(parts) if parts[i] in files]}")
        dest = VIDEO_DIR / f"{stem}.mp4"
        n = 2
        while dest.exists():
            dest = VIDEO_DIR / f"{stem} ({n}).mp4"
            n += 1
        result = assemble(files, dest)
        for f in files:
            f.unlink(missing_ok=True)  # 合并后剩下的半成品
        log(f"视频就绪 → {result[0]}{'（只有声音）' if result[1] else ''}")
        return result
    except (OSError, ValueError) as e:
        cleanup()
        send({"type": "error", "message": f"保存视频失败：{e}"})
        return None


def find_html(done):
    out_dir = Path(done.get("out_dir") or "")
    for p in (out_dir / "exports/course.html", out_dir / "course.html"):
        if p.is_file():
            return str(p)
    return None


def problems_of(outcomes):
    """course2md 允许部分成功：列出状态不是成功 / 未请求的步骤。"""
    bad = []
    for name, value in (outcomes or {}).items():
        if not isinstance(value, dict):
            continue
        if "status" in value:
            if value["status"] not in OK_STATUSES:
                bad.append(name)
        else:  # 如 exports: {"html": {...}, "md": {...}}
            bad += [f"{name}.{k}" for k, v in value.items()
                    if isinstance(v, dict) and v.get("status") not in OK_STATUSES]
    return bad


SUB_DIR = HOME / "Library/Caches/course2md-ext"
MAX_SUBTITLE_BYTES = 32 * 1024 * 1024  # 与 course2md 读取字幕的字节上限一致


def write_subtitle(job_id, text):
    """把扩展在页面里取到的字幕存成临时 VTT（按任务区分，避免同一视频并发时互相覆盖）。"""
    data = text.encode("utf-8")
    if len(data) > MAX_SUBTITLE_BYTES:
        raise ValueError("字幕太大")
    SUB_DIR.mkdir(parents=True, exist_ok=True)
    name = re.sub(r"[^\w-]", "_", job_id or str(os.getpid()))
    path = SUB_DIR / f"{name}.vtt"
    try:
        path.write_bytes(data)
    except OSError:
        path.unlink(missing_ok=True)  # 不留写了一半的文件
        raise
    return path


def cli_supports_subtitle():
    """已安装的 course2md 是否已有 --subtitle（新版才有）；旧版就退回语音识别，而不是报参数错误。"""
    try:
        r = subprocess.run([str(CLI), "--help"], capture_output=True, text=True, timeout=20)
        return "--subtitle" in r.stdout
    except (OSError, subprocess.SubprocessError):
        return False


def convert_message(msg):
    """处理 convert 消息：带字幕就用 --subtitle；要求兜底就改用语音识别（避开会 429 的在线字幕）。"""
    extra, notes, sub_path = [], [], None
    if msg.get("subtitle") and not cli_supports_subtitle():
        log("已安装的 course2md 还不支持 --subtitle，改用语音识别")
        extra = ["--transcript-source", "asr"]
        notes.append("course2md 版本过旧，不能直接用网页字幕，已改用语音识别（升级后即可）")
    elif msg.get("subtitle"):
        try:
            sub_path = write_subtitle(msg.get("jobId"), msg["subtitle"])
            extra = ["--subtitle", str(sub_path)]
        except (OSError, ValueError) as e:
            log(f"字幕写入失败，改用语音识别：{e}")
            extra = ["--transcript-source", "asr"]
            notes.append("网页字幕不可用，改用语音识别")
    elif msg.get("asrFallback"):
        extra = ["--transcript-source", "asr"]
        if msg.get("note"):
            notes.append(msg["note"])
    try:
        convert(msg.get("source") or "", extra_args=extra, notes=notes)
    finally:
        if sub_path:
            sub_path.unlink(missing_ok=True)


def convert(source, audio_only=False, extra_args=(), notes=()):
    if not CLI.is_file():
        return send({"type": "error", "message": f"没找到 {CLI}，请先安装 course2md"})

    cmd = [str(CLI), source, "-o", library_root(), "--json", "--formats", "md,html", "--no-llm-hint",
           *LLM_ARGS, *extra_args]
    log("运行: " + " ".join(cmd))
    env = dict(os.environ)
    # Chrome 拉起的进程不读 shell 配置，补上 ffmpeg / yt-dlp / claude 的常见位置。
    env["PATH"] = ":".join([env.get("PATH", ""), "/opt/homebrew/bin", "/usr/local/bin",
                            str(HOME / ".local/bin"), str(HOME / "Library/pnpm")])
    try:
        # 独立进程组：扩展断开、本程序被结束时，转换仍能跑完。
        proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, env=env, start_new_session=True)
    except OSError as e:
        return send({"type": "error", "message": f"启动 course2md 失败：{e}"})

    stderr_tail = collections.deque(maxlen=20)
    drain = threading.Thread(target=lambda: stderr_tail.extend(proc.stderr), daemon=True)
    drain.start()

    done, errors, last_stage = None, [], None
    for line in proc.stdout:
        try:
            event = json.loads(line)
        except ValueError:
            continue
        kind = event.get("type")
        if kind == "stage" and event.get("status") == "start" and event.get("stage") != last_stage:
            last_stage = event.get("stage")
            send({"type": "stage", "stage": last_stage})
        elif kind == "error":
            errors.append(event.get("message") or "未知错误")
        elif kind == "done":
            done = event
    code = proc.wait()
    drain.join(timeout=5)

    if code != 0 or done is None:
        detail = "；".join(errors) or "".join(stderr_tail).strip()[-300:] or f"退出码 {code}"
        log(f"转换失败（退出码 {code}）：{detail}")
        return send({"type": "error", "message": f"course2md 转换失败：{detail}"})

    html = find_html(done)
    problems = problems_of(done.get("outcomes"))
    if audio_only:
        problems.append("只拿到声音，没有截图")

    if not html and "exports.html" not in problems:
        problems.append("exports.html")
    send({"type": "done", "title": done.get("title"), "html": html,
          "partial": bool(problems or done.get("partial")), "problems": problems,
          "notes": list(notes)})
    subprocess.run(["open", "-a", str(APP)], check=False)


def open_note(msg):
    """只打开笔记库里的 .html，避免被当成任意文件打开器。"""
    path = Path(msg.get("path") or "").resolve()
    root = Path(library_root()).resolve()
    if path.suffix == ".html" and path.is_file() and root in path.parents:
        subprocess.run(["open", str(path)], check=False)
        subprocess.run(["open", "-a", str(APP)], check=False)
        send({"type": "opened"})
    else:
        send({"type": "error", "message": "笔记文件不存在或不在笔记库里"})


def main():
    log(f"启动 argv={sys.argv[1:]}")
    msg = read_message()
    kind = msg and msg.get("type")
    log(f"收到消息 {kind}")
    if not msg:
        return
    if kind == "convert":
        convert_message(msg)
    elif kind == "begin":
        if not CLI.is_file():
            return send({"type": "error", "message": f"没找到 {CLI}，请先安装 course2md"})
        received = receive_video(msg.get("title"))
        if received:
            convert(str(received[0]), audio_only=received[1])
    elif kind == "open":
        open_note(msg)
    else:
        send({"type": "error", "message": f"未知消息类型：{kind}"})


if __name__ == "__main__":
    main()
