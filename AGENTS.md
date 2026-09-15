# AGENTS.md

给在本仓库里工作的 AI 编码助手（以及第一次接手的人）。

**产品目标只有一条，它决定了所有取舍**：在文件管理器里双击一张图片，窗口出现的那一刻就已经是图像 —— 没有白屏、没有加载动画、没有中间确认步骤。

性能上的硬指标（Windows release 实测）：**6000×4000 PNG 首帧带图 305 ms**，其中解码 59 ms 完全被 GPUI 的 249 ms 平台初始化掩盖。

---

## 1. 常用命令

```bash
cargo test                          # 全部 168 项测试
cargo test --lib render             # 只跑某一层
cargo test --lib fs_ops             # 子模块
cargo check --all-targets           # 必须零警告
cargo clippy --all-targets          # 若已安装 clippy
cargo run --release -- 图片路径      # 日常运行
cargo build --release               # 首次 5–10 分钟，见下方「构建很慢」
```

### 性能测量（改动了启动路径或解码路径时必做）

```powershell
# Windows（发布版无控制台，必须写日志文件）
$env:NGY_PERF='1'; $env:NGY_BENCH_MS='5000'
$env:NGY_PERF_LOG="$PWD\target\bench\perf.log"
.\target\release\ngy-image-viewer.exe 图片路径
Get-Content target\bench\perf.log
```

```bash
# macOS / Linux
NGY_PERF=1 NGY_BENCH_MS=5000 ./target/release/ngy-image-viewer 图片路径
```

看 `first_frame_with_image` 这个打点。它应当与首帧渲染**同一时刻**触发；如果它消失了、或者晚了很多，说明你把某件事挪到了关键路径上。

---

## 2. 分层与边界（最重要的一节）

```
main.rs  →  app.rs  →  ui/  →  render/  →  model/  →  decode/
                        │                                ↑
                        └──────── input/  fs_ops/ ───────┘
```

**硬性规则：`decode/` 与 `model/` 不得引用任何 UI 类型。**

这条边界不是为了好看，它有两个具体作用：

1. 「滚轮缩放能否钉住光标下的像素」「EXIF 方向与用户旋转如何叠加」这类**最容易写错又最难用肉眼验证**的逻辑，可以在没有窗口的情况下被精确断言；
2. 万一将来要换渲染框架，核心逻辑一行都不用改。

各层职责与「不知道什么」：

| 目录 | 职责 | 明确不知道 |
| --- | --- | --- |
| `decode/` | `Path` → 像素 + 元数据 | 窗口、纹理、帧率、线程调度 |
| `model/` | 文档事实与视图变换**纯数学** | 窗口、像素布局、GPU |
| `render/` | 像素 → GPU 纹理 → canvas 自绘 | 交互、文件系统 |
| `ui/` | 界面与交互状态：标题栏 + 菜单栏、工具栏、状态栏、EXIF 面板 | 像素格式、通道顺序 |
| `input/` | 手势状态机、滚轮换算（纯逻辑） | 元素与事件回调的接线（那在 `ui/view.rs`） |
| `fs_ops/` | 剪贴板 / 打开 / 另存为 / 重命名 / 回收站 / **用户偏好读写** | 界面 |
| `ui/command.rs` | 动作清单、菜单结构、快捷键表（**零 UI 依赖**） | gpui、窗口、元素 |
| `ui/theme.rs` | 两套皮肤的**颜色取值** + 尺寸/时长常量 | 窗口、平台、状态 |

`render/` 是整个项目里**唯一**同时知道「图像数据长什么样」和「GPUI 怎么画」的地方。像素格式、通道顺序、纹理上限、坐标系换算都关在这一层。

---

## 3. 皮肤（深色 / 浅色）

### 两套皮肤是「两列颜色」，不是「两套代码」

`ui/theme.rs` 用一个 `Skin` 结构体承载两套取值（模块级 `static DARK` / `LIGHT`，
`LazyLock` 构造 —— `rgb()` 与 `Rgba → Hsla` 都不是 const 函数）。**布局、尺寸、动效两套完全相同**，
`Skin` 里只放颜色。新增一个颜色时两套都必须填上：`Skin` 的字段没有默认值，漏一个就编译不过。
不要在各个绘制函数里写 `if dark {..} else {..}` —— 那会让「漏了浅色那一支」变成
只在某个界面上才看得见、且是白底白字这种最难自查的形态。

