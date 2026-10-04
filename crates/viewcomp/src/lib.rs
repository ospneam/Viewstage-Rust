//! viewcomp — DirectComposition 合成基础设施。
//!
//! 固化 P0 spike（crates/spike-dcomp + crates/dcomp-rs-min）验证过的链路与结论：
//!
//! # 硬约束（违反即窗口全白 / AV，均已实测）
//! 1. **target/visual 的 COM 引用必须保活到窗口销毁**：`IDCompositionTarget` 被
//!    释放即销毁 DComp target 对象、拆除 visual 树与 HWND 的绑定。本 crate 在
//!    [`DcompCompositor`] 结构体字段中持有全部引用，并在 Drop 中显式解绑。
//! 2. **DComp 设备用 IDXGIDevice 路线创建**：本机 D3D12 command queue 路线
//!    `E_NOINTERFACE`（v2/v3 均如此）；IDXGIDevice 与 null rendering device
//!    实测均可合成。
//! 3. **Present1 必须传 `DXGI_PRESENT_PARAMETERS` 默认结构体**：nullptr 会 AV。
//!
//! # 分层（对齐全量重写目标架构）
//! root visual 下挂多个 child visual（UI 合成交换链 / NV12 媒体交换链 / 纯色），
//! 顺序即 z-order，通过 [`DcompCompositor::add_visual`] 控制。

#![cfg(windows)]

use windows::Win32::Foundation::BOOL;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIDevice, IDXGISwapChain1, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_PRESENT_PARAMETERS, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};

pub mod compositor;
pub mod swapchain;

pub use compositor::{DcompCompositor, VisualSlot};
pub use swapchain::create_composition_swapchain;

/// composition swapchain 参数（P1 默认 B8G8R8A8 双缓冲；P3 加 NV12）。
#[derive(Debug, Clone, Copy)]
pub struct SwapchainDesc {
    pub buffer_count: u32,
    pub format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
}

impl Default for SwapchainDesc {
    fn default() -> Self {
        Self {
            buffer_count: 2,
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
        }
    }
}

/// DComp 设备创建：IDXGIDevice 路线（Chrome 路线）。
///
/// 调用方从 wgpu-hal 拿 raw device 后 QI `IDXGIDevice`（或用 D3D11 device 直接
/// QI）传入。P0 实测本机 D3D12 command queue 不能作为渲染设备来源。
pub fn create_device(dxgi_device: &IDXGIDevice) -> windows::core::Result<IDCompositionDevice> {
    unsafe { DCompositionCreateDevice2::<_, IDCompositionDevice>(dxgi_device) }
}

/// composition swapchain 双缓冲默认描述（P0 验证参数）。
pub(crate) fn base_swapchain_desc(
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    buffer_count: u32,
) -> DXGI_SWAP_CHAIN_DESC1 {
    DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: format,
        Stereo: BOOL(0),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: buffer_count,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    }
}

/// `IDCompositionDevice` 的公共 re-export（调用方类型标注用）。
pub type DcompDevice = IDCompositionDevice;
pub type DcompTarget = IDCompositionTarget;
pub type DcompVisual = IDCompositionVisual;

/// Present 辅助：P0 硬约束 #3 的封装（params 必须非空）。
///
/// # Safety
/// swapchain 必须有效。
pub unsafe fn present_one(swapchain: &IDXGISwapChain1, sync_interval: u32) -> windows::core::Result<()> {
    let params = DXGI_PRESENT_PARAMETERS::default();
    // IDXGISwapChain1::Present1 在 windows-rs 0.58 返回 HRESULT（非 Result）
    swapchain
        .Present1(sync_interval, windows::Win32::Graphics::Dxgi::DXGI_PRESENT(0), &params)
        .ok()
}

/// 工厂创建（swapchain 前置）。
pub(crate) fn create_factory() -> windows::core::Result<windows::Win32::Graphics::Dxgi::IDXGIFactory2> {
    unsafe { CreateDXGIFactory2::<windows::Win32::Graphics::Dxgi::IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0)) }
}
