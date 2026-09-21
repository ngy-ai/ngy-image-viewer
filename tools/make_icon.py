"""生成应用图标：多尺寸 .ico（供 Windows exe 嵌入）+ png（预览）。

用法：

    python tools/make_icon.py                    # 默认写 ./assets/
    python tools/make_icon.py --assets path/dir

主题是「照片」意象：深底上一轮太阳 + 两座山。形状画在 4 倍尺寸上再降采样，
所以边缘自带抗锯齿，不必手工描边。改图标 = 改下面那几组常量再跑一次。
"""

from __future__ import annotations

import argparse
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

BASE = 256  # 最终基准边长
SUPERSAMPLE = 4
CANVAS = BASE * SUPERSAMPLE  # 绘制用画布

# 底色比 Windows 深色任务栏（约 #1F1F1F）亮一档。用纯黑会和任务栏糊在一起，轮廓就没了。
BG_TOP = (56, 68, 92)
BG_BOTTOM = (30, 40, 58)
# 太阳用暖金色，在深底上辨识度最高。
SUN_TOP = (255, 214, 102)
SUN_BOTTOM = (243, 158, 44)
# 远山偏灰蓝（往后退），近山接近白（往前跳）。两座山必须拉开明度，16px 下才分得出层次。
FAR_MOUNTAIN = (143, 163, 200)
NEAR_MOUNTAIN = (233, 238, 247)

CORNER_RATIO = 0.22
# 下面都是占画布边长的比例。(cx, cy, r) 为圆心与半径；山是三元组 (apex_x, apex_y, base_x)。
# base_y 一律到画布底边（超出圆角矩形的部分被蒙版裁掉）。
SUN = (0.30, 0.30, 0.115)
FAR = (0.44, 0.40, 0.12)
NEAR = (0.72, 0.50, 0.22)

ICO_SIZES = [(256, 256), (128, 128), (64, 64), (48, 48), (32, 32), (16, 16)]


def vertical_gradient(
    size: int, top: tuple[int, int, int], bottom: tuple[int, int, int]
) -> Image.Image:
    """竖直渐变。逐行生成再放大，比逐像素赋值快三个数量级。"""
    column = Image.new("RGB", (1, size))
    pixels = column.load()
    for y in range(size):
        t = y / max(size - 1, 1)
        pixels[0, y] = tuple(round(a + (b - a) * t) for a, b in zip(top, bottom))
    return column.resize((size, size), Image.Resampling.NEAREST)


def rounded_mask(size: int, radius: int) -> Image.Image:
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, size - 1, size - 1), radius=radius, fill=255
    )
    return mask


def mountain_polygon(canvas: int, apex_x: float, apex_y: float, base_x: float):
    """一座山：顶点在上，底边拉到画布底边之外（由圆角蒙版裁齐）。"""
    return [
        (apex_x * canvas, apex_y * canvas),
        ((apex_x - base_x) * canvas, canvas * 1.1),
        ((apex_x + base_x) * canvas, canvas * 1.1),
    ]


def composite_clipped(icon: Image.Image, layer: Image.Image, corner: Image.Image) -> Image.Image:
    """把 layer 裁进圆角矩形后叠到 icon 上。

    不要用 paste(..., corner)：paste 按 mask 逐通道混合，layer 在圆角外 alpha=0，
    会把底下的像素连同 alpha 一起抹掉（实测底色和太阳直接消失）。
    alpha_composite 才是真正的「往上叠」。
    """
    alpha = ImageChops.multiply(layer.getchannel("A"), corner)
    layer = layer.copy()
    layer.putalpha(alpha)
    return Image.alpha_composite(icon, layer)


def render(canvas: int) -> Image.Image:
    icon = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
    corner = rounded_mask(canvas, round(canvas * CORNER_RATIO))

    # 底色渐变
    bg = vertical_gradient(canvas, BG_TOP, BG_BOTTOM).convert("RGBA")
    icon = composite_clipped(icon, bg, corner)

    # 太阳
    cx, cy, r = SUN
    sun_mask = Image.new("L", (canvas, canvas), 0)
    ImageDraw.Draw(sun_mask).ellipse(
        (
            (cx - r) * canvas,
            (cy - r) * canvas,
            (cx + r) * canvas,
            (cy + r) * canvas,
        ),
        fill=255,
    )
    sun = vertical_gradient(canvas, SUN_TOP, SUN_BOTTOM).convert("RGBA")
    sun.putalpha(sun_mask)
    icon = composite_clipped(icon, sun, corner)

    # 两座山：先远后近，近山压在远山前面
    for apex_x, apex_y, base_x, color in (
        (*FAR, FAR_MOUNTAIN),
        (*NEAR, NEAR_MOUNTAIN),
    ):
        layer = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
        ImageDraw.Draw(layer).polygon(
            mountain_polygon(canvas, apex_x, apex_y, base_x), fill=(*color, 255)
        )
        icon = composite_clipped(icon, layer, corner)

    return icon


def build(assets: Path) -> tuple[Path, Path]:
    assets.mkdir(parents=True, exist_ok=True)
    base = render(CANVAS).resize((BASE, BASE), Image.Resampling.LANCZOS)

    png = assets / "icon.png"
    ico = assets / "icon.ico"
    base.save(png)
    # Pillow 会为每个尺寸单独重采样，不要自己先缩好再塞进去。
    base.save(ico, format="ICO", sizes=ICO_SIZES)
    return ico, png


def main() -> None:
    parser = argparse.ArgumentParser(description="生成应用图标")
    parser.add_argument(
        "--assets",
        type=Path,
        default=Path("assets"),
        help="输出目录（默认 ./assets）",
    )
    args = parser.parse_args()

    ico, png = build(args.assets)
    print(f"已写入 {ico}（{len(ICO_SIZES)} 个尺寸）与 {png}")
    print("定稿前跑 scripts/preview_sizes.py 看一眼小尺寸效果。")


if __name__ == "__main__":
    main()