### 「跟随系统」靠每帧读平台值，不靠自持布尔

- 视图持有的是**用户偏好**（`Preference::{System, Fixed(Polarity)}`），**不是**「当前是哪套皮肤」。
- 当前皮肤由 `ImageViewerView::current_skin(window.appearance())` 每帧现算。
- `window.appearance()` 是平台值：Windows 走 `ImmersiveColorSet` 消息 →
  `gpui-pre-windows` 的 `handle_system_theme_changed` → `appearance_changed` → 窗口重绘。
  因此**不需要** `observe_window_appearance` 订阅，也不需要任何状态副本。
- 自己缓存一个「现在是深色」的布尔就会重演 `fullscreen` 那个缺陷（见下方 GPUI 事实清单）：
  自持的那份先于平台生效，随后打架，界面停在旧配色上直到用户碰一下鼠标。

### 手动覆盖与持久化

- 菜单「视图」里有三项互斥项：跟随系统 / 深色 / 浅色（`Command::SkinFollowSystem` /
  `SkinDark` / `SkinLight`），当前生效的那个用主色 + `●` 标出。
- 选择写在 `%APPDATA%/ngy-image-viewer/config`（macOS / Linux 见 `fs_ops/settings.rs`），
  极简 `key=value`。**删掉文件即回到跟随系统。**
- **读只有一次，且在建窗之前**（`app.rs` 的 `run`）：`App::window_appearance()` 在之后返回的
  就是覆盖值，窗口一出生就是对的。视图的 `preference` 字段默认 `System`，
  由 `set_loaded_preference` 覆盖 —— 别在 `new()` 里再读一次盘。
- **写永远在后台线程**（`view.rs::set_preference` 里 `std::thread::spawn`）：
  一次 `write` 是几毫秒，放渲染循环里就是几毫秒的卡顿。
- 手动覆盖时还调用 `cx.set_window_appearance(...)` —— 只有 **macOS** 真的实现了它
  （`App::set_window_appearance`，`gpui-pre-0.3.4/src/app.rs:1403`），Windows 是空实现。
  自绘配色不依赖它，调用只是为了平台侧那点外围（标题栏按钮、边框）对得上。

`App::set_window_appearance` 的语义是**清除覆盖**用 `None`，不是「设成跟随系统的值」。

---

## 3. 解码层的硬性约定

### 格式判定以内容为准

`decode/sniff.rs` 用 magic bytes 判定格式，**不看扩展名**。这解决的是最常见的「打不开」原因（下载改名、导出工具写错后缀）。扩展名只承担三件事：

1. **兜底**：TGA 头部没有可靠特征码；
2. **消歧**：NEF / ARW / DNG 等相机原片与普通 TIFF 共用文件头；
3. **告警**：内容与扩展名不一致时告诉用户「已按内容解码」。

新增格式时：实现 `Decoder` trait，在 `DecoderRegistry::new` 里注册。**注册顺序 = 兜底尝试顺序**，调用方一行都不用改。

### 像素的统一口径

- 全部解码器产出 **非预乘 RGBA8**、逐行紧密排列（`width * 4`，无 padding）。
- 预乘只在 `render/surface.rs` 上传纹理前处理一次（GPUI 要 BGRA 且按 BGRA 解释）。
- **`Frame::width/height` 是纹理像素尺寸；`ImageData::width()/height()` 是逻辑尺寸。** 两者由 `ImageData::supersample` 联系：`逻辑 = 像素 ÷ supersample`。SVG 的 supersample > 1（光栅化到 8 倍）；超大位图被降采样时 supersample < 1。

### 方向不在解码层应用

`ImageData::orientation` 只是**记录** EXIF 方向，像素保持原样。真正的应用发生在：

```
document.orientation() = exif.then(user)     // 先 EXIF 摆正，再用户旋转
    ↓
render/surface.rs 用 decode/orientation.rs 做像素级重排并烘进纹理
```

