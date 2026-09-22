# AGENTS.md

给在本仓库里工作的 AI 编码助手（以及第一次接手的人）。

**产品目标只有一条，它决定了所有取舍**：在文件管理器里双击一张图片，窗口出现的那一刻就已经是图像 —— 没有白屏、没有加载动画、没有中间确认步骤。

性能上的硬指标（Windows release 实测）：**6000×4000 PNG 首帧带图 305 ms**，其中解码 59 ms 完全被 GPUI 的 249 ms 平台初始化掩盖。

---

## 1. 常用命令

```bash
cargo test                          # 全部 259 项测试
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
| `ui/` | 界面与交互状态：标题栏 + 菜单栏、工具栏、状态栏、EXIF 面板、设置关联浮层 | 像素格式、通道顺序 |
| `input/` | 手势状态机、滚轮换算（纯逻辑） | 元素与事件回调的接线（那在 `ui/view.rs`） |
| `fs_ops/` | 剪贴板 / 打开 / 另存为 / 重命名 / 回收站 / **用户偏好读写** / **文件关联** | 界面 |
| `ui/command.rs` | 动作清单、菜单结构、快捷键表（**零 UI 依赖**） | gpui、窗口、元素 |
| `ui/theme.rs` | 两套皮肤的**颜色取值** + 尺寸/时长常量 | 窗口、平台、状态 |
| `ui/assoc.rs` | 「设置关联格式」浮层的绘制 | 注册表、平台能力（那些在 `fs_ops/associations.rs`） |

`render/` 是整个项目里**唯一**同时知道「图像数据长什么样」和「GPUI 怎么画」的地方。像素格式、通道顺序、纹理上限、坐标系换算都关在这一层。

### 多页文档：页 ≠ 动图（同一个 `frames` 数组的两种语义）

`ImageData.frames` 被两种语义共用：**动图**的帧挂在时间轴上（自动播放），
**多页容器**（多页 TIFF 的 IFD 链）的帧是并列的页（用户翻）。同一份数据、两种含义，
所以每一层都要各自问清「我要的是哪一种」，**只看 `frames.len()` 一定出错**：

| 层 | 问法 |
| --- | --- |
| `decode/types.rs` | `is_animated()` = `format.may_be_animated() && frames.len() > 1`。**格式这一维不能省**：多页 TIFF 补页之后 `frames.len()` 也大于 1，只数帧数的话打开一份 30 页的扫描件它会自己翻起来，而且用户按「下一页」永远追不上它。`may_be_animated()` 是「哪些格式的多个 frame 表示时间轴」的唯一答案，必须与 `decode/raster.rs` 的格式分派一致 |
| `model/document.rs` | `is_paged()` = `pages > 1 && !format.may_be_animated()`；`content(index) -> Option<&Frame>` 对**未解码**的页返回 `None`，**绝不退化为第一页** |
| `render/surface.rs` | `Textures::Frames`（一张纹理含全部帧）vs `Textures::Pages`（每页一张单帧纹理） |
| `ui/command.rs` | `needs_pages()` 是独立于 `is_available` / `needs_image` 的**第三维**（「这张图有没有别的页」） |

### 打开多页文档：先出首页、其余后台补

双击打开必须在首帧就有图，所以打开路径**只解第 1 页**，其余页等用户翻过去时按需解
（`ui/view.rs` 的 `ensure_page` / `pump_page` / `accept_page`）。由此长出三条硬约束：

- **未解码的页绝不退而画别的页。** `ViewportConfig.frame_index: Option<usize>`，
  `None` = 这一份内容还没解好，此时只铺底色、由占位层说明「正在解码第 k 页」。
  「页码变了、画还是上一张」在画面上与「翻对了」长得一模一样，所以这条约束落在**类型**上，
  而不是靠某处的一次判断。同理，`paint_image` 里原先那句「把帧号夹到最后一帧」的兜底
  必须**删掉** —— 夹帧看着安全，实际是「你要第 5 页、我给你第 3 页」。
- **每页一张单帧纹理，而不是一张多帧纹理。** `RenderImage` 构造后不可变，而它的
  `id: ImageId` 是 GPU 上传的**缓存键**；每次补页都重建一张含全部页的纹理，
  会让先前每一页都重新上传一次 —— 翻到第 N 页是 O(N²) 的上传量。
  分页因此走 `Textures::Pages`，且每页按**自己的**长边算降采样倍率
  （同一份扫描件里正文 A4、插页 A3 很常见，用第一页的倍率会让纹理超 GPU 上限）。
- **约束可注入**：`ImageDocument.budget: PageBudget`（生产路径恒为 `default()`）。
  测试把它调到极小，就能把「触到 512 页 / 2 GiB 上限之后界面怎么办」这条真实代码路径
  真的走一遍，而不必造出几十 GB 内存。触到上限时 `pages` 收敛到已载入的页数，
  免得把用户送到一个永远解不出来的页上。

页号有两份，用途不同，**别混**：`frame_index` 是**用户请求的**页（页码要立刻跟着走，
否则那几十毫秒的解码间隙里按了键看不到任何反应），`document.current_page()` 是
**已经画得出来的**页。状态栏与画布角标用前者；`displayed_pixels()`（复制图像 / 另存为）
用的是 `document.content(visible_index()?)` —— 与画布要画的那一份同源。

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

## 4. 文件关联（Windows）

「设置关联格式，之后双击图片用本程序打开」。实现在 `fs_ops/associations.rs`（注册表，零 gpui 依赖）
与 `ui/assoc.rs`（自绘浮层），入口是「工具 → 设置关联格式…」。

这个功能分两步，别把两步混成一步：**登记**（写注册表，程序能做）与**成为默认**
（改 `UserChoice`，程序做不到 —— 只能把用户送到系统 UI）。面板上的「全部关联」做前者，
「设为默认…」做后者。

### 生效规则：三级优先级，第一级我们写不进去

Windows 决定「双击 `.foo` 用哪个程序」时，按顺序取第一个非空值：

```text
1. HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.<ext>\UserChoice
      ← 带哈希校验；2024-02 累积更新起内核驱动 UCPD.sys 还会直接拦截写入。
        程序写不进去，写了也无效（且会弹「应用默认值已重置」）
