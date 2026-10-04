//! VIEWSTAGE_DCOMP 门控的 DirectComposition 合成 surface（P2 主视觉接管）。
//!
//! 链路与 P0 spike（crates/spike-dcomp 的 wgpu 默认模式）完全一致：
//!
//! 1. wgpu Device `as_hal::<Dx12>` 取 raw `ID3D12CommandQueue`；
//! 2. queue 的 IUnknown 上 `CreateSwapChainForComposition`
//!    （FLIP_SEQUENTIAL + PREMULTIPLIED + Bgra8Unorm 双缓冲）；
//! 3. `viewcomp::DcompCompositor::new_from_rendering_device` 建 DComp 树
//!    （v2 → v3 → null 设备 fallback；本机 queue 路线 E_NOINTERFACE，null 可合成）；
//! 4. 后缓冲 `texture_from_raw` → `create_texture_from_hal` 导入 wgpu；
//! 5. 每帧：渲染直入 `views[GetCurrentBackBufferIndex()]` → Present1 → Commit。
//!
//! # P0 硬约束
//! - #1（保活）：[`DcompSurface`] 字段持有 `DcompCompositor`（device/target/
//!   visual 全部 COM 引用），target 释放 = 拆除 visual 与 HWND 绑定 = 窗口全白。
//! - #3（Present1 非 null params）：走 `viewcomp::present_one` 封装。

// wgpu-hal / Win32 COM 互操作是本模块存在的意义（P0 spike 已验证的 unsafe 面）
#![allow(unsafe_code)]

use std::sync::OnceLock;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use viewcomp::{create_composition_swapchain, present_one, SwapchainDesc};
use windows::core::{Interface, IUnknown};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::Win32::Graphics::Dxgi::{IDXGISwapChain1, IDXGISwapChain3};
#[cfg(debug_assertions)]
use windows::Win32::UI::HiDpi::GetDpiForWindow;
#[cfg(debug_assertions)]
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::core::Color;
use crate::graphics::compositor::SurfaceError;
use crate::graphics::Viewport;
use crate::Renderer;

/// VIEWSTAGE_DCOMP 环境变量门控（非 "0" 即启用；进程内只读一次）。
pub fn dcomp_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("VIEWSTAGE_DCOMP")
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

