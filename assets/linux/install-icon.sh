#!/usr/bin/env bash
# 把图标和桌面入口装到当前用户目录（不需要 root）。
#
# Wayland 下窗口图标不在应用进程里设置 —— compositor 按窗口的 app_id
# 去找同名的 .desktop，再读其中的 Icon= 到图标主题里查。所以这三步：
#   1. 图标进 hicolor 主题（各尺寸各一份，compositor 按需挑）
#   2. .desktop 进 applications
#   3. 刷新缓存（不刷的话要等下次登录才生效）
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
icon_dir="$HOME/.local/share/icons/hicolor"
app_dir="$HOME/.local/share/applications"

for size in 16 24 32 48 64 128 256 512 1024; do
    src="$here/../icon/zex-$size.png"
    [ -f "$src" ] || continue
    install -Dm644 "$src" "$icon_dir/${size}x${size}/apps/zex.png"
done
install -Dm644 "$here/../icon/zex.svg" "$icon_dir/scalable/apps/zex.svg"
install -Dm644 "$here/zex.desktop" "$app_dir/zex.desktop"

command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$icon_dir" >/dev/null 2>&1 || true
command -v update-desktop-database >/dev/null && update-desktop-database "$app_dir" >/dev/null 2>&1 || true

echo "已安装："
echo "  图标  $icon_dir/*/apps/zex.png"
echo "  入口  $app_dir/zex.desktop"