2. HKCU\Software\Classes\.<ext> 的默认值              ← 我们能写，靠它生效
3. HKLM\Software\Classes\.<ext> 的默认值              ← 需管理员，本程序不碰
```

在真机上（58 个候选扩展名）**实测**得到的结论，不是照文档推测：

| 现象 | 证据 |
| --- | --- |
| 第 2 条写得进去、确实生效 | `.qoi` / `.jxl` 原来「问你要用哪个程序」，写入后立刻变成我们 |
| **第 1 条存在时，第 2 条被完全压住** | `.png`（WPS 占着）、`.heic`（美图占着）写入后系统仍用原来那个 |
| 本机 **26 / 58 个扩展名带 UserChoice**（44%） | `read_all` 在真机上的输出 |
| 覆盖第 2 条会顶掉别的来源，**必须备份才可逆** | `.jp2` 原本由 SumatraPDF 经 `OpenWithProgids` 认领，写入后掉成「没有程序认领」 |

判定「系统实际会用哪个 exe」的权威接口是 `AssocQueryStringW`（flags=0，`ASSOCSTR_EXECUTABLE`），
排查时用它，别自己按上面的顺序"推理"。

### 让本程序成为默认：只能把用户送到系统 UI

`UserChoice` 那一层程序改不动，这是 Windows 的设计而非实现缺陷：微软的官方口径是
「默认程序只能在系统 UI 里由用户改」，`UCPD.sys` 就是为此加的内核保护。因此
`associations::open_defaults_settings()` 做的是**受支持范围内的最后一步**：

```text
ms-settings:defaultapps?registeredAppUser=<RegisteredApplications 里的值名>
```

- 值名就是 `APP_EXE`（我们写在 `HKCU\Software\RegisteredApplications` 的名字），
  不是 `Capabilities` 的路径、也不是 ProgID。写错则翻不到本程序那一页。
- `?registeredAppUser=` 自 Windows 11 21H2 / 22H2（2023-04 CU）与 23H2 及以后可用；
  更早的系统忽略该参数、退化成「默认应用」列表页 —— 不会出错，只是少走一步。
- 跳转前必须先 `ensure_registered` + 登记好候选格式：设置页里本程序那一页只列
  `Capabilities\FileAssociations` 里登记过的格式，没登记的格式点了「设置默认值」也切不过来。
- 打开走 `cmd /C start "" <uri>` + `CREATE_NO_WINDOW`，不走 `ShellExecute`（那要 FFI，
  而本项目的 `unsafe` 只在 `decode/wic.rs`）。`start` 会把第一个带引号的参数当窗口标题，
  所以那个空标题不能省。
- **不要**试着自己算 `UserChoice` 的哈希再写注册表（SetUserFTA 那一套）：微软明令禁止，
  且在装了 UCPD 的机器上会被拦掉 —— 表现为「有时行有时不行」，比不做更糟。

### 因此界面必须是三态，不能是勾/不勾

`AssocState` 有三档，`ui/assoc.rs` 给三种**可区分**的画法：

| 状态 | 含义 | 画法 |
| --- | --- | --- |
| `None` | 与本程序无关 | 灰字灰框 |
| `Registered` | 已登记，但系统当前用别的程序打开 | **主色字**、灰框 |
| `Default` | 双击立即生效 | 主色字、**主色框**、深一档的底 |

把后两者画成一样，用户勾完 `.png` 去双击、结果打开 WPS，只会认为功能是坏的 ——
而真相是系统上另有程序持有该扩展名。所以 `summary()` 会明说「另有 N 个已登记，
但系统当前用别的程序打开 —— 点「设为默认…」到系统设置里一次性切过来」，
并且面板左下角就摆着那个按钮：把用户送到该去的地方，而不是让他自己去「设置」里翻。

### 可逆性

- 覆盖默认值前先把原值备份到 `HKCU\Software\ngy-image-viewer\PreviousDefaults`。
- 取消关联时**仅当当前值确实是我们写的**才还回备份（或删值），不覆盖用户后来的选择。
- WPS 抢关联时也在 `.png` 下留了 `ksobak` 值备份原值 —— 同一套思路，属于业界常规做法。

### 其它约定

- 写入的位置固定在 `HKCU` 下：`Classes\<PROG_ID>`（默认值 + `DefaultIcon` + `shell\open\command`）、
  `Classes\<ext>\OpenWithProgids`（值是 `REG_NONE`，判定存在性要用 `get_raw_value`，
  `Vec<u8>` 没实现 `FromRegValue`）、`RegisteredApplications`、`Capabilities`、
  `Classes\Applications\<exe>.exe\{shell\open\command, SupportedTypes}`。
- **不调用** `SHChangeNotify(SHCNE_ASSOCCHANGED, …)`：实测不刷新也立即生效，
  没必要为本项目新开一处 `unsafe`（它也是全项目除 `decode/wic.rs` 外唯一的例外）。
  若将来发现某台机器需要，再补。
- 「能不能改关联」是**平台能力**，与「有没有打开图片」是两维：
  `Command::is_available()` 管前者（Windows 才可用），`Command::needs_image()` 管后者。
  别把 `is_available` 写成 `needs_image` 的别名 —— 空窗口下这个菜单项必须可点。
- 候选扩展名**不另立清单**，直接用 `file_ops::IMAGE_EXTENSIONS`：
  新增图片格式时只改那一处，解码与关联两端自动同步。
- 读一次全部候选扩展名会做上百次注册表查询（实测 ~10 ms），**只在用户主动打开面板时读**，
  绝不放渲染循环里。
- **同一个键上「先读后写」时，权限要一起给**（`KEY_READ | KEY_SET_VALUE`）。
  只开 `KEY_SET_VALUE` 时那次读会以 `ERROR_ACCESS_DENIED` 失败；若它被 `.ok()` 吞掉，
  调用方就会判定「当前值不是我们写的」，于是**取消关联静默地不还回备份** ——
  界面清掉了候选列表、也弹了提示，只有默认值还留在我们名下。实测症状就是
  「点两次之后 `.png` 的默认值仍指着本程序」，单测与肉眼都看不出来。
  因此「值不存在」与「读失败」必须分开：`ErrorKind::NotFound` 是前者，
  其余一律往上抛（见 `windows_impl::default_of` 与只读路径的 `default_value` 的分工）。

---

## 5. 解码层的硬性约定

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
| `raw.rs` | rawloader **不做裁剪**，`crops = [top, right, bottom, left]` 要自己应用；`SensorView` 用整幅传感器上的**绝对坐标**判断 CFA 颜色，所以必须传**原始** `cfa`，**不是** `cropped_cfa()`（那会把位移应用两次）。另外：NEF 的 huffman 解码表要吃掉约 **640 KB 栈**，而且这个值与**图像尺寸无关**（5 MP 与 45 MP 实测同值，release）—— 解码线程默认的 2 MiB 够用（约 3 倍余量），但**别把解码线程的栈调小**：栈溢出是 `STATUS_STACK_OVERFLOW`，进程直接死，没有任何错误提示 |
| `xpm.rs` | 颜色表的**键**是行首的 `cpp` 个字符，不能拿 `split_whitespace()` 取词 —— `"  c none"`（空格作键、含义为透明）是大量 XPM 生成器的默认写法，取词会把空格键整个吃掉，表现为「像素引用了未定义的颜色键」 |
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

## 6. GPUI 事实清单（核实过源码，别凭记忆写）

**本仓库的 `gpui` 不是 `gpui-0.2.2`。** `gpui-kit 0.6.1` 依赖的是 `gpui-pre 0.3.4`（`Cargo.toml` 里 package 重命名为 `gpui`）。查文档时注意这一点。

写任何 GPUI 代码之前，请**先去 cargo registry 源码里确认签名**。以下几条是已经确认过的、最容易写错的：

| 事实 | 后果 |
| --- | --- |
| `Window::paint_image(bounds, image_bounds, corner_radii, data, frame_index, grayscale)` **只接受轴对齐矩形**，整个 crate 没有公开的仿射变换入口 | **旋转与翻转必须在像素层完成**（`decode/orientation.rs`）。这换来一个好处：屏幕所见与「另存为」导出逐像素一致 |
| `RenderImage` 的缓冲区是 `image::Frame`（RGBA 布局）但 GPU 按 **BGRA** 解释，非预乘 | 上传前必须交换红蓝通道，否则人脸变蓝 |
| `RenderImage` 构造后**不可变**，它的 `id: ImageId` 是 GPU 上传的**缓存键**（`gpui-pre-0.3.4/src/assets.rs`） | 想「往已有纹理里再加一帧」只能重建一张新的，而新 `id` 意味着整张重新上传。多页文档因此**每页一张单帧纹理**（`Textures::Pages`），不是一张多帧纹理 |
| `RenderImage::as_bytes(i)` / `size(i)` 对越界帧号返回 `None` / 默认值，**不 panic**；`paint_image` 对越界帧号也不 panic（帧数为 0 时直接 return） | 先前那条「`frame_index` 必须先 `min(frame_count - 1)` 夹一次，否则换图时整界面 panic」**经核实不成立**，那句兜底已经删掉：在翻页场景里夹帧就是「你要第 5 页、我给你第 3 页」。取不到纹理就不要画，让占位层去说明 |
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

## 7. 依赖与构建的坑

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

## 8. 代码风格

- **注释与面向用户的字符串一律用中文。**
- **注释解释「为什么」，不复述「做了什么」。** 例如不要写「// 遍历所有帧」，要写「// 用整数倍率做盒式滤波：每个输出像素对应固定数量的输入像素，既好并行也不会有累积舍入」。
- 有取舍的地方要把**被放弃的那个选项**也写出来（「之所以不…，是因为…」）。
- **非测试代码不得出现 `unwrap()` / `expect()`。** 唯一的例外是「不可能失败」处，且必须写清理由。
- **`unsafe` 只允许出现在 `decode/wic.rs`**（平台 FFI），每处都要有 SAFETY 注释说明前置条件。其它地方一律不允许。
- 模块顶部写模块级文档：这个模块解决什么问题、边界在哪、为什么这么切。
- 在输入边界拒绝非法值（NaN / 无穷 / 负数）。一个 NaN 的鼠标坐标只要写进平移量，之后所有坐标都会变成 NaN，表现为「图像凭空消失」，极难定位。

---

## 9. 测试约定

259 项，分三层：

| 位置 | 关注点 |
| --- | --- |
| 各模块内的 `#[cfg(test)] mod tests` | 纹理构建、去马赛克、方向代数、文件操作、输入换算、菜单与快捷键表、多页文档（页 ≠ 动图、页数与内存上限的收敛、每页按自身尺寸建纹理、`goto_page` 只在已解码时成功、`content()` 不给退化值） |
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

