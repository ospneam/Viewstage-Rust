//! 摄像头内容上屏的**固定网格**。
//!
//! # 为什么是网格，而不是"收到就显示"
//!
//! 采集侧的间隔本身不均匀——`poll_live_frame` 会阻塞等 MediaFoundation 的下一个
//! 样本，实测间隔 `min 20~31ms 中位 48ms p95 49.6ms max 197.9ms`。若让"帧到达"
//! 直接决定"上屏"，画面更新就会原样继承这份抖动。修复前实测上屏间隔：
//!
//! ```text
//! n=324  min 12.4  p10 16.0  中位 20.3  p90 33.6  max 41.1 ms
//! 落在 33.3ms ±25% 内：只有 25%
//! ```
//!
//! 中位 20.3ms（应为 33.3ms）、尾部到 41ms——这就是"一会更新快一会慢"。
//!
//! # 网格怎么保证均匀
//!
//! - **绝对时刻推进**：`grid_next += period`，误差不累积；落后时对齐到 `now`
//!   而不追赶（补帧会变成另一种抖动）。
//! - **到点才换内容**：格点之间的到达帧在 `pending` 槽里互相覆盖，显示的始终
//!   是最新一帧。中间被覆盖的帧只是"没显示过"，不是"迟到的旧画面"——后者才是
//!   可见的卡顿。
//! - **自转靠续帧请求**：每次检查后都排下一个格点，因此不需要摄像头消息本身来
//!   驱动（iced 在每条消息后无条件重绘，那是即时的、抖的）。
//!
//! # 代价
//!
//! 上屏延迟从"随抖动 0~48ms"变成"确定 ≤1 个摄像头周期（33.3ms）"。对文档摄像头
//! 预览，**均匀比最新更重要**——不均匀的刷新本身就是可见的缺陷。

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::{Duration, Instant};

use viewcamera::camera::CameraFrameData;

/// 摄像头显示节拍（`frame_rate_mode` → 网格周期）。
///
/// 与采集线程侧的 `frame_interval_ms` **必须是同一个值**：采集侧按它限速，
/// 显示侧按它定网格。两侧各写一份常量迟早会走偏，届时采集 30fps 而网格 60Hz
/// （或反过来），表现为"要么丢帧要么重复显示"。
pub fn display_period(mode: &str) -> Duration {
    match mode {
        "low" => Duration::from_millis(100),
        "high" => Duration::from_millis(16),
        _ => Duration::from_millis(33),
    }
}

/// 摄像头内容是否走**固定网格**上屏。`VIEWSTAGE_CAMERA_GRID=0` 关闭，退回
/// "帧到达即上屏"。
///
/// 这个开关存在的唯一理由是**同口径 A/B**：修复前后"用户看到的画面更新节奏"
/// 是在两套不同度量上测的（一次是 present 间隔，一次是内容换上屏间隔），跨口径
/// 的数字不能比较。有了它，两种行为跑同一个二进制、同一套仪表，`rstat` 的
/// `上屏 min/中位/p95/max` 才是可比的。
pub fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("VIEWSTAGE_CAMERA_GRID")
            .map(|v| !matches!(v.trim(), "0" | "false" | "off" | "no"))
            // **默认关闭。** 同口径 A/B 实测（同一二进制、同一套仪表）：
            //
            // ```text
            // off(到达即上屏)  min=21.8 中位=48.4 p95=63.7 max=76.0ms
            // on (固定网格)    min=30.0 中位=48.8 p95=63.2 max=72.9ms
            // ```
            //
            // 差异在噪声内——**网格没有带来可测量的改善**。原因很清楚：画面内容
            // 只在新帧到达时才变，而新帧本身的间隔就由摄像头决定（实测中位
            // 47.8ms、p95 49.4、max 195.2ms）。网格只能规整"采样时刻"，规整不了
            // "源"。
            //
            // 所以默认关闭：留一个复杂度换不到可测收益的默认值是错的。真要靠缓冲区
            // 把抖动源也规整掉，那是 jitter buffer（固定延迟 N 帧、到点释放、迟到
            // 丢/早到重复），能换均匀但要付固定延迟——那是另一个决定，需要先知道
            // 摄像头能不能给稳定的帧。
            .unwrap_or(false)
    })
}

