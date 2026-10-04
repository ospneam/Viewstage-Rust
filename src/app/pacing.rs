//! 帧率节流：把"目标帧率上限"翻译成"该跳过几个 vsync"。
//!
//! # 为什么不能用定时器
//!
//! 旧实现在未达目标间隔时用 `Action::request_redraw_at(now + (target - elapsed))`。
//! 但渲染帧的起点是 **vsync 对齐**的，于是 `last_frame_start + target` 同样落在
//! vsync 边界附近——而"离 vsync 截止线只剩 1µs"意味着**必然赶不上这一帧**：
//! prepare + render + submit 要 ~2ms，于是每一帧都被推迟到下一个 vsync，
//! 帧间隔变成 2 个 vsync。
//!
//! 实测（60Hz 面板，`annotation_fps_cap` 各种取值，`work=2ms`）：
//!
//! | cap | 定时器方案实际帧率 | 本模块（跳 vsync） |
//! | --- | --- | --- |
//! | 30 | 20.0 | 30.0 |
//! | 40 | 30.0 | 30.0 |
//! | 50 | 30.0 | 60.0 |
//! | 60 | **30.0** | 60.0 |
//! | 90 / 120 / 144 / 165 | 60.0 | 60.0（= 面板刷新率） |
//!
//! 定时器方案在**任何**取值下都上不去 60fps，且取值越接近刷新率越糟。
//!
//! # 本方案
//!
//! vsync 本身已经把帧率钉在刷新率上，所以"降帧"只需**隔帧不重绘**。
//! 要降到 `refresh/(N+1)`，就在第 N+1 个 vsync 照常呈现、中间跳过 N 个。
//! 全程没有定时器，因此没有"差一点赶上 vsync"的可能。
//!
//! 上限不低于刷新率时（`cap >= refresh`）直接返回 0 —— 完全交给 vsync，
//! 这正是"稳定 60fps"最常见的情形。

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 面板刷新周期（一个 vsync 的时长），来自系统报告的显示模式。
///
/// 与自测周期 `display_period` 的区别：后者反映的是"我们自己请求得多快"，
/// 拿它当面板刷新率会形成循环反馈（满速渲染 → 判定面板很快 → 不节流 → 继续
/// 满速）。本函数是唯一无反馈的来源。
///
/// **当前只用于诊断与空闲判定，不用于节流。** 曾试过用它把书写期的呈现节奏
/// 压到面板刷新率（60Hz 面板实测跑 88~111fps，其中约 43% 的帧必然被显示端
/// 丢弃），但实测帧率从 ~90~110/s 掉到 ~55~70/s，而本模块文档开头就写明
/// `request_redraw_at` 的唤醒相位问题会让帧率上不去。收益无法证明、代价
/// （帧率减半）可测，故不采用。见 [`super::paced_redraw`]。
pub fn panel_period() -> Option<Duration> {
    let hz = crate::sys::display::refresh_hz()?;
    (hz > 0.0).then(|| Duration::from_secs_f64(1.0 / hz))
}

/// 判定"这串输入是连续的"的上限：相邻两个输入事件的间隔小于它，才把
/// "事件→下一帧"算作管线延迟。
///
/// 手指停顿后落笔时，第一个事件距上一个事件隔了整段停顿，量到的其实是停顿
/// 长度。设成 40ms：120Hz 数字板的事件间隔是 8ms，40ms 留了 5 倍余量；
/// 而人的停顿一般在 50ms 以上。
pub const STREAM_CONTINUITY_GAP: Duration = Duration::from_millis(40);

/// 本帧 prepare+render+submit 的保守耗时估计（微秒）。
///
/// 用于判断"跳过 vsync 后的余量是否还够提交"。取 3ms 覆盖 1080p 核显上的
/// 实测量级；`refresh_period / 2 <= 该值` 时节流无意义（余量不够），改为
/// 每帧都渲染。
pub const WORK_ESTIMATE_US: u64 = 3_000;

