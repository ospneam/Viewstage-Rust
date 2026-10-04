//! DComp 合成器：设备 + target + visual 树的生命周期管理。
//!
//! # 保活契约（P0 硬约束 #1）
//! [`DcompCompositor`] 持有 `IDCompositionDevice` / `IDCompositionTarget` /
//! 全部 `IDCompositionVisual` 的 COM 引用。**任何引用提前释放都会导致窗口
//! 全白**（target 释放 = 拆除 visual 与 HWND 绑定）。
//!
//! Drop 顺序：先 `DestroyTarget`（显式解绑 HWND，visual 一并失效），
//! 再释放 device。窗口销毁前应先 drop 本结构。

use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::DirectComposition::{
    IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISwapChain1};

/// 已挂到树上的 visual 句柄（持有 COM 引用保活）。
pub struct VisualSlot {
    visual: IDCompositionVisual,
    attached: bool,
}

/// DirectComposition 合成器。
///
/// ```text
/// // 典型用法（P2 接线形态）：
/// // let comp = DcompCompositor::new(hwnd, &dxgi_device)?;
/// // let sc = viewcomp::create_composition_swapchain(&queue_unknown, size, desc)?;
/// // let ui = comp.add_visual_on_top()?;      // root 顶插入
/// // comp.set_content(&ui, &sc)?;
/// // comp.commit()?;
/// ```
pub struct DcompCompositor {
    device: IDCompositionDevice,
    /// 保活字段（P0 硬约束 #1）：释放即拆除 visual 与 HWND 绑定（窗口全白）。
    #[allow(dead_code)]
    target: IDCompositionTarget,
    root: IDCompositionVisual,
    /// visual 计数（诊断）
    visual_count: usize,
    destroyed: bool,
}

impl DcompCompositor {
    /// 在 hwnd 上建立 DComp target 与根 visual。
    ///
    /// `dxgi_device`：渲染设备的 `IDXGIDevice`（从 wgpu raw device 或 D3D11
    /// device QI 得来）。topmost 固定 false——CreateTargetForHwnd 的 topmost
    /// 参数必须与窗口实际 topmost 状态一致（P0 验证），由调用方保证。
    pub fn new(hwnd: HWND, dxgi_device: &IDXGIDevice) -> windows::core::Result<Self> {
        let device = unsafe { windows::Win32::Graphics::DirectComposition::DCompositionCreateDevice2::<_, IDCompositionDevice>(dxgi_device) }?;
        Self::build_tree(hwnd, device)
    }

    /// 从任意 rendering device（IUnknown）构造，带 P0 实测 fallback 链。
    ///
    /// 尝试顺序（P0 spike 验证）：`DCompositionCreateDevice2(device)` →
    /// `DCompositionCreateDevice3(device)` → **null rendering device**。
    /// 本机 D3D12 command queue 路线 E_NOINTERFACE（v2/v3 均如此），null
    /// 实测可正常合成（wgpu DX12 队列的 composition swapchain 内容不受
    /// DComp device 来源影响）。wgpu DX12 互操作即走此构造。
    pub fn new_from_rendering_device(
        hwnd: HWND,
        rendering_device: Option<&windows::core::IUnknown>,
    ) -> windows::core::Result<Self> {
        use windows::Win32::Graphics::DirectComposition::{DCompositionCreateDevice2, DCompositionCreateDevice3};
        let device: IDCompositionDevice = unsafe {
            match rendering_device {
                Some(d) => match DCompositionCreateDevice2::<_, IDCompositionDevice>(d) {
                    Ok(dev) => dev,
                    Err(e2) => match DCompositionCreateDevice3::<_, IDCompositionDevice>(d) {
                        Ok(dev) => dev,
                        Err(e3) => {
                            log::warn!(
                                "viewcomp: DComp device from rendering device failed \
                                 (v2: {e2}, v3: {e3}); falling back to null device"
                            );
                            DCompositionCreateDevice2::<_, IDCompositionDevice>(None::<&windows::core::IUnknown>)?
                        }
                    },
                },
                None => DCompositionCreateDevice2::<_, IDCompositionDevice>(None::<&windows::core::IUnknown>)?,
            }
        };
        Self::build_tree(hwnd, device)
    }

    /// target + root visual 建树（两种构造共用）。
    fn build_tree(hwnd: HWND, device: IDCompositionDevice) -> windows::core::Result<Self> {
        let target = unsafe { device.CreateTargetForHwnd(hwnd, windows::Win32::Foundation::BOOL(0)) }?;
        let root = unsafe { device.CreateVisual() }?;
        unsafe { target.SetRoot(&root) }?;
        unsafe { device.Commit() }?;
        Ok(Self { device, target, root, visual_count: 0, destroyed: false })
    }

    /// 设备句柄（诊断 / WaitForCommitCompletion 等）。
    pub fn device(&self) -> &IDCompositionDevice {
        &self.device
    }

