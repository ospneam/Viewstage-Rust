// P0 spike 对照组：与 spike-dcomp（Rust）逐行对应的最小 C++ 版。
// D3D11 → composition swapchain → DCompositionCreateDevice2(IDXGIDevice)
// → CreateTargetForHwnd → visual.SetContent → Commit → 清屏 CopyResource → Present1。
// 品红显示 5 秒后退出。若 C++ 版显示而 Rust 版不显示，差异在 Rust 代码；
// 若两者都不显示，本机驱动/系统 DComp 合成有问题。
#ifndef UNICODE
#define UNICODE
#endif
#ifndef _UNICODE
#define _UNICODE
#endif
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_3.h>
#include <dcomp.h>
#include <cstdio>

#pragma comment(lib, "d3d11.lib")
#pragma comment(lib, "dxgi.lib")
#pragma comment(lib, "dcomp.lib")
#pragma comment(lib, "user32.lib")
#pragma comment(lib, "gdi32.lib")

LRESULT CALLBACK WndProc(HWND h, UINT m, WPARAM w, LPARAM l) {
    return DefWindowProc(h, m, w, l);
}

int main() {
    HRESULT hr;
    hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    printf("CoInitializeEx: 0x%08X\n", hr);

    // 窗口（WS_EX_NOREDIRECTIONBITMAP，与 Rust 版一致）
    WNDCLASSEX wc = { sizeof(wc) };
    wc.style = CS_HREDRAW | CS_VREDRAW;
    wc.lpfnWndProc = WndProc;
    wc.hInstance = GetModuleHandle(nullptr);
    wc.lpszClassName = L"DCOMP_SPIKE_CPP";
    RegisterClassEx(&wc);
    HWND hwnd = CreateWindowEx(WS_EX_NOREDIRECTIONBITMAP, wc.lpszClassName, L"DCOMP SPIKE CPP",
                               WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100, 800, 600,
                               nullptr, nullptr, wc.hInstance, nullptr);
    printf("CreateWindowEx: hwnd=%p\n", (void*)hwnd);
    // Rust 版行为注入测试 1：窗口创建后立即 TOPMOST 置顶
#ifdef SPIKE_TOPMOST
    SetWindowPos(hwnd, HWND_TOPMOST, 100, 100, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
    printf("SPIKE_TOPMOST: SetWindowPos(HWND_TOPMOST) done\n");
#endif
    // Rust 版行为注入测试 2：SetProcessDPIAware
#ifdef SPIKE_DPIAWARE
    SetProcessDPIAware();
    printf("SPIKE_DPIAWARE: SetProcessDPIAware done\n");
#endif
    // Rust 版行为注入测试 5：SetForegroundWindow + BringWindowToTop（DComp 设备创建前）
#ifdef SPIKE_FOREGROUND
    BringWindowToTop(hwnd);
    SetForegroundWindow(hwnd);
    printf("SPIKE_FOREGROUND: SetForegroundWindow/BringWindowToTop done\n");
#endif

    // D3D11 设备
    ID3D11Device* dev = nullptr;
    ID3D11DeviceContext* ctx = nullptr;
    hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
                           D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION,
                           &dev, nullptr, &ctx);
    printf("D3D11CreateDevice: 0x%08X\n", hr);

    IDXGIDevice* dxgiDev = nullptr;
    dev->QueryInterface(&dxgiDev);

    // composition swapchain
    IDXGIFactory2* factory = nullptr;
    CreateDXGIFactory2(0, __uuidof(IDXGIFactory2), (void**)&factory);
    DXGI_SWAP_CHAIN_DESC1 scd = {};
    scd.Width = 782; scd.Height = 553;
    scd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    scd.SampleDesc.Count = 1;
    scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    scd.BufferCount = 2;
    scd.Scaling = DXGI_SCALING_STRETCH;
    scd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
    scd.AlphaMode = DXGI_ALPHA_MODE_PREMULTIPLIED;
    IDXGISwapChain1* swap = nullptr;
    hr = factory->CreateSwapChainForComposition(dev, &scd, nullptr, &swap);
    printf("CreateSwapChainForComposition: 0x%08X\n", hr);

    // Rust 版行为注入测试 3：composition swapchain 上调用 GetCurrentBackBufferIndex
    // （文档：composition swapchain 不支持此调用；Rust 版每帧调用）
#ifdef SPIKE_GETBACKIDX
    {
        IDXGISwapChain3* swap3 = nullptr;
        swap->QueryInterface(__uuidof(IDXGISwapChain3), (void**)&swap3);
        if (swap3) {
            UINT idx = swap3->GetCurrentBackBufferIndex();
            printf("SPIKE_GETBACKIDX: GetCurrentBackBufferIndex = %u\n", idx);
            swap3->Release();
        }
    }
#endif
    // Rust 版行为注入测试 4：消息泵（Rust 每帧 PeekMessage 循环）
#ifdef SPIKE_PUMP
    {
        MSG msg;
        while (PeekMessage(&msg, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&msg);
            DispatchMessage(&msg);
        }
        printf("SPIKE_PUMP: message pump done\n");
    }
#endif

    // DComp 设备 + target + visual + Commit
    IDCompositionDevice* dcomp = nullptr;
    hr = DCompositionCreateDevice2(dxgiDev, __uuidof(IDCompositionDevice), (void**)&dcomp);
    printf("DCompositionCreateDevice2: 0x%08X\n", hr);
    IDCompositionTarget* target = nullptr;
    hr = dcomp->CreateTargetForHwnd(hwnd, FALSE, &target);
    printf("CreateTargetForHwnd: 0x%08X\n", hr);
    IDCompositionVisual* visual = nullptr;
    hr = dcomp->CreateVisual(&visual);
    printf("CreateVisual: 0x%08X\n", hr);
    hr = visual->SetContent(swap);
    printf("visual.SetContent: 0x%08X\n", hr);
    hr = target->SetRoot(visual);
    printf("target.SetRoot: 0x%08X\n", hr);
    hr = dcomp->Commit();
    printf("Commit: 0x%08X\n", hr);

    // 渲染：自建纹理清屏 → CopyResource → Present1（与 Rust 版一致，绕开 buffer RTV 限制）
    D3D11_TEXTURE2D_DESC td = {};
    td.Width = 782; td.Height = 553; td.MipLevels = 1; td.ArraySize = 1;
    td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET;
    ID3D11Texture2D* solid = nullptr;
    hr = dev->CreateTexture2D(&td, nullptr, &solid);
    printf("CreateTexture2D(solid): 0x%08X\n", hr);
    ID3D11RenderTargetView* rtv = nullptr;
    hr = dev->CreateRenderTargetView(solid, nullptr, &rtv);
    printf("CreateRenderTargetView(solid): 0x%08X (buffer RTV 曾在本机 E_INVALIDARG，solid 应 OK)\n", hr);
    const float magenta[4] = { 1.0f, 0.0f, 1.0f, 1.0f };
    ctx->ClearRenderTargetView(rtv, magenta);
    for (UINT i = 0; i < 2; i++) {
        ID3D11Resource* buf = nullptr;
        swap->GetBuffer(i, __uuidof(ID3D11Resource), (void**)&buf);
        ctx->CopyResource(buf, solid);
        buf->Release();
    }
    // Present1 第三参：文档允许 nullptr，但本机 Intel 驱动对 null 直接 AV（Rust 矩阵已实证），
    // 传空结构体语义等价。
    DXGI_PRESENT_PARAMETERS pp = {};
    hr = swap->Present1(1, 0, &pp);
    printf("Present1: 0x%08X\n", hr);
    hr = dcomp->Commit();
    printf("Commit#2: 0x%08X\n", hr);
    printf(">>> 品红应已上屏，3 秒后屏幕探针判定 <<<\n");
    Sleep(3000);

#ifdef SPIKE_FULL
    // Rust 完整流程复刻：60 帧 ×2 相位循环，每帧 GetBuffer×2 + CopyResource +
    // Present1 + Commit + 消息泵 + GetCurrentBackBufferIndex；phase 1 换青色。
    printf("SPIKE_FULL: begin rust-flow replication\n");
    for (int phase = 0; phase < 2; phase++) {
        const float c0[4] = { 1.0f, 0.0f, 1.0f, 1.0f };
        const float c1[4] = { 0.0f, 1.0f, 1.0f, 1.0f };
        ctx->ClearRenderTargetView(rtv, phase == 0 ? c0 : c1);
        for (int f = 0; f < 60; f++) {
            IDXGISwapChain3* swap3 = nullptr;
            swap->QueryInterface(__uuidof(IDXGISwapChain3), (void**)&swap3);
            if (swap3) { swap3->GetCurrentBackBufferIndex(); swap3->Release(); }
            for (UINT i = 0; i < 2; i++) {
                ID3D11Resource* buf = nullptr;
                swap->GetBuffer(i, __uuidof(ID3D11Resource), (void**)&buf);
                ctx->CopyResource(buf, solid);
                buf->Release();
            }
            DXGI_PRESENT_PARAMETERS pp = {};
            swap->Present1(1, 0, &pp);
            dcomp->Commit();
            MSG msg;
            while (PeekMessage(&msg, nullptr, 0, 0, PM_REMOVE)) {
                TranslateMessage(&msg);
                DispatchMessage(&msg);
            }
            Sleep(16);
        }
        Sleep(150);
        POINT pt = { 782 / 2, 553 / 2 };
        ClientToScreen(hwnd, &pt);
        HDC dc = GetDC(nullptr);
        COLORREF c = GetPixel(dc, pt.x, pt.y);
        ReleaseDC(nullptr, dc);
        printf("SPIKE_FULL phase %d screen: R=%d G=%d B=%d (expect %d 0 %d)\n", phase,
               GetRValue(c), GetGValue(c), GetBValue(c), 255, phase == 0 ? 255 : 255);
    }
    printf("SPIKE_FULL: done\n");
#endif

    // 屏幕探针（与 Rust 版一致：客户区中心 → 屏幕坐标 → GetPixel）
    {
        // 置顶保证不被遮挡
        SetWindowPos(hwnd, HWND_TOPMOST, 100, 100, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
        SetForegroundWindow(hwnd);
        Sleep(1000);
        POINT pt = { 782 / 2, 553 / 2 };
        ClientToScreen(hwnd, &pt);
        HDC dc = GetDC(nullptr);
        COLORREF c = GetPixel(dc, pt.x, pt.y);
        ReleaseDC(nullptr, dc);
        printf("screen center pixel: R=%d G=%d B=%d (expect 255 0 255)\n",
               GetRValue(c), GetGValue(c), GetBValue(c));
    }
    printf("=== DONE ===\n");
    return 0;
}
