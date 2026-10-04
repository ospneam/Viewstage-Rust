//! P0 互操作 spike：验证 DComp 重写的核心链条。
//!
//! 链条：DX12 wgpu 设备 → raw_device/raw_queue → 自建 DXGI factory 的
//! composition 交换链 → `texture_from_raw` + `create_texture_from_hal`
//! 导入后缓冲回 wgpu → wgpu 清屏 → Present1 → DComp Commit。
//!
//! 自验证：渲染 60 帧品红后切换青色，`PrintWindow(PW_CLIENTONLY|2)` 抓
//! 窗口位图，读中心像素判定 PASS/FAIL（判定写日志，退出码 0/1）。
//!
//! 运行：`cargo run -p spike-dcomp`；日志：`spike-dcomp.log`（exe 同目录）。
//!
//! ## 隔离开关（环境变量，二分定位崩溃层用）
//! - `SPIKE_PURE_D3D12=1`     跳过 wgpu：D3D12CreateDevice + CreateCommandQueue 原生路径
//! - `SPIKE_NO_DCOMP=1`       跳过 DComp 设备/target/视觉树
//! - `SPIKE_NO_IMPORT=1`      跳过后缓冲导入（仅 wgpu 模式有意义）
//! - `SPIKE_NO_PASS=1`        跳过 render pass（空 encoder 提交）
//! - `SPIKE_NO_PRESENT=1`     跳过 Present1
//! - `SPIKE_ONE_FRAME=1`      每相位只跑 1 帧（快速矩阵）
//! - `SPIKE_FLIP_DISCARD=1`   SwapEffect 用 FLIP_DISCARD 而非 FLIP_SEQUENTIAL
//! - `SPIKE_ALPHA_IGNORE=1`   AlphaMode 用 IGNORE 而非 PREMULTIPLIED
//! - `SPIKE_SYNC_0=1`         Present1 sync interval 用 0 而非 1
//! - `SPIKE_NO_NOREDIR=1`     窗口不加 WS_EX_NOREDIRECTIONBITMAP
//! - `SPIKE_LEGACY_PRESENT=1` 用 IDXGISwapChain::Present 而非 Present1
//! - `SPIKE_BUF3=1`           BufferCount 用 3 而非 2

#![cfg(windows)]

use std::fs::OpenOptions;
use std::io::Write as _;
use std::sync::Mutex;

use wgpu::hal::api::Dx12;
use windows::core::{w, IUnknown, Interface, PCWSTR};
use windows::Win32::Foundation::{BOOL, HINSTANCE, HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext,
    ID3D11RenderTargetView, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12CreateDevice, D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC,
    D3D12_COMMAND_QUEUE_FLAG_NONE, ID3D12CommandQueue, ID3D12Device, ID3D12Resource,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, DCompositionCreateDevice3,
    IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIFactory4, IDXGIDevice, IDXGISwapChain1, IDXGISwapChain3,
    DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_PRESENT_PARAMETERS, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    GetPixel, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HGDIOBJ, HDC, ClientToScreen,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS, PW_CLIENTONLY};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetAncestor, GetClientRect,
    IsWindowVisible, PeekMessageW, RegisterClassExW, SetForegroundWindow, SetProcessDPIAware,
    SetWindowPos, ShowWindow, TranslateMessage, BringWindowToTop,
    WindowFromPoint, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GA_ROOT, HMENU,
    HWND_TOPMOST, MSG, PM_REMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_SHOW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WNDCLASSEXW, WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

fn log(msg: &str) {
    if let Ok(mut guard) = LOG.lock() {
        if let Some(f) = guard.as_mut() {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
        }
    }
}

fn step_ok(name: &str, ok: bool) -> bool {
    log(&format!(
        "  {} {name}",
        if ok { "ok:" } else { "FAIL:" }
    ));
    ok
}

// ---------------------------------------------------------------------------
// 隔离开关矩阵
// ---------------------------------------------------------------------------

fn flag(name: &str) -> bool {
    std::env::var(name).map(|v| v != "0").unwrap_or(false)
}

#[allow(dead_code)] // 诊断开关矩阵，按需启用
struct Config {
    d3d11: bool,
    pure_d3d12: bool,
    no_dcomp: bool,
    no_import: bool,
    no_pass: bool,
    no_present: bool,
    one_frame: bool,
    flip_discard: bool,
    alpha_ignore: bool,
    sync_0: bool,
    no_noredir: bool,
    legacy_present: bool,
    buf3: bool,
    null_params: bool,
    no_readback: bool,
    no_dpiaware: bool,
    show_after_commit: bool,
    surface_test: bool,
}

impl Config {
    fn from_env() -> Self {
        Self {
            d3d11: flag("SPIKE_D3D11"),
            pure_d3d12: flag("SPIKE_PURE_D3D12"),
            no_dcomp: flag("SPIKE_NO_DCOMP"),
            no_import: flag("SPIKE_NO_IMPORT"),
            no_pass: flag("SPIKE_NO_PASS"),
            no_present: flag("SPIKE_NO_PRESENT"),
            one_frame: flag("SPIKE_ONE_FRAME"),
            flip_discard: flag("SPIKE_FLIP_DISCARD"),
            alpha_ignore: flag("SPIKE_ALPHA_IGNORE"),
            sync_0: flag("SPIKE_SYNC_0"),
            no_noredir: flag("SPIKE_NO_NOREDIR"),
            legacy_present: flag("SPIKE_LEGACY_PRESENT"),
            buf3: flag("SPIKE_BUF3"),
            null_params: flag("SPIKE_NULL_PARAMS"),
            no_readback: flag("SPIKE_NO_READBACK"),
            no_dpiaware: flag("SPIKE_NO_DPIAWARE"),
            show_after_commit: flag("SPIKE_SHOW_AFTER_COMMIT"),
            surface_test: flag("SPIKE_SURFACE_TEST"),
        }
    }

