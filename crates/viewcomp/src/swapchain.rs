//! composition swapchain 创建（DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL + PREMULTIPLIED）。
//!
//! P0 结论：本机 D3D12 queue 路线 DComp 设备创建失败（E_NOINTERFACE），但
//! **composition swapchain 本身可以挂在 D3D12 queue 上**（CreateSwapChainForComposition
//! 的 pDevice 接受 queue 的 IUnknown）——wgpu DX12 后端走此路线；D3D11 路线传
//! device 即可。Present1 必须传默认 `DXGI_PRESENT_PARAMETERS`（nullptr AV）。

use windows::core::{Interface, IUnknown};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;

use crate::{base_swapchain_desc, create_factory, SwapchainDesc};

/// 创建 composition swapchain。
///
/// * `device`：D3D12 command queue 或 D3D11 device 的 `IUnknown`。
/// * `size`：与窗口客户区物理像素一致（由调用方负责 DPI 换算）。
pub fn create_composition_swapchain(
    device: &IUnknown,
    size: (u32, u32),
    desc: &SwapchainDesc,
) -> windows::core::Result<IDXGISwapChain1> {
    let factory = create_factory()?;
    let dxgi_desc = base_swapchain_desc(size.0, size.1, desc.format, desc.buffer_count);
    unsafe { factory.CreateSwapChainForComposition(device, &dxgi_desc, None) }
}

/// 枚举双缓冲的一个后缓冲（P0 渲染循环形态：GetBuffer×2 → CopyResource → Present1）。
///
/// # Safety
/// swapchain 必须有效；T 须为 ID3D11Texture2D / ID3D12Resource 等资源接口。
pub unsafe fn get_buffer<T: Interface>(swapchain: &IDXGISwapChain1, index: u32) -> windows::core::Result<T> {
    swapchain.GetBuffer(index)
}