- `Orientation::then(a, b)` 的语义是「先 a 再 b」。顺序写反会在「旋转过的竖拍照片」上立刻露馅。
- `Orientation` 内部用二面体群 D4 的 `R^k ∘ Fh^m` 表示做复合，**不要**为 8×8 组合手写查找表。
- 有一条测试把代数复合与像素级实现逐一对齐（8×8 全组合），改动方向逻辑后它会保护你。

### 各解码器的坑（都是踩过的）

| 解码器 | 坑 |
| --- | --- |
| `raster.rs` | PNG/WebP 的动画要走 `apng()` / `has_animation()` 分支，静态路径会丢掉多帧 |
| `jxl.rs` | `Render::stream()` **已经应用过方向**，所以必须报 `Orientation::Normal`，否则渲染层会再转一次；`write_to_buffer` 每次只写一部分，必须循环到写满 |
| `svg.rs` | `tiny_skia` 的像素是**预乘**的，必须反预乘；`Pixmap::new` 返回 `Option` 而不是 `Result`；`resvg::render` 的第二个参数按**值**传 `Transform`；字体库用进程级 `OnceLock` 缓存（扫描系统字体约 50 ms） |
| `raw.rs` | rawloader **不做裁剪**，`crops = [top, right, bottom, left]` 要自己应用；`SensorView` 用整幅传感器上的**绝对坐标**判断 CFA 颜色，所以必须传**原始** `cfa`，**不是** `cropped_cfa()`（那会把位移应用两次） |
| `demosaic.rs` | 颜色索引 3 是某些传感器的「额外通道」，不参与可见光三通道 |
| `wic.rs` | 见下方 GPUI/WIC 事实清单 |

### 失败必须可操作

所有错误类型都遵循同一形态：

```rust
pub fn user_message(&self) -> String;   // 面向用户：说清发生了什么 + 下一步能做什么
pub fn short_reason(&self) -> String;   // 面向日志：短、单行、不引导用户
impl Display { /* 走 short_reason */ }
```

**绝不静默失败。** 任何失败都要在界面上看得到原因和下一步。分类的依据是「用户的下一步该做什么」，不同动作必须分成不同变体 —— 例如「不认识扩展名」与「认识但本构建编不了」是两个变体，「系统缺解码组件」与「文件损坏」也是。

---

## 4. GPUI 事实清单（核实过源码，别凭记忆写）

**本仓库的 `gpui` 不是 `gpui-0.2.2`。** `gpui-kit 0.6.1` 依赖的是 `gpui-pre 0.3.4`（`Cargo.toml` 里 package 重命名为 `gpui`）。查文档时注意这一点。

写任何 GPUI 代码之前，请**先去 cargo registry 源码里确认签名**。以下几条是已经确认过的、最容易写错的：