/// `cap >= refresh * ratio` 的判定阈值。
///
/// 比值低于此值时视为"上限不低于刷新率"，走 vsync 全速。留 1.5 的余量是
/// 因为非整数跳帧间隔会产生**抖动**（1 个和 2 个 vsync 交替），而稳定帧率
/// 正是本模块的目的——宁可实际帧率略高于上限，也不要抖动。
const NO_THROTTLE_RATIO: f64 = 1.5;

/// 本帧应跳过的 vsync 数。`0` = 本帧就渲染（交给 vsync）。
///
/// - `refresh_hz`：由渲染帧间隔实测得到的面板刷新率（见
///   [`merge_period_sample`]）；传 0 表示尚未测得，此时不节流。
/// - `cap_hz`：`canvas.annotation_fps_cap`。
pub fn skip_count(refresh_hz: f64, cap_hz: u32) -> u32 {
    if refresh_hz <= 0.0 || cap_hz == 0 {
        return 0;
    }
    let period_us = 1_000_000.0 / refresh_hz;
    let ratio = refresh_hz / cap_hz as f64;

    // 上限不低于刷新率：vsync 已是天花板，节流只会添乱。
    if ratio < NO_THROTTLE_RATIO {
        return 0;
    }
    // 跳 N 帧后的实际帧率 = refresh / (N+1)，故 N = round(ratio) - 1。
    let n = ratio.round() as i32 - 1;
    if n <= 0 {
        return 0;
    }
    // 余量校验：从 vsync 区间中点唤醒，提交前还剩 period/2 - work 的裕度。
    // 裕度不足说明这台机器每帧都做不完，节流没有意义。
    if period_us / 2.0 <= WORK_ESTIMATE_US as f64 {
        return 0;
    }
    // `n > 0` 上面已保证。**不能**写 `n.min(u32::MAX as i32)`：`u32::MAX as i32`
    // 是 -1（截断转换），min 会把任何正数压成 -1，再 `as u32` 变成 4294967295
    // 帧——实测表现为"永远跳帧、画面不动"。
    n as u32
}

/// 节流时相对上一帧起点的**唤醒偏移**（微秒）。
///
/// 关键：唤醒点落在 vsync 区间**正中**（`N*period + period/2`），
/// 前后各有 `period/2` 的裕度。旧实现把唤醒点放在 vsync 边界上
/// （`N*period`），那是命中率最差的相位。
pub fn throttle_wake_offset_us(refresh_hz: f64, skip: u32) -> u64 {
    if refresh_hz <= 0.0 {
        return 0;
    }
    let period_us = 1_000_000.0 / refresh_hz;
    (skip as f64 * period_us + period_us / 2.0).round() as u64
}

/// 笔迹期的跳帧数：**在不低于 `min_hz` 的前提下，取最大的跳帧数**。
///
/// 与 [`skip_count`] 的区别有两处，都是笔迹期的特殊要求：
///
/// 1. **硬下限而非上限**。`skip_count` 是"凑到 `cap_hz`"（`round`），会向下
///    溢出到下限以下：90Hz 面板上 `round(90/60)-1 = 1` → 45fps，165Hz 上
///    → 55fps。这里反过来——`min_hz`（60fps）是**绝不能破**的底线，能省
///    多少省多少，所以是 `floor(refresh/min_hz) - 1`。
/// 2. **不受 `WORK_ESTIMATE_US` 守卫约束**。那个守卫在 `period/2` 小于帧
///    工作估计时直接返回 0（"没有空闲窗口就别节流"），于是在 240Hz+ 面板上
///    节流被整个关掉——恰好是省 GPU 收益最大的地方被放弃。守卫的顾虑是
///    唤醒点落进本帧工作窗口内导致**丢帧**；而偏移量随 `skip` 线性增长
///    （`skip=3` 时是 `3*period + period/2`，远超工作估计），不存在这个风险。
///
/// 结果（`min_hz = 60`）：60→60、90→90（整数跳帧到不了 60，45 太低）、
/// 120→60、144→72、165→82.5、240→60、360→60。
pub fn stroke_skip(refresh_hz: f64, min_hz: f64) -> u32 {
    if refresh_hz <= 0.0 || !refresh_hz.is_finite() || min_hz <= 0.0 {
        return 0;
    }
    let divisor = (refresh_hz / min_hz).floor();
    if divisor < 1.0 {
        // 面板本身就低于下限：不跳帧（限帧只会更糟）
        return 0;
    }
    // divisor = refresh/(skip+1) ≥ min_hz 的最大整数解
    let skip = divisor - 1.0;
    if skip <= 0.0 {
        return 0;
    }
    skip.min(u32::MAX as f64) as u32
}