    /// 输出非默认开关（矩阵运行时写进日志头，方便对照）。
    fn describe(&self) -> String {
        let mut on: Vec<&str> = Vec::new();
        if self.d3d11 { on.push("D3D11"); }
        if self.pure_d3d12 { on.push("PURE_D3D12"); }
        if self.no_dcomp { on.push("NO_DCOMP"); }
        if self.no_import { on.push("NO_IMPORT"); }
        if self.no_pass { on.push("NO_PASS"); }
        if self.no_present { on.push("NO_PRESENT"); }
        if self.one_frame { on.push("ONE_FRAME"); }
        if self.flip_discard { on.push("FLIP_DISCARD"); }
        if self.alpha_ignore { on.push("ALPHA_IGNORE"); }
        if self.sync_0 { on.push("SYNC_0"); }
        if self.no_noredir { on.push("NO_NOREDIR"); }
        if self.legacy_present { on.push("LEGACY_PRESENT"); }
        if self.buf3 { on.push("BUF3"); }
        if self.null_params { on.push("NULL_PARAMS"); }
        if on.is_empty() {
            "default (wgpu + dcomp + import + pass + present)".into()
        } else {
            on.join("+")
        }
    }
}

// ---------------------------------------------------------------------------
// 窗口与探针
// ---------------------------------------------------------------------------

fn create_window() -> HWND {
    unsafe {
        // C++ 对照版无 SetProcessDPIAware 也能正常合成；DPI 感知改变 DWM 对窗口的
        // 合成路径，是当前头号嫌疑。SPIKE_NO_DPIAWARE=1 对照。
        if !flag("SPIKE_NO_DPIAWARE") {
            let _ = SetProcessDPIAware();
        }
        let hinstance: HINSTANCE = GetModuleHandleW(PCWSTR::null())
            .expect("GetModuleHandleW")
            .into(); // HMODULE -> HINSTANCE
        let class_name = w!("SPIKE_DCOMP");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            if flag("SPIKE_NO_NOREDIR") {
                WINDOW_EX_STYLE(0) // SPIKE_NO_NOREDIR：保留 GDI redirection
            } else {
                WS_EX_NOREDIRECTIONBITMAP
            },
            class_name,
            w!("DCOMP SPIKE"),
            WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 | WS_VISIBLE.0),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            800,
            600,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .expect("CreateWindowExW");
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd); // 探针读屏幕像素，窗口必须可见且不被遮挡
        // 后台进程抢不到前台（Windows 前台锁定策略），SetForegroundWindow 可能无效；
        // TOPMOST 置顶不受前台锁定限制，保证探针点不被遮挡。固定位置便于采样。
        // SPIKE_SHOW_AFTER_COMMIT=1 时跳过此处显示，改为 DComp Commit 之后再显示。
        // ⚠ 关键：CreateTargetForHwnd 的 topmost 参数必须与窗口实际 topmost 状态一致，
        // 否则 DComp visual 绑错合成层不显示（C++ 对照版实证）。SPIKE_NO_TOPMOST 对照。
        if !flag("SPIKE_SHOW_AFTER_COMMIT") && !flag("SPIKE_NO_TOPMOST") {
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                100,
                100,
                0,
                0,
                SWP_NOSIZE | SWP_SHOWWINDOW,
            );
        }
        hwnd
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    unsafe {
        let mut r = RECT::default();
        GetClientRect(hwnd, &mut r).expect("GetClientRect");
        (
            (r.right - r.left).max(1) as u32,
            (r.bottom - r.top).max(1) as u32,
        )
    }
}

/// 窗口客户区自探针：PrintWindow(客户端 | 全内容) 抓帧 → GetDIBits 读中心像素。
/// 返回 BGRA 中的 (B, G, R)。
fn probe_center_pixel(hwnd: HWND) -> Option<[u8; 3]> {
    unsafe {
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect).ok()?;
        let w = (rect.right - rect.left).max(1);
        let h = (rect.bottom - rect.top).max(1);

        let hdc_window: HDC = GetDC(hwnd);
        let hdc_mem = CreateCompatibleDC(hdc_window);
        let bmp = CreateCompatibleBitmap(hdc_window, w, h);
        let old = SelectObject(hdc_mem, HGDIOBJ(bmp.0));

        // windows 0.58 的 flags 新类型未实现 BitOr：PW_CLIENTONLY(1) | PW_RENDERFULLCONTENT(2)
        // 后者抓 DComp/DWM 合成内容（0.58 metadata 未导出该常量，用字面量 2）。
        let printed =
            PrintWindow(hwnd, hdc_mem, PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | 2)).as_bool();

        let mut px: Option<[u8; 3]> = None;
        if printed {
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0 as u32;
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let copied = GetDIBits(
                hdc_mem,
                bmp,
                0,
                h as u32,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            if copied == h {
                let cx = w / 2;
                let cy = h / 2;
                let o = ((cy * w + cx) * 4) as usize;
                px = Some([buf[o], buf[o + 1], buf[o + 2]]);
            }
        }

        SelectObject(hdc_mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(hdc_mem);
        let _ = ReleaseDC(hwnd, hdc_window);
        px
    }
}