| 事实 | 后果 |
| --- | --- |
| `Window::paint_image(bounds, image_bounds, corner_radii, data, frame_index, grayscale)` **只接受轴对齐矩形**，整个 crate 没有公开的仿射变换入口 | **旋转与翻转必须在像素层完成**（`decode/orientation.rs`）。这换来一个好处：屏幕所见与「另存为」导出逐像素一致 |
| `RenderImage` 的缓冲区是 `image::Frame`（RGBA 布局）但 GPU 按 **BGRA** 解释，非预乘 | 上传前必须交换红蓝通道，否则人脸变蓝 |
| `paint_image` 会直接索引帧数组 | `frame_index` 必须先 `min(frame_count - 1)` 夹一次，否则换图时整界面 panic |
| `image_bounds` 是「整张图在窗口里的矩形」，GPUI 用 `bounds ∩ image_bounds` 反算纹理子区域 | 画整张图时两个参数传**同一个**矩形；传「已裁剪过」的矩形会让纹理坐标映射出错 |
| `Pixels` 的字段是私有的 | 用 `f32::from(pixels)`，不能 `.0` |
| `canvas(prepaint, paint)` 的 `prepaint` 是唯一能拿到画布真实尺寸的地方，且它是 `'static` 闭包 | 尺寸要靠共享槽位（`render/viewport.rs` 的 `ViewportSlot`）回传给视图 |
| `cx.observe_window_bounds(&self, window, callback)` 需要 `&mut Window` | **没法在 `new(cx)` 里订阅**。窗口缩放已经由画布的 `prepaint` 覆盖，不需要额外订阅 |
| 窗口第一层必须是 `Root` | `app.rs` 里 `cx.new(|cx| Root::new(view, window, cx))` |
| `MouseDownEvent.click_count` 已经处理了双击间隔与位移容差 | 双击判定用它，**不要**自己数时间 |
| 滚轮 `ScrollDelta` 的正值 = 向下滚 | 正值 = 缩小，负值 = 放大。改方向只改 `input/handlers.rs` 一处，并同步改测试 |
| `on_drop::<T>` 配合平台的 `FileDropEvent` 转换，系统拖入的文件以 `ExternalPaths` 为载荷 | 写法是 `.on_drop(cx.listener(\|this, paths: &ExternalPaths, _window, cx\| ...))` |
| WIC：`IWICFormatConverter::Initialize` 有 7 个参数；`IWICBitmapSource::CopyPixels(&self, prc, cbstride, pbbuffer: &mut [u8])` | 见 `decode/wic.rs`，那里有完整的 SAFETY 注释 |
| WIC：`CopyPixels` 的目标格式用 `GUID_WICPixelFormat32bppBGRA` | 所有 WIC 编解码器都必须支持它；`32bppRGBA` 不一定 |
| `flex_1()` **只在 flex 容器里生效** | 排版树的中间层若忘了 `.flex()`，子元素的 `flex_1()` 会静默失效、高度塌成 0。画布是绝对定位的，不占空间，于是整块画布变成「宽 × 0」——而 `paint_image` 在可见区域为空时返回 `Ok(())`，**不报任何错**，表现为「界面全黑、控制台寂静」。改 `ui/view.rs` 的窗口骨架时，每一层都要问一句「这层有没有声明 flex 容器」 |
| `paint_image` 在可见区域为空时返回 `Ok(())` | 不能用返回值判断「有没有画出来」。需要判定时自己算 `bounds.intersect(&image_bounds)`（见 `render/viewport.rs`） |
| `Window::is_fullscreen()` 是只读查询；切换只有 `toggle_fullscreen()`，没有 set 版本 | 界面是否隐藏必须**每帧读平台值**，不要在切换的那一刻自己翻一个布尔：Windows 的 `toggle_fullscreen` 走 `executor.spawn` 异步投递（`gpui-pre-windows`），自持的那份会先于平台生效，随后两者打架 —— 界面「先藏起来再亮回来」 |
| `gpui_kit::*` 在 `test-support` 下含 GPUI 自己的 `test` 宏 | 测试模块里写 `use super::*` 会遮蔽内置 `#[test]`，报的是 `recursion limit reached while expanding \`#[test]\``，看不出根因（`gpui-kit` 的 lib.rs 里对此有明确说明）。测试模块按需显式导入，别 glob |
| `window.request_animation_frame()` 只请求**下一帧** | 任何「等平台状态翻面再重绘」的逻辑（如全屏切换）都必须自己跨若干帧重复请求，只请求一次会停在旧样子上，直到用户碰一下鼠标才变 |
| 绘制闭包能拿到 `window.scale_factor()` 与画布真实尺寸，`new()` / `accept()` 两样都拿不到 | 「打开图片时的初始缩放」这类要同时看两者才能定的状态，让绘制层**就地求值**（`ViewTransform::fitted` / `initial`）就够，不必等视图下一帧 —— 但两处必须调用**同一个纯函数**，各写一份的结果是第一帧与落定后的画面不一致，表现为一次莫名其妙的跳变。`ZoomMode::Pending` 就是为这段空档存在的：它不是缩放模式，而是「还没定」 |
| 「打开图片后收起界面」只对**启动就带图**成立 | 判据是 `ui/view.rs` 的 `compact_form(has_image, immersive)`：`immersive` 在 `ImageViewerView::new` 里由「命令行有没有图片路径」定死（见 `app.rs`）。从空窗口里用菜单 / 按钮 / 拖入打开的图片**不改界面形态** —— 用户是先开程序再选图，界面忽然少两栏会像出错，想全屏有 F11。谁把它简化回只看 `has_image`，`only_a_double_click_launch_collapses_the_interface` 立刻红 |