## 10. 不要做的事（范围边界）

本期**明确不做**，架构上也不堵死：

- 文件夹导航、相邻图片预加载
- 安装包打包（README 里给了三系统的手工步骤）
- 单实例转发
- macOS / Linux 的 HEIC / AVIF 实现

**文件关联现在做了**（「工具 → 设置关联格式…」，见第 4 节），但边界要守住：**只在用户显式勾选时写注册表** —— 启动、打开图片、换皮肤都不会碰它，也绝不覆盖用户已经做出的选择。Windows 之外平台返回「不支持」而不是假装成功。

最后一条要特别说明：那两个平台后端现在是**返回明确提示的桩**。原因是它们只能在对应平台上编译验证，而本仓库没有那两边的验证环境。**不要用「看起来应该能跑」的 `unsafe` 平台代码去补上它们** —— 这正是本层「宁可给出一条可操作的说法，也不提交从未被编译器检查过的代码」的约定。接入点是现成的：替换 `decode/heic/heic_macos.rs` 等文件里的 `decode`，其余各层一行都不用改。

---

## 11. 环境变量

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

## 12. 改完之后怎么验证

1. `cargo test` —— 全绿。
2. `cargo check --all-targets` —— **零警告**。这个仓库目前是零警告状态，请保持。
3. 如果改动了启动路径、解码路径或渲染路径：
   `cargo build --release` 之后跑一次第 1 节的性能测量，确认 `first_frame_with_image` 仍然与首帧同时刻。
