//! 面板对齐的帧节拍（Chrome 式 vsync 时钟）。
//!
//! # 要解决的问题
//!
//! 事件驱动的出帧模型里，**帧率由"请求了多少次重绘"决定，而不是由显示器的
//! 刷新率决定**。触摸设备每秒送来 100~700 个事件，每一个都触发一次
//! `RedrawRequested`，于是应用跑到 85~100fps，而面板只有 60Hz——每秒 10~40
//! 帧必然被显示端丢掉，但每次仍要付一遍完整的代价：
//!
//! - `UserInterface::draw`：整棵 widget 树重建 + 我们的 `Program::draw`
//! - `Compositor::present`：命令编码 + `queue.submit`（实测 1.2~1.8ms/帧）
//!
//! 在请求点逐个限流并不能根治：那需要每个请求点都记得限流，而重绘请求还有
//! 别的来源（`view()` 变化、窗口事件、框架内部），漏一个就超帧。
//!
//! # 做法（对齐 Chromium 的 `viz::DisplayScheduler`）
//!
//! Chromium 不是"限流渲染"，而是**让 vsync 成为唯一的时钟**：显示调度器每个
//! vsync 发一次 BeginFrame，客户端在这期间提交内容，两次 vsync 之间到达的
//! 输入被**合并进下一帧**。帧率因此天然等于刷新率，与内容更新速率无关。
//!
//! 这里做同样的事，但只在 `RedrawRequested` 分支里加一道闸：本帧若距上次
//! 呈现不足一个面板周期，就**整帧跳过**——不重建 widget 树、不编码、不提交、
//! 不呈现——并把下一次重绘对齐到下一个周期边界。
//!
//! 代价要说清楚：**输入到上屏的延迟上界从一个面板周期变成"等到下一个周期
//! 边界 + 本帧渲染"**，最坏约 2 个周期（60Hz 下 ~33ms）。换来的是延迟
//! **有界**（此前是"请求多少帧就多少帧"的抖动）以及不再做注定被丢弃的帧。
//! 这正是 Chromium 主动做的取舍。
//!
//! # 本应用默认不开启
//!
//! 见 [`enabled`] 的实测数据：**呈现本来就没有超过面板刷新率**，所以这道闸门
//! 在这里只省下少量 layout 重建，却给书写加上最多一个面板周期的延迟。
//!
//! # 周期从哪来
//!
//! [`panel_period`] 用 `EnumDisplaySettings` 读系统报告的当前显示模式，
//! 与 Chromium 在 Windows 上用 `DwmGetCompositionTimingInfo` 取 vsync 属于
//! 同一族 DWM/GDI 时序接口。**不能**用"我们自己画了多快"来推：那是循环
//! 反馈（画得快 → 判定面板很快 → 不节流 → 继续快），限幅永远不会生效。
//!
//! 关闭开关：环境变量 `ICED_PANEL_ALIGN=0`（用于 A/B 对照）。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// 是否启用面板对齐。
///
/// **默认关闭**，需显式设 `ICED_PANEL_ALIGN=1` 打开。
///
/// # 为什么默认关闭（本机实测）
///
/// Chromium 那套模型是为"能跑赢显示器的应用"设计的——它假定内容更新比刷新率
/// 快，于是让 vsync 当唯一时钟来合并。本应用不满足这个前提：
///
/// ```text
/// ALIGN=off   呈现/秒 min=0 中位=22 max=45   >60 的窗口 = 0/23
///             layout/秒 中位=48               呈现/layout = 0.46
/// ALIGN=on    layout/秒 中位=34（少 30%）     呈现/秒 同样不超 60
/// ```
///
/// **呈现本来就没有超过面板刷新率**——摄像头 15~30/s、输入事件又被合并，
/// 内容更新速率低于 60Hz。于是闸门只做到了"少做 30% 的 layout 重建"，
/// 代价是给书写这种延迟敏感交互**加上最多一个面板周期（60Hz 下 16.7ms）的
/// 等待**。用延迟换一个本来就不存在的浪费，是亏的。
///
/// 保留它是因为：当内容更新快于刷新率时（更高帧率的摄像头、未来的多通道
/// 合成、更高的面板刷新率），这个闸门是**唯一无遗漏**的出帧口，届时开启即
/// 生效。不设默认开启是因为上述实测——"看起来更正确"的默认值在这里是错的。
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("ICED_PANEL_ALIGN")
            .map(|v| matches!(v.trim(), "1" | "true" | "on" | "yes"))
            .unwrap_or(false)
    })
}

