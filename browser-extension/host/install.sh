#!/bin/sh
# 向 Chrome 注册 course2md 的 Native Messaging 桥接程序（macOS）。
#
# 用法：
#   1. Chrome 打开 chrome://extensions，开启「开发者模式」，「加载已解压的扩展程序」选 browser-extension/
#   2. 复制扩展卡片上的 ID，运行：sh browser-extension/host/install.sh <扩展ID>
#   扩展目录挪动后 ID 会变，需要用新 ID 重跑一次。
set -eu

HOST_NAME="com.course2md.host"
EXT_ID="${1:-}"

if ! printf '%s' "$EXT_ID" | grep -Eq '^[a-p]{32}$'; then
  echo "用法：sh $0 <扩展ID>（chrome://extensions 里显示的 32 位小写字母 ID）" >&2
  exit 1
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
HOST_PY="$HERE/course2md_host.py"
[ -f "$HOST_PY" ] || { echo "找不到 $HOST_PY" >&2; exit 1; }

# Chrome 拉起桥接程序时不读 shell 配置，找不到 PATH 里的 python3；
# 所以在这里（用户的 shell 里）解析出 python3 的绝对路径，写死进启动脚本。
PY="$(command -v python3 || true)"
if [ -z "$PY" ] || ! "$PY" -c 'import sys; sys.exit(sys.version_info < (3, 8))' 2>/dev/null; then
  echo "需要 Python 3.8+，请先安装（例如 brew install python）" >&2
  exit 1
fi
PY="$("$PY" -c 'import sys; print(sys.executable)')"

[ -x "/Applications/course2md.app/Contents/MacOS/course2md" ] || \
  echo "提醒：没找到 /Applications/course2md.app，转换前请先安装 course2md" >&2

# 启动脚本放在用户目录，不往仓库里写生成文件。
LAUNCH_DIR="$HOME/Library/Application Support/course2md/native-host"
LAUNCHER="$LAUNCH_DIR/course2md-host"
MANIFEST_DIR="$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts"
mkdir -p "$LAUNCH_DIR" "$MANIFEST_DIR"

# 用 Python 生成两个文件：路径经 shlex.quote / json.dumps 转义，含空格、引号、$ 的路径也不会出错。
"$PY" - "$PY" "$HOST_PY" "$LAUNCHER" "$MANIFEST_DIR/$HOST_NAME.json" "$HOST_NAME" "$EXT_ID" <<'EOF'
import json, os, shlex, sys
py, host_py, launcher, manifest, name, ext_id = sys.argv[1:]
with open(launcher, "w") as f:
    f.write(f'#!/bin/sh\nexec {shlex.quote(py)} {shlex.quote(host_py)} "$@"\n')
os.chmod(launcher, 0o755)
with open(manifest, "w") as f:
    json.dump({
        "name": name,
        "description": "course2md browser extension bridge",
        "path": launcher,
        "type": "stdio",
        "allowed_origins": [f"chrome-extension://{ext_id}/"],
    }, f, indent=2)
EOF

echo "已注册：$MANIFEST_DIR/$HOST_NAME.json"
echo "Python：$PY"
echo "启动脚本：$LAUNCHER"
