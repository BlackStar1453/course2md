"""course2md 浏览器扩展的 Native Messaging 桥接程序（只用标准库）。

由 Chrome 经 install.sh 生成的启动脚本拉起。stdin/stdout 是 Chrome 的消息通道：
每条消息 = 4 字节本机字节序长度 + UTF-8 JSON。stdout 只能写这种消息，
所以 course2md 的输出一律接管道解析后转发，诊断信息只写 stderr。

收到的消息：
  {"type": "convert", "jobId", "source", "isFile", "title"}  转换一个视频
  {"type": "open", "path"}                                    打开笔记网页版
发出的消息：
  {"type": "stage", "stage"} / {"type": "done", ...} / {"type": "error", "message"}
"""
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


def log(message):
    print(f"[course2md-host] {message}", file=sys.stderr, flush=True)


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


def keep_video(path, title):
    """把下载好的视频挪进 VIDEO_DIR，按页面标题命名（course2md 用文件名当笔记标题）。"""
    src = Path(path)
    if not src.is_file():
        raise RuntimeError(f"找不到下载好的视频：{src}")
    VIDEO_DIR.mkdir(parents=True, exist_ok=True)
    stem = safe_name(title)
    dest = VIDEO_DIR / f"{stem}{src.suffix or '.mp4'}"
    n = 2
    while dest.exists():
        dest = VIDEO_DIR / f"{stem} ({n}){src.suffix or '.mp4'}"
        n += 1
    shutil.move(str(src), dest)
    return dest


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


def convert(msg):
    if not CLI.is_file():
        return send({"type": "error", "message": f"没找到 {CLI}，请先安装 course2md"})
    source = msg.get("source") or ""
    if msg.get("isFile"):
        try:
            source = str(keep_video(source, msg.get("title")))
        except (OSError, RuntimeError) as e:
            return send({"type": "error", "message": str(e)})

    cmd = [str(CLI), source, "-o", library_root(), "--json", "--formats", "md,html", "--no-llm-hint", *LLM_ARGS]
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
        return send({"type": "error", "message": f"course2md 转换失败：{detail}"})

    html = find_html(done)
    problems = problems_of(done.get("outcomes"))
    if not html and "exports.html" not in problems:
        problems.append("exports.html")
    send({"type": "done", "title": done.get("title"), "html": html,
          "partial": bool(problems or done.get("partial")), "problems": problems})
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
    msg = read_message()
    if not msg:
        return
    kind = msg.get("type")
    if kind == "convert":
        convert(msg)
    elif kind == "open":
        open_note(msg)
    else:
        send({"type": "error", "message": f"未知消息类型：{kind}"})


if __name__ == "__main__":
    main()