4. 如果新增了格式或改了方向逻辑：同时更新 `tests/decode_test.rs` 或 `tests/transform_test.rs`，并确认 README 的格式表仍然准确。
5. 如果新增了错误路径：确认它同时有非空的 `user_message()` 与 `short_reason()`，且 `user_message()` 里带上了文件名（若该错误与某个文件相关）。
6. **碰了文件关联、皮肤跟随系统这类「平台值」的功能，单测不够** —— 它们的正确性取决于本机的注册表与系统设置，只有真机跑一遍才算验证：
   ```bash
   python .workbuddy/scripts/assoc_state.py .workbuddy/shots/before.txt   # 先留快照
   python .workbuddy/scripts/assoc_e2e.py                                 # 端到端闭环
   ```
   这个脚本**不能**放进 `tests/`：第 9 节写明测试不得写仓库外的文件，而它必须改真实注册表。
   它会先备份、再写入、最后无条件清理；判据用 `AssocQueryStringW`（系统实际会用哪个 exe），
   而不是「注册表里写了什么」—— 后者会把「写了但被 UserChoice 压住」误判成成功。
   `assoc_probe.py` 是它的只读版（只定位面板、不改注册表），改界面布局时用它。
7. **碰了翻页 / 多页文档，单测不够** —— 它牵涉键盘分发、菜单可用性、后台解码、纹理追加
   与「画布到底画了哪一份」四件事，只有真机跑一遍才算验证：
   ```bash
   python .workbuddy/scripts/verify_multipage.py
   ```
   判据**不是截图**：本机可能根本没有交互桌面（`GetForegroundWindow()` 返回 0、
   `OpenInputDesktop()` 返回 0 / err=5、`PrintWindow` 一律返回 0），此时任何 GDI 取像素
   的方案都无解。改用两条别的通道（细节见 `gpui-gui-automation` 技能）：
   - **程序日志**（`NGY_TRACE=1`，主力）：`文档就绪…已解页数=1 总页数=3`、
     `第 N 页已就绪（已解码 x/y）`、`paint_image…内容序号=N/M 纹理内帧号=0 纹理尺寸=…`。
     后两条是「画布真的换到了那一页」与「每页一张单帧纹理」的直接证据 ——
     `paint.*` 的打点 key 里带内容序号，正是为了翻页之后日志不会变成一片沉默。
   - **剪贴板**（给真像素）：菜单「编辑 → 复制图像」把 `displayed_pixels()` 放进剪贴板，
     再把 DIB 读回来数颜色（`CF_DIBV5`，`BITMAPV5HEADER` 124 字节、32bpp、BI_BITFIELDS）。
     **不要走 `Ctrl+C`**：GPUI 用 `GetKeyState` 判修饰键，看不见投递的
     `WM_KEYDOWN VK_CONTROL`（已实测），投递过的「Ctrl+C」只是个普通的 `c`。
     剪贴板也不会随翻页自动更新，每读一页都要重新走一次菜单。