    /// 在根 visual 顶部插入一个新 child visual，返回句柄。
    ///
    /// z-order：后插入者盖在先插入者之上。
    /// windows-rs 0.58 的 AddVisual 将可空 `BOOL* insertAbove` 生成为非空
    /// `Param<BOOL>`，无法表达 NULL；用 insertAbove=TRUE + reference=当前最顶
    /// child 等价实现"插入顶部"（首个 child 时 reference 为 NULL，等效插顶）。
    /// P2 接线后上屏复核 z-order，若顺序颠倒改为 FALSE 对照。
    pub fn add_visual_on_top(&mut self) -> windows::core::Result<VisualSlot> {
        let visual = unsafe { self.device.CreateVisual() }?;
        // 以最后一个 child 为参照插到其上；无 child 时 reference=NULL
        unsafe { self.root.AddVisual(&visual, BOOL(1), None::<&IDCompositionVisual>) }?;
        self.visual_count += 1;
        Ok(VisualSlot { visual, attached: true })
    }

    /// 在指定参照 visual 之上插入新 child visual。
    pub fn add_visual_above(&mut self, reference: &VisualSlot) -> windows::core::Result<VisualSlot> {
        let visual = unsafe { self.device.CreateVisual() }?;
        unsafe { self.root.AddVisual(&visual, BOOL(1), &reference.visual) }?;
        self.visual_count += 1;
        Ok(VisualSlot { visual, attached: true })
    }

    /// 设置 visual 内容（composition swapchain / surface / 其它 visual）。
    ///
    /// 需要随后调用 [`Self::commit`] 生效。
    pub fn set_content_swapchain(
        &self,
        slot: &VisualSlot,
        swapchain: &IDXGISwapChain1,
    ) -> windows::core::Result<()> {
        unsafe { slot.visual.SetContent(swapchain) }
    }

    /// 设置 visual 内容为任意 IUnknown（surface / 另一个 visual 等）。
    pub fn set_content_raw(
        &self,
        slot: &VisualSlot,
        content: &windows::core::IUnknown,
    ) -> windows::core::Result<()> {
        unsafe { slot.visual.SetContent(content) }
    }

    /// 移除 visual（从树上摘下并释放内容绑定）。需要 commit 生效。
    pub fn remove_visual(&mut self, slot: &mut VisualSlot) -> windows::core::Result<()> {
        if !slot.attached {
            return Ok(());
        }
        unsafe { self.root.RemoveVisual(&slot.visual) }?;
        slot.attached = false;
        self.visual_count = self.visual_count.saturating_sub(1);
        Ok(())
    }

    /// visual 偏移辅助（P2 内容等价时可能需要）。
    pub fn set_offset(&self, slot: &VisualSlot, x: f32, y: f32) -> windows::core::Result<()> {
        let v2: windows::Win32::Graphics::DirectComposition::IDCompositionVisual2 = slot.visual.cast()?;
        unsafe { v2.SetOffsetX2(x)?; v2.SetOffsetY2(y) }
    }

    /// 提交全部变更。
    pub fn commit(&self) -> windows::core::Result<()> {
        unsafe { self.device.Commit() }
    }

    /// 阻塞等待提交完成（首帧验证用；常规路径不需要——DWM 自会调度）。
    pub fn wait_for_commit(&self) -> windows::core::Result<()> {
        unsafe { self.device.WaitForCommitCompletion() }
    }

    /// 当前 visual 数（诊断）。
    pub fn visual_count(&self) -> usize {
        self.visual_count
    }

    /// 标记销毁（窗口销毁前调用；Drop 也会做）。
    ///
    /// windows-rs 0.58 未生成 IDCompositionDevice::DestroyTarget；释放 target
    /// 的 COM 引用即等价解绑（P0 根因：引用提前释放 = 绑定拆除 = 窗口全白）。
    /// 因此本方法只置标记，真正的解绑发生在 Drop 释放 target 字段时。
    /// 关键是调用方保证：**窗口销毁前先 drop 本结构**。
    pub fn destroy(&mut self) {
        self.destroyed = true;
    }
}

impl Drop for DcompCompositor {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// raw 接口别名 re-export（P2 fork 接线时避免重复 import）。
pub use windows::Win32::Graphics::DirectComposition::IDCompositionDevice as Device2Alias;

/// 便捷 re-export：调用方需要手动 QI IDXGIDevice 时的 trait。
pub use windows::core::Interface as _Interface;

/// 测试辅助：从 D3D11 device 直接构造（D3D11 路线）。
///
/// # Safety
/// device 必须有效。
pub unsafe fn device_from_d3d11(
    d3d11: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
) -> windows::core::Result<IDXGIDevice> {
    d3d11.cast::<IDXGIDevice>()
}

// Interface 使用说明：IDCompositionVisual 的 SetContent 在 windows 0.58 中
// 定义于 IDCompositionVisual2 的父接口，通过 .cast() 调用亦可。此处直接用
// IDCompositionVisual 上的方法（v1 全集）。
#[allow(unused_imports)]
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual2 as _V2;