/// 笔迹期的实际帧率（Hz）。
#[cfg(test)]
fn stroke_hz(refresh_hz: f64, min_hz: f64) -> f64 {
    if refresh_hz <= 0.0 {
        return 0.0;
    }
    refresh_hz / (stroke_skip(refresh_hz, min_hz) as f64 + 1.0)
}

/// 笔迹期的帧率下限（Hz）。用户硬要求"帧率必须要有 60FPS"。
///
/// 笔迹热路径（书写 / 橡皮擦）**也要限帧**：这是 GPU 占用率的主要来源，在
/// 高刷屏上满刷新率意味着 2 倍以上的无谓工作量（144→72、240→60）。为此
/// 引入的额外延迟由笔尖外推补偿，两处是配套改动（见
/// `App::effective_frame_period_ms` 与 `App::paced_redraw`）——只回退其中
/// 一个，笔尖就会重新落后于指针。
pub const STROKE_MIN_HZ: f64 = 60.0;

/// 有效帧量化周期（毫秒）：帧内容实际上屏的间隔。
///
/// 笔迹热路径被限帧到 `cap_hz` 后，量化周期**不再是**面板刷新周期，而是
/// `(skip + 1) ×` 面板周期。笔尖外推必须按这个值补偿——按面板周期算会在
/// 限帧后欠补将近一半延迟，笔尖照样落后指针。
///
/// 非笔迹期（`stroke_rendering == false`）返回面板周期。
/// `None` = 刷新率未测得 → 调用方应**不外推**（保守退化）。
/// `cap_hz` 仅在笔迹期生效。
pub fn effective_period_ms(refresh_hz: f64, stroke_rendering: bool, min_hz: f64) -> Option<f32> {
    if refresh_hz <= 0.0 || !refresh_hz.is_finite() {
        return None;
    }
    let panel_ms = (1000.0 / refresh_hz) as f32;
    if !stroke_rendering {
        return Some(panel_ms);
    }
    let skip = stroke_skip(refresh_hz, min_hz);
    Some(panel_ms * (skip as f32 + 1.0))
}

/// **呈现周期**的全局估计，由成功呈现驱动（见 [`note_present`]）。
///
/// # 为什么不放在 `App` 的字段里
///
/// 唯一知道"呈现真的发生了"的代码在 `iced_winit` 的 `present()` 之后，而那里
/// 拿不到 `&mut App`。因此由宿主应用在 `main()` 里把这个函数注册成回调
/// （`iced_winit::frame_pacing::set_present_hook`），节拍状态只能落在全局。
///
/// # 为什么必须用呈现间隔，而不是 `App::draw` 的间隔
///
/// `App::draw` 是 canvas widget 的 `Program::draw`，iced 在 **layout 阶段**就调
/// 它来构建几何——**不是每次呈现才调一次**。实测两者能差一倍以上：
///
/// ```text
/// 呈现/秒   min=25.3  中位=37.7  max=57.6   （60Hz 面板，从不超 60）
/// layout/秒 中位=74
/// ```
///
/// 拿 layout 间隔当刷新率会同时坏掉三处：
///
/// 1. [`effective_period_ms`] —— 笔尖外推的量化周期。实测报 ~74Hz → 13.5ms，
///    而真实周期 ~26ms，**欠补近一半**，笔尖因此落后指针。
/// 2. [`stroke_skip`] / [`skip_count`] —— 按一个并不存在的面板速率算跳帧档位。
/// 3. 曾经的"面板对齐闸门"—— 据此认定"跑到了 70~100fps"，而实际从未超 60。
static DISPLAY_PERIOD: Mutex<PeriodEstimate> =
    Mutex::new(PeriodEstimate { period: None, off_band: 0, off_band_faster: false });