---

## 13. 发布与打包

产物由 `packaging/package.py` 打出，**CI 与本地同一个脚本**。
「本地跑一遍 Windows 分支」因此等价于验证了 CI 的那一环 —— 这条等价关系是刻意维持的，
别在 workflow 里写一份只存在于 CI 的打包命令（那份代码永远不会被本地跑过）。

```bash
git tag v0.1.0 && git push origin v0.1.0     # 触发 .github/workflows/release.yml
```

| 平台 | 产物 | 形态 |
| --- | --- | --- |
| Windows | `.zip` | `ngy-image-viewer.exe` + README + LICENSE |
| macOS arm64 / x86_64 | **两个** `.zip` | `ngy-image-viewer.app`（Info.plist + icns + 二进制） |
| Linux | `.tar.gz` | 二进制 + `.desktop` + 图标 + README + LICENSE |

### 别把这三件事当成细节

- **macOS 的 zip 必须用 `ditto` 打，不能用 `zipfile`。** 只有 ditto 保留 `.app`
  内部的可执行位；用普通 zip 打出来的包，用户解压后会得到一个「双击没反应」的 app，
  而发布者本机上是好的 —— 这类问题在发布侧几乎不可能发现。
- **macOS 的 `.app` 要 ad-hoc 签名（`codesign --force --sign -`），先内后外。**
  Apple Silicon 的内核拒绝运行**完全没有签名**的可执行文件。它只解决「能不能运行」，
  不解决 Gatekeeper：没有 Developer ID 证书就无法公证，用户首次打开仍需右键 →「打开」。
  Release 说明里必须写明这一点，否则用户会以为是包坏了。
