#!/usr/bin/env python3
"""2048 - pygame 实现

操作:
    方向键 / WASD  移动
    U              撤销
    R              重新开始
    C              达成 2048 后继续
    ESC            退出
"""

import math
import os
import random

import pygame

# ---------- 配置 ----------

GRID = 4                # 4x4 棋盘
CELL = 110              # 单元格边长(像素)
GAP = 12                # 单元格间距
PAD = 12                # 棋盘与窗口边缘的间距
BOARD_MARGIN = 12       # 棋盘底板与格子之间的留白
HEADER = 90             # 顶部标题区高度

# 棋盘内格子区域尺寸
BOARD_W = GRID * CELL + (GRID - 1) * GAP
BOARD_H = BOARD_W
# 底板尺寸 = 格子区域 + 四周留白
PANEL_W = BOARD_W + BOARD_MARGIN * 2
PANEL_H = BOARD_H + BOARD_MARGIN * 2

WIDTH = PANEL_W + PAD * 2
HEIGHT = HEADER + PANEL_H + PAD
FPS = 60

SLIDE_MS = 110          # 滑动动画时长
POP_MS = 90             # 合并/新生弹出的时长
HISTORY_MAX = 20        # 撤销步数上限

BG_COLOR = (250, 248, 239)
BOARD_COLOR = (187, 173, 160)
EMPTY_COLOR = (205, 193, 180)
TEXT_DARK = (119, 110, 101)
TEXT_LIGHT = (249, 246, 242)

# 数字 -> (背景色, 前景色); 2/4 用深色字, 其余用浅色字
TILE_COLORS = {
    2:    ((238, 228, 218), TEXT_DARK),
    4:    ((237, 224, 200), TEXT_DARK),
    8:    ((242, 177, 121), TEXT_LIGHT),
    16:   ((245, 149, 99),  TEXT_LIGHT),
    32:   ((246, 124, 95),  TEXT_LIGHT),
    64:   ((246, 94, 59),   TEXT_LIGHT),
    128:  ((237, 207, 114), TEXT_LIGHT),
    256:  ((237, 204, 97),  TEXT_LIGHT),
    512:  ((237, 200, 80),  TEXT_LIGHT),
    1024: ((237, 197, 63),  TEXT_LIGHT),
    2048: ((237, 194, 46),  TEXT_LIGHT),
}
FALLBACK_COLOR = ((60, 58, 50), TEXT_LIGHT)  # 超过 2048 的值

# 移动方向
UP, DOWN, LEFT, RIGHT = 0, 1, 2, 3

BEST_FILE = os.path.join(os.path.expanduser("~"), ".2048_best")

# ---------- 游戏逻辑 ----------


def new_board():
    """空棋盘, 随机放两个 2(90%) 或 4(10%)"""
    board = [[0] * GRID for _ in range(GRID)]
    spawn(board)
    spawn(board)
    return board


def spawn(board):
    """在空格随机放一个新数字, 返回该格坐标(无空格时返回 None)"""
    empty = [(r, c) for r in range(GRID) for c in range(GRID) if board[r][c] == 0]
    if not empty:
        return None
    r, c = random.choice(empty)
    board[r][c] = 2 if random.random() < 0.9 else 4
    return (r, c)


def line_cells(direction, i):
    """按移动方向列出第 i 条线上的格子, 顺序为"靠近目标边在前" """
    if direction == LEFT:
        return [(i, c) for c in range(GRID)]
    if direction == RIGHT:
        return [(i, c) for c in range(GRID - 1, -1, -1)]
    if direction == UP:
        return [(r, i) for r in range(GRID)]
    return [(r, i) for r in range(GRID - 1, -1, -1)]