/// DComp 路径全局帧序号（诊断节流 + readback 触发用）。
static FRAME_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 从实现了 `HasWindowHandle` 的窗口取 HWND（裸 isize）。
pub fn hwnd_of<W: HasWindowHandle>(window: &W) -> Option<isize> {
    let handle = window.window_handle().ok()?;
    match RawWindowHandle::from(handle) {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// DirectComposition 合成 surface：一个 iced 窗口的全部呈现状态。
pub struct DcompSurface {
    swapchain: IDXGISwapChain1,
    swapchain3: IDXGISwapChain3,
    /// 保活字段（P0 硬约束 #1）——**绝不可删**：释放即拆绑定、窗口全白。
    compositor: viewcomp::DcompCompositor,
    /// UI 合成交换链挂在的 visual（保活；P3 媒体 overlay 时以它为 z-order 参照）。
    #[allow(dead_code)]
    ui_visual: viewcomp::VisualSlot,
    /// wgpu device（resize 时 poll 等待 GPU 空闲用）。
    device: wgpu::Device,
    /// wgpu queue（readback 提交用）。
    queue: wgpu::Queue,
    /// HWND（debug 诊断用）。
    #[cfg(debug_assertions)]
    hwnd_debug: Option<windows::Win32::Foundation::HWND>,
    /// 导入的后缓冲纹理与渲染 view（下标 = swapchain buffer index）。
    #[allow(dead_code)]
    textures: Vec<wgpu::Texture>,
    views: Vec<wgpu::TextureView>,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
}

impl DcompSurface {
    /// 在给定 HWND 上建立 composition swapchain + DComp 树并导入后缓冲。
    pub fn new(
        hwnd: isize,
        device: &wgpu::Device,
        queue: wgpu::Queue,
        width: u32,
        height: u32,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let width = width.max(1);
        let height = height.max(1);

        // 1. wgpu raw D3D12 queue（P0 spike 手法：as_hal 期间 clone COM 引用）
        let hal_guard = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }
            .ok_or("device.as_hal::<Dx12> returned None (backend is not DX12?)")?;
        let raw_queue = hal_guard.raw_queue().clone();
        drop(hal_guard);
        let queue_unknown: IUnknown = raw_queue.cast()?;

        // 2. composition swapchain（挂 wgpu 队列）
        let swapchain = create_composition_swapchain(
            &queue_unknown,
            (width, height),
            &SwapchainDesc::default(),
        )?;
        let swapchain3: IDXGISwapChain3 = swapchain.cast()?;

        // 3. DComp 树（v2 → v3 → null fallback；target 由 compositor 保活）
        let hwnd = windows::Win32::Foundation::HWND(hwnd as *mut _);
        let mut compositor =
            viewcomp::DcompCompositor::new_from_rendering_device(hwnd, Some(&queue_unknown))?;
        let ui_visual = compositor.add_visual_on_top()?;
        compositor.set_content_swapchain(&ui_visual, &swapchain)?;
        compositor.commit()?;

        // 4. 导入全部后缓冲
        let (textures, views) =
            import_backbuffers(device, &swapchain, (width, height));

        // 尺寸诊断（VIEWSTAGE_DCOMP_DEBUG=1）：交换链尺寸必须与窗口客户区
        // 物理像素一致，否则 DComp 缩放采样 → 内容整体发虚（icon 首先可见）。
        #[cfg(debug_assertions)]
        if std::env::var("VIEWSTAGE_DCOMP_DEBUG").as_deref() == Ok("1") {
            let (cw, ch, dpi) = client_metrics(hwnd);
            log::info!(
                "viewstage.dcomp: swapchain {width}x{height}, client {cw}x{ch}, \
                 dpi {dpi}{}",
                if (cw, ch) == (width as i32, height as i32) {
                    " (MATCH)"
                } else {
                    " *** MISMATCH ***"
                }
            );
        }

        Ok(Self {
            swapchain,
            swapchain3,
            compositor,
            ui_visual,
            device: device.clone(),
            queue,
            #[cfg(debug_assertions)]
            hwnd_debug: Some(hwnd),
            textures,
            views,
            format: wgpu::TextureFormat::Bgra8Unorm,
            width,
            height,
        })
    }

    /// 物理尺寸变更：等 GPU 空闲 → ResizeBuffers → 重新导入。
    pub fn resize(&mut self, width: u32, height: u32) {
        if (width == self.width && height == self.height) || width == 0 || height == 0 {
            return;
        }
        // 等 GPU 排空，避免 resize 时仍有对旧后缓冲的引用
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        self.views.clear();
        self.textures.clear();
        unsafe {
            // ResizeBuffers：count=0 保持缓冲数；format=UNKNOWN 保持格式
            let _ = self.swapchain.ResizeBuffers(
                0,
                width,
                height,
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN,
                windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG(0),
            );
        }
        let (textures, views) =
            import_backbuffers(&self.device, &self.swapchain, (width, height));
        self.textures = textures;
        self.views = views;
        self.width = width;
        self.height = height;
        if std::env::var("VIEWSTAGE_DCOMP_DEBUG").as_deref() == Ok("1") {
            #[cfg(debug_assertions)]
            if let Some(h) = self.hwnd_debug {
                let (cw, ch, _) = client_metrics(h);
                log::info!(
                    "viewstage.dcomp: resize to {width}x{height}, client {cw}x{ch}{}",
                    if (cw, ch) == (width as i32, height as i32) {
                        " (MATCH)"
                    } else {
                        " *** MISMATCH ***"
                    }
                );
            }
        }
    }

    /// 物理尺寸（诊断）。
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 呈现一帧：渲染直入当前后缓冲 → Present1 → DComp Commit。
    pub fn present(
        &self,
        renderer: &mut Renderer,
        viewport: &Viewport,
        background_color: Color,
        on_pre_present: impl FnOnce(),
    ) -> Result<(), SurfaceError> {
        let idx = unsafe { self.swapchain3.GetCurrentBackBufferIndex() } as usize;
        let Some(view) = self.views.get(idx) else {
            return Err(SurfaceError::Lost);
        };

        let _submission =
            renderer.present(Some(background_color), self.format, view, viewport);

        // 帧序号 + 节流诊断：viewport 物理尺寸 / scale_factor 必须与窗口物理
        // 尺寸 / DPI 一致，否则渲染端就在错误分辨率上出图。
        let frame = FRAME_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if std::env::var("VIEWSTAGE_DCOMP_DEBUG").as_deref() == Ok("1")
            && frame % 300 == 0
        {
            let phys = viewport.physical_size();
            log::info!(
                "viewstage.dcomp: frame {frame} viewport {}x{} scale {:.3}",
                phys.width,
                phys.height,
                viewport.scale_factor()
            );
        }

        // 一次性后缓冲读回（取证：渲染端 vs 合成层的模糊二分）。
        // VIEWSTAGE_DCOMP_READBACK=<帧号> → 该帧把后缓冲 dump 成 BMP。
        if let Ok(n) = std::env::var("VIEWSTAGE_DCOMP_READBACK") {
            if let Ok(n) = n.parse::<u32>() {
                if frame == n {
                    self.debug_readback_bmp(idx, &format!(
                        "tmp-probe/dcomp_backbuffer_f{n}.bmp"
                    ));
                }
            }
        }

        // Present the frame
        on_pre_present();
        unsafe { present_one(&self.swapchain, 1) }.map_err(|_| SurfaceError::Lost)?;
        self.compositor.commit().map_err(|_| SurfaceError::Other)?;
        Ok(())
    }

    /// 把指定后缓冲读回并写成 32 位 BMP（仅诊断用；GPU→CPU 一次拷贝）。
    fn debug_readback_bmp(&self, buffer_idx: usize, path: &str) {
        dump_texture_bmp(
            &self.device,
            &self.queue,
            &self.textures[buffer_idx],
            path,
        );
    }
}

/// 把 wgpu 纹理读回并写成 32 位 BMP（诊断取证用；GPU→CPU 一次拷贝）。
pub(crate) fn dump_texture_bmp(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    path: &str,
) {
    let (w, h) = (texture.width(), texture.height());
    let bytes_per_row = (w * 4) as u64;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewstage.readback"),
        size: bytes_per_row * h as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    let _submission = queue.submit([encoder.finish()]);
    let _ = device.poll(wgpu::PollType::wait_indefinitely());

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let Ok(Ok(())) = rx.recv() else {
        log::error!("viewstage.dcomp: readback map_async failed");
        return;
    };

    let data = slice.get_mapped_range();
    let file_header: [u8; 14] = [
        b'B', b'M', 0, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0,
    ];
    let mut info_header = [0u8; 40];
    info_header[0..4].copy_from_slice(&40u32.to_le_bytes());
    info_header[4..8].copy_from_slice(&(w as i32).to_le_bytes());
    info_header[8..12].copy_from_slice(&(h as i32).to_le_bytes());
    info_header[12..14].copy_from_slice(&1u16.to_le_bytes());
    info_header[14..16].copy_from_slice(&32u16.to_le_bytes());
    let mut out = Vec::with_capacity(54 + data.len());
    out.extend_from_slice(&file_header);
    out.extend_from_slice(&info_header);
    // BMP 行序自底向上；wgpu 拷贝布局行对齐 256B（1920*4=7680 恰好对齐）
    for row in (0..h as usize).rev() {
        let start = row * bytes_per_row as usize;
        out.extend_from_slice(&data[start..start + bytes_per_row as usize]);
    }
    drop(data);
    buffer.unmap();
    match std::fs::write(path, &out) {
        Ok(()) => log::info!("viewstage.dcomp: texture dumped to {path}"),
        Err(e) => log::error!("viewstage.dcomp: BMP write failed: {e}"),
    }
}

/// 逐帧读回并记录亮度指纹（诊断取证用）。
///
/// `VIEWSTAGE_FRAME_STATS` 指定帧范围（`start..end`，闭区间；也接受单个帧号），
/// 落在范围内的每一帧都读回一次后缓冲，打印一条：
///
/// ```text
/// [fstats] f=1234 mean=137.2 min=0 max=255 grid=<8x8 分块均亮度的 hex>
/// ```
///
/// `grid` 是把画面分成 8×8 块、每块取平均亮度后的 64 字节十六进制。用途是
/// **定位**一帧异常：逐帧比对相邻两帧的 grid，就能同时得到"哪一帧闪了"和
/// "闪在画面哪个位置"——单看平均亮度只能知道闪了，位置全无。
///
/// 代价：每帧一次 GPU→CPU 同步回读（`poll(wait_indefinitely)`），会显著拖慢
/// 渲染。**因此默认关闭**，只在排查时按帧段开启。
pub(crate) fn log_frame_stats(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    frame_no: u64,
) {
    const GRID: usize = 8;
    let (w, h) = (texture.width(), texture.height());
    if w == 0 || h == 0 {
        return;
    }
    let unpadded = w as usize * 4;
    let padded = unpadded.div_ceil(256) * 256;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewstage.frame_stats"),
        size: (padded * h as usize) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded as u32),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    let _submission = queue.submit([encoder.finish()]);
    let _ = device.poll(wgpu::PollType::wait_indefinitely());

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let Ok(Ok(())) = rx.recv() else {
        log::error!("viewstage: frame_stats map_async failed");
        return;
    };

    let data = slice.get_mapped_range();
    let mut grid = [0u8; GRID * GRID];
    let mut sum = 0f64;
    let mut min = 255u8;
    let mut max = 0u8;
    let mut block_sum = [0f64; GRID * GRID];
    let mut block_n = [0u64; GRID * GRID];

    for y in 0..h as usize {
        let row = y * padded;
        let gy = (y * GRID / h as usize).min(GRID - 1);
        for x in 0..w as usize {
            let i = row + x * 4;
            let b = data[i];
            let g = data[i + 1];
            let r = data[i + 2];
            // 递归亮度（与 sRGB 主观亮度权重一致）
            let luma = (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) as u8;
            sum += luma as f64;
            if luma < min {
                min = luma;
            }
            if luma > max {
                max = luma;
            }
            let gx = (x * GRID / w as usize).min(GRID - 1);
            let cell = gy * GRID + gx;
            block_sum[cell] += luma as f64;
            block_n[cell] += 1;
        }
    }
    for cell in 0..GRID * GRID {
        grid[cell] = if block_n[cell] > 0 {
            (block_sum[cell] / block_n[cell] as f64).round().clamp(0.0, 255.0) as u8
        } else {
            0
        };
    }
    let n = (w as u64) * (h as u64);
    let mean = sum / n as f64;
    let hex: String = grid.iter().map(|v| format!("{v:02x}")).collect();
    log::info!(
        "[fstats] f={frame_no} mean={mean:.2} min={min} max={max} grid={hex}"
    );

    drop(data);
    buffer.unmap();
}