/// 屏幕 DC 探针（网格版）：客户区 3x3 网格采样 GetPixel（DWM 合成输出的真实结果），
/// 同时用 WindowFromPoint 检查每个采样点实际命中的顶层窗口（遮挡校验）。
/// NOREDIRECTIONBITMAP 窗口无 redirection bitmap，PrintWindow 抓不到 DComp 视觉，
/// 屏幕探针是权威判定。返回 (各采样点 RGB, 中心点命中的顶层窗口)。
fn probe_screen_grid(hwnd: HWND) -> (Vec<Option<[u8; 3]>>, Option<HWND>) {
    unsafe {
        let mut r = RECT::default();
        if GetClientRect(hwnd, &mut r).is_err() {
            return (Vec::new(), None);
        }
        let cw = r.right - r.left;
        let ch = r.bottom - r.top;
        let dc: HDC = GetDC(HWND::default()); // 屏幕 DC
        let mut samples = Vec::new();
        let mut center_hit: Option<HWND> = None;
        for gy in 1..=3usize {
            for gx in 1..=3usize {
                // 1/4、1/2、3/4 位置，避开标题栏与窗口边框
                let mut pt = POINT {
                    x: cw * gx as i32 / 4,
                    y: ch * gy as i32 / 4,
                };
                let _ = ClientToScreen(hwnd, &mut pt);
                let hit_root = GetAncestor(WindowFromPoint(pt), GA_ROOT);
                if gx == 2 && gy == 2 {
                    center_hit = Some(hit_root);
                }
                let c = GetPixel(dc, pt.x, pt.y);
                samples.push(if c.0 == u32::MAX {
                    None // CLR_INVALID
                } else {
                    Some([
                        (c.0 & 0xFF) as u8,
                        ((c.0 >> 8) & 0xFF) as u8,
                        ((c.0 >> 16) & 0xFF) as u8,
                    ])
                });
            }
        }
        let _ = ReleaseDC(HWND::default(), dc);
        (samples, center_hit)
    }
}

// ---------------------------------------------------------------------------
// 设备路径：wgpu / 纯 D3D12 / D3D11（Chrome 路线对照）
// ---------------------------------------------------------------------------

/// DComp 设备的内容来源：
/// - D3D12Queue：DCompositionCreateDevice2/3 传 ID3D12CommandQueue（Win10 2004+，
///   本机 Intel 驱动实测 E_NOINTERFACE）
/// - DxgiDevice：DCompositionCreateDevice2 传 IDXGIDevice（Chrome 实际路线）
enum DCompSource {
    D3D12Queue(ID3D12CommandQueue),
    DxgiDevice(IDXGIDevice),
}

#[allow(dead_code)]
struct DevicePath {
    /// CreateSwapChainForComposition 的 pdevice（D3D12: command queue；D3D11: device）
    swapchain_device: IUnknown,
    dcomp_source: DCompSource,
    /// wgpu 模式的设备与队列（保活）
    wgpu: Option<(wgpu::Device, wgpu::Queue)>,
    /// 纯 D3D12 模式（保活）
    pure: Option<(ID3D12Device, ID3D12CommandQueue)>,
    /// D3D11 模式（保活）
    d3d11: Option<(ID3D11Device, ID3D11DeviceContext)>,
}

fn init_device(cfg: &Config) -> DevicePath {
    if cfg.d3d11 {
        log("warn: SPIKE_D3D11 set - Chrome-style D3D11 + IDXGIDevice + DComp path");
        unsafe {
            let mut dev: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            let adapter: Option<&windows::Win32::Graphics::Dxgi::IDXGIAdapter> = None;
            D3D11CreateDevice::<Option<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, HMODULE>(
                adapter,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
            .expect("D3D11CreateDevice");
            let device = dev.expect("ID3D11Device out param");
            let context = ctx.expect("ID3D11DeviceContext out param");
            let dxgi_device: IDXGIDevice = device.cast().expect("QI IDXGIDevice");
            log("ok: D3D11 device + immediate context created");
            DevicePath {
                swapchain_device: device.cast::<IUnknown>().expect("QI IUnknown"),
                dcomp_source: DCompSource::DxgiDevice(dxgi_device),
                wgpu: None,
                pure: None,
                d3d11: Some((device, context)),
            }
        }
    } else if cfg.pure_d3d12 {
        log("warn: SPIKE_PURE_D3D12 set - native D3D12 path (no wgpu render)");
        unsafe {
            let mut dev: Option<ID3D12Device> = None;
            D3D12CreateDevice::<_, ID3D12Device>(
                None::<&IUnknown>,
                D3D_FEATURE_LEVEL_11_0,
                &mut dev,
            )
            .expect("D3D12CreateDevice");
            let device = dev.expect("ID3D12Device out param");
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    Priority: 0,
                    Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
                    NodeMask: 0,
                })
                .expect("CreateCommandQueue");
            log("ok: native D3D12 device + direct queue created");
            DevicePath {
                swapchain_device: queue.clone().cast::<IUnknown>().expect("QI IUnknown"),
                dcomp_source: DCompSource::D3D12Queue(queue.clone()),
                wgpu: None,
                pure: Some((device, queue)),
                d3d11: None,
            }
        }
    } else {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..Default::default()
        });
        let adapter = pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        )
        .expect("request adapter");
        let info = adapter.get_info();
        log(&format!(
            "ok: adapter = {} (backend {:?})",
            info.name, info.backend
        ));
        if info.backend != wgpu::Backend::Dx12 {
            panic!("backend is not Dx12");
        }
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("spike-dcomp.device"),
                ..Default::default()
            }))
            .expect("request device");

        // HAL 设备/队列（guard 期间 clone COM 引用，guard 可随后释放）
        let hal_guard = unsafe { device.as_hal::<Dx12>() };
        let Some(hal_guard) = hal_guard else {
            panic!("device.as_hal::<Dx12> returned None");
        };
        let raw_queue = hal_guard.raw_queue().clone();
        log("ok: raw ID3D12Device + ID3D12CommandQueue acquired");
        DevicePath {
            swapchain_device: raw_queue.clone().cast::<IUnknown>().expect("QI IUnknown"),
            dcomp_source: DCompSource::D3D12Queue(raw_queue),
            wgpu: Some((device, queue)),
            pure: None,
            d3d11: None,
        }
    }
}