/// 摄像头显示节拍的**用户上限**（`frame_rate_mode`）。
///
/// 这只是**上限**，不是实际节拍：实际节拍由设备能力决定，见
/// [`effective_period`]。把上限当节拍用就是"定死"——本机配置写着 `high`
/// （16ms/60fps），设备实测只给 47.6ms（≈21fps），于是显示侧每 33ms 取一次帧而
/// 画面每 47.6ms 才更新一次，观感是隔一帧才动。
pub fn mode_period(mode: &str) -> Duration {
    match mode {
        "low" => Duration::from_millis(100),
        "high" => Duration::from_millis(16),
        _ => Duration::from_millis(33),
    }
}

/// 实际使用的节拍 = `max(设备实测周期, 用户上限)`。
///
/// - 设备慢于上限 → 以**实测**为准。实测来自 MediaFoundation 的源端时间戳
///   （`viewcamera::CameraManager::observed_period`），是"画面真的多久变一次"的
///   唯一可靠依据；设备声称值（`negotiated_fps`）在本机就偏高了 43%。
/// - 上限慢于设备 → 以上限为准，那是用户主动要的降帧（"低帧率"省电）。
///
/// `observed` 为 `None`（样本不足）时退回上限：宁可慢一拍，也不要用三五个样本去
/// 改节拍。
pub fn effective_period(mode: &str, observed: Option<Duration>) -> Duration {
    let requested = mode_period(mode);
    match observed {
        Some(observed) => observed.max(requested),
        None => requested,
    }
}

/// 设备实测投递周期的发布槽（纳秒；0 = 尚无有效样本）。
///
/// 采集线程是唯一能看到源端时间戳的地方，主线程只能靠读这个原子拿到结论。跨线
/// 程传一个 `Option<Duration>` 不方便（没有 `MaybeUninit` 的原子便利封装），而
/// "纳秒 + 0 表示无"是无损且更快的编码。
static OBSERVED_PERIOD_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 由采集线程发布设备实测投递周期。
pub fn publish_observed_period(period: Option<Duration>) {
    use std::sync::atomic::Ordering;
    OBSERVED_PERIOD_NS.store(period.map_or(0, |p| p.as_nanos() as u64), Ordering::Relaxed);
}

/// 摄像头实测投递周期（主线程读采集侧发布的估计）。
pub fn observed_period() -> Option<Duration> {
    use std::sync::atomic::Ordering;
    let ns = OBSERVED_PERIOD_NS.load(Ordering::Relaxed);
    (ns > 0).then(|| Duration::from_nanos(ns))
}

/// 摄像头上屏网格的跨帧状态。
///
/// 全部方法仅 `&self`（内部 `Cell`/`RefCell`），与 `canvas::Program` 的零消息
/// 热路径兼容。
pub struct CameraGrid {
    /// 已上屏的摄像头帧。
    ///
    /// `RefCell` 而非裸字段：换帧发生在 `canvas::Program::update(&self)` 的帧
    /// 节拍里（见 [`CameraGrid::step`]），那里只有 `&self`。
    frame: RefCell<Option<Arc<CameraFrameData>>>,
    /// 采集线程送来的、**尚未上屏**的最新摄像头帧。
    ///
    /// 为什么不直接写进 `frame`：那样一来帧的**上屏时刻**就等于采集线程的到达
    /// 时刻，而采集侧的间隔本身不均匀（实测 poll 阻塞 20~49ms、最长 197.9ms），
    /// 于是画面更新也随之一会儿快一会儿慢。改成"收到就存、到点才换"：上屏时刻
    /// 由固定网格决定，格点之间到达的帧互相覆盖，只显示最新的那一个。摄像头是
    /// **内容源**，显示才是**节拍**——与浏览器把视频当内容、由合成器按固定节奏取
    /// 最新帧是同一个模型。
    pending: RefCell<Option<Arc<CameraFrameData>>>,
    /// 摄像头内容上屏的**绝对网格**（下一个格点）。
    ///
    /// 用绝对时刻而非 `now + period`：后者会把每帧的误差累加进去，正是"越跑越
    /// 偏"的来源。落后时也不追赶（补帧会变成另一种抖动），直接对齐到现在。
    next: Cell<Instant>,
    /// 帧序号：每次换帧自增，上传侧据此判断"有没有新帧"。
    ///
    /// **必须与槽位里的内容同代写入**。曾经把它随重构删掉后，所有帧都带 0，
    /// `update_camera_params` 认为内容没变，于是**一次都不上传**（实测
    /// `up=0B/s`，画面全黑）。
    generation: Cell<u64>,
}