### WIC 的两个运行时约束

- 解码在**自己的后台线程**上进行，因此 `decode/wic.rs` 用 `ComApartment` 成对地 `CoInitializeEx` / `CoUninitialize`。注意 `RPC_E_CHANGED_MODE` 时不拥有所有权，**不能**反初始化。
- 缺组件与文件损坏必须分开报：前者引导用户去 Microsoft Store 装扩展，后者提示换文件。混淆会让用户白折腾一圈。

---

## 5. 依赖与构建的坑

| 事项 | 说明 |
| --- | --- |
| **构建很慢** | release 开了 `lto = "fat"` + `codegen-units = 1`，改动本项目代码后重建约 3–6 分钟。这是为启动速度付的代价，不要"优化"掉 |
| `image` 关掉了默认特性 | 默认特性里的 `avif` 只是**编码**能力（ravif → rav1e）。不要为了 AVIF 打开它 —— 解码仍然需要 dav1d（C 库） |
| `resvg` 固定在 0.46 | 刻意与 gpui 自己依赖的那一份对齐，否则会同时编进两份 resvg/usvg/svgtypes |
| **不要**单独依赖 `usvg` | 用 `resvg::usvg`（它的 re-export）。两个 crate 的版本因此永远一致 |
| **不要**引入 `rav1d` | 它的默认特性需要 NASM 汇编器；而且我们已经决定 AVIF 走系统原生解码 |
| **不要**引入 `zenavif` | AGPL-3.0，与 Apache-2.0 不兼容 |
| `trash` 关掉默认特性 | 去掉没用的 `chrono`，只留 `coinit_apartmentthreaded`（删除在 UI 线程上执行） |
| `rfd` 的对话框是**阻塞**的 | 必须放到独立线程上弹（`fs_ops/file_ops.rs` 已经这么做），否则界面冻住 |
| Windows 静态链接 CRT | `.cargo/config.toml` 里的 `+crt-static`，消除启动期 DLL 查找 |
| debug 构建 | `[profile.dev.package]` 已为 GPUI 依赖开 `opt-level = 3`，但**我们自己的解码代码仍未优化**。请始终用 `--release` 测性能 |

---

## 6. 代码风格

- **注释与面向用户的字符串一律用中文。**
- **注释解释「为什么」，不复述「做了什么」。** 例如不要写「// 遍历所有帧」，要写「// 用整数倍率做盒式滤波：每个输出像素对应固定数量的输入像素，既好并行也不会有累积舍入」。
- 有取舍的地方要把**被放弃的那个选项**也写出来（「之所以不…，是因为…」）。
- **非测试代码不得出现 `unwrap()` / `expect()`。** 唯一的例外是「不可能失败」处，且必须写清理由。
- **`unsafe` 只允许出现在 `decode/wic.rs`**（平台 FFI），每处都要有 SAFETY 注释说明前置条件。其它地方一律不允许。
- 模块顶部写模块级文档：这个模块解决什么问题、边界在哪、为什么这么切。
- 在输入边界拒绝非法值（NaN / 无穷 / 负数）。一个 NaN 的鼠标坐标只要写进平移量，之后所有坐标都会变成 NaN，表现为「图像凭空消失」，极难定位。

---

## 7. 测试约定

168 项，分三层：

| 位置 | 关注点 |
| --- | --- |
| 各模块内的 `#[cfg(test)] mod tests` | 纹理构建、去马赛克、方向代数、文件操作、输入换算、菜单与快捷键表 |
| `tests/decode_test.rs` | 格式判定以内容为准、损坏文件不崩、超大图在分配前被拒、动图帧与延迟不丢、缺解码器时给可操作提示 |
| `tests/transform_test.rs` | 缩放锚点不变性（含连续缩放不漂移）、1:1 在高分屏的含义、平移边界、方向复合与像素实现 8×8 全对齐 |

约定：

