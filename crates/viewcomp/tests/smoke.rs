//! viewcomp 全链路冒烟测试（本机 GPU 环境运行）。
//!
//! 验证 P0 固化结论在 viewcomp 封装下依然成立：
//! 设备创建（IDXGIDevice 路线）→ target/visual → composition swapchain →
//! SetContent → Commit 全部成功，且 **compositor 生命周期内 target 保活**。
//!
//! 运行：cargo test -p viewcomp -- --nocapture

use windows::core::w;
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassExW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    HMENU, HWND_TOPMOST, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos, WINDOW_EX_STYLE, WINDOW_STYLE,
    WNDCLASSEXW, WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPEDWINDOW,
};
use windows::core::{IUnknown, PCWSTR};

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn create_test_window() -> HWND {
    unsafe {
        let hinstance: windows::Win32::Foundation::HINSTANCE = GetModuleHandleW(PCWSTR::null()).unwrap().into();
        let class_name = w!("VIEWCOMP_TEST");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            class_name,
            w!("viewcomp smoke"),
            WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            400,
            300,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .expect("CreateWindowExW");
        SetWindowPos(hwnd, HWND_TOPMOST, 100, 100, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW)
            .expect("SetWindowPos");
        hwnd
    }
}

fn create_d3d11_device() -> ID3D11Device {
    unsafe {
        let mut dev: Option<ID3D11Device> = None;
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
            None,
        )
        .expect("D3D11CreateDevice");
        dev.expect("device out")
    }
}

#[test]
fn full_chain_smoke() {
    unsafe {
        let _hr = CoInitializeEx(None, COINIT_MULTITHREADED);

        eprintln!("[1] window");
        let hwnd = create_test_window();
        eprintln!("[2] window ok");
        let device = create_d3d11_device();
        eprintln!("[3] d3d11 ok");
        let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice =
            device.cast().expect("QI IDXGIDevice");

        // 1. DComp 合成器（IDXGIDevice 路线）
        eprintln!("[4] dxgi ok");
        let mut comp = viewcomp::DcompCompositor::new(hwnd, &dxgi_device)
            .expect("DcompCompositor::new");
        eprintln!("[5] compositor ok");
        assert_eq!(comp.visual_count(), 0);

        // 2. composition swapchain（挂 D3D11 device 的 IUnknown）
        let mut r = RECT::default();
        windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut r)
            .expect("GetClientRect");
        let (cw, ch) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
        let swapchain_device: IUnknown = device.cast().expect("QI IUnknown");
        let swapchain = viewcomp::create_composition_swapchain(
            &swapchain_device,
            (cw, ch),
            &viewcomp::SwapchainDesc::default(),
        )
        .expect("create_composition_swapchain");
        eprintln!("[6] swapchain ok");

        // 3. visual 树：两个 visual（模拟 UI 层 + 预留层）
        let ui_slot = comp.add_visual_on_top().expect("add_visual_on_top #1");
        let _reserve_slot = comp.add_visual_above(&ui_slot).expect("add_visual_above #2");
        eprintln!("[7] visuals ok");
        assert_eq!(comp.visual_count(), 2);

        // 4. 绑定 swapchain 内容 + commit
        comp.set_content_swapchain(&ui_slot, &swapchain)
            .expect("set_content_swapchain");
        comp.commit().expect("commit #1");
        eprintln!("[8] commit ok");
        comp.wait_for_commit().expect("wait_for_commit");
        eprintln!("[9] wait ok");

        // 5. 偏移 API（IDCompositionVisual2 cast 路径）
        comp.set_offset(&ui_slot, 0.0, 0.0).expect("set_offset");

        // 6. Present（硬约束 #3：非空 params）
        viewcomp::present_one(&swapchain, 0).expect("present_one");
        eprintln!("[10] present ok");

        // 7. drop 顺序：compositor 先于窗口销毁（窗口由进程退出回收）
        comp.destroy();
        drop(comp);

        println!("full_chain_smoke: all APIs OK ({cw}x{ch})");
    }
}