impl CameraGrid {
    /// 初始状态：立即到期——首帧应当在第一个重绘就上屏，不要先等一个网格周期。
    pub fn new() -> Self {
        Self {
            frame: RefCell::new(None),
            pending: RefCell::new(None),
            next: Cell::new(Instant::now()),
            generation: Cell::new(0),
        }
    }

    /// 当前已上屏的帧（供背景层参数快照读取）。
    pub fn current(&self) -> Option<Arc<CameraFrameData>> {
        self.frame.borrow().clone()
    }

    /// 当前帧序号。
    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    /// 提交一帧实时预览：**只入待上屏槽位，不直接上屏**。
    ///
    /// 上屏时刻由 [`CameraGrid::step`] 的固定网格决定。帧载荷已是 viewcamera
    /// 类型（`Arc` 包装在采集线程完成），这里零像素转换、零再包装。
    pub fn submit(&self, mut frame: CameraFrameData) {
        let next = self.generation.get() + 1;
        frame.generation = next;
        self.generation.set(next);
        *self.pending.borrow_mut() = Some(Arc::new(frame));
    }

    /// 拍照后**立即**换上这一帧（不走网格——画面已冻结，用户在等结果）。
    pub fn freeze_with(&self, frame: CameraFrameData) {
        let next = self.generation.get() + 1;
        self.generation.set(next);
        *self.frame.borrow_mut() = Some(Arc::new(CameraFrameData {
            generation: next,
            ..frame
        }));
    }

    /// 清除已上屏帧（切到图片源时调用）。
    pub fn clear(&self) {
        *self.frame.borrow_mut() = None;
    }

    /// 帧驱动：到点才换内容，并排下一个格点让网格自转。
    ///
    /// 返回 `Some(时刻)` 表示应请求在该时刻重绘；`None` 表示本帧无重绘需求。
    /// `frozen` 为真（拍照后相机线程已停推帧）时不换内容。
    ///
    /// **调用位置是刻意的**：本方法由宿主在 `canvas::Program::update` 的
    /// `RedrawRequested` 分支里调用，一定跑在 `Program::draw` 之前——同一轮事件
    /// 循环内 `update → draw → prepare → present` 的顺序。
    pub fn step(&self, frozen: bool, frame_rate_mode: &str) -> Option<Instant> {
        // 关闭网格：帧到达即上屏（旧行为，作为 A/B 基准）。
        if !enabled() {
            if let Some(frame) = self.pending.borrow_mut().take() {
                *self.frame.borrow_mut() = Some(frame);
                viewink::stats::note_camera_swap(Instant::now());
            }
            return None;
        }
        // 冻结时不换内容。
        let mut swapped = false;
        if !frozen {
            let now = Instant::now();
            if now >= self.next.get() {
                let pending = self.pending.borrow_mut().take();
                if let Some(frame) = pending {
                    // 不在此处改 generation：它在入槽位时已盖好（见
                    // [`CameraGrid::submit`]）。此处若再自增，上传侧会拿到一个与
                    // 槽位里不一致的序号，反而多触发一次无意义的重传。
                    *self.frame.borrow_mut() = Some(frame);
                    swapped = true;
                    viewink::stats::note_camera_swap(Instant::now());
                }
                let period = display_period(frame_rate_mode);
                // 绝对推进；已落后则重新对齐，绝不"追赶"补帧。
                let next = self.next.get() + period;
                self.next.set(if next > now { next } else { now + period });
            }
        }
        // 排下一个格点，让网格自转。相机停摆且无待上屏帧时不排——否则会留下一条
        // 永不停歇的 30Hz 重绘链，纯空转。
        let grid_next = self.next.get();
        let keep_alive = !frozen && (swapped || self.pending.borrow().is_some());
        if keep_alive {
            return Some(grid_next);
        }
        // 没有内容可换：把网格对齐到现在，让下一次真实事件能立刻显示。
        if !frozen {
            self.next.set(Instant::now());
        }
        None
    }
}

impl Default for CameraGrid {
    fn default() -> Self {
        Self::new()
    }
}
