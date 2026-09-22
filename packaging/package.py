#!/usr/bin/env python3
"""把 release 二进制打成各平台可直接分发的发布包。

CI（`.github/workflows/release.yml`）和本机用的是**同一个脚本**，理由很实际：
「本地能出的包」和「CI 出的包」必须是同一种东西，否则发布产物就是一份没人验过的
二进制。本地跑一遍 Windows 分支即等价于验证了 CI 那一环。

用法：

    python packaging/package.py --platform windows --arch x86_64 \\
        --binary target/release/ngy-image-viewer.exe --version 0.1.0

三平台的产物形态：

| 平台 | 产物 | 里面装什么 |
| --- | --- | --- |
| Windows | `.zip` | `ngy-image-viewer.exe` + README + LICENSE |
| macOS | `.zip` | `ngy-image-viewer.app`（Info.plist + icns + 二进制）+ README + LICENSE |
| Linux | `.tar.gz` | 二进制 + `.desktop` + 图标 + README + LICENSE |

macOS 的 zip 用 `ditto` 而不是 `zipfile`：只有 ditto 会保留 `.app` 内部的可执行位。
用普通 zip 打出来的包，用户解压后会得到一个「双击没反应」的 app（可执行位丢了），
而这类问题在发布者这一侧几乎不可能发现 —— 因为发布者本机上是好的。
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

# stdout / stderr 强制 UTF-8。
#
# 不设的话，Windows 上 Python 用**控制台的代码页**编码输出。本机是中文代码页，
# 编得了中文，所以本地一直是好的；而 GitHub 的 Windows runner 是 en-US（cp1252），
# 于是 `print("产物：…")` 这一句直接抛 UnicodeEncodeError —— 而那时**包已经打好了**，
# 却因为最后一行日志崩掉而报失败。这个 bug 让三轮 CI 白跑
# （macOS / Linux 默认就是 UTF-8，所以只有 Windows 挂，症状是「构建成功、打包失败」）。
#
# 放在脚本自己这里，而不是指望调用方记得设 PYTHONIOENCODING ——
# 「本地能跑、CI 挂」这类问题应该在脚本内部被消灭掉。
for _stream in (sys.stdout, sys.stderr):
    if hasattr(_stream, "reconfigure"):
        _stream.reconfigure(encoding="utf-8", errors="replace")

ROOT = Path(__file__).resolve().parent.parent
PACKAGING = ROOT / "packaging"
APP = "ngy-image-viewer"


def run(cmd: list, **kwargs) -> None:
    """跑一条外部命令，失败就抛 —— 打包脚本里没有「继续试试看」的余地。"""
    print("  $ " + " ".join(str(part) for part in cmd), flush=True)
    subprocess.run([str(part) for part in cmd], check=True, **kwargs)


def stage_tree(stage: Path, version: str, arch: str, platform: str) -> Path:
    """在 stage 下摆出一个待打包的目录树，返回顶层目录。

    顶层目录名与压缩包同名（去掉扩展名）：用户解压出来就是一个有名字的文件夹，
    而不是一堆散落到当前目录的文件。
    """
    top = stage / f"{APP}-v{version}-{arch}-{platform}"
    if top.exists():
        shutil.rmtree(top)
    top.mkdir(parents=True)
    return top


def add_common(top: Path) -> None:
    """LICENSE 与 README 进每一个包。

    LICENSE 不是可选项：Apache-2.0 第 4 条要求分发二进制时附上许可证副本。
    """
    shutil.copy2(ROOT / "LICENSE", top / "LICENSE")
    shutil.copy2(ROOT / "README.md", top / "README.md")


def pack_windows(top: Path, binary: Path) -> None:
    # exe 里已经嵌了图标（build.rs 写进资源段），不需要额外带图标文件。
    shutil.copy2(binary, top / f"{APP}.exe")


def pack_linux(top: Path, binary: Path) -> None:
    target = top / APP
    shutil.copy2(binary, target)
    target.chmod(0o755)
    shutil.copy2(PACKAGING / "linux" / f"{APP}.desktop", top / f"{APP}.desktop")
    # 图标与 .desktop 的 `Icon=ngy-image-viewer` 对上：装到
    # ~/.local/share/icons/hicolor/256x256/apps/ngy-image-viewer.png 即可显示。
    shutil.copy2(ROOT / "assets" / "icon.png", top / f"{APP}.png")


def make_icns(png: Path, out: Path, work: Path) -> None:
    """用 macOS 自带的 sips + iconutil 把 PNG 转成 .icns。

    图标源图只有 256×256，所以 512 与 1024 这两档是**放大**出来的 —— 在 Retina 的
    Dock 里会略显模糊。要真正清晰，得先有一张 ≥1024 的源图；这里宁可给一张略微
    模糊的图标，也不留一个「图标不显示」的空位。
    """
    iconset = work / "icon.iconset"
    if iconset.exists():
        shutil.rmtree(iconset)
    iconset.mkdir(parents=True)

    with open("/dev/null", "wb") as sink:
        for base in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                pixels = base * scale
                suffix = "@2x" if scale == 2 else ""
                name = f"icon_{base}x{base}{suffix}.png"
                run(
                    ["sips", "-z", pixels, pixels, png, "--out", iconset / name],
                    stdout=sink,
                    stderr=sink,
                )

    run(["iconutil", "-c", "icns", iconset, "-o", out])


def pack_macos(top: Path, binary: Path, version: str, work: Path) -> None:
    app = top / f"{APP}.app"
    contents = app / "Contents"
    macos_dir = contents / "MacOS"
    resources = contents / "Resources"
    macos_dir.mkdir(parents=True)
    resources.mkdir(parents=True)

    executable = macos_dir / APP
    shutil.copy2(binary, executable)
    executable.chmod(0o755)

    template = (PACKAGING / "macos" / "Info.plist.in").read_text(encoding="utf-8")
    (contents / "Info.plist").write_text(
        template.replace("@VERSION@", version), encoding="utf-8"
    )

    make_icns(ROOT / "assets" / "icon.png", resources / "icon.icns", work)

    # ad-hoc 签名（`-` 表示「不指定证书」）。这一步不是走过场：
    # Apple Silicon 上内核拒绝运行**完全没有签名**的可执行文件；
    # 而 bundle 内的可执行文件与 bundle 自身要分别签，顺序是先内后外。
    # 注意它只解决「能不能运行」，不解决 Gatekeeper —— 没有 Developer ID 证书
    # 就无法公证，用户首次打开仍需右键 →「打开」。
    if shutil.which("codesign"):
        run(["codesign", "--force", "--sign", "-", executable])
        run(["codesign", "--force", "--sign", "-", app])
    else:
        print("  注意：找不到 codesign，跳过 ad-hoc 签名（arm64 上可能无法运行）")


def write_zip(top: Path, out: Path) -> None:
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as archive:
        for path in sorted(top.rglob("*")):
            if path.is_file():
                # arcname 用 POSIX 分隔符：Windows 上 rglob 出来的是反斜杠，
                # 直接塞进 zip 会得到一个在 macOS / Linux 上解不开的包。
                archive.write(path, path.relative_to(top.parent).as_posix())


def write_tar_gz(top: Path, out: Path, executables: set) -> None:
    """打 tar.gz。

    权限位**显式写死**，不从文件系统读。原因很具体：`chmod` 在 Windows 上基本无效
    （只能标记只读），`tarfile` 于是把文件记成 666 —— 用户装进 `PATH` 后运行报
    「权限不够」，而打包这一侧看不出任何异常。写死之后这条路径在三个平台上行为
    一致，也就能够在任意一个平台上被验证（这次就是在 Windows 上验的）。
    """
    with tarfile.open(out, "w:gz") as archive:
        for path in sorted(top.rglob("*")):
            arcname = path.relative_to(top.parent).as_posix()
            info = archive.gettarinfo(str(path), arcname=arcname)
            if path.is_dir() or path.name in executables:
                info.mode = 0o755
            else:
                info.mode = 0o644
            if path.is_dir():
                archive.addfile(info)
            else:
                with open(path, "rb") as handle:
                    archive.addfile(info, handle)


def main() -> int:
    parser = argparse.ArgumentParser(description="打发布包")
    parser.add_argument("--platform", required=True, choices=["windows", "macos", "linux"])
    parser.add_argument("--arch", required=True, help="如 x86_64 / arm64")
    parser.add_argument("--binary", required=True, help="release 二进制的路径")
    parser.add_argument("--version", required=True, help="版本号，不带 v 前缀")
    parser.add_argument("--out", default="dist", help="产物输出目录（默认 dist/）")
    parser.add_argument("--stage", default="build/package", help="中转目录")
    args = parser.parse_args()

    binary = Path(args.binary).resolve()
    if not binary.is_file():
        print(f"错误：找不到二进制 {binary}", file=sys.stderr)
        print("提示：先跑 cargo build --release（交叉编译的话带上 --target）", file=sys.stderr)
        return 2

    out_dir = Path(args.out).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    work = Path(args.stage).resolve() / f"{args.platform}-{args.arch}"
    work.mkdir(parents=True, exist_ok=True)

    top = stage_tree(work, args.version, args.arch, args.platform)
    add_common(top)

    if args.platform == "windows":
        pack_windows(top, binary)
        archive = out_dir / f"{top.name}.zip"
        write_zip(top, archive)
    elif args.platform == "macos":
        pack_macos(top, binary, args.version, work)
        archive = out_dir / f"{top.name}.zip"
        # 见模块文档：只有 ditto 能保住 .app 里的可执行位。
        if shutil.which("ditto"):
            run(
                [
                    "ditto",
                    "-c",
                    "-k",
                    "--sequesterRsrc",
                    "--keepParent",
                    top,
                    archive,
                ]
            )
        else:
            print("  注意：找不到 ditto，退回 zipfile（可执行位可能丢失）")
            write_zip(top, archive)
    else:
        pack_linux(top, binary)
        archive = out_dir / f"{top.name}.tar.gz"
        write_tar_gz(top, archive, {APP})

    size_mb = archive.stat().st_size / (1024 * 1024)
    print(f"产物：{archive}（{size_mb:.1f} MiB）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