/// 上一次成功呈现的时刻。`None` = 尚未呈现过（此时不外推、不节流）。
static LAST_PRESENT: Mutex<Option<Instant>> = Mutex::new(None);

/// 呈现样本计数，仅用于诊断播报。
static PRESENT_SAMPLES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 记一次**成功呈现**，并把呈现间隔并入周期估计。
///
/// 由 `iced_winit` 在 `Compositor::present` 返回 `Ok` 之后调用——这是全流程
/// 唯一无法伪造的"一帧真的上屏了"的时刻。离群样本由 [`merge_period_sample`]
/// 的分层拒绝处理。
pub fn note_present(at: Instant) {
    let mut last = match LAST_PRESENT.lock() {
        Ok(guard) => guard,
        // 锁中毒时退化为"不更新估计"：拿不到周期只是不节流、不外推，不会让
        // 画面错乱；反过来 `unwrap` 会把一次 panic 变成全局崩溃。
        Err(poisoned) => poisoned.into_inner(),
    };
    let previous = *last;
    if let Some(previous) = previous {
        let mut estimate = match DISPLAY_PERIOD.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *estimate = merge_period_sample(*estimate, at.duration_since(previous));
    }
    *last = Some(at);

    // 每 60 次呈现播报一次。节拍估计是笔尖外推与跳帧档位的输入，必须能直接
    // 看到它落在多少 Hz——它曾经被 layout 间隔污染（74Hz vs 真实 37.7Hz），
    // 而那个错误在别的指标上完全看不出来。
    let n = PRESENT_SAMPLES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n % 60 == 0 {
        let hz = measured_refresh_hz();
        log::info!(
            "[pacing] 呈现样本={n} 估计刷新率={hz:.1}Hz 量化周期={:.2}ms（笔尖外推按此补偿）",
            if hz > 0.0 { 1000.0 / hz } else { 0.0 },
        );
    }
}

