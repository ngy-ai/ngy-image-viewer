//! Windows 上基于 WIC（Windows Imaging Component）的解码后端，HEIC 与 AVIF 共用。
//!
//! # 为什么这两种格式能共用一条路径
//!
//! 在 Windows 眼里，它们几乎是同一件事：
//!
//! | 格式 | 容器 | 需要系统提供的解码器 |
//! | --- | --- | --- |
//! | HEIC/HEIF | ISOBMFF | HEIF 图像扩展（HEVC 解码） |
//! | AVIF | ISOBMFF | AV1 图像扩展（AV1 解码） |
//!
//! 容器解析、像素格式转换、错误分类全部一致，唯一的差别是
//! 「缺组件时该让用户去装什么」。那一条提示由调用方通过 [`WicRequest`] 传入，
//! 于是这里可以只留一份实现 —— 少一份实现就少一处会走偏的地方。
//!
//! # 为什么转到 BGRA 而不是 RGBA
//!
//! `GUID_WICPixelFormat32bppBGRA` 是**所有** WIC 编解码器都必须支持的目标格式，
//! 而 `32bppRGBA` 只在部分编解码器上可用。拿到 BGRA 后我们自己交换红蓝通道，
//! 代价是一次线性遍历，换来的是确定性。
//!
//! # 线程与 COM
//!
//! WIC 要求调用线程先初始化 COM。解码发生在我们自己的后台线程上，
//! 因此这里用 [`ComApartment`] 成对地初始化 / 反初始化，
//! 不依赖主线程（UI 线程）的单元模型。

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{
    GENERIC_READ, WINCODEC_ERR_BADHEADER, WINCODEC_ERR_BADIMAGE, WINCODEC_ERR_COMPONENTNOTFOUND,
    WINCODEC_ERR_FRAMEMISSING, WINCODEC_ERR_UNKNOWNIMAGEFORMAT,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICBitmapDecoder, IWICBitmapFrameDecode,
    IWICFormatConverter, IWICImagingFactory, IWICPalette, WICBitmapDitherTypeNone,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::{Error as WinError, PCWSTR};

use super::types::{
    DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat, Orientation,
};

/// 一次 WIC 解码请求。
pub struct WicRequest<'a> {
    /// 解码结果的格式标签（状态栏会显示它）。
    pub format: ImageFormat,
    /// 缺少系统解码组件时，用户看到的「缺少什么」。
    pub component: &'a str,
    /// 缺少系统解码组件时，用户可以照做的安装步骤。
    pub install_hint: &'a str,
}

/// COM 单元守卫。
pub(crate) struct ComApartment {
    /// 只有「本次调用确实从零初始化了 COM」时才由我们负责反初始化。
    owns_initialization: bool,
}