def slide(board, direction):
    """执行一次移动, 返回 (新棋盘, 移动记录, 合并格, 得分, 是否变化)

    移动记录是 (value, 起点, 终点) 三元组, 供动画插值使用。
    """
    new = [[0] * GRID for _ in range(GRID)]
    moves = []
    merged = []
    score = 0
    changed = False

    for i in range(GRID):
        cells = line_cells(direction, i)
        tiles = [(board[r][c], (r, c)) for (r, c) in cells if board[r][c]]
        slot = 0
        k = 0
        while k < len(tiles):
            value, src = tiles[k]
            dst = cells[slot]
            if k + 1 < len(tiles) and tiles[k + 1][0] == value:
                value2, src2 = tiles[k + 1]
                new[dst[0]][dst[1]] = value * 2
                score += value * 2
                merged.append(dst)
                moves.append((value, src, dst))
                moves.append((value2, src2, dst))
                changed = True
                k += 2
            else:
                new[dst[0]][dst[1]] = value
                moves.append((value, src, dst))
                if src != dst:
                    changed = True
                k += 1
            slot += 1

    return new, moves, merged, score, changed


def move(board, direction):
    """纯逻辑版移动: 返回 (新棋盘, 是否变化, 得分)"""
    new, _moves, _merged, score, changed = slide(board, direction)
    return new, changed, score


def can_move(board):
    """还有没有可走的步"""
    for d in (UP, DOWN, LEFT, RIGHT):
        _, changed, _ = move(board, d)
        if changed:
            return True
    return False


def has_won(board):
    return any(2048 in row for row in board)


def load_best():
    try:
        with open(BEST_FILE) as f:
            return int(f.read().strip() or 0)
    except (OSError, ValueError):
        return 0


def save_best(v):
    try:
        with open(BEST_FILE, "w") as f:
            f.write(str(v))
    except OSError:
        pass


# ---------- 渲染 ----------

_tile_cache = {}


def panel_rect():
    """棋盘底板矩形"""
    return pygame.Rect(PAD, HEADER, PANEL_W, PANEL_H)


def cell_xy(r, c):
    """格子左上角坐标(相对窗口)"""
    return (PAD + BOARD_MARGIN + c * (CELL + GAP),
            HEADER + BOARD_MARGIN + r * (CELL + GAP))


