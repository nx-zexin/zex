#!/usr/bin/env bash
# 把设计文档渲染成深色 HTML 并用浏览器打开。
#
# 为什么不直接用系统默认的 .md 关联（本机是 MarkText）：
#   MarkText 是 Electron 应用，在 Wayland 会话下报
#   "'--ozone-platform=wayland' is not compatible with Vulkan"，
#   进程起来了但窗口不进 X11 层，等于看不见。
#   chromium 加 --ozone-platform=x11 稳定可用。
#
# 用法：
#   ./preview.sh              渲染 ui-spec.md
#   ./preview.sh ui-prompt.md 渲染指定文件
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOC="${1:-ui-spec.md}"
SRC="$HERE/$DOC"
OUT_DIR="/tmp/zex-doc"
OUT="$OUT_DIR/${DOC%.md}.html"

[[ -f "$SRC" ]] || { echo "找不到 $SRC" >&2; exit 1; }
command -v pandoc >/dev/null || { echo "需要 pandoc" >&2; exit 1; }

mkdir -p "$OUT_DIR"

# 用 ui-spec.md 附录里的视觉令牌渲染文档本身（自举）
cat > "$OUT_DIR/style.css" <<'CSS'
:root{--bg-app:#0E1014;--bg-panel:#16191F;--bg-code:#12151A;--bg-hover:#1E232B;
--border:#252A33;--text-primary:#E6E8EC;--text-muted:#8B92A0;--brand:#E8A33D;
--mono:"JetBrains Mono","SF Mono",monospace;
--sans:"Noto Sans CJK SC","Source Han Sans SC",sans-serif}
*{box-sizing:border-box}
body{margin:0;background:var(--bg-app);color:var(--text-primary);
font-family:var(--sans);font-size:15px;line-height:1.75;-webkit-font-smoothing:antialiased}
main,.content{max-width:980px;margin:0 auto;padding:48px 32px 120px}
h1{font-size:30px;margin:0 0 8px;color:#fff}
h1+blockquote{margin-top:0}
h2{font-size:21px;margin:56px 0 16px;padding-bottom:10px;
border-bottom:1px solid var(--border);color:var(--brand)}
h3{font-size:17px;margin:32px 0 12px;color:#fff}
h4{font-size:15px;margin:24px 0 8px}
p{margin:12px 0}
a{color:var(--brand);text-decoration:none;border-bottom:1px solid rgba(232,163,61,.3)}
blockquote{margin:16px 0;padding:12px 20px;border-left:3px solid var(--brand);
background:var(--bg-panel);color:var(--text-muted);border-radius:0 6px 6px 0}
blockquote strong{color:var(--text-primary)}
code{font-family:var(--mono);font-size:.88em;background:var(--bg-code);
padding:2px 6px;border-radius:4px;color:var(--brand)}
pre{background:var(--bg-code);border:1px solid var(--border);border-radius:8px;
padding:18px 20px;overflow-x:auto;margin:18px 0;line-height:1.5}
/* 布局图必须严格等宽，关掉连字 */
pre code{background:none;padding:0;color:var(--text-primary);font-size:13px;
font-variant-ligatures:none;font-feature-settings:"liga" 0,"calt" 0}
table{width:100%;border-collapse:collapse;margin:18px 0;font-size:14px;
background:var(--bg-panel);border-radius:8px;overflow:hidden}
th{background:var(--bg-hover);text-align:left;padding:11px 14px;font-weight:600;
color:var(--text-muted);font-size:13px;border-bottom:1px solid var(--border)}
td{padding:11px 14px;border-bottom:1px solid var(--border);vertical-align:top}
tr:last-child td{border-bottom:none}
tr:hover td{background:rgba(30,35,43,.5)}
ul,ol{padding-left:26px;margin:12px 0}
li{margin:6px 0}
hr{border:none;border-top:1px solid var(--border);margin:48px 0}
strong{color:#fff;font-weight:600}
::-webkit-scrollbar{width:10px;height:10px}
::-webkit-scrollbar-track{background:var(--bg-app)}
::-webkit-scrollbar-thumb{background:#2E3440;border-radius:5px}
CSS

pandoc "$SRC" -f markdown -t html5 -s \
  --metadata title="${DOC%.md}" \
  --metadata lang=zh-CN \
  --css="$OUT_DIR/style.css" \
  --embed-resources \
  -o "$OUT" 2>/dev/null

echo "已生成 $OUT"

# Linux/Wayland 下强制走 X11，避免 Electron/Chromium 的 Vulkan 冲突
if command -v chromium >/dev/null; then
  nohup chromium --ozone-platform=x11 --new-window "file://$OUT" >/dev/null 2>&1 &
elif command -v firefox >/dev/null; then
  nohup firefox "file://$OUT" >/dev/null 2>&1 &
else
  xdg-open "$OUT"
fi