impl ComApartment {
    pub(crate) fn enter() -> Self {
        // SAFETY: `CoInitializeEx` 只改动当前线程的 COM 单元状态，没有跨线程副作用；
        // 下面通过 `Drop` 与它严格配对。
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // `ok()` 对 S_OK 与 S_FALSE 都成立：前者是首次初始化，后者是「本线程已初始化过」。
        // 两种情况下引用计数都 +1，都需要配对调用 `CoUninitialize`。
        // 唯一要当心的是 RPC_E_CHANGED_MODE：线程已被初始化为另一种单元模型，
        // 此时 COM 仍然可用，但所有权不属于我们，绝不能去反初始化。
        Self {
            owns_initialization: result.ok().is_ok(),
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owns_initialization {
            // SAFETY: 与上面那次成功的 `CoInitializeEx` 严格配对。
            unsafe { CoUninitialize() };
        }
    }
}


pub fn decode(src: &Path, limits: &DecodeLimits, request: WicRequest<'_>) -> DecodeResult<ImageData> {
    let _apartment = ComApartment::enter();

    // SAFETY: 参数都是常量 GUID 与一个空的外部对象指针，
    // 返回的接口由 `Result` 保证有效。
    let factory: IWICImagingFactory = unsafe {
        CoCreateInstance(
            &CLSID_WICImagingFactory,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
    }
    .map_err(|error| DecodeError::corrupt(format!("无法初始化 WIC 图像组件：{error}")))?;

    // WIC 的文件名参数是「以 NUL 结尾的 UTF-16」。用 `encode_wide` 自己拼，
    // 比经由 `HSTRING` 少一层不确定性，对非 ASCII 路径更稳妥。
    let mut wide: Vec<u16> = src.as_os_str().encode_wide().collect();
    wide.push(0);
    let filename = PCWSTR(wide.as_ptr());

    // SAFETY: `filename` 在整个调用期间有效（`wide` 活到函数结束），其余参数都是常量。
    let decoder: IWICBitmapDecoder = unsafe {
        factory.CreateDecoderFromFilename(
            filename,
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )
    }
    .map_err(|error| classify(src, &request, error))?;

    // SAFETY: `decoder` 由上面的 `Result` 保证有效。
    let frame: IWICBitmapFrameDecode =
        unsafe { decoder.GetFrame(0) }.map_err(|error| classify(src, &request, error))?;

    let mut width = 0u32;
    let mut height = 0u32;
    // SAFETY: 两个 out 参数都是本函数的局部变量，生命周期覆盖整个调用。
    unsafe { frame.GetSize(&mut width, &mut height) }
        .map_err(|error| classify(src, &request, error))?;
    limits.check_dimensions(width, height)?;

    // SAFETY: `factory` 有效。
    let converter: IWICFormatConverter = unsafe { factory.CreateFormatConverter() }
        .map_err(|error| DecodeError::corrupt(format!("无法创建 WIC 像素格式转换器：{error}")))?;

    // SAFETY: `converter` 与 `frame` 都有效，目标格式是合法常量；
    // 32 位真彩不需要调色板，故传 `None`。
    unsafe {
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None::<&IWICPalette>,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
    }
    .map_err(|error| classify(src, &request, error))?;

    // 尺寸上限已由 `check_dimensions` 核过，这里只完成分配。
    // 用 `checked_mul` 而不是直接相乘：多一层保险，避免任何情况下算出个回绕的小数。
    let stride = width as usize * 4;
    let buffer_len = stride
        .checked_mul(height as usize)
        .ok_or_else(|| DecodeError::corrupt("图像尺寸溢出，无法分配解码缓冲区"))?;
    let mut pixels = vec![0u8; buffer_len];

    // SAFETY: `pixels` 的长度正好是 `stride * height`，满足 WIC 的要求；
    // 传空指针表示「整幅图像」。
    unsafe { converter.CopyPixels(std::ptr::null(), stride as u32, &mut pixels) }
        .map_err(|error| classify(src, &request, error))?;

    // BGRA → RGBA。整条解码链的约定是非预乘 RGBA（与 `image` crate 一致），
    // 而 `32bppBGRA` 正好也是非预乘的，所以只需要交换首尾两个通道。
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    // 元数据是「尽力而为」：这两种容器的 EXIF 读不到时方向退化为「无需纠正」，
    // 不影响出图，只影响信息面板的丰富程度。
    let exif = super::exif::from_path(src);

    Ok(ImageData {
        format: request.format,
        frames: vec![Frame::new(width, height, pixels, 0)],
        orientation: exif
            .as_ref()
            .map(|data| data.orientation)
            .unwrap_or(Orientation::Normal),
        exif: exif
            .map(|data| data.summary)
            .filter(|summary| !summary.is_empty()),
        supersample: 1.0,
    })
}

/// 把 WIC 的 HRESULT 翻译成用户能据以行动的 [`DecodeError`]。
///
/// 分类的依据是「用户的下一步该做什么」：
/// 装组件、换文件，还是别的 —— 三类提示必须分开，
/// 混淆会让用户白折腾一圈。
pub(crate) fn classify(src: &Path, request: &WicRequest<'_>, error: WinError) -> DecodeError {
    let code = error.code().0;

    // 系统里没有能处理这种编码的组件。
    if code == WINCODEC_ERR_COMPONENTNOTFOUND.0 || code == WINCODEC_ERR_UNKNOWNIMAGEFORMAT.0 {
        return DecodeError::MissingPlatformSupport {
            what: request.component.to_string(),
            how_to_fix: request.install_hint.to_string(),
        };
    }

    let name = src
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| src.display().to_string());
    let format = request.format.display_name();

    // 文件本身坏了，与系统组件无关。
    if code == WINCODEC_ERR_BADHEADER.0
        || code == WINCODEC_ERR_BADIMAGE.0
        || code == WINCODEC_ERR_FRAMEMISSING.0
    {
        return DecodeError::corrupt(format!("「{name}」的 {format} 数据不完整或已损坏：{error}"));
    }

    DecodeError::corrupt(format!("「{name}」解码 {format} 失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::E_FAIL;

    fn heic_request() -> WicRequest<'static> {
        WicRequest {
            format: ImageFormat::Heic,
            component: "HEIC/HEIF",
            install_hint: "请在 Microsoft Store 安装「HEIF 图像扩展」。",
        }
    }

    #[test]
    fn missing_codec_becomes_an_install_hint() {
        let error = classify(
            Path::new("photo.heic"),
            &heic_request(),
            WinError::from_hresult(WINCODEC_ERR_COMPONENTNOTFOUND),
        );
        assert!(matches!(error, DecodeError::MissingPlatformSupport { .. }));
        let message = error.user_message();
        assert!(message.contains("HEIF 图像扩展"), "{message}");
        assert!(message.contains("HEIC"), "{message}");
    }

    #[test]
    fn unknown_format_also_points_at_the_codec() {
        let error = classify(
            Path::new("photo.heic"),
            &heic_request(),
            WinError::from_hresult(WINCODEC_ERR_UNKNOWNIMAGEFORMAT),
        );
        assert!(matches!(error, DecodeError::MissingPlatformSupport { .. }));
    }

    #[test]
    fn corrupted_file_is_not_reported_as_a_missing_codec() {
        let error = classify(
            Path::new("photo.heic"),
            &heic_request(),
            WinError::from_hresult(WINCODEC_ERR_BADIMAGE),
        );
        assert!(matches!(error, DecodeError::Corrupt { .. }), "{error:?}");
        assert!(error.user_message().contains("photo.heic"));
    }

    #[test]
    fn the_request_decides_which_component_is_named() {
        // 同一段代码服务两种格式，提示必须跟着请求走 ——
        // 给 AVIF 用户提示去装 HEIF 扩展是彻底的误导。
        let request = WicRequest {
            format: ImageFormat::Avif,
            component: "AVIF",
            install_hint: "请在 Microsoft Store 安装「AV1 图像扩展」。",
        };
        let error = classify(
            Path::new("photo.avif"),
            &request,
            WinError::from_hresult(WINCODEC_ERR_COMPONENTNOTFOUND),
        );
        let message = error.user_message();
        assert!(message.contains("AVIF"), "{message}");
        assert!(message.contains("AV1 图像扩展"), "{message}");
        assert!(!message.contains("HEIF"), "不应提示去装 HEIF 扩展：{message}");
    }

    #[test]
    fn other_errors_still_name_the_file() {
        let error = classify(
            Path::new("photo.heic"),
            &heic_request(),
            WinError::from_hresult(E_FAIL),
        );
        assert!(matches!(error, DecodeError::Corrupt { .. }));
        assert!(error.user_message().contains("photo.heic"));
    }
}