def tile_surface(fonts, value):
    """把一块瓦片渲染成 CELL x CELL 的表面, 按数值缓存"""
    surf = _tile_cache.get(value)
    if surf is not None:
        return surf

    surf = pygame.Surface((CELL, CELL), pygame.SRCALPHA)
    bg, fg = TILE_COLORS.get(value, FALLBACK_COLOR)
    pygame.draw.rect(surf, bg, surf.get_rect(), border_radius=6)

    text = str(value)
    if len(text) <= 2:
        font = fonts["tile3"]
    elif len(text) <= 4:
        font = fonts["tile4"]
    else:
        font = fonts["tile5"]
    label = font.render(text, True, fg)
    surf.blit(label, ((CELL - label.get_width()) // 2,
                      (CELL - label.get_height()) // 2))
    _tile_cache[value] = surf
    return surf


def blit_tile(scene, fonts, value, x, y, scale=1.0):
    """在浮点坐标处画一块瓦片, scale != 1 时按中心缩放"""
    surf = tile_surface(fonts, value)
    if abs(scale - 1.0) > 0.01:
        side = max(4, int(round(CELL * scale)))
        surf = pygame.transform.smoothscale(surf, (side, side))
    else:
        side = CELL
    scene.blit(surf, (round(x + (CELL - side) / 2), round(y + (CELL - side) / 2)))


def draw_grid(scene, fonts):
    """画底板和空格"""
    pygame.draw.rect(scene, BOARD_COLOR, panel_rect(), border_radius=10)
    for r in range(GRID):
        for c in range(GRID):
            x, y = cell_xy(r, c)
            pygame.draw.rect(scene, EMPTY_COLOR, pygame.Rect(x, y, CELL, CELL),
                             border_radius=6)


TITLE_TOP = 6           # 标题距窗口顶端
BOX_H = 64              # 分数框高度
BOX_W = 108             # 分数框宽度
BOX_GAP = 12            # 两个分数框之间


def header_layout(fonts):
    """按字体的实际高度算出顶部区域各元素的位置与所需高度

    返回 (header_height, title_y, hint_y)
    """
    title_h = fonts["title"].get_height()
    hint_h = fonts["hint"].get_height()
    title_y = TITLE_TOP
    hint_y = title_y + title_h + 2
    height = max(hint_y + hint_h + 10, BOX_H + 20)
    return height, title_y, hint_y


def draw_header(scene, fonts, score, best):
    _height, title_y, hint_y = header_layout(fonts)

    title = fonts["title"].render("2048", True, TEXT_DARK)
    scene.blit(title, (PAD, title_y))

    hint = fonts["hint"].render("←↑→↓/WASD 移动   U 撤销   R 重开", True, TEXT_DARK)
    scene.blit(hint, (PAD, hint_y))

    # 分数框右对齐, 距窗口右边 PAD
    right = WIDTH - PAD
    for label, value, idx in (("SCORE", score, 0), ("BEST", best, 1)):
        x = right - BOX_W - idx * (BOX_W + BOX_GAP)
        box = pygame.Rect(x, TITLE_TOP, BOX_W, BOX_H)
        pygame.draw.rect(scene, BOARD_COLOR, box, border_radius=6)
        name = fonts["small"].render(label, True, TEXT_LIGHT)
        num = fonts["score"].render(str(value), True, TEXT_LIGHT)
        scene.blit(name, (x + (BOX_W - name.get_width()) // 2, box.y + 5))
        scene.blit(num, (x + (BOX_W - num.get_width()) // 2, box.y + 24))
def draw_overlay(scene, fonts, msg, tip):
    overlay = pygame.Surface((WIDTH, HEIGHT), pygame.SRCALPHA)
    overlay.fill((250, 248, 239, 160))
    scene.blit(overlay, (0, 0))
    big = fonts["msg"].render(msg, True, TEXT_DARK)
    small = fonts["small"].render(tip, True, TEXT_DARK)
    scene.blit(big, ((WIDTH - big.get_width()) // 2, HEIGHT // 2 - 62))
    scene.blit(small, ((WIDTH - small.get_width()) // 2, HEIGHT // 2 + 18))


def draw(scene, fonts, state, now):
    """按当前状态(含动画进度)绘制一帧"""
    scene.fill(BG_COLOR)
    draw_header(scene, fonts, state["score"], state["best"])
    draw_grid(scene, fonts)

    anim = state["anim"]
    if anim is None:
        for r in range(GRID):
            for c in range(GRID):
                value = state["board"][r][c]
                if value:
                    blit_tile(scene, fonts, value, *cell_xy(r, c))
    else:
        elapsed = now - anim["start"]
        if elapsed < SLIDE_MS:
            # 滑动阶段: 每块瓦片带着旧数值从起点插值到终点
            t = elapsed / SLIDE_MS
            t = 1 - (1 - t) ** 2  # ease-out
            for value, (fr, fc), (tr, tc) in anim["moves"]:
                x0, y0 = cell_xy(fr, fc)
                x1, y1 = cell_xy(tr, tc)
                blit_tile(scene, fonts, value, x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)
        else:
            # 弹出阶段: 画落定后的棋盘, 合并格放大回弹, 新生格从小长大
            t = min(1.0, (elapsed - SLIDE_MS) / POP_MS)
            pop = math.sin(math.pi * t)  # 0 -> 1 -> 0 的单个鼓包
            merged = anim["merged"]
            new_cell = anim["spawn"]
            for r in range(GRID):
                for c in range(GRID):
                    value = state["board"][r][c]
                    if not value:
                        continue
                    if (r, c) in merged:
                        scale = 1.0 + 0.16 * pop
                    elif (r, c) == new_cell:
                        scale = 0.25 + 0.75 * (1 - (1 - t) ** 2)
                    else:
                        scale = 1.0
                    blit_tile(scene, fonts, value, *cell_xy(r, c), scale=scale)
    if state["over"]:
        draw_overlay(scene, fonts, "GAME OVER", "按 R 重新开始")
    elif state["won"]:
        draw_overlay(scene, fonts, "达成 2048!", "按 C 继续   R 重开")

    pygame.display.flip()


def load_fonts():
    """按优先级选字体; 本机有 Noto Sans CJK, 可正常显示中文提示"""
    preferred = ["notosanscjksc", "notosanscjkjp", "notosanssc",
                 "wqyzenhei", "dejavusans", "arial"]
    path = None
    for name in preferred:
        path = pygame.font.match_font(name)
        if path:
            break

    def get(size):
        return pygame.font.Font(path, size)

    return {
        "title": get(48),
        "score": get(26),
        "small": get(15),
        "hint": get(15),
        "msg": get(52),
        "tile3": get(44),
        "tile4": get(34),
        "tile5": get(26),
    }


# ---------- 主循环 ----------


def reset(state):
    state["board"] = new_board()
    state["score"] = 0
    state["over"] = False
    state["won"] = False
    state["win_acked"] = False
    state["anim"] = None
    state["history"] = []


def push_history(state):
    state["history"].append(([row[:] for row in state["board"]], state["score"]))
    if len(state["history"]) > HISTORY_MAX:
        state["history"].pop(0)


def undo(state):
    if not state["history"]:
        return
    board, score = state["history"].pop()
    state["board"] = board
    state["score"] = score
    state["over"] = False
    state["anim"] = None


def anim_running(state, now):
    anim = state["anim"]
    if anim is None:
        return False
    if now - anim["start"] >= SLIDE_MS + POP_MS:
        state["anim"] = None
        return False
    return True


def handle_key(key, state, now):
    """按键处理"""
    if key == pygame.K_ESCAPE:
        state["running"] = False
        return
    if key == pygame.K_r:
        reset(state)
        return
    if key == pygame.K_u:
        if not anim_running(state, now):
            undo(state)
        return
    if key == pygame.K_c:
        if state["won"]:
            state["won"] = False
            state["win_acked"] = True
        return

    if key not in KEY_MAP:
        return
    if state["over"] or state["won"] or anim_running(state, now):
        return

    direction = KEY_MAP[key]
    new, moves, merged, gained, changed = slide(state["board"], direction)
    if not changed:
        return

    push_history(state)
    new_cell = spawn(new)
    state["board"] = new
    state["score"] += gained
    if state["score"] > state["best"]:
        state["best"] = state["score"]
        save_best(state["best"])

    state["anim"] = {
        "moves": moves,
        "merged": merged,
        "spawn": new_cell,
        "start": now,
    }

    if has_won(new) and not state["win_acked"]:
        state["won"] = True
    elif not can_move(new):
        state["over"] = True


# 方向键 -> 方向常量, main() 中初始化(依赖 pygame 键常量)
KEY_MAP = {}


def apply_layout(fonts):
    """按字体实际尺寸确定顶部高度与窗口高度; 返回 (width, height)"""
    global HEADER, HEIGHT
    HEADER = header_layout(fonts)[0]
    HEIGHT = HEADER + PANEL_H + PAD
    return WIDTH, HEIGHT


def main():
    pygame.init()
    fonts = load_fonts()
    width, height = apply_layout(fonts)
    scene = pygame.display.set_mode((width, height))
    pygame.display.set_caption("2048")
    clock = pygame.time.Clock()

    KEY_MAP.update({
        pygame.K_UP: UP, pygame.K_w: UP,
        pygame.K_DOWN: DOWN, pygame.K_s: DOWN,
        pygame.K_LEFT: LEFT, pygame.K_a: LEFT,
        pygame.K_RIGHT: RIGHT, pygame.K_d: RIGHT,
    })

    state = {
        "board": [],
        "score": 0,
        "best": load_best(),
        "over": False,
        "won": False,
        "win_acked": False,
        "anim": None,
        "history": [],
        "running": True,
    }
    reset(state)

    while state["running"]:
        now = pygame.time.get_ticks()
        for event in pygame.event.get():
            if event.type == pygame.QUIT:
                state["running"] = False
            elif event.type == pygame.KEYDOWN:
                handle_key(event.key, state, now)

        draw(scene, fonts, state, now)
        clock.tick(FPS)

    pygame.quit()


if __name__ == "__main__":
    main()
