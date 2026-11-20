# 图标

## 一句话

Z 和 X 共用一条对角：第一眼看是 Z，第二眼看得出那条奶白斜线与橙色对角交叉成了 X。

## 几何

一枚图标里只有四条笔画：

```
Z = 顶横 + 对角（右上 → 左下）+ 底横      ← 一笔连写，橙色
X = 同一条对角 + 反向斜线（左上 → 右下）  ← 反向那条是奶白
              ↑
        两个字母共用这条对角
```

反向斜线画在 Z **下面**，交叉处让橙色压过来。于是橙色对角的两臂完整
（X 的右上臂、左下臂），奶白那条被切成左上、右下两段 —— 四个臂都在，
X 才立得住。若让奶白压在上面，橙色对角会被切断，整张图就读不出 X 了。

反向斜线的两端沿斜向内缩 62（画布 1024），收在横幅内部。露出去会在
横幅角上留下锯齿状的小三角，试过，脏。

## 配色

全部取自应用里的品牌变量，没有新造颜色：

| 用途 | 值 | 对应常量 |
|---|---|---|
| 底板 | `#1A1A1A` | `TEXT` |
| Z | `#F59E0B` | `BRAND` |
| X 的反向斜线 | `#FEF3E2` | `BRAND_SOFT` |

## 文件

| 文件 | 用途 |
|---|---|
| `zex.svg` | 标准版，48px 以上与所有矢量场景 |
| `zex-small.svg` | 16–32px 专用：笔画 132→156、圆角 224→150 |
| `zex-16/24/32.png` | 由 `zex-small.svg` 渲染 |
| `zex-48/…/1024.png` | 由 `zex.svg` 渲染 |
| `../linux/` | `.desktop` 与安装脚本 |

为什么要两套：16px 时笔画只剩 2 个像素、圆角占掉太多空间，标准版会糊。
小尺寸版只做两处光学补偿，几何完全一样。

## 重新生成

```bash
cd assets/icon
for s in 16 24 32; do rsvg-convert -w $s -h $s zex-small.svg -o zex-$s.png; done
for s in 48 64 128 256 512 1024; do rsvg-convert -w $s -h $s zex.svg -o zex-$s.png; done
```

## 接进系统

**X11**：`WindowOptions.icon` 直接给像素（`zex-app` 的 `window_icon()`，
嵌入 `zex-256.png`）。

**Wayland**：窗口图标不在进程里设置 —— compositor 按窗口的 `app_id`
（代码里是 `"zex"`）去找同名 `.desktop`，再按其中的 `Icon=zex` 到图标
主题里查。所以需要：

```bash
assets/linux/install-icon.sh
```

装完重启应用即可。`app_id`、`.desktop` 文件名、`StartupWMClass` 三者
必须是同一个名字，对不上就没有图标。
