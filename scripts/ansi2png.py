#!/usr/bin/env python3
"""把 `tmux capture-pane -e -p` 的 ANSI 文本渲染成 PNG（终端级视觉验证用）。

用法: ansi2png.py <in.ansi> <out.png>
颜色: 解析 SGR（256 色索引/RGB/粗体/反显），宽字符按显示宽度推进光标，
      与真实终端行为一致（capture 不含宽字符占位格）。
"""
import re
import sys
import unicodedata

from PIL import Image, ImageDraw, ImageFont

CW, CH = 11, 24
FONT_SIZE = 19
DEFAULT_FG = (197, 200, 198)
DEFAULT_BG = (30, 32, 40)

# xterm 256 色板（0-15 + 6x6x6 立方 + 灰阶）
def xterm(n: int):
    if n < 16:
        base = [
            (0, 0, 0), (205, 49, 49), (13, 188, 121), (229, 229, 16),
            (36, 114, 200), (188, 63, 188), (17, 168, 205), (229, 229, 229),
            (102, 102, 102), (241, 76, 76), (35, 209, 139), (245, 245, 120),
            (59, 142, 234), (214, 112, 214), (41, 184, 219), (255, 255, 255),
        ]
        return base[n]
    if n < 232:
        n -= 16
        steps = (0, 95, 135, 175, 215, 255)
        return (steps[n // 36], steps[(n // 6) % 6], steps[n % 6])
    g = 8 + (n - 232) * 10
    return (g, g, g)

def load_font(bold: bool):
    files = [
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc", True),
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", False),
    ]
    for path, is_bold in files:
        if bold and not is_bold:
            continue
        if not bold and is_bold:
            continue
        for idx in range(10):
            try:
                f = ImageFont.truetype(path, FONT_SIZE, index=idx)
                name = " ".join(f.getname())
                if "Mono CJK SC" in name:
                    return f
            except Exception:
                break
    # 兜底：任意 CJK 面
    return ImageFont.truetype(files[1][0], FONT_SIZE, index=0)

def char_width(ch: str) -> int:
    if unicodedata.east_asian_width(ch) in ("W", "F"):
        return 2
    return 1

def parse(text: str):
    """返回 rows：每行是 [(char, fg, bg, bold)] 的顺序流。"""
    rows, cur, fg, bg, bold = [], [], DEFAULT_FG, None, False
    i = 0
    token = re.compile(r"\x1b\[([0-9;?]*)([a-zA-Z])|\x1b[()][B0]|\x1b[=>]")
    while i < len(text):
        m = token.match(text, i)
        if m:
            if m.group(1) is not None and m.group(2) == "m":
                params = m.group(1) or "0"
                nums = [int(p) if p else 0 for p in params.split(";")]
                j = 0
                while j < len(nums):
                    n = nums[j]
                    if n == 0:
                        fg, bg, bold = DEFAULT_FG, None, False
                    elif n == 1:
                        bold = True
                    elif n == 7:
                        fg, bg = (bg or DEFAULT_BG), fg
                    elif n == 39:
                        fg = DEFAULT_FG
                    elif n == 49:
                        bg = None
                    elif 30 <= n <= 37:
                        fg = xterm(n - 30)
                    elif 90 <= n <= 97:
                        fg = xterm(n - 90 + 8)
                    elif 40 <= n <= 47:
                        bg = xterm(n - 40)
                    elif 100 <= n <= 107:
                        bg = xterm(n - 100 + 8)
                    elif n in (38, 48) and j + 2 < len(nums) and nums[j + 1] == 5:
                        color = xterm(nums[j + 2])
                        if n == 38:
                            fg = color
                        else:
                            bg = color
                        j += 2
                    elif n in (38, 48) and j + 4 < len(nums) and nums[j + 1] == 2:
                        color = (nums[j + 2], nums[j + 3], nums[j + 4])
                        if n == 38:
                            fg = color
                        else:
                            bg = color
                        j += 4
                    j += 1
            i = m.end()
            continue
        ch = text[i]
        if ch == "\x1b":
            i += 1
            continue
        if ch == "\n":
            rows.append(cur)
            cur = []
        elif ch == "\r":
            pass
        else:
            cur.append((ch, fg, bg, bold))
        i += 1
    if cur:
        rows.append(cur)
    return rows

def render(text: str, out_path: str):
    rows = parse(text)
    cols = max((sum(char_width(c) for c, *_ in row) for row in rows), default=1)
    img = Image.new("RGB", (cols * CW + 2, len(rows) * CH + 2), DEFAULT_BG)
    draw = ImageDraw.Draw(img)
    fonts = {}
    for y, row in enumerate(rows):
        col = 0
        for ch, fg, bg, bold in row:
            w = char_width(ch)
            if bg is not None:
                draw.rectangle([col * CW, y * CH, (col + w) * CW - 1, (y + 1) * CH - 1], fill=bg)
            if ch != " ":
                key = bold
                if key not in fonts:
                    fonts[key] = load_font(bold)
                draw.text((col * CW + 1, y * CH + 2), ch, font=fonts[key], fill=fg)
            col += w
    img.save(out_path)
    print(f"{out_path}: {img.size[0]}x{img.size[1]}, {len(rows)} 行")

if __name__ == "__main__":
    render(open(sys.argv[1], encoding="utf-8").read(), sys.argv[2])