- **测试样本现场生成，仓库里不放二进制素材。** 「怎么造出这种格式」本身就写进了测试里。
- 测试名读起来要像一条行为断言：`cursor_anchored_zoom_keeps_the_pixel_under_the_cursor`，而不是 `test_zoom`。
- **不要在 `use gpui_kit::*` 的文件里写 `#[test]`** —— 测试宏展开会撞上递归上限。把纯逻辑挪到不引入 gpui 的模块（`ui/format.rs` 就是这么来的）。
- 测试**不得**弹对话框、碰真实回收站、写仓库外的文件。

### 界面动作的单一事实来源

新增或修改一个动作（菜单项 / 快捷键）**只需要改 `ui/command.rs`**：菜单结构、右侧显示的
快捷键、按键到动作的映射全部出自那里，视图只多一行 `match` 分支。这不是为了少写代码，
而是为了消掉这类界面最常见的缺陷 —— 「菜单里写着 Ctrl+S，按下去却没反应」，
它的根因永远是快捷键文案与键盘分发各写了一份。

判断「一个改动该不该加测试」的标准：**这个错误是不是「看起来只是有点别扭」那种？** 是的话就写 —— 缩放锚点漂移、方向叠加写反、alpha 没摊平、通道顺序错、Shift+R 被 R 抢走，全都属于这一类，肉眼很难断定「到底对不对」，但都能被精确断言。

---

## 8. 不要做的事（范围边界）

本期**明确不做**，架构上也不堵死：

- 文件夹导航、相邻图片预加载
- 文件关联的自动注册、安装包打包（README 里给了三系统的手工步骤）
- 单实例转发
- macOS / Linux 的 HEIC / AVIF 实现

最后一条要特别说明：那两个平台后端现在是**返回明确提示的桩**。原因是它们只能在对应平台上编译验证，而本仓库没有那两边的验证环境。**不要用「看起来应该能跑」的 `unsafe` 平台代码去补上它们** —— 这正是本层「宁可给出一条可操作的说法，也不提交从未被编译器检查过的代码」的约定。接入点是现成的：替换 `decode/heic/heic_macos.rs` 等文件里的 `decode`，其余各层一行都不用改。

---

## 9. 环境变量

| 变量 | 作用 |
| --- | --- |
| `NGY_PERF=1` | 打开启动打点（默认完全关闭：零 I/O、零输出） |
| `NGY_PERF_LOG=<路径>` | 指定日志文件位置，默认在系统临时目录 |
| `NGY_BENCH_MS=<毫秒>` | 基准采集模式：到时间自动退出 |
| `NGY_DIAG_FONTS=1` | 打印系统字体枚举耗时（排查启动问题） |
| `NGY_TRACE=1` | 打开「打开图片全链路」调试日志（debug 构建默认开，release 默认关） |

**打点本身不能成为启动的负担** —— 新增打点时保持这个性质。

### 排查「界面全黑但没有任何报错」

`NGY_TRACE=1` 会逐步打印：命令行参数 → 文件头十六进制 → 格式判定 → 每个候选解码器的成败 →
文档尺寸 → 纹理尺寸 → 画布实测尺寸 → 绘制决策 → `paint_image` 的返回。

看两处就够：

- `画布实测尺寸=W×H`：**H 为 0** 就意味着画布塌陷，一张图都画不出来；
- `[fail:…]` 行：这是「静默失败」的唯一出口。`trace::fail` 无条件输出，
  专门用来接住那些「调用成功但什么都没画出来」的情况（GPUI 有多处这种 API）。

失败信息按 key 去重，所以逐帧路径上的一处持续故障只会留一条，不会刷屏。

---

## 10. 改完之后怎么验证

1. `cargo test` —— 全绿。
2. `cargo check --all-targets` —— **零警告**。这个仓库目前是零警告状态，请保持。
3. 如果改动了启动路径、解码路径或渲染路径：
   `cargo build --release` 之后跑一次第 1 节的性能测量，确认 `first_frame_with_image` 仍然与首帧同时刻。
4. 如果新增了格式或改了方向逻辑：同时更新 `tests/decode_test.rs` 或 `tests/transform_test.rs`，并确认 README 的格式表仍然准确。
5. 如果新增了错误路径：确认它同时有非空的 `user_message()` 与 `short_reason()`，且 `user_message()` 里带上了文件名（若该错误与某个文件相关）。