fn create_swapchain(
    swapchain_device: &IUnknown,
    size: (u32, u32),
    cfg: &Config,
) -> (IDXGISwapChain1, IDXGISwapChain3) {
    let factory: IDXGIFactory4 =
        unsafe { CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .expect("CreateDXGIFactory2");
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: size.0,
        Height: size.1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: if cfg.buf3 { 3 } else { 2 },
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: if cfg.flip_discard {
            DXGI_SWAP_EFFECT_FLIP_DISCARD
        } else {
            DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL
        },
        AlphaMode: if cfg.alpha_ignore {
            DXGI_ALPHA_MODE_IGNORE
        } else {
            DXGI_ALPHA_MODE_PREMULTIPLIED
        },
        Flags: 0,
    };
    let swapchain: IDXGISwapChain1 = unsafe {
        factory
            .CreateSwapChainForComposition(swapchain_device, &desc, None::<&windows::Win32::Graphics::Dxgi::IDXGIOutput>)
            .expect("CreateSwapChainForComposition")
    };
    log(&format!(
        "ok: composition swapchain created (2 buffers, {:?}, {:?})",
        desc.SwapEffect, desc.AlphaMode
    ));
    let swapchain3: IDXGISwapChain3 = swapchain.cast().expect("cast IDXGISwapChain3");
    (swapchain, swapchain3)
}

/// texture_from_raw → create_texture_from_hal 导入两个后缓冲。
fn import_backbuffers(
    device: &wgpu::Device,
    swapchain: &IDXGISwapChain1,
    size: (u32, u32),
) -> Vec<wgpu::TextureView> {
    let (cw, ch) = size;
    let mut views = Vec::new();
    for i in 0..2u32 {
        let resource: ID3D12Resource = unsafe { swapchain.GetBuffer(i) }.expect("GetBuffer");
        let hal_tex = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                resource,
                wgpu::TextureFormat::Bgra8Unorm,
                wgpu::TextureDimension::D2,
                wgpu::Extent3d {
                    width: cw,
                    height: ch,
                    depth_or_array_layers: 1,
                },
                1,
                1,
            )
        };
        let tex = unsafe {
            device.create_texture_from_hal::<Dx12>(
                hal_tex,
                &wgpu::TextureDescriptor {
                    label: Some("spike.backbuffer"),
                    size: wgpu::Extent3d {
                        width: cw,
                        height: ch,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
            )
        };
        views.push(tex.create_view(&wgpu::TextureViewDescriptor::default()));
        drop(tex); // view 持有 Arc，wgpu 壳释放安全；COM 引用归 swapchain 管
    }
    log("ok: both backbuffers imported into wgpu");
    views
}

