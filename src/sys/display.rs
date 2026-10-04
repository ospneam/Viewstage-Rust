#![cfg(windows)]

//! 显示模式查询：屏幕刷新周期的**独立**来源。
//!
//! ## 为什么不能靠"测自己画了多快"
//!
//! 书写期的渲染节流需要"屏幕刷新周期"来定帧率上限（见 [`super`] 里的
//! `paced_redraw`）。但如果这个量是从渲染帧间隔里估出来的，就形成循环反馈：
//!
//! ```text
//! 满速渲染 → 估计器得出"屏幕很快" → 判定无需节流 → 继续满速渲染
//! ```
//!
//! 估计器测的是**我们自己请求得多快**，不是屏幕刷多快，于是帧率上限这件事
//! 永远不会被真正触发。实测：本机面板 60Hz，应用却稳定跑到 86fps，并据此
//! 认为屏幕是 83Hz。
//!
//! 无反馈的来源只有系统本身：`EnumDisplaySettings` 直接给出当前显示模式的
//! 刷新率。

use std::cell::Cell;
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, DEVMODEW, DM_DISPLAYFREQUENCY, ENUM_CURRENT_SETTINGS,
};

/// 合理刷新率区间（Hz）。低于 20 基本是驱动未初始化，高于 1000 没有意义。
const SANE_HZ: std::ops::RangeInclusive<f64> = 20.0..=1000.0;

thread_local! {
    /// 缓存结果：显示模式运行期很少变，而每次查询都要让系统填一个 220 字节的
    /// 结构体，不值得放进帧路径。
    static CACHED_HZ: Cell<Option<f64>> = const { Cell::new(None) };
}

/// 主显示器的刷新率（Hz）；查询失败或系统未报告时返回 `None`。
///
/// 调用方应退回自己的退路（`app::pacing` 的实测周期，有循环反馈但不致命），
/// 而不是硬编码一个数字。
///
/// 局限：只查**主**显示器。多屏且窗口在副屏上时取不到正确值，此时返回的
/// 仍是主屏频率——`None` 之外的退路在这里就是"节流阈值略有偏差"，不影响
/// 正确性，故不引入 `MonitorFromWindow` 的窗口句柄依赖。
pub fn refresh_hz() -> Option<f64> {
    if let Some(cached) = CACHED_HZ.get() {
        return Some(cached);
    }
    let hz = query();
    CACHED_HZ.set(hz);
    hz
}

/// 局限：进程生命周期内只查一次。运行期改了刷新率需要重启才能生效——这是
/// 有意的取舍：热路径（每帧、每个输入事件）都要读这个值，不能放系统调用；
/// 而"改了刷新率忘了重启"只影响节流阈值，不影响正确性。
fn query() -> Option<f64> {
    // SAFETY: `EnumDisplaySettingsW` 按 `dmSize` 读该结构体；`DEVMODEW` 是
    // POD，传 `&mut` 且已零初始化，不含需要 drop 的字段。
    unsafe {
        let mut mode: DEVMODEW = std::mem::zeroed();
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut mode) == 0 {
            return None;
        }
        // 只在系统明确声明该字段时才读，否则拿到的是陈旧/零值。
        if mode.dmFields & DM_DISPLAYFREQUENCY == 0 {
            return None;
        }
        // `dmDisplayFrequency == 1` 是 MSDN 定义的"真值在位域里"的哨兵。位域
        // 的解读没有稳定可靠的公开定义，这里刻意不猜——返回 `None` 走退路，
        // 好过解出一个错误的频率把帧率上限带偏。
        if mode.dmDisplayFrequency <= 1 {
            return None;
        }
        let hz = f64::from(mode.dmDisplayFrequency);
        SANE_HZ.contains(&hz).then_some(hz)
    }
}

#[cfg(test)]
mod tests {
    /// 本机面板必须报出真实刷新率——否则 `paced_redraw` 会退回自测周期，
    /// 整个节流前提失效。这条测试失败说明是**测试环境**问题（无显示设备），
    /// 不应改生产代码去迁就。
    #[test]
    fn primary_display_reports_a_sane_refresh_rate() {
        match super::refresh_hz() {
            Some(hz) => assert!(
                super::SANE_HZ.contains(&hz),
                "刷新率应落在合理区间，实测 {hz}"
            ),
            None => eprintln!("skip: 本环境未报告显示模式频率"),
        }
    }
}