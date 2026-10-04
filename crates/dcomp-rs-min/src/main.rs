//! P0 二分：无 wgpu 的纯 windows-rs DComp 最小复刻。
//! 与 C++ dcomp-min.cpp（/DSPIKE_FULL 黄金参照，已实证品红/青上屏）逐行对应。
//! 显示 → spike-dcomp 的问题来自 wgpu 链接或 spike 自身代码；
//! 不显示 → windows-rs 封装/编译层问题。
#![cfg(windows)]

use windows::core::{w, IUnknown, Interface, PCWSTR};
use windows::Win32::Foundation::{BOOL, HINSTANCE, HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext,
    ID3D11RenderTargetView, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIFactory4, IDXGIDevice, IDXGISwapChain1, IDXGISwapChain3, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_PRESENT, DXGI_PRESENT_PARAMETERS, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC, ClientToScreen, HDC};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos,
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, PeekMessageW,
    RegisterClassExW, SetForegroundWindow, ShowWindow, TranslateMessage,
    BringWindowToTop, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HMENU, HWND_TOPMOST, MSG, PM_REMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_SHOW, WINDOW_STYLE, WNDCLASSEXW, WS_EX_NOREDIRECTIONBITMAP,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn main() {
    // wgpu 链接测试：仅构造 Instance（不创建设备/不触碰 DComp），其余流程不变
    let _inst = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        println!("CoInitializeEx: {hr:?}");

        // 窗口（与 C++ 完全一致：无 SetProcessDPIAware、无 TOPMOST、无 FOREGROUND）
        let hinstance: HINSTANCE = GetModuleHandleW(PCWSTR::null()).unwrap().into();
        let class_name = w!("DCOMP_RS_MIN");
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
            WS_EX_NOREDIRECTIONBITMAP,
            class_name,
            w!("DCOMP RS MIN"),
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
        println!("CreateWindowExW: ok");
        // spike 窗口处理全套（组合实验）
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 100, 100, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
        println!("spike window handling: done");

        // D3D11 设备
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
        let device = dev.unwrap();
        let context = ctx.unwrap();
        println!("D3D11CreateDevice: ok");
        let dxgi_device: IDXGIDevice = device.cast().expect("QI IDXGIDevice");

        // composition swapchain（C++ 用 IDXGIFactory2，这里同）
        let factory: IDXGIFactory4 =
            CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)).expect("factory");
        let mut r = RECT::default();
        GetClientRect(hwnd, &mut r).expect("GetClientRect");
        let cw = (r.right - r.left) as u32;
        let ch = (r.bottom - r.top) as u32;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: cw,
            Height: ch,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: 0,
        };
        let swapchain_device: IUnknown = device.cast::<IUnknown>().expect("QI IUnknown");
        let swapchain: IDXGISwapChain1 = factory
            .CreateSwapChainForComposition(&swapchain_device, &desc, None)
            .expect("CreateSwapChainForComposition");
        let _swapchain3: IDXGISwapChain3 = swapchain.cast().expect("cast IDXGISwapChain3");
        println!("CreateSwapChainForComposition: ok ({cw}x{ch})");

        // DComp 树
        let dcomp: IDCompositionDevice =
            DCompositionCreateDevice2::<_, IDCompositionDevice>(&dxgi_device)
                .expect("DCompositionCreateDevice2");
        let target: IDCompositionTarget =
            dcomp.CreateTargetForHwnd(hwnd, BOOL(0))
                .expect("CreateTargetForHwnd");
        let root: IDCompositionVisual = dcomp.CreateVisual().expect("CreateVisual");
        root.SetContent(&swapchain).expect("SetContent");
        target.SetRoot(&root).expect("SetRoot");
        dcomp.Commit().expect("Commit");
        println!("DComp tree committed");

        // solid 纹理 + RTV + 清成品红
        let td = D3D11_TEXTURE2D_DESC {
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
        device.CreateTexture2D(&td, None, Some(&mut tex_opt)).expect("CreateTexture2D");
        let solid = tex_opt.unwrap();
        let mut rtv_opt: Option<ID3D11RenderTargetView> = None;
        device.CreateRenderTargetView(&solid, None, Some(&mut rtv_opt))
            .expect("CreateRenderTargetView");
        let rtv = rtv_opt.unwrap();
        context.ClearRenderTargetView(&rtv, &[1.0, 0.0, 1.0, 1.0]);

        // 60 帧 ×2 相位（C++ FULL 流程）
        for phase in 0..2usize {
            if phase == 1 {
                context.ClearRenderTargetView(&rtv, &[0.0, 1.0, 1.0, 1.0]);
            }
            for _f in 0..60 {
                for i in 0..2u32 {
                    let buffer: ID3D11Resource = swapchain.GetBuffer(i).expect("GetBuffer");
                    context.CopyResource(&buffer, &solid);
                }
                let params = DXGI_PRESENT_PARAMETERS::default();
                swapchain.Present1(1, DXGI_PRESENT(0), &params)
                    .ok()
                    .expect("Present1");
                dcomp.Commit().expect("Commit");
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            // 相位末探针（与 C++ FULL 探针一致）
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                100,
                100,
                0,
                0,
                SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let _ = SetForegroundWindow(hwnd);
            std::thread::sleep(std::time::Duration::from_millis(1000));
            let mut pt = POINT { x: cw as i32 / 2, y: ch as i32 / 2 };
            let _ = ClientToScreen(hwnd, &mut pt);
            let dc: HDC = GetDC(HWND::default());
            let c = GetPixel(dc, pt.x, pt.y);
            ReleaseDC(HWND::default(), dc);
            let rgb = [(c.0 & 0xFF) as u8, ((c.0 >> 8) & 0xFF) as u8, ((c.0 >> 16) & 0xFF) as u8];
            let expect: [u8; 3] = if phase == 0 { [255, 0, 255] } else { [0, 255, 255] };
            let ok = (rgb[0] as i32 - expect[0] as i32).abs() < 60
                && (rgb[1] as i32 - expect[1] as i32).abs() < 60
                && (rgb[2] as i32 - expect[2] as i32).abs() < 60;
            println!(
                "phase {phase}: screen RGB = {rgb:?} vs expect {expect:?} → {}",
                if ok { "OK" } else { "MISMATCH" }
            );
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
}
