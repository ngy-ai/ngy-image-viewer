# ngy-image-viewer

极速跨平台桌面图片查看器。设计目标只有一条，但它决定了所有取舍：

> 在文件管理器里双击一张图片，窗口出现的那一刻**就已经是图像**——没有白屏，没有中间确认，没有加载动画。

## 下载（预编译包）

不想自己编译的话，去 [Releases](https://github.com/ngy-ai/ngy-image-viewer/releases) 取对应平台的包：

| 平台 | 文件 |
| --- | --- |
| Windows 10 / 11 | `ngy-image-viewer-v<版本>-x86_64-windows.zip` |
| macOS（Apple Silicon） | `ngy-image-viewer-v<版本>-arm64-macos.zip` |
| macOS（Intel） | `ngy-image-viewer-v<版本>-x86_64-macos.zip` |
| Linux x86_64 | `ngy-image-viewer-v<版本>-x86_64-linux.tar.gz` |

**这些包没有签名，也没有公证。** 首次打开：

- **macOS**：右键 → **打开**（直接双击会被 Gatekeeper 拦下说「无法验证开发者」），
  或者 `xattr -dr com.apple.quarantine /Applications/ngy-image-viewer.app`。
- **Windows**：SmartScreen 提示时点 **更多信息 → 仍要运行**。

包内结构、以及怎么让系统把它们和图片格式关联起来，见下面第三节。

## 平台支持状态

请先读这一节，它决定了下面哪些步骤你已经可以照做。

| 平台 | 构建与运行 | HEIC / HEIF | AVIF | 说明 |
| --- | --- | --- | --- | --- |
| **Windows 10 / 11** | ✅ 已实测 | ✅ | ✅ | 分别依赖系统的「HEIF 图像扩展」与「AV1 图像扩展」（免费，缺失时应用会给出安装指引） |
| **macOS 15+** | ⚠️ 构建已跑通，**运行未实测** | ⛔ 未接入 | ⛔ 未接入 | 需要 macOS 15 或更高（gpui-kit 0.6 的要求）。两个架构都编过并通过测试（x86_64 那个是在 arm64 runner 上交叉编译的，产物本机跑不起来，因此不跑测试）。HEIC/AVIF 需要 ImageIO 绑定，当前没接，会返回一条明确的提示而非静默失败 |
| **Linux** | ⚠️ 构建已跑通，**运行未实测** | ⛔ 未接入 | ⛔ 未接入 | 同上，HEIC/AVIF 需要 libheif / libavif 绑定 |

> **「构建已跑通」说的是什么**：发布包由 CI 在三个**原生** runner 上各自构建
> （`.github/workflows/release.yml`），所以「能不能编过、测试过没过、包打不打得出来」
> 这三件事在 macOS 与 Linux 上是有结论的。
> **没有结论的是「窗口在你的桌面上长什么样」** —— 没有人在这两个系统上真正打开过它。
> gpui 的窗口后端、字体回退、HiDPI、输入法这些只有在真机上才看得出来，遇到问题请提 issue。
>
> 除 HEIC/AVIF 之外的全部功能（14 种栅格格式、JPEG XL、SVG、相机 RAW、缩放平移、
> 文件操作）都是**平台无关**的 Rust 代码，三个系统上跑的是同一份逻辑。
>
> **关于 HEIC/AVIF 的取舍**：这两种格式的图像数据是 HEVC / AV1 编码。自己接需要一个
> 成熟的视频解码器外加一整套 YUV→RGB 色彩换算（数百行 `unsafe` FFI），而三大系统都已
> 内置这两种解码能力。因此 Windows 复用 WIC，macOS / Linux 预留了 ImageIO / libheif 的
> 接入点。**宁可给出一条可操作的说法，也不提交从未被编译器检查过的平台代码** ——
> 这正是本项目「绝不静默失败」这条约定的延伸。

---

## 一、从源码构建

### 共同前提

- **Rust stable 1.90 或更高**（2024 edition 需要 1.85+，gpui-kit 0.6 要求 1.90+）。作者使用 1.97 实测。
  安装：<https://rustup.rs>
- **平台 C 工具链**（用于链接，不是本项目的依赖需要——本项目的图像解码全部是纯 Rust）：
  - Windows：Visual Studio Build Tools 的「使用 C++ 的桌面开发」工作负载（提供 MSVC 链接器）
  - macOS：`xcode-select --install`
  - Linux：`build-essential`（或等价包）＋ 系统图形库的开发包

### Windows（已实测）

```powershell
# 1. 确认工具链是 MSVC（不是 GNU）
rustup show            # 应看到 x86_64-pc-windows-msvc
# 若不是：rustup default stable-x86_64-pc-windows-msvc

# 2. 构建（首次约 5–10 分钟：release 开了 LTO + codegen-units=1，这是为启动速度付的代价）
cargo build --release

# 3. 产物
.\target\release\ngy-image-viewer.exe
```

`.cargo/config.toml` 里为 MSVC 打开了静态链接 CRT（`-C target-feature=+crt-static`）：这样发布版不依赖 VC++ 运行库，双击打开时也少一次 DLL 查找。

### macOS（构建已在 CI 上跑通，运行未实测）

```bash
# 1. 前提：macOS 15 或更高
sw_vers

# 2. Xcode 命令行工具
xcode-select --install

# 3. Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 4. 构建
cargo build --release

# 5. 产物
./target/release/ngy-image-viewer
```

macOS 侧**不需要额外装系统库**：字体走 CoreText、HEIC/AVIF 走 ImageIO，都是系统自带的
（这一条在 CI 上得到验证 —— 那边的 macOS job 一个 `brew install` 都没有做过）。

### Linux（构建已在 CI 上跑通，运行未实测）

```bash
# 1. 系统依赖。少一个 -dev 包就编译失败，而报错点离真正的原因很远
#    （一句 `undefined reference`，或者 pkg-config 的 `Package X was not found`）。
sudo apt-get install -y \
  build-essential cmake pkg-config clang libclang-dev \
  libfontconfig1-dev libfreetype-dev \
  libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libx11-dev libxcb1-dev libvulkan-dev

# 2. Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 3. 构建
cargo build --release

# 4. 产物
./target/release/ngy-image-viewer
```

**`libxkbcommon-x11-dev` 是单独一个包、最容易漏**：`libxkbcommon-dev` 里**没有**
`libxkbcommon-x11.so`，而 gpui 会链接它 —— 少了它，前面全都编得过，只在链接时报
`-lxkbcommon-x11`。

Linux 上有两处**运行期**依赖值得先确认（都来自依赖 crate 的默认特性）：

- **另存为 / 重命名对话框**走 `xdg-desktop-portal`。若对话框不弹出，请确认 `xdg-desktop-portal` 及其后端（`xdg-desktop-portal-gtk` / `-kde`）已安装并正在运行。
- **SVG 里的 `<text>`** 依赖系统字体。应用首次打开 SVG 时会扫描一次系统字体并缓存（实测约 50 ms），因此系统里至少要装有一份基础字体。

### 构建模式的选择

```bash
cargo run --release -- 图片路径    # 推荐
```

**请始终用 `--release`**。这不是「快一点」的问题：debug 构建下解码与光栅化未优化，打开一张 2400 万像素的照片可能要几秒。作为对照，本项目的核心验收指标是 **6000×4000 PNG 在 305 ms 内完成首帧带图呈现**，那个数字来自 release 构建。

（`Cargo.toml` 已经为 GPUI 相关依赖在 debug 下单独开了 `opt-level = 3`，所以 debug 模式界面本身不卡，但**我们自己的解码代码**仍是未优化的。）

---

## 二、打开一张图片

三种方式，效果完全一样：

### 1. 命令行

```powershell
ngy-image-viewer.exe D:\photos\IMG_0001.jpg        # Windows
./ngy-image-viewer ~/Pictures/IMG_0001.jpg         # macOS / Linux
```

- 只取**第一个不以 `-` 开头**的参数作为图片路径；多余参数忽略。
- 不带参数启动会进入空状态，提示你拖入图片。

### 2. 从文件管理器双击

需要先把图片类型关联到本程序——见下面的「三、与系统集成」。

### 3. 直接把文件拖进窗口

空状态下拖入、已有图片时拖入另一张，都会立刻切换过去。一次拖入多个文件时取第一个。

---

## 三、与系统集成

> 各平台的手工步骤都在下面。Windows 版**程序内**另有一个入口：菜单
> **工具 → 设置关联格式…**（见下面的推荐做法）。

### Windows：把 `.jpg` / `.png` 等关联到本程序

**推荐：用程序自己的入口。** 菜单 **工具 → 设置关联格式…** → 勾选要关联的格式 →
**全部关联**。它把这些格式登记到 `HKCU\Software\Classes` 下，指向本程序。

> 已有**别的程序**持有某个扩展名时（本机实测 58 个候选扩展名里有 26 个如此），
> 光登记不足以生效 —— Windows 决定用哪个程序的 `UserChoice` 那一层程序改不动
> （带哈希校验，2024-02 起内核驱动 `UCPD.sys` 还会拦截写入）。
> 面板会把这种情况如实画成「已登记，但系统当前用别的程序打开」，
> 点 **设为默认…** 跳到系统的「默认应用」页，在那里一次性切过来。

**手工做法**（不想用面板时的等价操作）：

1. 把 `ngy-image-viewer.exe` 放到一个**不会移动**的位置，例如 `C:\Program Files\ngy-image-viewer\`。
2. 在资源管理器里右键任意一张图片 → **打开方式** → **选择其他应用**。
3. 点击 **更多应用** → **在这台电脑上查找其他应用**，选中 `ngy-image-viewer.exe`。
4. 勾选 **始终使用此应用打开 .jpg 文件** → **确定**。
5. 对其余格式（`.png`、`.webp`、`.heic` …）重复第 2–4 步。

验证：双击一张图片，窗口应当在 300 ms 左右出现并**直接显示图像**。

若要取消关联：**设置 → 应用 → 默认应用** 里按文件类型改回原程序。

### macOS：包里的 `.app`，拖进「应用程序」就能用

Releases 下载的 `*-macos.zip` 解压出来就是一个 `ngy-image-viewer.app`。拖进「应用程序」后：

1. **首次打开**：右键 → **打开**（直接双击会被 Gatekeeper 拦下，见下面的说明）。
2. **绑定图片格式**：右键任意图片 → **打开方式** → **其他…** → 选中 `ngy-image-viewer.app`
   → 勾选「始终以此方式打开」。

`.app` 里的 `Info.plist` 已经声明了它认得的类型（PNG / JPEG / TIFF / GIF / WebP / SVG /
HEIC / AVIF / BMP / ICO / JPEG 2000 / PSD / 相机 RAW），所以「打开方式」菜单里能直接看到它 ——
没有这份声明，Finder 根本不会把它列为候选。`LSHandlerRank` 写的是 `Alternate`：
**不抢**系统「预览」的默认位置，要设成默认得由你选一次，这与 Windows 版的行为一致。

从源码构建时，`packaging/package.py` 负责组出这个 `.app`（写 `Info.plist`、从
`assets/icon.png` 生成 `.icns`、补一次 ad-hoc 签名）：

```bash
cargo build --release --target aarch64-apple-darwin     # Intel 机器换成 x86_64-apple-darwin
python3 packaging/package.py --platform macos --arch arm64 \
    --binary target/aarch64-apple-darwin/release/ngy-image-viewer --version 0.1.0
```

只想从命令行用、不装 `.app` 也可以：

```bash
./target/release/ngy-image-viewer ~/Pictures/photo.jpg
sudo cp ./target/release/ngy-image-viewer /usr/local/bin/   # 进 PATH 后不必写全路径
```

### Linux：二进制进 `PATH`，再装 `.desktop`

Releases 的 `*-linux.tar.gz` 解压后是几个文件（二进制、`.desktop`、图标、README、LICENSE）：

```bash
tar -xzf ngy-image-viewer-v0.1.0-x86_64-linux.tar.gz
cd ngy-image-viewer-v0.1.0-x86_64-linux

# 1. 二进制进 PATH
install -Dm755 ngy-image-viewer ~/.local/bin/ngy-image-viewer

# 2. desktop 入口与图标
install -Dm644 ngy-image-viewer.desktop ~/.local/share/applications/ngy-image-viewer.desktop
install -Dm644 ngy-image-viewer.png \
    ~/.local/share/icons/hicolor/256x256/apps/ngy-image-viewer.png

# 3. 注册，并设为这些类型的默认查看器
update-desktop-database ~/.local/share/applications
xdg-mime default ngy-image-viewer.desktop image/png image/jpeg image/gif image/webp \
    image/tiff image/bmp image/svg+xml
```

`Exec=` 里的 `%f` 是关键：它让文件管理器把「被双击的那个文件路径」作为参数传进来，
应用因此能瞬间打开它；漏掉的话双击只会开一个空窗口。

包里的 `Exec=ngy-image-viewer` 依赖它已经在 `PATH` 里（也就是第 1 步做的事）。
装在别的位置就得把那一行改成绝对路径，否则文件管理器找不到它。

验证：`xdg-mime query default image/png` 应当输出 `ngy-image-viewer.desktop`。

---

## 四、界面与操作

窗口自下而上分成四个区块，另外有两个浮层：

### 界面有两种形态

- **双击图片打开**（命令行里带了图片路径）：标题栏不画菜单、底部不画状态栏，纵向空间全给图像；
- **先开程序、再选图**（菜单 / 窗口正中按钮 / 拖入）：界面与打开前完全一样，只是画布里多了一张图。
  想要更干净的界面按 `F11` —— 全屏是显式动作，不会在你选完图时自己发生。

```
┌──────────────────────────────────────────────────────────────┐
│ 文件名 [格式]     缩放读数  适应/1:1    旋转 翻转 复制 另存 …  │  工具栏 44px
├────────────────────────────────────────────┬─────────────────┤
│                                            │                 │
│              画布（近黑背景）              │  EXIF 信息面板  │  可折叠
│         透明图显示低对比棋盘格             │    320px        │
│                                            │                 │
│                  ┌────────────┐            │                 │
│                  │  操作提示  │            │                 │
│                  └────────────┘            │                 │
├────────────────────────────────────────────┴─────────────────┤
│ 尺寸 · 格式 · 文件大小 · 缩放比例                加载耗时     │  状态栏 28px
└──────────────────────────────────────────────────────────────┘
```

### 鼠标

| 操作 | 效果 |
| --- | --- |
| 滚轮 | 以**光标位置为锚点**缩放——光标下的那个像素始终钉在原地 |
| 按住左键拖动 | 平移画面（图像比窗口小时可在窗口内自由移动，不会被拖出视野） |
| 双击 | 在「适应窗口」与「1:1 原尺寸」之间切换 |
| 把文件拖进窗口 | 直接打开该图片 |

### 键盘

| 按键 | 效果 |
| --- | --- |
| `F11` / `Esc` | 切换全屏 |
| `+` / `=` | 放大一档 |
| `-` / `_` | 缩小一档 |
| `0` | 适应窗口 |
| `1` | 1:1 原尺寸 |
| `R` | 顺时针旋转 90° |
| `Shift + R` | 逆时针旋转 90° |
| `H` | 水平翻转 |
| `V` | 垂直翻转 |
| `I` | 显示 / 隐藏 EXIF 信息面板 |
| `↑` / `←` | 同目录里的上一个图片（到头后绕回最后一张） |
| `↓` / `→` | 同目录里的下一个图片（到头后绕回第一张） |
| `PageUp` | 多页文档的上一页（单页图片上无反应，菜单里也是灰的） |
| `PageDown` | 多页文档的下一页（同上） |
| `Ctrl + C` | 复制当前显示的图像到剪贴板 |
| `Ctrl + S` | 另存为 |

（macOS 上 `Ctrl` 的位置是 `Cmd`。）

### 工具栏

从左到右：文件名与格式角标 · 缩放读数 · 适应窗口 / 1:1 · 逆时针 / 顺时针旋转 · 水平 / 垂直翻转 · 复制 · 另存为 · 重命名 · 删除 · EXIF 面板开关。

高亮成主色的按钮表示**当前生效的模式**（适应窗口或 1:1）。没有打开图片时按钮整体变淡且不响应点击。

### 关于「旋转」与「另存为」的一个约定

旋转与翻转是**像素级**完成的（不是只改一个显示角度）。因此你在屏幕上看到的方向，与「复制到剪贴板」「另存为」得到的**逐像素一致**——不存在「导出后才发现方向不对」这种情况。

另存为时会**先按当前方向烘好像素再编码**，所以把一张竖拍照片摆正后另存为 PNG，得到的是一张真的摆正了的 PNG。

### 信息面板

默认收起。展开后分四组：

- **文件**：文件名、格式、像素尺寸、文件大小
- **相机与镜头**：相机、镜头、软件
- **曝光参数**：快门、光圈、ISO、焦距
- **时间与方向**：拍摄时间、EXIF 原始方向、已应用的旋转

### 文件操作

| 操作 | 行为 | 失败时会怎样 |
| --- | --- | --- |
| 复制 | 把当前显示的像素放进系统剪贴板 | 浮层提示剪贴板被占用等原因 |
| 另存为 | 弹出系统保存对话框；格式由扩展名决定 | JPEG / BMP / PNM 会自动把透明区域摊平到白底（否则编码器必失败）；不认识或本构建编不了的格式会明确说明 |
| 重命名 | 弹出对话框改文件名，也支持移动到别的目录 | 目标已存在时**不覆盖**，明确提示 |
| 删除 | **移入回收站**（可恢复） | 删除目录会被拒绝；回收站不可用时给出可操作提示 |

> 删除默认走回收站是一个刻意的选择：误删一张照片不该是不可恢复的。想彻底删除请去回收站里再清空。

### 操作结果浮层

复制成功、另存路径、删除结果、各种错误都会从画布底部居中浮现，2 秒后自动淡出。**任何失败都不会只在日志里存在**——界面上一定看得到原因和下一步。

---

## 五、支持的格式

| 分类 | 格式 | 备注 |
| --- | --- | --- |
| 栅格 | PNG、JPEG、GIF、BMP、WebP、TIFF、ICO、PNM、TGA、DDS、HDR、EXR、QOI、Farbfeld | 多帧 GIF / APNG / 动画 WebP 完整播放；**多页 TIFF 可逐页浏览**（打开时先出第 1 页，其余页在翻到时后台解码，`PageUp` / `PageDown` 或「视图」菜单翻页）；DDS 见下方说明 |
| 矢量 | SVG、SVGZ（gzip 压缩的 SVG） | 光栅化到声明尺寸的 8 倍（上限 1600 长边），放大时依然清晰 |
| 现代格式 | JPEG XL（`.jxl`，两种封装都支持） | 单帧 |
| 系统原生 | HEIC / HEIF | **仅 Windows**；需系统「HEIF 图像扩展」 |
| 系统原生 | AVIF | **仅 Windows**；需系统「AV1 图像扩展」 |
| 相机 RAW | CR2、NEF、NRW、ARW、SR2、ORF、RAF、RW2、DNG、PEF、SRW、ERF、MEF… | 自实现双线性去马赛克 + 白平衡 + sRGB 色调映射，按行并行 |

### DDS 的特殊说明

DDS 由专用解码器处理（不再走 `image` crate 的栅格路径，早年该路径会把未压缩 DDS 误报成「格式不支持」）：

- **未压缩 RGBA**（A8R8G8B8 等）：跨平台纯 Rust 实现，按位掩码提取各通道并翻转 DDS 自底向上的行序。
- **DXT1 / DXT3 / DXT5（含 DX10 头的 BC1–BC3）**：跨平台，交给 `image` crate 的 DDS 解码器。
- **BC4–BC7**：**仅 Windows**，由系统 WIC 覆盖（与 HEIC/AVIF 同一条后端）。非 Windows 构建打开这类 DDS 会得到一条明确提示，而不是静默失败。

换句话说：**未压缩与 BC1–BC3 在所有平台都能打开；BC4–BC7 只在 Windows 上能打开。**

### 格式判定的原则

**以文件内容（magic bytes）为准，不看扩展名。** 这解决的是最常见的「打不开」原因——下载后被改名、导出工具写错了后缀。当内容与扩展名不一致时，应用会按内容解码，并在提示里说明这一点。

只有两类格式必须靠扩展名消歧：

- **TGA** 的头部没有可靠特征码；
- **相机 RAW**（NEF / ARW / DNG 等）与普通 TIFF 共用同一个文件头。

### 明确不支持的

- **Canon CR3**：它用的是 ISOBMFF 容器，`rawloader` 不支持。应用会直接说明这一点并建议转成 DNG，而不是抛一句「找不到解码器」。
- **动图的 AVIF / HEIC / JPEG XL**：只取第一帧。

---

## 六、性能与诊断

### 实测数据（Windows，release 构建）

| 场景 | 首帧带图 | 其中解码 | 说明 |
| --- | --- | --- | --- |
| 6000×4000 PNG（2400 万像素） | **305 ms** | 59 ms | 解码完全被 GPUI 的 249 ms 平台初始化掩盖 |
| 800×600 半透明 PNG | 280 ms | 1 ms | 走棋盘格路径 |
| 64×32 SVG | 276 ms | 52 ms | 含首次系统字体扫描 |

关键结论：**打开速度的瓶颈是 GPUI 的固定启动开销，不是解码。** 应用在进程启动的第一毫秒就并行开启后台解码，因此感知等待接近「平台初始化」与「解码」两者中的较大值，而不是两段之和。对全尺寸照片来说，解码那一侧是隐藏的。

### 环境变量

| 变量 | 作用 |
| --- | --- |
| `NGY_PERF=1` | 打开启动打点。记录各阶段耗时到 stderr 与日志文件 |
| `NGY_PERF_LOG=<路径>` | 指定日志文件位置（默认在系统临时目录） |
| `NGY_BENCH_MS=<毫秒>` | 基准采集模式：到时间自动退出，便于脚本化测量 |
| `NGY_DIAG_FONTS=1` | 打印系统字体枚举耗时（排查启动问题用） |
| `NGY_TRACE=1` | 打开「打开图片全链路」调试日志。逐步打印格式判定、解码器、各层尺寸与绘制决策；失败信息无条件输出（debug 构建默认开，release 默认关） |

Windows 上发布版没有控制台，测量时请指定日志文件：

```powershell
$env:NGY_PERF='1'; $env:NGY_BENCH_MS='5000'
$env:NGY_PERF_LOG="$PWD\target\bench\perf.log"
.\target\release\ngy-image-viewer.exe 图片路径
Get-Content target\bench\perf.log
```

打点默认**完全关闭**：零 I/O、零输出。打开性能开关本身不能成为启动的负担。

排查「界面全黑、控制台却没有任何报错」时打开 `NGY_TRACE=1`，重点看两处：
`画布实测尺寸=W×H`（高度为 0 即画布塌陷）与 `[fail:…]` 行。

---

## 七、已知限制

1. **文件关联注册与安装包不在本版本范围内**（见第三节的手工步骤）。
2. **单实例转发未实现**：双击两张图片会开两个窗口。
3. **文件夹导航与相邻图片预加载未实现**：一次只看一张。
4. **macOS / Linux 的 HEIC 与 AVIF 未接入**（第三节的表格与应用内的提示都会说明这一点）。
5. **macOS / Linux 构建未实测**——见开头「平台支持状态」。
6. **界面没有做过交互式的逐项验证**。启动路径是靠打点确认的（首帧带图、无 panic），解码与视图变换是靠 259 项测试确认的，**多页翻页另有一遍真机闭环**（键盘 `PageUp` / `PageDown`、「视图」菜单点击、补页期间只铺底色不画别的页、末页不越界、每页按自身尺寸建纹理，脚本见 `.workbuddy/scripts/verify_multipage.py`）；但「拖动进度、滚轮手感、按钮悬停、文件拖入、菜单展开与窗口按钮」这类需要真实鼠标键盘的操作，只做过编译期与逻辑层的验证，没有在窗口里逐一点过。如果你发现某处手感不对，那大概率是这些地方之一。
7. **没有像素级的视觉回归测试**：配色与排版按设计规范实现，没有截图对比。

---

## 八、开发

```bash
cargo test                 # 全部测试
cargo test --lib render    # 只跑某一层
cargo clippy --all-targets
```

测试共 168 项，分三类，覆盖的重点都不是「能不能跑」而是**几条产品级约定**：

- **`tests/decode_test.rs`**：格式判定以内容为准、损坏文件不崩、超大图在分配前就被拒绝、动图每一帧与延迟都不丢、缺失解码器时给出可操作提示。
- **`tests/transform_test.rs`**：缩放时**光标下的像素必须钉在原地**（连续缩放也不能漂移）、1:1 在高分屏上的含义、平移边界、以及方向的代数复合与像素级实现 8×8 全组合对齐。
- **各模块内的单元测试**：纹理构建（BGRA 顺序、方向烘入、超限降采样）、去马赛克（颜色索引、裁剪后的绝对坐标）、文件操作（扩展名推断、alpha 摊平、错误粒度）、菜单与快捷键表（菜单显示的快捷键必须真的能触发那个动作）。

### 项目结构

```
src/
├── decode/      解码层：14 种栅格（raster 13 + 专用 DDS）+ JXL + SVG + 平台 HEIC/AVIF + RAW。零 UI 依赖
├── model/       领域层：图像文档与视图变换纯数学。零 UI 依赖
├── render/      渲染适配层：像素 → GPU 纹理，canvas 自绘
├── ui/          界面层：主视图、工具栏、状态栏、信息面板、格式与配色
├── input/       输入逻辑：拖拽手势状态机、滚轮 → 缩放倍率换算
├── fs_ops/      文件能力：剪贴板、另存为、重命名、回收站
├── open_job.rs  后台打开任务（与平台初始化并行）
└── perf.rs      启动打点（默认关闭）
```

**最重要的一条架构边界**：`decode/` 与 `model/` **不引用任何 UI 类型**。因此「滚轮缩放能否钉住光标下的像素」「EXIF 方向与用户旋转如何叠加」这类最容易写错、又最难用肉眼验证的逻辑，可以在没有窗口的情况下被精确断言。它同时保证了：万一将来要换渲染框架，核心逻辑一行都不用改。

### 一条实现上的说明

GPUI 的 `Window::paint_image` **只接受轴对齐矩形**，整个 crate 没有公开的绘制变换入口（这是核实过源码的结论，不是推测）。因此旋转与翻转在**像素层**完成：代价是一次整图拷贝，换来的是「屏幕上看到的」与「另存为导出的」逐像素一致——不存在方向对不上的隐患。同一份像素级方向实现（`decode/orientation.rs`）同时服务 EXIF 摆正、用户旋转与另存为三条路径，并由单元测试保证它们一致。

### 应用图标

图标（深蓝圆角底 + 太阳与双山的「照片」意象）**在构建时嵌进 exe 的 PE 资源段**——
GPUI 的 `WindowOptions::icon` 只对 X11 生效，Windows 的窗口 / 任务栏 / 资源管理器
图标只能来自 exe 资源段，代码里设不了。

- 图标源码：`tools/make_icon.py`（Pillow 生成多尺寸 `assets/icon.ico`，4 倍超采样
  降下来带抗锯齿）。**改图标唯一入口是这个脚本**，别直接改 `icon.ico` 这个孤儿二进制。
- 嵌入：`build.rs`（`winresource`），只在 Windows 生效；找不到 `rc.exe` 或图标缺失
  只发 `cargo:warning`，不影响其他平台构建。同时写入 VERSIONINFO（名称、描述、版本
  都取自 `Cargo.toml`）。
- `assets/icon.ico` 需要进版本库（构建依赖它）；`target/` 已忽略。
- 验证：`python tools/verify_embedded.py target/release/ngy-image-viewer.exe --ico assets/icon.ico --string "极速跨平台图片查看器"`——直接解析 exe 字节流，确认 6 个尺寸与 VERSIONINFO 真的进了资源段。**编译通过不等于图标嵌进去了。**
- 改完图标的预览：`python tools/preview_sizes.py assets/icon.ico target/icon_preview.png`——每个尺寸放大 + 实际大小并排，小尺寸可辨性定稿前必看。

### 发布

打一个 `v*` tag 即触发 `.github/workflows/release.yml`：三个**原生** runner
（Windows / macOS / Linux）各自构建、打包，产物挂到同一个 Release 上。

```bash
git tag v0.1.0
git push origin v0.1.0
```

几个刻意的设计：

- **只在原生 runner 上编，不交叉编译。** macOS 包只能在 macOS 上编（Apple 的工具链
  不外授权到别的宿主）；而在 Windows 上凑 Linux 交叉链还得额外拼 C 工具链（JPEG 2000
  走 openjpeg 的 C 后端），那是条没人验证过的路。
- **macOS 出两个包**（arm64 与 x86_64）。x86_64 那个是在 arm64 runner 上交叉编译的，
  因此**不跑测试** —— 产物本机跑不起来，「跑不了所以不测」比「跳过测试装作测过了」诚实。
- **tag 与 `Cargo.toml` 的版本号必须一致**，否则 CI 直接失败。不校验的话会产出
  「文件名写着 A、exe 资源段里写着 B」的包，而这种错只有用户来问的时候才会发现。
- **手动触发（Actions 页面的 `workflow_dispatch`）只构建、留 artifact，不创建 Release。**
  第一次验证某个平台能不能编过时用它，不会污染 Releases。
- 包由 `packaging/package.py` 打出，**CI 与本地是同一个脚本** —— 这样「本地能出包」
  才是有意义的说法：本地跑一遍 Windows 分支即等价于验证了 CI 那一环。

```bash
# 本地出 Windows 包
cargo build --release --target x86_64-pc-windows-msvc
python packaging/package.py --platform windows --arch x86_64 \
    --binary target/x86_64-pc-windows-msvc/release/ngy-image-viewer.exe --version 0.1.0
```

---

## 九、许可证

Apache-2.0，全文见 [`LICENSE`](LICENSE)。发布包里也带了一份 —— Apache-2.0 第 4 条
要求分发二进制时附上许可证副本。

第三方依赖的许可证与选择理由都写在 `Cargo.toml` 的注释里——特别是为什么关掉 `image` 的默认特性、为什么 `rav1d` 要被排除（AGPL 与 `zenavif`）、为什么 SVG 只依赖 `resvg` 而不单独依赖 `usvg`。