/// DComp 设备 + target + 视觉树。来源按 DCompSource 决定：
/// D3D12 queue（v2→v3 尝试，失败回退 null）或 IDXGIDevice（Chrome 路线，直接成功）。
fn setup_dcomp(
    source: &DCompSource,
    swapchain: &IDXGISwapChain1,
    hwnd: HWND,
) -> (IDCompositionDevice, IDCompositionTarget) {
    if flag("SPIKE_SURFACE_TEST") {
        // 决定性二分：D2D 完整路线 CPU surface 直通测试，完全绕开 swapchain。
        // 微软文档标准的 v2 surface 更新路线：D2D1CreateDevice →
        // DCompositionCreateDevice2(d2d_device) → BeginDraw(ID2D1DeviceContext)。
        // surface 显示 → DComp 合成链路（target/visual/commit/窗口）OK，
        // 问题锁定在 swapchain+visual 结合；不显示 → 链路本身断了。
        let dxgi_device = match source {
            DCompSource::DxgiDevice(d) => d,
            _ => panic!("surface test requires DxgiDevice source"),
        };
        let d2d_device: windows::Win32::Graphics::Direct2D::ID2D1Device =
            unsafe {
                windows::Win32::Graphics::Direct2D::D2D1CreateDevice(dxgi_device, None)
            }
            .expect("D2D1CreateDevice");
        log("surface-test: D2D device created");
        let dcomp: IDCompositionDevice =
            unsafe { DCompositionCreateDevice2::<_, IDCompositionDevice>(&d2d_device) }
                .expect("DCompositionCreateDevice2 (D2D)");
        log("surface-test: v2 DComp device created (from D2D)");
        let target: IDCompositionTarget =
            unsafe { dcomp.CreateTargetForHwnd(hwnd, BOOL(0)) }.expect("CreateTargetForHwnd");
        let root: IDCompositionVisual =
            unsafe { dcomp.CreateVisual() }.expect("CreateVisual");
        let surface = unsafe {
            dcomp.CreateSurface(128, 128, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_PREMULTIPLIED)
        }
        .expect("CreateSurface");
        log("surface-test: surface created");
        let rect = RECT { left: 0, top: 0, right: 128, bottom: 128 };
        let mut offset = POINT::default();
        let ctx: windows::Win32::Graphics::Direct2D::ID2D1DeviceContext =
            unsafe { surface.BeginDraw(Some(&rect as *const _), &mut offset) }
                .expect("BeginDraw (ID2D1DeviceContext)");
        log(&format!(
            "surface-test: begin draw ok (offset = {}, {})",
            offset.x, offset.y
        ));
        unsafe {
            ctx.Clear(Some(&windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
                r: 1.0,
                g: 0.0,
                b: 1.0,
                a: 1.0,
            }))
        };
        log("surface-test: d2d clear done (magenta)");
        unsafe { surface.EndDraw() }.expect("EndDraw");
        log("surface-test: end draw ok");
        unsafe {
            root.SetContent(&surface).expect("SetContent(surface)");
            // 128x128 surface 缩放铺满客户区，网格探针可直接采样中心区域
            let mut cr = RECT::default();
            GetClientRect(hwnd, &mut cr).expect("GetClientRect");
            let (cw, ch) = (cr.right as f32, cr.bottom as f32);
            root.SetTransform2(&windows::Foundation::Numerics::Matrix3x2 {
                M11: cw / 128.0,
                M12: 0.0,
                M21: 0.0,
                M22: ch / 128.0,
                M31: 0.0,
                M32: 0.0,
            })
            .expect("SetTransform2");
            target.SetRoot(&root).expect("SetRoot");
            dcomp.Commit().expect("Commit #1");
        }
        log("ok: DComp tree committed (root visual = v1 CPU surface, scaled to fill)");
        return (dcomp, target);
    }
    let dcomp: IDCompositionDevice = unsafe {
        match source {
            DCompSource::D3D12Queue(queue) => {
                match DCompositionCreateDevice2::<_, IDCompositionDevice>(queue) {
                    Ok(d) => {
                        log("ok: DComp device from ID3D12CommandQueue (v2)");
                        d
                    }
                    Err(e2) => {
                        match DCompositionCreateDevice3::<_, IDCompositionDevice>(queue) {
                            Ok(d) => {
                                log("ok: DComp device from ID3D12CommandQueue (v3)");
                                d
                            }
                            Err(e3) => {
                                log(&format!(
                                    "warn: DComp from queue failed both v2 ({e2}) and v3 ({e3}); \
                                     falling back to null rendering device"
                                ));
                                DCompositionCreateDevice2::<_, IDCompositionDevice>(None::<&IUnknown>)
                                    .expect("DCompositionCreateDevice2(null)")
                            }
                        }
                    }
                }
            }
            DCompSource::DxgiDevice(dxgi_device) => {
                DCompositionCreateDevice2::<_, IDCompositionDevice>(dxgi_device)
                    .expect("DCompositionCreateDevice2 from IDXGIDevice")
            }
        }
    };
    let target: IDCompositionTarget =
        unsafe { dcomp.CreateTargetForHwnd(hwnd, BOOL(0)) }.expect("CreateTargetForHwnd");
    let root: IDCompositionVisual = unsafe { dcomp.CreateVisual() }.expect("CreateVisual");
    unsafe {
        root.SetContent(swapchain).expect("SetContent");
        target.SetRoot(&root).expect("SetRoot");
        dcomp.Commit().expect("Commit #1");
    }
    log("ok: DComp tree committed (root visual = composition swapchain)");
    (dcomp, target)
}