/// 解析 `VIEWSTAGE_FRAME_STATS`：返回要采样的闭区间帧号。
///
/// 支持 `123`、`100..200`。非法值按"不采样"处理（诊断开关不该让应用起不来）。
fn frame_stats_range() -> Option<(u64, u64)> {
    use std::sync::OnceLock;
    static RANGE: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *RANGE.get_or_init(|| {
        let raw = std::env::var("VIEWSTAGE_FRAME_STATS").ok()?;
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        if let Some((a, b)) = raw.split_once("..") {
            match (a.trim().parse::<u64>(), b.trim().parse::<u64>()) {
                (Ok(a), Ok(b)) if a <= b => Some((a, b)),
                _ => None,
            }
        } else {
            raw.parse::<u64>().ok().map(|n| (n, n))
        }
    })
}

/// 该帧是否需要采样（`VIEWSTAGE_FRAME_STATS` 未设置时恒为 false）。
pub(crate) fn frame_stats_wanted(frame_no: u64) -> bool {
    frame_stats_range().is_some_and(|(a, b)| frame_no >= a && frame_no <= b)
}

/// `texture_from_raw` → `create_texture_from_hal` 导入全部后缓冲（P0 spike 手法）。
fn import_backbuffers(
    device: &wgpu::Device,
    swapchain: &IDXGISwapChain1,
    size: (u32, u32),
) -> (Vec<wgpu::Texture>, Vec<wgpu::TextureView>) {
    let buffer_count = SwapchainDesc::default().buffer_count;
    let mut textures = Vec::with_capacity(buffer_count as usize);
    let mut views = Vec::with_capacity(buffer_count as usize);
    for i in 0..buffer_count {
        let resource: ID3D12Resource =
            unsafe { swapchain.GetBuffer(i) }.expect("GetBuffer");
        let hal_tex = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                resource,
                wgpu::TextureFormat::Bgra8Unorm,
                wgpu::TextureDimension::D2,
                wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
                1,
                1,
            )
        };
        let tex = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                hal_tex,
                &wgpu::TextureDescriptor {
                    label: Some("viewstage.dcomp.backbuffer"),
                    size: wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    // COPY_SRC：诊断 readback 需要（取证后缓冲内容用）
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                },
            )
        };
        views.push(tex.create_view(&wgpu::TextureViewDescriptor::default()));
        textures.push(tex);
    }
    (textures, views)
}

/// 窗口客户区物理尺寸 + DPI（诊断用）。
#[cfg(debug_assertions)]
fn client_metrics(hwnd: windows::Win32::Foundation::HWND) -> (i32, i32, u32) {
    unsafe {
        let mut rect = windows::Win32::Foundation::RECT::default();
        let cwch = if GetClientRect(hwnd, &mut rect).is_ok() {
            (rect.right - rect.left, rect.bottom - rect.top)
        } else {
            (-1, -1)
        };
        (cwch.0, cwch.1, GetDpiForWindow(hwnd))
    }
}
