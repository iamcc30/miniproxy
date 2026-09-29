#!/usr/bin/env python3
"""生成 MiniProxy 的 App 图标（packaging/AppIcon.png）。

纯 Pillow 绘制，无外部素材：macOS 圆角方形（超椭圆）+ 蓝色渐变底 + 圆头 "M" 折线。
修改配色/几何后重跑本脚本，再执行 packaging/package-macos.sh 即可更新图标。

用法：python3 packaging/make-icon.py
"""
import math
import os
import sys

from PIL import Image, ImageDraw, ImageFilter

SIZE = 1024        # 最终边长
SS = 4             # 超采样倍数（先画 SS 倍大再缩回来，拿抗锯齿）
W = SIZE * SS

INSET = 92         # 四周留白（macOS 图标规范：图形不铺满画布）
SQ_N = 5.0         # 超椭圆指数，Apple squircle ≈ 5
RADIUS = None

BG_A = (59, 130, 246)     # 左上 #3B82F6
BG_B = (9, 30, 80)        # 右下 #091E50
FG_A = (255, 255, 255)
FG_B = (173, 224, 255)
GLOW = (56, 189, 248)     # #38BDF8

# "M" 折线顶点（1024 坐标系）
STROKE = 88
M_POINTS = [(270, 672), (270, 352), (512, 596), (754, 352), (754, 672)]


def squircle_points(size: int, inset: int, steps: int = 4096):
    """超椭圆（squircle）轮廓点，比 rounded_rectangle 更接近 macOS 图标外形。"""
    a = (size - 2 * inset) / 2.0
    c = size / 2.0
    pts = []
    for i in range(steps):
        t = 2 * math.pi * i / steps
        co, si = math.cos(t), math.sin(t)
        x = c + a * math.copysign(abs(co) ** (2.0 / SQ_N), co)
        y = c + a * math.copysign(abs(si) ** (2.0 / SQ_N), si)
        pts.append((x, y))
    return pts


def gradient(size: int, c0, c1, lowres: int = 96) -> Image.Image:
    """对角线性渐变：低分辨率逐像素生成后放大，避免在 4096² 上跑 Python 循环。"""
    small = Image.new("RGB", (lowres, lowres))
    px = small.load()
    for y in range(lowres):
        for x in range(lowres):
            t = (x + y) / (2 * (lowres - 1))
            px[x, y] = tuple(round(c0[i] + (c1[i] - c0[i]) * t) for i in range(3))
    return small.resize((size, size), Image.BICUBIC)


def main() -> int:
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out = os.path.join(root, "packaging", "AppIcon.png")

    # 底色
    img = gradient(W, BG_A, BG_B).convert("RGBA")

    # M 的实心形状（灰度量版），后续既当彩色遮罩也用来做光晕
    shape = Image.new("L", (W, W), 0)
    d = ImageDraw.Draw(shape)
    pts = [(x * SS, y * SS) for x, y in M_POINTS]
    d.line(pts, fill=255, width=STROKE * SS, joint="curve")
    # PIL 的 line 没有圆头端点，在顶点补圆点实现圆角连接
    r = STROKE * SS / 2.0
    for x, y in pts:
        d.ellipse((x - r, y - r, x + r, y + r), fill=255)

    # 光晕：把 M 模糊后涂成青色垫在底下
    glow = shape.filter(ImageFilter.GaussianBlur(STROKE * SS * 0.9))
    glow_layer = Image.new("RGBA", (W, W), GLOW + (0,))
    glow_layer.putalpha(glow.point(lambda v: int(v * 0.55)))
    img = Image.alpha_composite(img, glow_layer)

    # M 本体：白→浅蓝的竖向渐变
    fg = gradient(W, FG_A, FG_B, lowres=8).convert("RGBA")
    fg.putalpha(shape)
    img = Image.alpha_composite(img, fg)

    # 裁成圆角方形
    mask = Image.new("L", (W, W), 0)
    ImageDraw.Draw(mask).polygon(squircle_points(W, INSET * SS), fill=255)
    img.putalpha(mask)

    img.resize((SIZE, SIZE), Image.LANCZOS).save(out)
    print(f"已生成 {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