/// 实测呈现频率（Hz）。未测得时返回 0.0（此时不节流、不外推）。
pub fn measured_refresh_hz() -> f64 {
    let hz = match DISPLAY_PERIOD.lock() {
        Ok(guard) => guard.period(),
        Err(poisoned) => poisoned.into_inner().period(),
    };
    match hz {
        Some(period) if !period.is_zero() => 1_000_000_000.0 / period.as_nanos() as f64,
        _ => 0.0,
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    /// 呈现周期估计必须只被**呈现**推进。
    ///
    /// 这条测试存在的理由：`App::draw` 的调用次数（layout 次数）曾被当成帧率，
    /// 连续多轮把"呈现 37.7/s"误读成"跑到了 70~100fps"，并据此加了一道拦掉
    /// 100% 输入重绘的闸门。本测试钉住"节拍只认呈现"。
    #[test]
    fn period_estimate_advances_only_on_present() {
        let t0 = Instant::now();
        note_present(t0);
        // 连续"呈现"，间隔 20ms → 50Hz。
        note_present(t0 + Duration::from_millis(20));
        note_present(t0 + Duration::from_millis(40));
        let hz = measured_refresh_hz();
        assert!(
            (hz - 50.0).abs() < 1.0,
            "呈现间隔 20ms 应得 50Hz，实得 {hz}"
        );
    }

    /// 笔迹期的实际帧率必须 **≥ 60fps**（硬要求），且不高于面板刷新率。
    #[test]
    fn stroke_cap_keeps_at_least_60fps() {
        for refresh in [60.0, 75.0, 90.0, 120.0, 144.0, 165.0, 240.0, 360.0] {
            let hz = stroke_hz(refresh, STROKE_MIN_HZ);
            assert!(
                hz >= STROKE_MIN_HZ - 1e-9,
                "{refresh}Hz 面板限到 {STROKE_MIN_HZ} 后只剩 {hz}fps，低于下限"
            );
            assert!(hz <= refresh + 1e-9, "{hz}fps > 面板 {refresh}Hz");
        }
    }

    /// 高刷屏上每秒工作量必须真的下降（否则 GPU 占用率目标达不到）。
    #[test]
    fn stroke_cap_cuts_per_second_work_on_high_refresh_panels() {
        for refresh in [120.0, 144.0, 165.0, 240.0, 360.0] {
            let ratio = stroke_hz(refresh, STROKE_MIN_HZ) / refresh;
            assert!(
                ratio <= 0.55,
                "{refresh}Hz 限帧后帧率占比 {ratio} 过高，省不下 GPU"
            );
        }
    }

    /// 关键不变量：限帧**只会拉长**量化周期，绝不缩短。
    ///
    /// 若此式被破坏，笔尖外推会按比真实更短的时域预测 → 预测过头，
    /// 笔尖跑到指针前面（另一种难看的错位）。
    #[test]
    fn throttling_never_shortens_the_quantization_period() {
        for refresh in [50.0, 60.0, 75.0, 120.0, 144.0, 240.0] {
            let panel_ms = 1000.0 / refresh as f32;
            let eff = effective_period_ms(refresh, true, STROKE_MIN_HZ).expect("已知刷新率");
            assert!(
                eff >= panel_ms - 1e-3,
                "{refresh}Hz: 有效周期 {eff}ms < 面板周期 {panel_ms}ms"
            );
            let idle = effective_period_ms(refresh, false, STROKE_MIN_HZ).expect("已知刷新率");
            assert!(
                (idle - panel_ms).abs() < 1e-3,
                "非笔迹期应等于面板周期：{idle} vs {panel_ms}"
            );
        }
    }

    /// 未测得刷新率 → `None` → 调用方不外推（不猜周期）。
    #[test]
    fn unknown_refresh_disables_extrapolation() {
        assert_eq!(effective_period_ms(0.0, true, STROKE_MIN_HZ), None);
        assert_eq!(effective_period_ms(f64::NAN, true, STROKE_MIN_HZ), None);
        assert_eq!(effective_period_ms(f64::INFINITY, true, STROKE_MIN_HZ), None);
    }

    /// 钉住常见面板的量化周期：跳帧算法一动，这些值和笔尖补偿就跟着变。
    ///
    /// 144Hz 取 n=1（72fps）而非 n=2（48fps）：48 会跌破 60fps 下限。
    #[test]
    fn known_panel_rates_pin_the_quantization_period() {
        let p = effective_period_ms(144.0, true, STROKE_MIN_HZ).unwrap();
        assert!((p - 13.889).abs() < 0.01, "144Hz 笔迹期周期实为 {p}ms");
        let p = effective_period_ms(60.0, true, STROKE_MIN_HZ).unwrap();
        assert!((p - 16.667).abs() < 0.01, "60Hz 笔迹期周期实为 {p}ms");
        let p = effective_period_ms(240.0, true, STROKE_MIN_HZ).unwrap();
        assert!((p - 16.667).abs() < 0.01, "240Hz 笔迹期周期实为 {p}ms");
        // 90Hz 无法整数跳到 60（45 会跌破下限）→ 跑原生 90fps
        assert_eq!(stroke_hz(90.0, STROKE_MIN_HZ), 90.0);
    }
}

/// 连续多少个**同向**带外样本后重锚基线。
///
/// 带外样本有两种成因，统计特征却相同（相对基线都是 ~0.5x 或 ~2x）：
/// 单帧尖峰（GC、首次上传、后台线程抢占）与**刷新率模式切换**（60Hz 面板
/// 换到 120Hz 外接屏）。无状态时二者不可区分，故用计数分离：孤立尖峰直接
/// 丢弃，同向连续 REANCHOR_AFTER 次才承认真切换。
///
/// 8 帧 @60Hz ≈ 133ms：切换后 0.2s 内跟上，期间最多按旧周期算跳帧
/// （宁多画几帧，不漏画）。
const REANCHOR_AFTER: u32 = 8;

/// 刷新率估计（`Cell` 内可无分配更新，故字段全 `Copy`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct PeriodEstimate {
    /// 当前估计；`None` = 尚未测得（此时不节流）。
    period: Option<Duration>,
    /// 连续同向带外样本数。
    off_band: u32,
    /// 上一个带外样本是否比基线**快**（周期更短）。方向翻转即重新计数。
    off_band_faster: bool,
}