- **`Info.plist` 的 `CFBundleDocumentTypes` 不是装饰。** macOS 上「双击图片用本程序打开」
  完全依赖它声明了哪些 UTI —— 没有它，Finder 的「打开方式」里根本不会出现本程序。
  它与 Windows 那边写注册表是同一件事的两个平台版本。

### 校验与幂等

- **tag 与 `Cargo.toml` 的版本号必须一致**，CI 会硬失败。不校验的话会产出
  「文件名写着 A、exe 资源段里写着 B」的包 —— 这种错只有用户来问的时候才会发现。
- Release 步骤是**幂等**的（`gh release view` 存在就改成 `upload --clobber`），
  CI 失败重跑或 tag 推倒重来都不会卡在「release 已存在」。
- 产物**少一个就不发**：一个只挂了一半产物的 Release 比没有 Release 更容易误导人。
- 手动触发（`workflow_dispatch`）只构建、留 artifact，**不创建 Release** ——
  验证新平台能不能编过时用它，不会污染 Releases。

### 首次跨平台的现实

macOS 与 Linux 的构建原本从未跑过（HEIC/AVIF 在那两个平台是有意的桩，不链接 C 库；
但 gpui 的 Linux / macOS 后端从没编过）。CI 的第一轮就是这个功能的验证，
失败点大概率落在 Linux 的系统依赖（`libclang` / `libfontconfig` / wayland）与
macOS 交叉编译的 C 侧（`CMAKE_OSX_ARCHITECTURES`）上。