/// 当前显示模式的一个刷新周期；查询失败时返回 `None`（= 不限幅，退回原行为）。
pub fn panel_period() -> Option<Duration> {
    if !enabled() {
        // 只播报一次。这行在闸门路径上每帧都会被调用，若不设闸就会刷屏——
        // 实测被它淹没过一次（每次呈现打一行）。
        static WARNED: OnceLock<()> = OnceLock::new();
        let _ = WARNED.get_or_init(|| log::info!("[frame_pacing] 面板对齐已关闭（ICED_PANEL_ALIGN）"));
        return None;
    }
    static PERIOD: OnceLock<Option<Duration>> = OnceLock::new();
    let period = *PERIOD.get_or_init(|| {
        let p = query_panel_period();
        // 只播报一次：面板周期拿不到时表现为"闸门从不触发"，而症状（帧率照旧
        // 超面板）看起来像闸门坏了——必须能在日志里区分这两种情况。
        match p {
            Some(d) => log::info!(
                "[frame_pacing] 面板周期 = {:.2}ms（{:.1}Hz）",
                d.as_secs_f64() * 1000.0,
                1.0 / d.as_secs_f64()
            ),
            None => log::warn!(
                "[frame_pacing] 读不到系统刷新率，闸门**不会生效**（退回原行为）"
            ),
        }
        p
    });
    period
}

#[cfg(windows)]
// 本 crate 全局 `unsafe_code = "deny"`。这里是必要的例外：`EnumDisplaySettingsW`
// 没有安全封装，且它是**只读**地填一个调用方提供的 POD 结构体。
#[allow(unsafe_code)]
fn query_panel_period() -> Option<Duration> {
    use windows_sys::Win32::Graphics::Gdi::{
        DEVMODEW, DM_DISPLAYFREQUENCY, EnumDisplaySettingsW, ENUM_CURRENT_SETTINGS,
    };

    /// 合理刷新率区间（Hz）。低于 20 基本是驱动未初始化，高于 1000 无意义。
    const SANE_HZ: std::ops::RangeInclusive<f64> = 20.0..=1000.0;

    // SAFETY: `EnumDisplaySettingsW` 按 `dmSize` 读取该结构体；`DEVMODEW` 是
    // POD，全零初始化后传入，不含需要 drop 的字段。
    let hz = unsafe {
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
        // 的解读没有稳定可靠的公开定义，刻意不猜——返回 None 走"不限幅"的
        // 退路，好过解出一个错误的周期把帧率带偏。
        if mode.dmDisplayFrequency <= 1 {
            return None;
        }
        f64::from(mode.dmDisplayFrequency)
    };
    SANE_HZ.contains(&hz).then(|| Duration::from_secs_f64(1.0 / hz))
}

#[cfg(not(windows))]
fn query_panel_period() -> Option<Duration> {
    // 其他平台没有可靠的系统刷新率查询，退回不限幅（= 保持原行为）。
    None
}

/// 距上次呈现还不足一个面板周期时，返回应唤醒的时刻；否则返回 `None`（本帧放行）。
///
/// `last_present` 为 `None`（首帧）时放行。
#[inline]
pub fn defer_until(last_present: Option<std::time::Instant>) -> Option<std::time::Instant> {
    let period = panel_period()?;
    let last = last_present?;
    let at = last + period;
    if std::time::Instant::now() < at {
        note_event(true);
        Some(at)
    } else {
        note_event(false);
        None
    }
}

/// 闸门统计：放行 = 真的产出了一帧；延后 = 整帧被跳过。
static PRESENTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DEFERRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 记录一次**成功呈现**。与 [`enabled`] 无关——即使闸门关闭也要计。
///
/// 必要性：本项目曾经长期把"每帧 `App::draw` 调用次数"当成帧率。那是错的：
/// iced 的 canvas widget 在 **layout 阶段**就会调 `Program::draw` 构建几何，
/// 不是每次呈现才调一次，两者能差一倍（实测 layout 74/s 而呈现 37.7/s）。
/// 而任何"间接"呈现数（draw call 计数、绑定组计数）在画布为空时恒为 0，
/// 同样不可用。**只有这里知道真相**。
pub fn note_presented() {
    use std::sync::atomic::Ordering::Relaxed;
    let n = PRESENTED.fetch_add(1, Relaxed) + 1;
    // 每 20 次呈现播报一次：60fps 下约每 0.33s 一行，相邻两行的时间差足够
    // 细，能区分 30 与 60fps 而不是被量化成两档。
    if n % 10 != 0 {
        return;
    }
    report();
}