/// 渲染循环：两相位清屏（品红→青）+ 每相位像素探针。
/// 各步骤按 cfg 开关启停，首帧分步 trace。
fn render_loop(
    cfg: &Config,
    hwnd: HWND,
    size: (u32, u32),
    swapchain3: &IDXGISwapChain3,
    swapchain: &IDXGISwapChain1,
    dcomp: Option<&IDCompositionDevice>,
    wgpu: Option<(&wgpu::Device, &wgpu::Queue, &[wgpu::TextureView])>,
    d3d11: Option<(&ID3D11Device, &ID3D11DeviceContext)>,
) -> bool {
    let (cw, ch) = size;
    let colors = [(1.0f64, 0.0, 1.0), (0.0, 1.0, 1.0)];
    let frames = if cfg.one_frame { 1 } else { 60 };
    let mut pass = true;
    let mut d3d11_solid: Option<ID3D11Texture2D> = None;
    let mut d3d11_staging: Option<ID3D11Texture2D> = None;

    for phase in 0..2usize {
        for f in 0..frames {
            let trace = f == 0;
            let idx = unsafe { swapchain3.GetCurrentBackBufferIndex() } as usize;
            if trace {
                log(&format!("frame0: backbuffer index = {idx}"));
            }

            // wgpu 渲染（纯 D3D12 模式无渲染，present 空缓冲）
            if let Some((device, queue, views)) = wgpu {
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("spike.frame"),
                });
                if trace {
                    log("frame0: encoder created");
                }
                if cfg.no_pass {
                    if trace {
                        log("frame0: SPIKE_NO_PASS set - skipping render pass");
                    }
                } else {
                    {
                        let rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("spike.clear"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &views[idx],
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color {
                                        r: colors[phase].0,
                                        g: colors[phase].1,
                                        b: colors[phase].2,
                                        a: 1.0,
                                    }),
                                    store: wgpu::StoreOp::Store,
                                },
                                depth_slice: None,
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                        });
                        drop(rpass);
                    }
                    if trace {
                        log("frame0: render pass encoded (clear only)");
                    }
                }
                queue.submit(Some(encoder.finish()));
                if trace {
                    log("frame0: submitted");
                }
            }

            // D3D11 渲染（Chrome 路线）：自建纹理清屏 → CopyResource 拷进两个后缓冲。
            // 注：swapchain buffer 的 CreateRenderTargetView 在本机直接 E_INVALIDARG
            // （bind=0x20 正常，疑似 FLIP composition buffer 限制），绕开。
            if let Some((device, context)) = d3d11 {
                if !cfg.no_pass {
                    // 相位首帧：建同尺寸纹理 + 清成当相位色
                    if f == 0 {
                        let desc = D3D11_TEXTURE2D_DESC {
                            Width: cw,
                            Height: ch,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Usage: D3D11_USAGE_DEFAULT,
                            BindFlags: 0x20, // D3D11_BIND_RENDER_TARGET
                            CPUAccessFlags: 0,
                            MiscFlags: 0,
                        };
                        let mut tex_opt: Option<ID3D11Texture2D> = None;
                        unsafe {
                            device.CreateTexture2D(&desc, None, Some(&mut tex_opt))
                        }
                        .expect("CreateTexture2D");
                        let solid = tex_opt.expect("texture out param");
                        let mut rtv_opt: Option<ID3D11RenderTargetView> = None;
                        unsafe {
                            device.CreateRenderTargetView(&solid, None, Some(&mut rtv_opt))
                        }
                        .expect("CreateRenderTargetView(solid)");
                        let rtv = rtv_opt.expect("rtv out param");
                        unsafe {
                            context.ClearRenderTargetView(
                                &rtv,
                                &[
                                    colors[phase].0 as f32,
                                    colors[phase].1 as f32,
                                    colors[phase].2 as f32,
                                    1.0,
                                ],
                            )
                        };
                        d3d11_solid = Some(solid);
                        log("frame0: d3d11 solid texture cleared");
                    }
                    // 每帧：拷贝到两个后缓冲
                    if let Some(solid) = &d3d11_solid {
                        for i in 0..2u32 {
                            let buffer: ID3D11Resource =
                                unsafe { swapchain.GetBuffer(i) }.expect("GetBuffer");
                            unsafe { context.CopyResource(&buffer, solid) };
                        }
                        if trace {
                            log("frame0: d3d11 copied to both buffers");
                        }

                        // Readback 验证：present 前读回 backbuffer0 中心像素。
                        // 决定性分界：buffer 里若已是相位色 → 内容正确，问题在
                        // DComp 合成层；若透明黑 → CopyResource/Clear 没生效。
                        // 注意：Map 需在 present 前（present 后 buffer 归 DWM，
                        // C++ 对照版无 readback——SPIKE_NO_READBACK 验证其影响）。
                        if !cfg.no_readback {
                        let staging: &ID3D11Texture2D = d3d11_staging.get_or_insert_with(|| {
                            let sdesc = D3D11_TEXTURE2D_DESC {
                                Width: cw,
                                Height: ch,
                                MipLevels: 1,
                                ArraySize: 1,
                                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                                Usage: D3D11_USAGE_STAGING,
                                BindFlags: 0,
                                CPUAccessFlags: 0x20000, // D3D11_CPU_ACCESS_READ
                                MiscFlags: 0,
                            };
                            let mut so: Option<ID3D11Texture2D> = None;
                            unsafe { device.CreateTexture2D(&sdesc, None, Some(&mut so)) }
                                .expect("CreateTexture2D(staging)");
                            so.expect("staging out param")
                        });
                        let buf0: ID3D11Resource =
                            unsafe { swapchain.GetBuffer(0) }.expect("GetBuffer(0) readback");
                        unsafe { context.CopyResource(staging, &buf0) };
                        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                        unsafe {
                            context.Map(
                                staging,
                                0,
                                D3D11_MAP_READ,
                                0,
                                Some(&mut mapped as *mut _),
                            )
                        }
                        .expect("Map(staging)");
                        let pitch = mapped.RowPitch as usize;
                        let base =
                            (ch as usize / 2) * pitch + (cw as usize / 2) * 4;
                        let bytes =
                            unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, base + 4) };
                        let bgra = [bytes[base], bytes[base + 1], bytes[base + 2], bytes[base + 3]];
                        unsafe { context.Unmap(staging, 0) };
                        let expect_bgra: [u8; 4] = if phase == 0 {
                            [255, 0, 255, 255] // 品红
                        } else {
                            [255, 255, 0, 255] // 青
                        };
                        log(&format!(
                            "frame{f}: readback backbuffer0 center BGRA = {bgra:?} (expect {expect_bgra:?})"
                        ));
                        } // !cfg.no_readback
                    }
                }
            }

            // Present1
            if cfg.no_present {
                if trace {
                    log("frame0: SPIKE_NO_PRESENT set - skipping present");
                }
            } else if cfg.legacy_present {
                unsafe {
                    swapchain
                        .Present(if cfg.sync_0 { 0 } else { 1 }, DXGI_PRESENT(0))
                        .ok()
                        .expect("Present");
                }
                if trace {
                    log("frame0: presented (legacy)");
                }
            } else {
                // Present1 的 params 指针：文档允许 NULL，但部分 Intel 驱动对 null
                // 直接 AV——默认传空结构体（语义等价），SPIKE_NULL_PARAMS 复现崩溃。
                let params: *const DXGI_PRESENT_PARAMETERS = if cfg.null_params {
                    std::ptr::null()
                } else {
                    &DXGI_PRESENT_PARAMETERS::default()
                };
                unsafe {
                    swapchain
                        .Present1(
                            if cfg.sync_0 { 0 } else { 1 },
                            DXGI_PRESENT(0),
                            params,
                        )
                        .ok()
                        .expect("Present1");
                }
                if trace {
                    log("frame0: presented");
                }
            }

            // DComp Commit
            if let Some(d) = dcomp {
                unsafe { d.Commit() }.expect("Commit");
                if trace {
                    log("frame0: dcomp committed");
                }
            }

            pump_messages();
            std::thread::sleep(std::time::Duration::from_millis(16));
        }

        // 等 DWM 合成稳定再探针
        pump_messages();
        std::thread::sleep(std::time::Duration::from_millis(150));
        pump_messages();

        // 合成器深度诊断：DWM 是否认识本窗口 + DComp 合成器状态
        {
            use windows::Win32::Graphics::Dwm::{
                DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
            };
            let mut bounds = RECT::default();
            let dwm_hr = unsafe {
                DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    &mut bounds as *mut _ as *mut _,
                    std::mem::size_of::<RECT>() as u32,
                )
            };
            match dwm_hr {
                Ok(()) => log(&format!(
                    "diag: DWM knows window (extended bounds {:?})",
                    (bounds.left, bounds.top, bounds.right, bounds.bottom)
                )),
                Err(e) => log(&format!("diag: DWM does NOT know window: {e}")),
            }
        }
        if let Some(d) = dcomp {
            let mut stats = Default::default();
            let hr = unsafe { d.GetFrameStatistics(&mut stats) };
            match hr {
                Ok(()) => log(&format!(
                    "diag: dcomp frame stats: lastFrameTime = {}, currentTime = {}, rate = {}/{}",
                    stats.lastFrameTime,
                    stats.currentTime,
                    stats.currentCompositionRate.Numerator,
                    stats.currentCompositionRate.Denominator
                )),
                Err(e) => log(&format!("diag: GetFrameStatistics failed: {e}")),
            }
        }

        // 双探针：PrintWindow 在 NOREDIRECTIONBITMAP 窗口上抓不到 DComp 视觉（已知局限），
        // 屏幕 DC 网格采样读 DWM 合成后的真实输出，是权威判定。
        let print_px = probe_center_pixel(hwnd);
        let (samples, center_hit) = probe_screen_grid(hwnd);
        log(&format!(
            "probe: probe-point top window = {}",
            if center_hit == Some(hwnd) {
                "self (unoccluded)"
            } else {
                "OTHER (probe point occluded - see grid)"
            }
        ));
        log(&format!(
            "SELF-PROBE phase {phase}: printwindow BGR = {print_px:?}, screen grid = {samples:?}"
        ));
        let content_skipped = cfg.pure_d3d12 || cfg.no_pass;
        let expect = if phase == 0 {
            [255u8, 0, 255] // 品红 R,G,B
        } else {
            [0, 255, 255] // 青 R,G,B
        };
        if content_skipped {
            log(&format!(
                "  verdict skipped (content-skipped run; ref R,G,B = {expect:?})"
            ));
        } else {
            let matched = samples
                .iter()
                .flatten()
                .filter(|rgb| {
                    (rgb[0] as i32 - expect[0] as i32).abs() < 60
                        && (rgb[1] as i32 - expect[1] as i32).abs() < 60
                        && (rgb[2] as i32 - expect[2] as i32).abs() < 60
                })
                .count();
            let total = samples.iter().flatten().count();
            // 网格 9 点中 ≥1 点匹配即判定内容已上屏（容忍部分遮挡）；
            // 全不匹配才是真 MISMATCH。
            if matched > 0 {
                log(&format!(
                    "  verdict: screen grid matched {matched}/{total} points vs expect {expect:?} → OK"
                ));
            } else {
                log(&format!(
                    "  verdict: screen grid matched 0/{total} points vs expect {expect:?} → MISMATCH"
                ));
                pass = false;
            }
        }
    }
    pass
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