impl PeriodEstimate {
    /// 当前估计的周期。`None` = 尚未测得（此时不节流）。
    #[inline]
    pub fn period(&self) -> Option<Duration> {
        self.period
    }
}

/// 把一帧间隔并入刷新率估计，返回新估计。
///
/// **分层拒绝**：
///
/// 1. 落在合理区间外（<4ms / >200ms，即 >250Hz 或 <5Hz）→ 完全忽略，
///    连计数器都不动（否则连续几次荒谬样本会触发重锚）；
/// 2. 与当前估计相差 ±20% 外 → 记一次带外，同向连续
///    [`REANCHOR_AFTER`] 次才重锚（模式切换），否则视为尖峰丢弃；
/// 3. 带内 → EMA 融合，权重 1/8，并清零带外计数。
///
/// 带外样本若被直接采纳，一次 GC 停顿就能把估计拉到 200ms 量级，
/// 之后每帧都算成"该跳几十帧"（画面卡死）；但一味拒绝又让换屏后
/// 永远停在旧周期——两侧都得防。
pub fn merge_period_sample(mut estimate: PeriodEstimate, sample: Duration) -> PeriodEstimate {
    const MIN: Duration = Duration::from_micros(4_000);
    const MAX: Duration = Duration::from_millis(200);
    if sample < MIN || sample > MAX {
        return estimate;
    }
    let Some(previous) = estimate.period else {
        estimate.period = Some(sample);
        return estimate;
    };
    let ratio = sample.as_nanos() as f64 / previous.as_nanos() as f64;
    if !(0.8..=1.2).contains(&ratio) {
        let faster = sample < previous;
        // 方向翻转说明这是另一个尖峰，计数从头来
        estimate.off_band = if estimate.off_band > 0 && estimate.off_band_faster != faster {
            1
        } else {
            estimate.off_band + 1
        };
        estimate.off_band_faster = faster;
        if estimate.off_band >= REANCHOR_AFTER {
            estimate.period = Some(sample);
            estimate.off_band = 0;
        }
        return estimate;
    }
    // EMA，样本权重 1/8：足以跟随刷新率模式切换，又不会被单帧噪声带偏。
    let blended = (previous.as_nanos() as u128 * 7 + sample.as_nanos() as u128) / 8;
    estimate.period = Some(Duration::from_nanos(blended as u64));
    estimate.off_band = 0;
    estimate
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 60Hz 面板 + cap=60 必须**不节流**——这是"稳定 60fps"的核心断言。
    /// 旧实现在此退化为 30fps。
    #[test]
    fn cap_equal_to_refresh_does_not_throttle() {
        assert_eq!(skip_count(60.0, 60), 0, "cap=60 @60Hz 必须交给 vsync");
        assert_eq!(skip_count(59.94, 60), 0, "cap 高于实际刷新率同样不节流");
        assert_eq!(skip_count(60.0, 90), 0);
        assert_eq!(skip_count(60.0, 144), 0);
        assert_eq!(skip_count(60.0, 165), 0);
    }

    /// 高刷面板上 cap=60 应当真的降到 60（60Hz 的一半）而不是保持 144。
    #[test]
    fn cap_below_refresh_throttles_to_nearest_divisor() {
        assert_eq!(skip_count(144.0, 60), 1, "144→60 应隔 1 帧");
        assert_eq!(skip_count(120.0, 60), 1);
        assert_eq!(skip_count(60.0, 30), 1, "60→30 隔 1 帧");
        assert_eq!(skip_count(60.0, 20), 2, "60→20 隔 2 帧");
        assert_eq!(skip_count(60.0, 15), 3, "60→15 隔 3 帧");
        assert_eq!(skip_count(90.0, 30), 2, "90→30 隔 2 帧");
    }

    /// 有效帧率 = refresh / (skip + 1)，必须落在上限的合理邻域内。
    #[test]
    fn effective_rate_tracks_the_cap() {
        for &refresh in &[60.0f64, 75.0, 90.0, 120.0, 144.0, 165.0] {
            for &cap in &[30u32, 60, 90, 120] {
                let skip = skip_count(refresh, cap);
                let effective = refresh / (skip + 1) as f64;
                if skip == 0 {
                    // 不节流时实际帧率 = 刷新率。**cap 是上限不是目标**：cap 高于
                    // 面板刷新率时（如 60Hz 屏 + cap=90）vsync 就是天花板，实际
                    // 帧率必然低于 cap——那是正确行为，不是缺陷。故期望取两者小值。
                    let floor = cap.min(refresh as u32) as f64;
                    assert!(
                        effective >= floor,
                        "refresh={refresh} cap={cap}: 不节流时 {effective} 应 ≥ {floor}"
                    );
                } else {
                    // 节流后实际帧率只能取 refresh/k（k 为整数）上的值，故允许
                    // **略超**上限：`n = round(ratio) - 1` 意味着最坏情况是
                    // ratio→1.5⁻ 时有效帧率 = 1.5/2 = 0.75×cap… 反过来
                    // ratio=2.4（144Hz + cap=60）取 k=2 → 72fps，高于上限 20%。
                    // 取最近可达值而非抖动是本模块的既定取舍（见模块文档），
                    // 故上界按 ratio 的取整误差给 25%。
                    assert!(
                        effective <= cap as f64 * 1.25 + 1e-9,
                        "refresh={refresh} cap={cap}: 节流后 {effective} 超过上限太多"
                    );
                    // 也不应低太多：最近可达值的偏差 ≤ 半个 divisor
                    let relative_gap = (cap as f64 - effective) / cap as f64;
                    assert!(
                        relative_gap < 0.5,
                        "refresh={refresh} cap={cap}: 实际 {effective} 偏离上限过远"
                    );
                }
            }
        }
    }

    /// 未测得刷新率（0）时不得节流——宁可全速也不要按猜测跳帧。
    #[test]
    fn unknown_refresh_never_throttles() {
        assert_eq!(skip_count(0.0, 30), 0);
        assert_eq!(skip_count(-1.0, 30), 0);
        assert_eq!(skip_count(60.0, 0), 0, "cap=0 视为不限制");
    }

    /// 唤醒点必须落在 vsync 区间**正中**，前后裕度对称——
    /// 这正是旧实现（唤醒点压在 vsync 边界）导致必然错帧的原因。
    #[test]
    fn wake_offset_lands_mid_interval() {
        for &refresh in &[60.0f64, 90.0, 144.0, 240.0] {
            let period = 1_000_000.0 / refresh;
            for skip in 0..4u32 {
                let offset = throttle_wake_offset_us(refresh, skip) as f64;
                let within_interval = offset % period;
                let margin_before_vsync = period - within_interval;
                assert!(
                    (margin_before_vsync - period / 2.0).abs() < 1.0,
                    "refresh={refresh} skip={skip}: 唤醒点 {offset} 应在区间正中，\
                     实际距下一个 vsync 还有 {margin_before_vsync}（应为 {}）",
                    period / 2.0
                );
            }
        }
    }

    /// 余量校验：高刷 + 每帧工作很重时，节流没有意义，应退回全速。
    #[test]
    fn insufficient_margin_falls_back_to_full_rate() {
        // period/2 = 2.5ms < WORK_ESTIMATE_US(3ms) → 不节流
        assert_eq!(skip_count(400.0, 30), 0, "余量不足时不应节流");
        // period/2 = 4.17ms（60Hz）> 3ms → 允许节流
        assert_eq!(skip_count(60.0, 30), 1);
    }

    /// 刷新率估计：首个样本建立基线，之后用 EMA 跟随。
    #[test]
    fn period_estimate_seeds_then_ema() {
        let base = Duration::from_micros(16_667);
        let first = merge_period_sample(PeriodEstimate::default(), base);
        assert_eq!(first.period(), Some(base), "首个合理样本应直接作为基线");

        // 连续同周期样本 → 估计保持在该周期附近
        let mut est = first;
        for _ in 0..40 {
            est = merge_period_sample(est, base);
        }
        let us = est.period().expect("估计").as_micros() as f64;
        assert!(
            (us - 16_667.0).abs() < 1.0,
            "同周期样本下 EMA 应稳定在 16667µs，实得 {us}"
        );
    }

    /// 离群样本（长帧尖峰）必须被拒绝，否则会算错跳帧数。
    ///
    /// 注意这与「模式切换」的区别只在于**孤立 vs 连续**：本测试每个样本
    /// 都从同一基线出发，永远攒不满 REANCHOR_AFTER 次同向带外。
    #[test]
    fn period_estimate_rejects_outliers() {
        let base = Duration::from_micros(16_667);
        let est = merge_period_sample(PeriodEstimate::default(), base);
        // 200ms 的卡顿帧（> +20%）
        assert_eq!(
            merge_period_sample(est, Duration::from_millis(200)).period(),
            est.period(),
            "长帧尖峰不得污染刷新率估计"
        );
        // 8ms 的异常快帧（< -20%）
        assert_eq!(
            merge_period_sample(est, Duration::from_micros(8_000)).period(),
            est.period(),
            "异常快帧不得污染刷新率估计"
        );
        // 区间外的样本（<4ms 或 >200ms）一律忽略
        assert_eq!(
            merge_period_sample(est, Duration::from_micros(500)).period(),
            est.period()
        );
        assert_eq!(
            merge_period_sample(est, Duration::from_secs(1)).period(),
            est.period()
        );
        // 同一方向连续 REANCHOR_AFTER - 1 次也还不足以重锚
        let mut repeated = est;
        for _ in 1..REANCHOR_AFTER {
            repeated = merge_period_sample(repeated, Duration::from_millis(200));
            assert_eq!(
                repeated.period(),
                est.period(),
                "连续 {}_200ms 尖峰仍应视为噪声",
                repeated.off_band
            );
        }
    }

    /// 刷新率模式切换（如从 60Hz 切到 120Hz 外接屏）最终要被跟上——
    /// 连续 REANCHOR_AFTER 个同向带外样本后重锚，EMA 随即收敛到新周期。
    #[test]
    fn period_estimate_follows_a_mode_switch() {
        let mut est = merge_period_sample(PeriodEstimate::default(), Duration::from_micros(16_667));
        for _ in 0..40 {
            est = merge_period_sample(est, Duration::from_micros(8_333));
        }
        let us = est.period().expect("估计").as_micros() as f64;
        assert!(
            (us - 8_333.0).abs() < 100.0,
            "切到 120Hz 后估计应收敛到 8333µs 附近，实得 {us}"
        );
    }
}