/// 宿主应用注册的"成功呈现"回调。
///
/// 由宿主在 `main()` 里注册（`iced_winit::frame_pacing::set_present_hook`），
/// 指向自己的周期估计器。本 crate 不知道宿主的节拍语义（面板对齐闸门只需
/// 要"呈现了几次"用于统计），但**节拍的唯一权威是呈现时刻**，而只有这个
/// crate 站在那个位置上。
///
/// 签名用 `fn` 而非闭包：回调需要在 `'static` 全局里存活，闭包捕获不了。
static PRESENT_HOOK: Mutex<Option<fn(std::time::Instant)>> = Mutex::new(None);

/// 注册"成功呈现"回调。同一时刻只保留最后一次注册。
pub fn set_present_hook(hook: fn(std::time::Instant)) {
    *PRESENT_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
}

/// 通知宿主"一帧真的上屏了"。由 `present()` 返回 `Ok` 之后调用。
#[inline]
pub fn note_present(at: std::time::Instant) {
    if let Some(hook) = *PRESENT_HOOK.lock().unwrap_or_else(|e| e.into_inner()) {
        hook(at);
    }
}

/// 周期性打印呈现速率与闸门命中率。
fn report() {
    use std::sync::atomic::Ordering::Relaxed;
    // 用"进程启动以来的毫秒数"而不是墙钟：相邻两行的差值才是速率，而日志
    // 时间戳只有秒级精度，会把速率量化成 60/dt（dt 为整数秒）——只能得到
    // 30 或 60 这种假值。实测踩过这个坑：60/30 交替全是度量假象。
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = *START.get_or_init(std::time::Instant::now);
    let t = start.elapsed().as_secs_f64();

    let p = PRESENTED.load(Relaxed);
    let d = DEFERRED.load(Relaxed);
    let skip_ratio = d as f64 / (p + d).max(1) as f64;
    log::info!(
        "[frame_pacing] t={t:.3}s 呈现={p} 跳过={d} 跳过率={:.0}% 闸门={}",
        skip_ratio * 100.0,
        if enabled() { "开" } else { "关" }
    );
}

/// 周期性打印闸门命中率。
///
/// 没有这个数字就无法区分"闸门在工作（延后很多）"与"闸门根本没生效（从不
/// 延后）"——两者的外部症状都是"帧率照旧超面板"，而修法完全不同。
fn note_event(deferred: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    if !deferred {
        // 放行的那一帧紧接着就会 `note_presented`，不重复计数。
        return;
    }
    let n = DEFERRED.fetch_add(1, Relaxed) + 1;
    if n % 60 != 0 {
        return;
    }
    report();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn disabled_means_no_period() {
        // 本测试进程内 `enabled()` 由环境决定；只断言"关掉就没有周期"这条
        // 契约在实现上成立——直接测 `defer_until` 的两个分支。
        if !enabled() {
            assert!(panel_period().is_none());
            assert!(defer_until(Some(Instant::now())).is_none());
        }
    }

    #[test]
    fn first_frame_is_never_deferred() {
        // 首帧没有"上次呈现"，必须放行，否则窗口会永远黑屏。
        if enabled() {
            assert_eq!(defer_until(None), None);
        }
    }

    #[test]
    fn a_fresh_present_is_deferred_by_about_one_period() {
        let Some(period) = panel_period() else {
            return; // 本机读不到刷新率，跳过
        };
        let last = Instant::now();
        let at = defer_until(Some(last)).expect("刚呈现过应当延后");
        // 延后量应恰好是一个周期（允许极小的调度误差）。
        let delta = at.duration_since(last);
        assert!(
            delta >= period && delta < period + Duration::from_millis(2),
            "延后量应约等于一个周期（{period:?}），实测 {delta:?}"
        );
    }

    #[test]
    fn an_old_present_is_not_deferred() {
        let Some(period) = panel_period() else {
            return;
        };
        // 上次呈现早于一个周期 → 放行。
        let last = Instant::now() - period - Duration::from_millis(5);
        assert_eq!(defer_until(Some(last)), None);
    }
}