fn main() {
    let log_path = std::env::current_exe()
        .map(|p| p.with_file_name("spike-dcomp.log"))
        .unwrap_or_else(|_| "spike-dcomp.log".into());
    *LOG.lock().unwrap() = Some(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .expect("open log"),
    );
    let cfg = Config::from_env();
    log("=== DCOMP SPIKE START ===");
    log(&format!("matrix: [{}]", cfg.describe()));
    std::panic::set_hook(Box::new(|info| {
        log(&format!("PANIC: {info}"));
    }));

    let run = std::panic::catch_unwind(|| spike_run(&cfg));
    let ok = matches!(run, Ok(true));
    log(&format!(
        "=== DCOMP SPIKE {} ===",
        if ok { "PASS" } else { "FAIL" }
    ));
    std::process::exit(if ok { 0 } else { 1 });
}

fn spike_run(cfg: &Config) -> bool {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if !step_ok("CoInitializeEx", hr.is_ok()) {
            return false;
        }
    }

    let hwnd = create_window();
    if !flag("SPIKE_SHOW_AFTER_COMMIT") && !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        log("FAIL: window not visible");
        return false;
    }
    log("ok: window created (WS_EX_NOREDIRECTIONBITMAP)");

    let (cw, ch) = client_size(hwnd);
    log(&format!("ok: client {cw}x{ch}"));

    // 1. 设备路径（D3D11 / 纯 D3D12 / wgpu）
    let devices = init_device(cfg);

    // 2. composition 交换链（挂在 D3D12 queue 或 D3D11 device 上）
    let (swapchain, swapchain3) = create_swapchain(&devices.swapchain_device, (cw, ch), cfg);

    // 3. 后缓冲导入（仅 wgpu 模式）
    let views: Vec<wgpu::TextureView> = match (&devices.wgpu, cfg.no_import) {
        (Some((device, _)), false) => import_backbuffers(device, &swapchain, (cw, ch)),
        _ => {
            if !cfg.no_import && !cfg.d3d11 && !cfg.pure_d3d12 {
                log("warn: NO_IMPORT implied by PURE_D3D12");
            }
            Vec::new()
        }
    };

    // 4. DComp 视觉树
    // ⚠ 关键：IDCompositionTarget/Visual 的 COM 引用必须保活到窗口销毁。
    // target 被 Drop 会销毁 DComp target 对象、拆除 visual 与 HWND 的绑定 → 窗口全白。
    #[allow(unused_variables)] // P0 硬约束 #1：target COM 引用必须保活到窗口销毁
    let (dcomp, dcomp_target_keepalive): (Option<IDCompositionDevice>, Option<IDCompositionTarget>) = if cfg.no_dcomp {
        log("warn: SPIKE_NO_DCOMP set - skipping DComp tree");
        (None, None)
    } else {
        let (d, t) = setup_dcomp(&devices.dcomp_source, &swapchain, hwnd);
        (Some(d), Some(t))
    };

    // 5. Chrome 顺序对照：先 Commit 再显示窗口（Chrome 实际初始化顺序）
    if flag("SPIKE_SHOW_AFTER_COMMIT") {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 100, 100, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
            let _ = SetForegroundWindow(hwnd);
        }
        log("ok: window shown after commit (Chrome order)");
    }

    // 6. 渲染循环 + 探针
    let wgpu_parts = devices
        .wgpu
        .as_ref()
        .map(|(d, q)| (d, q, views.as_slice()));
    let d3d11_parts = devices
        .d3d11
        .as_ref()
        .map(|(d, c)| (d, c));

    // 7. SPIKE_MINIMAL：C++ FULL 黄金参照的 Rust 精确镜像。
    // 跳过现有 main/render_loop 全部包装，只做 C++ FULL 的调用序列。
    // 若 MINIMAL 显示 → 逐项加回 Rust 特有代码二分；不显示 → 差异在更底层。
    if flag("SPIKE_MINIMAL") {
        return minimal_cxx_mirror(hwnd, (cw, ch), &swapchain, &dcomp.unwrap(), &devices);
    }

    render_loop(
        cfg,
        hwnd,
        (cw, ch),
        &swapchain3,
        &swapchain,
        dcomp.as_ref(),
        wgpu_parts,
        d3d11_parts,
    )
}

/// C++ dcomp-min.cpp /DSPIKE_FULL 的 Rust 逐行镜像：
/// 单次 Init 路径 + 60 帧×2 相位（GetBuffer×2+CopyResource+Present1+Commit+泵+16ms）
/// + 相位末探针（探针前 TOPMOST+前台，与 C++ FULL 一致）。
fn minimal_cxx_mirror(
    hwnd: HWND,
    size: (u32, u32),
    swapchain: &IDXGISwapChain1,
    dcomp: &IDCompositionDevice,
    devices: &DevicePath,
) -> bool {
    let (cw, ch) = size;
    let (device, context) = devices.d3d11.as_ref().expect("MINIMAL needs d3d11");
    // C++ FULL：solid 纹理在 Commit 前建好并清成品红
    let desc = D3D11_TEXTURE2D_DESC {
        Width: cw,
        Height: ch,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0x20,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex_opt: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex_opt)) }.expect("CreateTexture2D");
    let solid = tex_opt.expect("texture out param");
    let mut rtv_opt: Option<ID3D11RenderTargetView> = None;
    unsafe { device.CreateRenderTargetView(&solid, None, Some(&mut rtv_opt)) }
        .expect("CreateRenderTargetView");
    let rtv = rtv_opt.expect("rtv out param");
    unsafe { context.ClearRenderTargetView(&rtv, &[1.0, 0.0, 1.0, 1.0]) };

    for phase in 0..2usize {
        if phase == 1 {
            unsafe { context.ClearRenderTargetView(&rtv, &[0.0, 1.0, 1.0, 1.0]) };
        }
        for _f in 0..60 {
            for i in 0..2u32 {
                let buffer: ID3D11Resource = unsafe { swapchain.GetBuffer(i) }.expect("GetBuffer");
                unsafe { context.CopyResource(&buffer, &solid) };
            }
            let params = DXGI_PRESENT_PARAMETERS::default();
            unsafe { swapchain.Present1(1, DXGI_PRESENT(0), &params) }
                .ok()
                .expect("Present1");
            unsafe { dcomp.Commit() }.expect("Commit");
            pump_messages();
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        // 相位末探针：TOPMOST + 前台 + 1 秒（与 C++ FULL 探针完全一致）
        unsafe {
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 100, 100, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
            let _ = SetForegroundWindow(hwnd);
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
        pump_messages();
        let mut pt = POINT { x: cw as i32 / 2, y: ch as i32 / 2 };
        let _ = unsafe { ClientToScreen(hwnd, &mut pt) };
        let dc: HDC = unsafe { GetDC(HWND::default()) };
        let c = unsafe { GetPixel(dc, pt.x, pt.y) };
        let _ = unsafe { ReleaseDC(HWND::default(), dc) };
        let rgb = [(c.0 & 0xFF) as u8, ((c.0 >> 8) & 0xFF) as u8, ((c.0 >> 16) & 0xFF) as u8];
        let expect: [u8; 3] = if phase == 0 { [255, 0, 255] } else { [0, 255, 255] };
        let ok = (rgb[0] as i32 - expect[0] as i32).abs() < 60
            && (rgb[1] as i32 - expect[1] as i32).abs() < 60
            && (rgb[2] as i32 - expect[2] as i32).abs() < 60;
        log(&format!(
            "MINIMAL phase {phase}: screen RGB = {rgb:?} vs expect {expect:?} → {}",
            if ok { "OK" } else { "MISMATCH" }
        ));
        if !ok {
            return false;
        }
    }
    true
}
