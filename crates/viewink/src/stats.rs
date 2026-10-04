//! 渲染统计：按秒聚合渲染帧数、相机帧数、view 重建开销与各层上传量。
//!
//! 仅计数与日志输出，不参与渲染行为；`end_frame` 每渲染帧调用一次，
//! 距上次输出满 1 秒时打印一行 `[rstat]` 摘要并清零窗口。

use iced_wgpu::wgpu;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static FRAMES: AtomicU64 = AtomicU64::new(0);
static CAMERA_MESSAGES: AtomicU64 = AtomicU64::new(0);
static VIEW_CALLS: AtomicU64 = AtomicU64::new(0);
static VIEW_NANOS: AtomicU64 = AtomicU64::new(0);
static PLANES_ALIGNED: AtomicU64 = AtomicU64::new(0);
static PLANES_STAGED: AtomicU64 = AtomicU64::new(0);
static UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static STAGED_BYTES: AtomicU64 = AtomicU64::new(0);
static MIP_PASSES: AtomicU64 = AtomicU64::new(0);
static STATIC_UPLOADS: AtomicU64 = AtomicU64::new(0);
static STATIC_UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static STATIC_VISIBLE: AtomicU64 = AtomicU64::new(0);
static STATIC_TOTAL: AtomicU64 = AtomicU64::new(0);
static STATIC_RESIDENT_SEGS: AtomicU64 = AtomicU64::new(0);
static STATIC_DRAW_RANGES: AtomicU64 = AtomicU64::new(0);
static DYN_UPLOADS: AtomicU64 = AtomicU64::new(0);
static DYN_UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static CURSOR_UPLOADS: AtomicU64 = AtomicU64::new(0);
static CURSOR_UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static STATIC_DRAWS: AtomicU64 = AtomicU64::new(0);
static DYNAMIC_DRAWS: AtomicU64 = AtomicU64::new(0);
static CURSOR_DRAWS: AtomicU64 = AtomicU64::new(0);
static BINDS: AtomicU64 = AtomicU64::new(0);
/// 因帧率节流而**主动跳过**的渲染请求数（预期行为，非异常）。
///
/// 见 `app::pacing`：高刷屏 + 较低上限时按 `skip` 个 vsync 跳帧渲染。
/// 这些事件没有请求重绘，因此不计入 `frames`。
static THROTTLED_FRAMES: AtomicU64 = AtomicU64::new(0);

/// **长帧**计数：相邻两渲染帧间隔显著超过实测刷新周期。
///
/// 这是"帧率是否稳定"的唯一硬指标。轻微抖动（几 %）不计入；超过
/// `LONG_FRAME_RATIO` 倍周期说明某一帧没赶上 vsync，整帧延迟一整个周期
/// ——肉眼可见为顿挫。
static LONG_FRAMES: AtomicU64 = AtomicU64::new(0);

/// 实测刷新周期（EMA，纳秒），0 = 尚未测得。
static REFRESH_PERIOD_NS: AtomicU64 = AtomicU64::new(0);

/// 窗口内最长帧间隔（纳秒）。单帧尖峰的绝对量级，是长帧计数的补充。
static MAX_FRAME_NS: AtomicU64 = AtomicU64::new(0);

/// 判定长帧的阈值倍数。AutoVsync 下帧间隔 ≈ 刷新周期，故 1.5 倍即意味着
/// 至少错过了一个 vsync。再宽松就会漏掉真实的卡顿。
const LONG_FRAME_RATIO: f64 = 1.5;

/// 批注层 prepare（上传 + 剔除）单帧耗时累计（纳秒）与调用数。
///
/// **帧预算的直接度量**：AutoVsync 下帧起点由 vsync 钉死，一帧的 CPU 工作
/// 只要超过刷新周期就必然错过本次 vsync、整帧延迟一整个周期（`LONG_FRAMES`
/// 的成因）。本指标给出"我们还能再放多少活进一帧"的答案——判断卡顿是
/// 我们的开销超预算还是别处抢占的唯一依据。
static PREPARE_NANOS: AtomicU64 = AtomicU64::new(0);
static PREPARE_CALLS: AtomicU64 = AtomicU64::new(0);
static PREPARE_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// `App::draw` 内属主 crate 的单帧耗时累计（纳秒）与调用数。
///
/// 与 [`PREPARE_NANOS`] 分开统计：前者是"输入→几何"（进行中笔迹增量
/// 镶嵌），后者是"几何→GPU"（打包上传 + 视口剔除）。两段之和才是宿主
/// 在帧内可控的 CPU 预算。
static FRAME_WORK_NANOS: AtomicU64 = AtomicU64::new(0);
static FRAME_WORK_CALLS: AtomicU64 = AtomicU64::new(0);
static FRAME_WORK_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// 帧间隔中"没有人请求重绘、我们在等输入"的次数与最长值（纳秒）。
///
/// **这是对旧 `long_frames` 的一次纠错。** `note_frame_interval` 测的是两次
/// `App::draw` 进入时刻之差，其中**包含空闲等待**：没人请求重绘时 iced 根本
/// 不调用 `draw`，于是下一帧的间隔里就塞进了整段空档。旧实现把这段空档
/// 也计入 `LONG_FRAMES` 与 `MAX_FRAME_NS`，于是空闲被读成停顿——实测报出
/// `long=9 worst=53ms`，实际是 9 段没有输入的安静期。
static IDLE_GAPS: AtomicU64 = AtomicU64::new(0);
static IDLE_GAP_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// 原始触摸事件计数（未去重，含 Hover/Move）。
///
/// 与 `touch_strokes_started`（笔画数）配对看：事件率远高于帧率时，多出来
/// 的部分就是被合并掉的中间采样点——这是"笔迹发涩"的直接量化依据。
static TOUCH_EVENTS: AtomicU64 = AtomicU64::new(0);

/// 上报一个原始触摸事件。
pub fn add_touch_event() {
    TOUCH_EVENTS.fetch_add(1, Ordering::Relaxed);
}

/// **输入→帧起点**延迟（纳秒）与调用数、最长值。
///
/// 这才是用户感知的"跟手"延迟的前半段：一次触摸/移动事件发生后，到下一次
/// `App::draw` 入口之间隔了多久。
///
/// 为什么必须有它：`draw` 的函数体耗时只覆盖我们自己的指令，而帧间隔里
/// 混着三种完全不同的东西——等待输入的空闲、GPU/呈现阻塞、以及**事件已经
/// 到达但还没被画**。前三者混在一起时（此前的 `long`/`worst`）无法区分
/// "没人动"和"动了没画出来"。本指标直接量最后这一种：事件已到达。
static INPUT_LATENCY_NANOS: AtomicU64 = AtomicU64::new(0);
static INPUT_LATENCY_CALLS: AtomicU64 = AtomicU64::new(0);
static INPUT_LATENCY_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// 上报"最近一次输入事件 → 本帧起点"的延迟。
pub fn add_input_latency(elapsed: Duration) {
    let nanos = elapsed.as_nanos() as u64;
    INPUT_LATENCY_NANOS.fetch_add(nanos, Ordering::Relaxed);
    INPUT_LATENCY_CALLS.fetch_add(1, Ordering::Relaxed);
    bump_max(&INPUT_LATENCY_MAX_NS, nanos);
}

/// `App::draw` 函数体自身的 CPU 耗时（纳秒）与调用数、最长值。
///
/// 帧间隔回答不了"我们慢不慢"（它混入了等待），**这个量才是**：它只覆盖
/// 我们自己的指令，从 `draw` 入口到出口。若它稳定在亚毫秒而帧间隔仍有
/// 几十毫秒，那几十毫秒就在我们之外（GPU / 驱动 / 合成器），继续优化本
/// crate 是白费力气。
static DRAW_NANOS: AtomicU64 = AtomicU64::new(0);
static DRAW_CALLS: AtomicU64 = AtomicU64::new(0);
static DRAW_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// `App::draw` 耗时的对数直方图（每桶 2 的幂，微秒），用于算 p95。
///
/// 固定 64 桶、每桶一个 `AtomicU64`，无分配、无锁；只在帧尾做一次
/// `fetch_add`。`fetch_add` 的 RMW 约 20ns，相对每帧数百微秒可忽略。
const DRAW_P95_BUCKETS: usize = 64;
static DRAW_HIST: [AtomicU64; DRAW_P95_BUCKETS] =
    [const { AtomicU64::new(0) }; DRAW_P95_BUCKETS];

/// 上一帧是否走了静态层离屏光栅缓存（见 [`note_raster_blit`]）。
static RASTER_BLIT: AtomicU64 = AtomicU64::new(0);

/// 缓存光栅走的是**增量**（只补画水位之后的段）的次数。
///
/// 与全量次数对照才能证明增量真的在跑，而不是"代码写了但永远走不到"。
/// 没有这个数就只能靠读代码确认，而读代码确认不了运行时分支。
static RASTER_EXTEND: AtomicU64 = AtomicU64::new(0);
static RASTER_PARTIAL: AtomicU64 = AtomicU64::new(0);
static RASTER_FULL: AtomicU64 = AtomicU64::new(0);
/// 增量路径实际补画的段数合计（用来量化省掉了多少）。
static RASTER_TAIL_SEGS: AtomicU64 = AtomicU64::new(0);

/// 记一次缓存光栅：增量还是全量、补画了多少段。
pub fn note_raster_pass(incremental: bool, partial: bool, tail_segs: u64) {
    if partial {
        // 局部重画（脏矩形）也算"没重画整层"，但要能单独看见——它是橡皮擦 /
        // 撤销 / 后台线程换扣除版这三条路径的主要归宿。
        RASTER_PARTIAL.fetch_add(1, Ordering::Relaxed);
        RASTER_EXTEND.fetch_add(1, Ordering::Relaxed);
        RASTER_TAIL_SEGS.fetch_add(tail_segs, Ordering::Relaxed);
    } else if incremental {
        RASTER_EXTEND.fetch_add(1, Ordering::Relaxed);
        RASTER_TAIL_SEGS.fetch_add(tail_segs, Ordering::Relaxed);
    } else {
        RASTER_FULL.fetch_add(1, Ordering::Relaxed);
    }
}

/// 本统计窗内 (增量次数, 全量次数, 增量段数)。
pub fn raster_pass_counts() -> (u64, u64, u64, u64) {
    (
        take(&RASTER_EXTEND),
        take(&RASTER_PARTIAL),
        take(&RASTER_FULL),
        take(&RASTER_TAIL_SEGS),
    )
}

/// 帧内 GPU 成本核算（见 [`set_raster_cost`]）。
static RASTER_COST_CACHE: AtomicU64 = AtomicU64::new(0);
static RASTER_COST_INK: AtomicU64 = AtomicU64::new(0);
static RASTER_COST_BLIT: AtomicU64 = AtomicU64::new(0);
static RASTER_COST_SEGS: AtomicU64 = AtomicU64::new(0);

/// 单调取大（诊断上界，原子竞争无碍——只是上界）。
#[inline]
/// 上报本帧是否走了静态层离屏缓存（一次 blit 代替重画整份可见文档）。
///
/// 高密度文档的 GPU 开销判读点：`raster=1` 说明缓存命中，
/// `raster=0` 说明密度闸门没开（正常墨迹量）或键正在变动（平移/缩放）。
pub fn note_raster_blit(used: bool) {
    RASTER_BLIT.store(used as u64, Ordering::Relaxed);
}

/// 上一帧是否用了静态层离屏缓存。
pub fn raster_blit_used() -> bool {
    RASTER_BLIT.load(Ordering::Relaxed) != 0
}

/// 帧内 GPU 成本核算（**估计值**，非硬件计时）：`(走缓存, 墨迹片元估计,
/// 全屏 blit 纹素, 可见段数)`。
///
/// 口径：一片胶囊覆盖约 `2r·L` 像素，取典型笔宽/段长 → 每段约 32 片元
/// （`raster_cache::FRAGMENTS_PER_SEG`）。这不是 profiler，量级对就行：它要回答的
/// 是"墨迹比全屏 blit 贵还是便宜"，而这个比值在两个数量级的差距上不会因
/// 常数不准而翻转。真正的计时需要 timestamp query，那要在真机上做。
pub fn set_raster_cost(cached: bool, ink_fragments: u64, blit_texels: u64, visible_segs: u64) {
    RASTER_COST_CACHE.store(cached as u64, Ordering::Relaxed);
    RASTER_COST_INK.store(ink_fragments, Ordering::Relaxed);
    RASTER_COST_BLIT.store(blit_texels, Ordering::Relaxed);
    RASTER_COST_SEGS.store(visible_segs, Ordering::Relaxed);
}

/// 读取 [`set_raster_cost`] 写入的核算值（只读不清零）。
pub fn raster_cost() -> (bool, u64, u64, u64) {
    (
        RASTER_COST_CACHE.load(Ordering::Relaxed) != 0,
        RASTER_COST_INK.load(Ordering::Relaxed),
        RASTER_COST_BLIT.load(Ordering::Relaxed),
        RASTER_COST_SEGS.load(Ordering::Relaxed),
    )
}

fn bump_max(slot: &AtomicU64, nanos: u64) {
    let mut current = slot.load(Ordering::Relaxed);
    while nanos > current {
        match slot.compare_exchange_weak(current, nanos, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

/// 上报批注层 prepare 单帧耗时。
#[inline]
pub fn note_prepare_work(elapsed: Duration) {
    let nanos = elapsed.as_nanos() as u64;
    PREPARE_NANOS.fetch_add(nanos, Ordering::Relaxed);
    PREPARE_CALLS.fetch_add(1, Ordering::Relaxed);
    bump_max(&PREPARE_MAX_NS, nanos);
}

/// 上报 `App::draw` 内属主 crate 的单帧耗时。
#[inline]
pub fn note_frame_work(elapsed: Duration) {
    let nanos = elapsed.as_nanos() as u64;
    FRAME_WORK_NANOS.fetch_add(nanos, Ordering::Relaxed);
    FRAME_WORK_CALLS.fetch_add(1, Ordering::Relaxed);
    bump_max(&FRAME_WORK_MAX_NS, nanos);
}

/// 报表节流时钟（1 秒）。用 `AtomicU64` 存 `Instant` 的 elapsed 毫秒而非
/// `Mutex<Instant>`：`end_frame` 在**每渲染帧**调用，原实现每帧一次
/// 无竞争 `Mutex` lock/unlock。取模回绕安全（`Instant` 单调，进程生命周期内
/// `elapsed` 不回绕）。
///
/// 初值取 [`REPORT_UNINIT`]（`u64::MAX`）而**不是 0**：`now_ms()` 是
/// 进程启动以来的毫秒数，进程启动第一毫秒内它本身就可能是 0——用 0 当
/// "未初始化"哨兵会让首次 `end_frame` 把基线写成 0，哨兵与真实值同形，
/// 之后每帧都判定为"刚初始化"→ 报表永远不输出（单元测试里表现为基线断言
/// 失败并把脏状态留给后续测试）。
static REPORT_AT_MS: AtomicU64 = AtomicU64::new(REPORT_UNINIT);

/// `REPORT_AT_MS` 的"尚未初始化"哨兵：与任何真实毫秒时间戳不同形。
const REPORT_UNINIT: u64 = u64::MAX;

/// 相机帧格式缓存（仅在尺寸变化时写入，锁持有时间极短）。
static CAMERA_FORMAT: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// 进程内单调递增的毫秒时钟。
///
/// **不要写成 `Instant::now().elapsed()`** —— `Instant::elapsed()` 的语义是
/// "从另一个 instant 到 **self** 的时长"，对一个刚创建的 instant 调用它，结果
/// 恒为 ≈0。于是 `report_window` 的 `now - last` 永远是 0，1 秒报表通道
/// 静默地永不输出（这个 bug 真实发生过：单元测试只验纯函数 `report_window`
/// 的合成输入，正好绕开了它）。
#[inline]
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis() as u64
}

#[inline]
fn take(counter: &AtomicU64) -> u64 {
    counter.swap(0, Ordering::Relaxed)
}

/// 相机帧消息计数；帧尺寸变化时打印一次上传行宽对齐信息。
pub fn note_camera_frame(width: u32, height: u32) {
    CAMERA_MESSAGES.fetch_add(1, Ordering::Relaxed);
    let mut last = CAMERA_FORMAT.lock().expect("camera format lock");
    if *last != Some((width, height)) {
        // UV 交错平面行宽（字节）= width，与 Y 平面一致，故只报一个对齐判定
        log::info!(
            "[rstat] camera frame {width}x{height} bytes_per_row={width} row_256_aligned={}",
            width % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT == 0
        );
        *last = Some((width, height));
    }
}

/// view 重建耗时与次数。
/// 采集线程相邻两轮的间隔。只在采集线程调用。
///
/// 与上屏侧（`iced_winit` 的呈现计数）**必须分开记**：两者不一致就说明抖动
/// 产生在事件循环/呈现路径而非摄像头；一致则抖动来自采集线程本身。修法完全
/// 不同，混在一起就无从下手。
static CAMERA_INTERVALS: Mutex<Vec<Duration>> = Mutex::new(Vec::new());

/// 因通道满（上一帧未被主线程消费）而整周期跳过的次数。
///
/// 每次丢弃都会让节拍断一整拍（下一轮变成"等摄像头的下一个间隔"），因此
/// **丢帧数直接等于可见的卡顿次数**。实测静默期曾观测到 17.9~59.6ms 的
/// 上屏间隔——最大的一跳正是一整拍被丢掉。
static CAMERA_DROPS: AtomicU64 = AtomicU64::new(0);

/// 记一次采集侧间隔。
pub fn note_camera_interval(delta: Duration) {
    if let Ok(mut samples) = CAMERA_INTERVALS.lock() {
        // 只留最近 240 个（≈4 秒 @60fps），够看出抖动形态又不至于无界增长。
        if samples.len() >= 240 {
            samples.remove(0);
        }
        samples.push(delta);
    }
}

/// 记一次"因通道满而丢弃整轮"。
pub fn note_camera_drop() {
    CAMERA_DROPS.fetch_add(1, Ordering::Relaxed);
}

/// MediaFoundation 报告的**源端**帧间隔。
///
/// 与 [`note_camera_interval`]（我们收到帧的间隔）配对使用，是判别抖动归属的
/// 决定性证据：
///
/// - 两者一致 → 抖动来自设备/驱动，我们的管线无责；
/// - 源端均匀而收到的不均匀 → **是我们丢了样本**，问题在采集管线。
static CAMERA_SOURCE: Mutex<Vec<Duration>> = Mutex::new(Vec::new());

/// 记一次源端（MF 时间戳）间隔。
pub fn note_camera_source_interval(delta: Duration) {
    if let Ok(mut samples) = CAMERA_SOURCE.lock() {
        if samples.len() >= 240 {
            samples.remove(0);
        }
        samples.push(delta);
    }
}

/// 摄像头画面**内容换上屏**的时刻间隔。
///
/// 这才是"用户看到的画面多久更新一次"——与呈现（present）间隔不是一回事：
/// 一秒可能呈现 60 次，但内容只换 30 次，其余 29 次画的是同一张。
///
/// 修复前无法测量：帧到达即上屏，两者被混在一起，导致"画面更新节奏"这个
/// 需求一直无法验证，只能靠肉眼。分成两个量之后才能证明规整是否生效。
static CAMERA_SWAPS: Mutex<Vec<Duration>> = Mutex::new(Vec::new());
static LAST_SWAP: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// 记一次内容换上屏。
pub fn note_camera_swap(at: std::time::Instant) {
    let Ok(mut last) = LAST_SWAP.lock() else {
        return;
    };
    if let Some(prev) = *last {
        if let Ok(mut samples) = CAMERA_SWAPS.lock() {
            if samples.len() >= 240 {
                samples.remove(0);
            }
            samples.push(at.duration_since(prev));
        }
    }
    *last = Some(at);
}

/// `poll_live_frame` 自身的阻塞耗时。
///
/// 与 [`note_camera_interval`] 分开量的原因：轮次间隔 = poll + sleep，二者混在
/// 一起就分不清"节拍被摄像头拉长"（poll 阻塞）与"节拍被自己的 sleep 算错"。
static CAMERA_POLL: Mutex<Vec<Duration>> = Mutex::new(Vec::new());

/// 记一次 poll 阻塞耗时。
pub fn note_camera_poll(elapsed: Duration) {
    let Ok(mut samples) = CAMERA_POLL.lock() else {
        return;
    };
    // 只留最近 240 个，与采集间隔窗口一致，便于同口径比较。
    if samples.len() >= 240 {
        samples.remove(0);
    }
    samples.push(elapsed);
}

/// view 重建耗时累加。
pub fn add_view(elapsed: Duration) {
    VIEW_CALLS.fetch_add(1, Ordering::Relaxed);
    VIEW_NANOS.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
}

/// 单个纹理平面上传：`aligned` 表示走 write_texture 直传，否则走暂存缓冲拷贝。
pub fn add_plane(aligned: bool, bytes: u64) {
    if aligned {
        PLANES_ALIGNED.fetch_add(1, Ordering::Relaxed);
    } else {
        PLANES_STAGED.fetch_add(1, Ordering::Relaxed);
    }
    UPLOAD_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// 经暂存缓冲的填充布局字节数（行尾补齐后的总长）。
pub fn add_staged_bytes(bytes: u64) {
    STAGED_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// mip 生成 render pass 次数（每级一个 pass）。
pub fn add_mip_pass() {
    MIP_PASSES.fetch_add(1, Ordering::Relaxed);
}

/// 静态批段表上传（仅在批内容变化时发生）。
pub fn add_static_upload(bytes: u64) {
    STATIC_UPLOADS.fetch_add(1, Ordering::Relaxed);
    STATIC_UPLOAD_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// 静态层视口剔除结果（最近一次剔除）：可见笔画数 / 总笔画数。
pub fn set_static_visible(visible: usize, total: usize) {
    STATIC_VISIBLE.store(visible as u64, Ordering::Relaxed);
    STATIC_TOTAL.store(total as u64, Ordering::Relaxed);
}

/// 静态层驻留式上传的规模与本帧绘制批次：`(驻留段数, draw 区间数)`。
///
/// 驻留式下 GPU 缓冲按**整份文档**增长而非当前视野，所以驻留段数是显存
/// 占用的直接度量（× 64 B/段）；draw 区间数是本帧的批次开销。两者一起看
/// 才能判断"剔除 + 分批"这笔交易划不划算。
pub fn set_static_resident(resident_segs: u64, draw_ranges: u64) {
    STATIC_RESIDENT_SEGS.store(resident_segs, Ordering::Relaxed);
    STATIC_DRAW_RANGES.store(draw_ranges, Ordering::Relaxed);
}

/// 上报一帧因帧率节流被主动跳过（预期行为）。
pub fn add_throttled_frame() {
    THROTTLED_FRAMES.fetch_add(1, Ordering::Relaxed);
}

// ==================== 触控输入诊断 ====================
//
// 触控 / 笔由 Windows 提升为指针消息，winit 转成 `WindowEvent::Touch`，iced
// 再转成 `Event::Touch`（`conversion.rs::touch_event`）——**框架自带通路，
// 应用侧没有 hook、没有自定义队列、没有 drain**。曾经那组 hook/WM_POINTER/
// WM_TOUCH/入队/drain 计数随自定义管线一起删除：它们会恒为「未装 / 0」，
// 是纯粹的误导性噪声（诊断信息说谎比没有诊断更糟）。
//
// 只保留仍有意义的那个：经 `Event::Touch` 真正起笔的次数。

static TOUCH_STROKE_STARTED: AtomicU64 = AtomicU64::new(0);

/// 上报经 `Event::Touch` 真正起了一笔（笔或橡皮擦）。
pub fn add_touch_stroke_started() {
    TOUCH_STROKE_STARTED.fetch_add(1, Ordering::Relaxed);
}

/// 经 `Event::Touch` 起笔的累计次数。
pub fn touch_strokes_started() -> u64 {
    TOUCH_STROKE_STARTED.load(Ordering::Relaxed)
}

/// 上报一个渲染帧间隔（相邻两帧起点之差，由 `App::draw` 每帧调用）。
///
/// 连续带外样本达到该数量才重锚刷新周期。
///
/// 单个长帧必须仍按离群处理（否则长帧会把"刷新率"估高，卡顿反而看不出来），
/// 但**连续**同向带外说明基线本身错了——这时必须能重新收敛，否则估计值会被
/// 启动首帧永久钉死（见 [`note_frame_interval`]）。
const REANCHOR_AFTER: u64 = 5;

/// 单帧间隔的合理上界（纳秒）。超过它的样本不参与基线也不累计重锚：
/// 启动首帧（窗口创建 + GPU 初始化）可以长达数秒，把它当基线会让
/// [`measured_refresh_ms`] 永久返回几秒。
const PLAUSIBLE_FRAME_NS: u64 = 200_000_000;

/// 带外样本的连续计数与累计和（重锚用；原子竞争无碍——这��是启发式估计）。
static REJECT_STREAK: AtomicU64 = AtomicU64::new(0);
static REJECT_SUM_NS: AtomicU64 = AtomicU64::new(0);

/// 首次调用只用于建立刷新周期基线。之后用 EMA 维护 `REFRESH_PERIOD_NS`，
/// 并统计长帧与窗口内最长帧间隔。
///
/// **必须带重锚路径。** 接受带（0.8..1.2）是相对当前值的，一旦基线被一个
/// 超长样本钉死，后续正常样本永远落在带外，EMA 就永久冻结——实测启动首帧
/// 3.3s 把估计值钉在 0.3Hz，导致 `effective_frame_period_ms` 给笔尖外推
/// 用了 3.3 秒的预测时域，笔迹肉眼可见地滞后错位，且再也回不来。
pub fn note_frame_interval(delta: Duration) {
    let nanos = delta.as_nanos() as u64;
    // 最长帧：单调取大（原子 load/store 竞争无碍——这只是诊断上界）
    bump_max(&MAX_FRAME_NS, nanos);

    let plausible = nanos <= PLAUSIBLE_FRAME_NS;
    let previous = REFRESH_PERIOD_NS.load(Ordering::Relaxed);
    if previous == 0 {
        // 首样本：只有量级合理才作为基线。启动首帧往往长达数秒，拿它当基线
        // 就是上面那个永久锁死的起点。
        if plausible {
            REFRESH_PERIOD_NS.store(nanos, Ordering::Relaxed);
        }
        return;
    }

    let ratio = nanos as f64 / previous as f64;
    // 接受带 0.8..1.2 → 0.7..1.4：帧率在两种稳态间切换（书写期满速 vs
    // 空闲被摄像头帧率钉住）时，带外样本会被**反复丢弃再重锚**，估计值在相邻
    // 秒之间整体跳变（实测 12ms↔33ms），笔尖外推的预测时域跟着抖 → 笔尖摆动。
    // 放宽接受带让中等幅度的速率变化走平滑 EMA，而不是走"全量重锚"这条
    // 粗粒度通道。单个长帧（≈2 倍周期）仍在带外，仍按离群丢弃。
    if (0.7..=1.4).contains(&ratio) {
        REFRESH_PERIOD_NS.store((previous * 7 + nanos) / 8, Ordering::Relaxed);
        REJECT_STREAK.store(0, Ordering::Relaxed);
        REJECT_SUM_NS.store(0, Ordering::Relaxed);
    } else if plausible {
        // 量级合理但落在带外：单个仍按离群丢弃，连续 N 个则认定基线已错，
        // 重锚到这段样本的均值（与 `app::pacing::merge_period_sample` 同策略）。
        let sum = REJECT_SUM_NS.fetch_add(nanos, Ordering::Relaxed) + nanos;
        let streak = REJECT_STREAK.fetch_add(1, Ordering::Relaxed) + 1;
        if streak >= REANCHOR_AFTER {
            REFRESH_PERIOD_NS.store(sum / streak, Ordering::Relaxed);
            REJECT_STREAK.store(0, Ordering::Relaxed);
            REJECT_SUM_NS.store(0, Ordering::Relaxed);
        }
    }

    if ratio > LONG_FRAME_RATIO {
        LONG_FRAMES.fetch_add(1, Ordering::Relaxed);
    }
}

/// 上报一段"帧间隔 = 空闲等待"（两次 `draw` 之间没有人请求重绘）。
///
/// 调用方必须能区分这两种帧间隔：
///
/// - 有人请求、我们却慢了那么久 → **真卡顿**，本 crate 的锅
/// - 没人请求、我们安静地等输入 → **正常**，此前被误报成卡顿
///
/// `idle_threshold_ns` 应取"面板刷新周期的数倍"（而不是绝对值），否则
/// 60Hz 面板上正常的 33ms 交错也会被判成空闲。
pub fn note_idle_gap(gap: Duration, idle_threshold_ns: u64) {
    if gap.as_nanos() as u64 > idle_threshold_ns {
        IDLE_GAPS.fetch_add(1, Ordering::Relaxed);
        bump_max(&IDLE_GAP_MAX_NS, gap.as_nanos() as u64);
    }
}

/// 上报 `App::draw` 函数体自身的 CPU 耗时。
///
/// 与帧间隔严格分开：帧间隔里混着等待输入的时间，拿它判断"代码慢"会把
/// 空闲误读成停顿（见 [`note_idle_gap`]）。这里只测我们自己那条指令流。
pub fn add_draw_cost(elapsed: Duration) {
    let nanos = elapsed.as_nanos() as u64;
    DRAW_NANOS.fetch_add(nanos, Ordering::Relaxed);
    DRAW_CALLS.fetch_add(1, Ordering::Relaxed);
    bump_max(&DRAW_MAX_NS, nanos);
    let bucket = leading_zeros_bucket(nanos / 1_000).min(DRAW_P95_BUCKETS - 1);
    DRAW_HIST[bucket].fetch_add(1, Ordering::Relaxed);
}

/// 微秒数 → 对数桶下标（`floor(log2(us))`）。
fn leading_zeros_bucket(micros: u64) -> usize {
    (u64::BITS - micros.max(1).leading_zeros()) as usize
}

/// 桶下标 → 该桶上界的微秒数（供 p95 报告用）。
fn bucket_ceil_micros(bucket: usize) -> u64 {
    1u64 << bucket
}

/// 取出并清空直方图，返回 `draw` 耗时的 p95（纳秒）。
fn take_draw_p95() -> u64 {
    let counts: [u64; DRAW_P95_BUCKETS] =
        std::array::from_fn(|i| DRAW_HIST[i].swap(0, Ordering::Relaxed));
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 0;
    }
    let target = (total * 95).div_ceil(100);
    let mut seen = 0u64;
    for (i, count) in counts.iter().enumerate() {
        seen += count;
        if seen >= target {
            return bucket_ceil_micros(i) * 1_000;
        }
    }
    DRAW_MAX_NS.load(Ordering::Relaxed)
}

/// 实测刷新周期（EMA，毫秒）；未测得时为 0。
pub fn measured_refresh_ms() -> f64 {
    let ns = REFRESH_PERIOD_NS.load(Ordering::Relaxed);
    if ns == 0 {
        0.0
    } else {
        ns as f64 / 1_000_000.0
    }
}

/// `(窗口内长帧数, 窗口内最坏帧间隔毫秒)`——帧率稳定性的直接观测点。
///
/// **只读不清零**：HUD 每帧调用，若在此 `swap` 会把计数从 1 秒一次的
/// `[rstat]` 日志里偷走（日志将恒为 0）。清零只由 `snapshot` 负责。
/// 因此 HUD 读到的是"距上次日志清零以来"的值，即当前 1 秒窗口。
pub fn frame_stability() -> (u64, f64) {
    (
        LONG_FRAMES.load(Ordering::Relaxed),
        MAX_FRAME_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
    )
}

/// `(可见笔画数, 总笔画数)`——视口剔除效果的观测点（供 HUD/基准读取）。
pub fn static_visible() -> (u64, u64) {
    (
        STATIC_VISIBLE.load(Ordering::Relaxed),
        STATIC_TOTAL.load(Ordering::Relaxed),
    )
}

/// 帧预算观测：`(prepare 平均毫秒, prepare 峰值毫秒, 帧内几何编排平均毫秒,
/// 峰值毫秒, 实测刷新周期毫秒)`。
///
/// **只读不清零**（同 [`frame_stability`]：清零只由 `snapshot` 负责，否则
/// 每帧调用的 HUD 会把 1 秒窗口的计数偷走）。
///
/// 判读：前两项与第四项之和若逼近第五项（刷新周期），说明一帧的 CPU 工作
/// 已经吃掉 vsync 预算 → 必然出现 `LONG_FRAMES`（整帧多等一个周期）。
pub fn frame_budget() -> (f64, f64, f64, f64, f64) {
    let prep_calls = PREPARE_CALLS.load(Ordering::Relaxed).max(1);
    let work_calls = FRAME_WORK_CALLS.load(Ordering::Relaxed).max(1);
    let ms = |nanos: u64, calls: u64| nanos as f64 / calls as f64 / 1_000_000.0;
    (
        ms(PREPARE_NANOS.load(Ordering::Relaxed), prep_calls),
        PREPARE_MAX_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        ms(FRAME_WORK_NANOS.load(Ordering::Relaxed), work_calls),
        FRAME_WORK_MAX_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        measured_refresh_ms(),
    )
}

/// `(驻留段数, draw 区间数)`——驻留式上传的显存规模与本帧批次开销。
pub fn static_resident() -> (u64, u64) {
    (
        STATIC_RESIDENT_SEGS.load(Ordering::Relaxed),
        STATIC_DRAW_RANGES.load(Ordering::Relaxed),
    )
}

/// 动态层（头部 + 触控轨迹）段表上传。
pub fn add_dynamic_upload(bytes: u64) {
    DYN_UPLOADS.fetch_add(1, Ordering::Relaxed);
    DYN_UPLOAD_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// 橡皮擦悬停光标段表上传。
pub fn add_cursor_upload(bytes: u64) {
    CURSOR_UPLOADS.fetch_add(1, Ordering::Relaxed);
    CURSOR_UPLOAD_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// 批注层单帧 draw / bind group 次数。
pub fn add_annotation_draws(static_draws: u64, dynamic_draws: u64, cursor_draws: u64) {
    STATIC_DRAWS.fetch_add(static_draws, Ordering::Relaxed);
    DYNAMIC_DRAWS.fetch_add(dynamic_draws, Ordering::Relaxed);
    CURSOR_DRAWS.fetch_add(cursor_draws, Ordering::Relaxed);
    BINDS.fetch_add(static_draws + dynamic_draws + cursor_draws, Ordering::Relaxed);
}

/// 一次窗口内取出的全部渲染计数。
#[derive(Debug, Default, Clone, Copy)]
pub struct Snapshot {
    pub frames: u64,
    pub camera_messages: u64,
    pub view_calls: u64,
    pub view_nanos: u64,
    pub planes_aligned: u64,
    pub planes_staged: u64,
    pub upload_bytes: u64,
    pub staged_bytes: u64,
    pub mip_passes: u64,
    pub static_uploads: u64,
    pub static_upload_bytes: u64,
    pub dynamic_uploads: u64,
    pub dynamic_upload_bytes: u64,
    pub cursor_uploads: u64,
    pub cursor_upload_bytes: u64,
    pub static_draws: u64,
    pub dynamic_draws: u64,
    pub cursor_draws: u64,
    pub binds: u64,
    pub throttled_frames: u64,
    pub long_frames: u64,
    pub max_frame_ns: u64,
    pub prepare_nanos: u64,
    pub prepare_calls: u64,
    pub prepare_max_ns: u64,
    pub frame_work_nanos: u64,
    pub frame_work_calls: u64,
    pub frame_work_max_ns: u64,
    /// 帧间隔中属于"没人请求重绘、我们在等输入"的部分（见 [`note_idle_gap`]）。
    pub idle_gaps: u64,
    pub idle_gap_max_ns: u64,
    /// `App::draw` 函数体内的 CPU 耗时——这才是"我们自己的代码慢不慢"的答案，
    /// 与帧间隔（可能大部分是空闲等待）严格区分。
    pub draw_nanos: u64,
    pub draw_calls: u64,
    pub draw_max_ns: u64,
    pub draw_p95_ns: u64,
    pub input_latency_nanos: u64,
    pub input_latency_calls: u64,
    pub input_latency_max_ns: u64,
    pub touch_events: u64,
}

impl Snapshot {
    /// view 重建平均耗时（毫秒）；无调用时为 0。
    pub fn view_avg_ms(&self) -> f64 {
        if self.view_calls == 0 {
            0.0
        } else {
            self.view_nanos as f64 / self.view_calls as f64 / 1_000_000.0
        }
    }

    /// 批注层 prepare 平均耗时（毫秒）。
    pub fn prepare_avg_ms(&self) -> f64 {
        if self.prepare_calls == 0 {
            0.0
        } else {
            self.prepare_nanos as f64 / self.prepare_calls as f64 / 1_000_000.0
        }
    }

    /// 帧内几何编排（`App::draw` 属主部分）平均耗时（毫秒）。
    pub fn frame_work_avg_ms(&self) -> f64 {
        if self.frame_work_calls == 0 {
            0.0
        } else {
            self.frame_work_nanos as f64 / self.frame_work_calls as f64 / 1_000_000.0
        }
    }

    /// `App::draw` 函数体平均耗时（毫秒）。**只有这个量能回答"我们的代码
    /// 慢不慢"**——帧间隔里可能大部分是在等输入，见 [`Self::idle_gap_max_ms`]。
    pub fn draw_avg_ms(&self) -> f64 {
        if self.draw_calls == 0 {
            0.0
        } else {
            self.draw_nanos as f64 / self.draw_calls as f64 / 1_000_000.0
        }
    }

    /// 输入→帧起点延迟平均值（毫秒）。
    ///
    /// 帧间隔回答不了"跟不跟手"——它混着等待输入的空闲、GPU 阻塞、以及事件
    /// 到了却还没被画。这个量只覆盖最后一种：事件已到达，到下一次 `draw`
    /// 入口之间的间隔。**平均值**低说明事件一来就被画，高说明压在了队列里。
    pub fn input_latency_avg_ms(&self) -> f64 {
        if self.input_latency_calls == 0 {
            0.0
        } else {
            self.input_latency_nanos as f64 / self.input_latency_calls as f64 / 1_000_000.0
        }
    }

    /// 最长空闲间隙（毫秒）：`App::draw` 两次进入之间、**没有人请求重绘**的
    /// 那部分。与卡顿无关，但此前被 [`Self::long_frames`] 当成长帧报出来，
    /// 导致几轮排查把空闲误读成停顿。
    pub fn idle_gap_max_ms(&self) -> f64 {
        self.idle_gap_max_ns as f64 / 1_000_000.0
    }
}

impl std::fmt::Display for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 取一次即可：`frames_by_source()` 是**取出并清零**的（与 `take` 同语义），
        // 调四次只会让第一个拿到真值、其余恒为 0。
        let src = frames_by_source();
            let (extend, partial, full, tail_segs) = raster_pass_counts();
            write!(
                f,
                "frames={} cam_msg={} view={} view_avg={:.2}ms \
               | plane aligned={} staged={} up={}B staged_fill={}B \
               | mip_pass={} \
               | anno_up static={}({}B) dyn={}({}B) cursor={}({}B) \
               | anno_draw static={} dyn={} cursor={} binds={} \
               | refresh={:.2}ms long={} worst={:.2}ms idle_gap={} worst_idle={:.2}ms throttled={} \
               | prep={:.3}ms/{:.3}ms work={:.3}ms/{:.3}ms \
               | cpu avg={:.3}ms p95={:.3}ms worst={:.3}ms \
               | in2draw avg={:.2}ms worst={:.2}ms touch_ev={} \
               | src unknown/input/momentum/bg={}/{}/{}/{}{} \
               | 光栅 增量={} 局部={} 全量={} 光栅段={}",
            self.frames,
            self.camera_messages,
            self.view_calls,
            self.view_avg_ms(),
            self.planes_aligned,
            self.planes_staged,
            self.upload_bytes,
            self.staged_bytes,
            self.mip_passes,
            self.static_uploads,
            self.static_upload_bytes,
            self.dynamic_uploads,
            self.dynamic_upload_bytes,
            self.cursor_uploads,
            self.cursor_upload_bytes,
            self.static_draws,
            self.dynamic_draws,
            self.cursor_draws,
            self.binds,
            measured_refresh_ms(),
            self.long_frames,
            self.max_frame_ns as f64 / 1_000_000.0,
            self.idle_gaps,
            self.idle_gap_max_ms(),
            self.throttled_frames,
            self.prepare_avg_ms(),
            self.prepare_max_ns as f64 / 1_000_000.0,
            self.frame_work_avg_ms(),
            self.frame_work_max_ns as f64 / 1_000_000.0,
            self.draw_avg_ms(),
            self.draw_p95_ns as f64 / 1_000_000.0,
            self.draw_max_ns as f64 / 1_000_000.0,
            self.input_latency_avg_ms(),
            self.input_latency_max_ns as f64 / 1_000_000.0,
            self.touch_events,
            src[0],
            src[1],
            src[2],
            src[3],
            camera_cadence_summary(),
            extend,
            partial,
            full,
            tail_segs,
        )
    }
}

/// 采集节拍摘要：间隔的 min/中位/p95/max + 丢帧数。
///
/// 这是判断"画面忽快忽慢"来自哪一层的唯一依据：
///
/// - 采集间隔本身就抖 → 摄像头/采集线程问题（MF 突发投递、丢帧断拍）；
/// - 采集间隔稳但上屏间隔抖 → 事件循环/呈现路径问题。
fn camera_cadence_summary() -> String {
    let drops = take(&CAMERA_DROPS);
    let fmt = |samples: &mut Vec<Duration>| {
        if samples.is_empty() {
            return "?".to_string();
        }
        samples.sort_unstable();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let n = samples.len();
        format!(
            "min={:.1} 中位={:.1} p95={:.1} max={:.1}(n={n})",
            ms(samples[0]),
            ms(samples[n / 2]),
            ms(samples[(n * 95 / 100).min(n - 1)]),
            ms(samples[n - 1]),
        )
    };
    let gap = CAMERA_INTERVALS
        .lock()
        .map(|mut s| fmt(&mut s))
        .unwrap_or_else(|_| "?".into());
    let poll = CAMERA_POLL.lock().map(|mut s| fmt(&mut s)).unwrap_or_else(|_| "?".into());
    let swap = CAMERA_SWAPS
        .lock()
        .map(|mut s| fmt(&mut s))
        .unwrap_or_else(|_| "?".into());
    let src = CAMERA_SOURCE.lock().map(|mut s| fmt(&mut s)).unwrap_or_else(|_| "?".into());
    format!(" | 源端 {src} cam_gap {gap} poll {poll} 上屏 {swap} 丢弃={drops}")
}

/// 取出并清零全部计数。
pub fn snapshot() -> Snapshot {
    Snapshot {
        frames: take(&FRAMES),
        camera_messages: take(&CAMERA_MESSAGES),
        view_calls: take(&VIEW_CALLS),
        view_nanos: take(&VIEW_NANOS),
        planes_aligned: take(&PLANES_ALIGNED),
        planes_staged: take(&PLANES_STAGED),
        upload_bytes: take(&UPLOAD_BYTES),
        staged_bytes: take(&STAGED_BYTES),
        mip_passes: take(&MIP_PASSES),
        static_uploads: take(&STATIC_UPLOADS),
        static_upload_bytes: take(&STATIC_UPLOAD_BYTES),
        dynamic_uploads: take(&DYN_UPLOADS),
        dynamic_upload_bytes: take(&DYN_UPLOAD_BYTES),
        cursor_uploads: take(&CURSOR_UPLOADS),
        cursor_upload_bytes: take(&CURSOR_UPLOAD_BYTES),
        static_draws: take(&STATIC_DRAWS),
        dynamic_draws: take(&DYNAMIC_DRAWS),
        cursor_draws: take(&CURSOR_DRAWS),
        binds: take(&BINDS),
        throttled_frames: take(&THROTTLED_FRAMES),
        long_frames: take(&LONG_FRAMES),
        max_frame_ns: take(&MAX_FRAME_NS),
        prepare_nanos: take(&PREPARE_NANOS),
        prepare_calls: take(&PREPARE_CALLS),
        prepare_max_ns: take(&PREPARE_MAX_NS),
        frame_work_nanos: take(&FRAME_WORK_NANOS),
        frame_work_calls: take(&FRAME_WORK_CALLS),
        frame_work_max_ns: take(&FRAME_WORK_MAX_NS),
        idle_gaps: take(&IDLE_GAPS),
        idle_gap_max_ns: take(&IDLE_GAP_MAX_NS),
        draw_nanos: take(&DRAW_NANOS),
        draw_calls: take(&DRAW_CALLS),
        draw_max_ns: take(&DRAW_MAX_NS),
        draw_p95_ns: take_draw_p95(),
        input_latency_nanos: take(&INPUT_LATENCY_NANOS),
        input_latency_calls: take(&INPUT_LATENCY_CALLS),
        input_latency_max_ns: take(&INPUT_LATENCY_MAX_NS),
        touch_events: take(&TOUCH_EVENTS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进程级计数器的测试串行锁。
    ///
    /// 本模块的计数器都是静态原子量，而 `cargo test` 默认并行跑测试：两个
    /// 测试各自 `add_*` 后 `snapshot()`（`swap(0)` 语义）会互相取走对方的
    /// 窗口，表现为随机的 `assert_eq!(s.throttled_frames, 2)` 之类假失败。
    /// 与其接受这种偶发红，不如显式串行——这些测试测的就是"窗口取走"
    /// 语义本身，并行没有意义。
    static COUNTER_LOCK: Mutex<()> = Mutex::new(());

    /// 取得串行锁（中毒时照常放行：锁中毒只是别的测试 panic 的后果）。
    fn lock_counters() -> std::sync::MutexGuard<'static, ()> {
        COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn snapshot_take_clears_counters() {
        let _guard = lock_counters();
        add_static_upload(64);
        add_dynamic_upload(32);
        add_cursor_upload(16);
        add_annotation_draws(1, 2, 3);
        add_throttled_frame();
        add_throttled_frame();

        let s = snapshot();
        assert_eq!(s.static_uploads, 1);
        assert_eq!(s.static_upload_bytes, 64);
        assert_eq!(s.dynamic_uploads, 1);
        assert_eq!(s.cursor_uploads, 1);
        assert_eq!(s.binds, 6, "binds 应累计三次 draw 之和");
        assert_eq!(s.throttled_frames, 2);

        // 再次取应全部清零（窗口语义）
        let s2 = snapshot();
        assert_eq!(s2.static_uploads, 0);
        assert_eq!(s2.dynamic_uploads, 0);
        assert_eq!(s2.cursor_uploads, 0);
        assert_eq!(s2.binds, 0);
        assert_eq!(s2.throttled_frames, 0);
    }

    #[test]
    fn static_visible_roundtrip() {
        set_static_visible(7, 42);
        assert_eq!(static_visible(), (7, 42));
    }

    /// 帧预算的聚合口径：窗口平均 + 单帧峰值，无样本不得除零。
    ///
    /// 这两个数是"笔迹是否跟手"的判据（逼近刷新周期 = 必然错过 vsync），
    /// 因此峰值必须独立于平均被单独保留：平均被单帧尖峰稀释后看不出风险。
    ///
    /// 用 `Snapshot` 字面量而非全局计数器断言：这些计数器是进程级静态，
    /// 而 `cargo test` 并行跑测试，别的测试调 `snapshot()` 会顺手取走本测试
    /// 的窗口。全局聚合路径由 `snapshot_take_clears_counters` 覆盖。
    #[test]
    fn frame_budget_math_is_average_over_peak() {
        let s = Snapshot {
            prepare_nanos: 600_000,
            prepare_calls: 2,
            prepare_max_ns: 400_000,
            frame_work_nanos: 100_000,
            frame_work_calls: 1,
            frame_work_max_ns: 100_000,
            ..Default::default()
        };
        assert!(
            (s.prepare_avg_ms() - 0.3).abs() < 1e-9,
            "prepare 平均应为 0.3ms，实得 {}",
            s.prepare_avg_ms()
        );
        assert!(
            (s.frame_work_avg_ms() - 0.1).abs() < 1e-9,
            "帧内几何编排平均应为 0.1ms，实得 {}",
            s.frame_work_avg_ms()
        );

        // 峰值只反映最慢的那一帧（不被平均稀释）
        assert!(s.prepare_max_ns > s.prepare_nanos / s.prepare_calls);

        // 空窗口：不得除零 / NaN
        let empty = Snapshot::default();
        assert_eq!(empty.prepare_avg_ms(), 0.0);
        assert_eq!(empty.frame_work_avg_ms(), 0.0);
        // 无样本时 frame_budget 也不得 panic（除数下限为 1）
        let _ = frame_budget();
    }

    /// 上报路径的累加与峰值由 `note_frame_interval` 复用同一 `bump_max`
    /// 辅助函数覆盖（长帧峰值测试）；此处只锁聚合口径，避免与其它测试
    /// 争抢进程级窗口计数器（`cargo test` 并行，`snapshot()` 会取走它们）。

    #[test]
    fn static_resident_roundtrip() {
        set_static_resident(123_456, 9);
        assert_eq!(static_resident(), (123_456, 9));
    }

    #[test]
    fn display_omits_removed_fields() {
        let _guard = lock_counters();
        // 移除瓦片/光栅/层缓存/自有 submit 计数后，摘要不应再输出这些键
        let text = format!("{}", snapshot());
        for gone in ["ink_tiles", "raster=", "iced_lc", "own_submit", "dropped="] {
            assert!(!text.contains(gone), "摘要不应包含 {gone}: {text}");
        }
        for kept in ["refresh=", "long=", "worst=", "throttled="] {
            assert!(text.contains(kept), "摘要应包含 {kept}: {text}");
        }
    }

    #[test]
    fn end_frame_throttles_without_mutex() {
        let _guard = lock_counters();
        // 首次调用只建基线，不产出报表（故不清零计数）
        REPORT_AT_MS.store(REPORT_UNINIT, Ordering::Relaxed);
        add_throttled_frame();
        end_frame();
        assert_ne!(
            REPORT_AT_MS.load(Ordering::Relaxed),
            REPORT_UNINIT,
            "首次 end_frame 应惰性初始化基线"
        );
        assert_eq!(
            THROTTLED_FRAMES.load(Ordering::Relaxed),
            1,
            "未到 1 秒窗口时不应清零计数"
        );

        // 连续调用（仍在同一窗口内）同样不清零
        end_frame();
        assert_eq!(THROTTLED_FRAMES.load(Ordering::Relaxed), 1);

        // 窗口判定用合成时间戳（不依赖进程启动时长）
        assert_eq!(
            report_window(REPORT_UNINIT, 0),
            (false, true),
            "未初始化：建基线但不输出"
        );
        assert_eq!(
            report_window(1_000, 1_500),
            (false, false),
            "窗口内：不输出也不动基线"
        );
        assert_eq!(
            report_window(1_000, 2_000),
            (true, true),
            "满 1 秒：输出并推进基线"
        );
        assert_eq!(
            report_window(1_000, 1_000 + REPORT_WINDOW_MS),
            (true, true),
            "边界：恰好等于窗口长度即输出"
        );
        assert_eq!(
            report_window(1_000, 1_999),
            (false, false),
            "边界：差 1ms 仍不输出"
        );

        // 到达窗口 → 计数被 snapshot 取走并清零（直调窗口内部分）
        emit_window(1_500);
        assert_eq!(THROTTLED_FRAMES.load(Ordering::Relaxed), 0);
        // 失真窗口同样清零（只是不打印）
        add_throttled_frame();
        emit_window(9_000);
        assert_eq!(
            THROTTLED_FRAMES.load(Ordering::Relaxed),
            0,
            "失真窗口也要清零，否则下次窗口的计数会失真"
        );
    }

    /// 首个帧间隔建立刷新周期基线；60Hz 序列下 EMA 稳定在 ~16.67ms 且无长帧。
    ///
    /// 这三项此前被写在 `emit_window` 函数体**内部**（Rust 允许函数体内
    /// 定义 item），编译通过但 `#[test]` 被忽略 → 从未运行；clippy 的
    /// `unnameable_test_items` 警告是唯一线索。
    #[test]
    fn frame_interval_seeds_refresh_and_stays_clean() {
        let _guard = lock_counters();
        REFRESH_PERIOD_NS.store(0, Ordering::Relaxed);
        LONG_FRAMES.store(0, Ordering::Relaxed);
        MAX_FRAME_NS.store(0, Ordering::Relaxed);

        let period = Duration::from_micros(16_670);
        note_frame_interval(period);
        assert!(
            measured_refresh_ms() > 16.0 && measured_refresh_ms() < 17.0,
            "首个样本应建立基线，实得 {}ms",
            measured_refresh_ms()
        );
        for _ in 0..60 {
            note_frame_interval(period);
        }
        assert_eq!(
            LONG_FRAMES.load(Ordering::Relaxed),
            0,
            "稳定 60fps 序列不得产生长帧"
        );
        let s = snapshot();
        assert_eq!(s.long_frames, 0);
        assert!(
            (s.max_frame_ns as f64 / 1e6 - 16.67).abs() < 0.1,
            "worst 应约等于一个周期，实得 {}ms",
            s.max_frame_ns as f64 / 1e6
        );
    }

    /// 超过 1.5 倍周期的间隔必须记为长帧，且**不得**污染刷新率 EMA
    /// （否则长帧会把刷新率估高，反而把卡顿掩盖掉）。
    #[test]
    fn long_frame_is_counted_without_corrupting_refresh_estimate() {
        let _guard = lock_counters();
        REFRESH_PERIOD_NS.store(0, Ordering::Relaxed);
        LONG_FRAMES.store(0, Ordering::Relaxed);
        MAX_FRAME_NS.store(0, Ordering::Relaxed);

        let period = Duration::from_micros(16_670);
        note_frame_interval(period);
        for _ in 0..20 {
            note_frame_interval(period);
        }
        let before = measured_refresh_ms();

        note_frame_interval(Duration::from_millis(50));
        assert_eq!(
            LONG_FRAMES.load(Ordering::Relaxed),
            1,
            "50ms 间隔应记为长帧"
        );
        let after = measured_refresh_ms();
        assert!(
            (after - before).abs() < 0.5,
            "长帧不得把刷新率估计从 {before}ms 拉到 {after}ms"
        );
        let s = snapshot();
        assert!(s.max_frame_ns as f64 / 1e6 > 49.0, "worst 应记录 50ms 尖峰");
    }

    /// 主动节流是预期行为，必须与"长帧"这个稳定性指标分开统计。
    #[test]
    fn throttled_frames_are_counted_separately() {
        let _guard = lock_counters();
        THROTTLED_FRAMES.store(0, Ordering::Relaxed);
        LONG_FRAMES.store(0, Ordering::Relaxed);
        add_throttled_frame();
        let s = snapshot();
        assert_eq!(s.throttled_frames, 1);
        assert_eq!(s.long_frames, 0, "主动节流不是长帧，不应混进稳定性指标");
    }

    /// 刷新周期估计**必须能从被污染的基线恢复**。
    ///
    /// 回归用例：启动首帧 3.3s（窗口创建 + GPU 初始化）曾把基线钉死，
    /// 之后 16.7ms 的正常帧 `ratio = 0.005` 永远落在 0.8..1.2 接受带外，
    /// EMA 永久冻结在 3.3 秒 —— `effective_frame_period_ms` 于是给笔尖外推
    /// 用了 3.3 秒的预测时域，笔迹肉眼可见地滞后错位且无法自愈。
    #[test]
    fn poisoned_baseline_recovers_to_steady_state() {
        let _guard = lock_counters();
        REFRESH_PERIOD_NS.store(0, Ordering::Relaxed);
        REJECT_STREAK.store(0, Ordering::Relaxed);
        REJECT_SUM_NS.store(0, Ordering::Relaxed);

        // 启动首帧：超长，必须**不**被当作基线
        note_frame_interval(Duration::from_millis(3300));
        assert_eq!(
            measured_refresh_ms(),
            0.0,
            "超长首帧不得建立基线，否则会永久钉死刷新率估计"
        );

        // 稳态 60Hz 序列
        let period = Duration::from_micros(16_670);
        for _ in 0..40 {
            note_frame_interval(period);
        }
        let measured = measured_refresh_ms();
        assert!(
            (16.0..17.5).contains(&measured),
            "稳态后应收敛到 ~16.7ms，实得 {measured}ms"
        );

        // 旧实现下这里会停在 3300ms；显式钉一个被污染的基线再验证可恢复
        REFRESH_PERIOD_NS.store(3_300_000_000, Ordering::Relaxed);
        REJECT_STREAK.store(0, Ordering::Relaxed);
        REJECT_SUM_NS.store(0, Ordering::Relaxed);
        for _ in 0..40 {
            note_frame_interval(period);
        }
        let recovered = measured_refresh_ms();
        assert!(
            (16.0..17.5).contains(&recovered),
            "被污染的基线（3.3s）必须能重锚回真实周期，实得 {recovered}ms"
        );
    }

    /// `now_ms()` 必须是**单调递增**的时钟。
    ///
    /// 回归用例：曾写成 `Instant::now().elapsed()`，而 `elapsed()` 是"从另一个
    /// instant 到 self 的时长"，对刚创建的 instant 调用恒为 ≈0 —— 1 秒报表
    /// 因此永不输出，且没有任何外部症状。
    #[test]
    fn now_ms_is_a_monotonic_clock() {
        let a = now_ms();
        std::thread::sleep(Duration::from_millis(30));
        let b = now_ms();
        assert!(b > a, "now_ms() 必须随时间推进（{a} -> {b}）");
        assert!(
            b - a >= 20,
            "1.1s 级别的推进量级应对得上真实耗时（{a} -> {b}）"
        );
    }

    /// 真实时序下 `end_frame` 必须**真的**输出一条 `[rstat]`。
    ///
    /// `end_frame_throttles_without_mutex` 只验了 `report_window` 这个纯函数；
    /// 这里验 `end_frame` 内部的 load/store 接线。诊断通道坏掉时没有任何外部
    /// 症状（只是日志少一行），所以必须有这条兜底。
    #[test]
    fn end_frame_actually_emits_after_a_second() {
        let _guard = lock_counters();
        REPORT_AT_MS.store(REPORT_UNINIT, Ordering::Relaxed);
        REPORT_ATTEMPTS.store(0, Ordering::Relaxed);
        REPORT_EMITTED.store(0, Ordering::Relaxed);
        REPORT_DROPPED_STALE.store(0, Ordering::Relaxed);

        end_frame(); // 惰性建基线，本帧不输出
        assert_eq!(
            REPORT_EMITTED.load(Ordering::Relaxed),
            0,
            "首帧只应建基线"
        );

        std::thread::sleep(Duration::from_millis(1_100));
        end_frame();
        assert_eq!(REPORT_ATTEMPTS.load(Ordering::Relaxed), 1, "满 1 秒应尝试输出");
        assert_eq!(
            REPORT_EMITTED.load(Ordering::Relaxed),
            1,
            "满 1 秒必须真的输出一条 [rstat] 报表"
        );
    }

    /// 单个长帧仍然按离群处理（不得把刷新率估高，否则长帧会被掩盖）。
    /// 空闲间隙不得被当成停顿。
    ///
    /// `note_frame_interval` 测的是两次 `draw` 入口之差，其中**包含**"没人
    /// 请求重绘、我们等输入"的空档。这段空档此前被计入 `LONG_FRAMES` /
    /// `MAX_FRAME_NS`，于是空闲被读成停顿——实测报出 `long=9 worst=53ms`，
    /// 实际是 9 段没有输入的安静期，几轮排查因此优化了并不存在的问题。
    ///
    /// 两条必须同时成立：
    /// 1. 间隙超阈值时 `note_idle_gap` 要把它单独记成空闲；
    /// 2. `add_draw_cost` 报的**函数体内耗时**仍应很小——这才是"我们慢不慢"。
    #[test]
    fn idle_gaps_are_not_reported_as_stalls() {
        let _guard = lock_counters();
        IDLE_GAPS.store(0, Ordering::Relaxed);
        IDLE_GAP_MAX_NS.store(0, Ordering::Relaxed);
        DRAW_NANOS.store(0, Ordering::Relaxed);
        DRAW_CALLS.store(0, Ordering::Relaxed);
        DRAW_MAX_NS.store(0, Ordering::Relaxed);
        for slot in DRAW_HIST.iter() {
            slot.store(0, Ordering::Relaxed);
        }

        let panel = 16_666_667u64;
        let idle_threshold = panel * 4; // 与 App::draw 里一致的阈值

        // 9 段安静期：200ms 内没有输入，draw 两次进入相隔 200ms
        for _ in 0..9 {
            let gap = Duration::from_millis(200);
            note_frame_interval(gap);
            note_idle_gap(gap, idle_threshold);
            // 我们自己只干了 0.2ms——这才是真实的 CPU 成本
            add_draw_cost(Duration::from_micros(200));
        }

        assert_eq!(
            IDLE_GAPS.load(Ordering::Relaxed),
            9,
            "9 段 200ms 空档必须全部被识别为空闲"
        );
        assert!((MAX_FRAME_NS.load(Ordering::Relaxed) / 1_000_000) as i64 == 200);

        let s = snapshot();
        let (avg_ms, p95_ms, max_ms) = (
            s.draw_avg_ms(),
            s.draw_p95_ns as f64 / 1e6,
            s.draw_max_ns as f64 / 1e6,
        );
        let (idle, worst_idle_ms) = (s.idle_gaps, s.idle_gap_max_ms());
        assert_eq!(idle, 9);
        assert!(
            worst_idle_ms > 190.0,
            "最长空闲应被单独报出（实测 {worst_idle_ms}ms）"
        );
        // 关键断言：帧间隔 200ms，但我们自己的 CPU 只有 0.2ms/帧——差 1000 倍。
        // 若把帧间隔当成"我们的耗时"，就会得出"每帧 200ms"的荒谬结论并
        // 去优化一个不存在的问题。
        //
        // `avg` / `max` 是精确值；`p95` 出自对数直方图，按桶上界量化
        // （0.2ms 落在 128µs..256µs 桶，报 0.128ms），故 p95 单独放宽。
        let gap_ms = 200.0;
        assert!(
            avg_ms < 1.0 && max_ms < 1.0,
            "draw 函数体耗时应远小于帧间隔（avg={avg_ms} max={max_ms}ms vs {gap_ms}ms）"
        );
        assert!(
            p95_ms < 1.0,
            "p95 应远小于帧间隔（实测 {p95_ms}ms；直方图按桶上界量化）"
        );
        assert!(
            avg_ms * 100.0 < gap_ms,
            "CPU 耗时至少应比帧间隔小两个数量级（{avg_ms} vs {gap_ms}ms），\
             否则说明这两个量被混为一谈了"
        );
    }

    /// 反向：真的慢（draw 函数体本身耗时 30ms）不能被当成空闲。
    #[test]
    fn genuine_draw_slowness_is_not_classified_as_idle() {
        let _guard = lock_counters();
        IDLE_GAPS.store(0, Ordering::Relaxed);
        DRAW_CALLS.store(0, Ordering::Relaxed);
        DRAW_NANOS.store(0, Ordering::Relaxed);
        DRAW_MAX_NS.store(0, Ordering::Relaxed);
        for slot in DRAW_HIST.iter() {
            slot.store(0, Ordering::Relaxed);
        }

        let idle_threshold = 16_666_667u64 * 4;
        for _ in 0..5 {
            // 帧间隔 20ms（连续有人请求），但 draw 自身烧了 15ms
            note_idle_gap(Duration::from_millis(20), idle_threshold);
            add_draw_cost(Duration::from_millis(15));
        }

        assert_eq!(
            IDLE_GAPS.load(Ordering::Relaxed),
            0,
            "连续有请求的 20ms 帧间隔不是空闲"
        );
        let avg_ms = snapshot().draw_avg_ms();
        assert!(avg_ms > 14.0, "真实 CPU 耗时必须被如实报出（{avg_ms}ms）");
    }

    #[test]
    fn single_long_frame_does_not_reanchor() {
        let _guard = lock_counters();
        REFRESH_PERIOD_NS.store(0, Ordering::Relaxed);
        REJECT_STREAK.store(0, Ordering::Relaxed);
        REJECT_SUM_NS.store(0, Ordering::Relaxed);

        let period = Duration::from_micros(16_670);
        for _ in 0..30 {
            note_frame_interval(period);
        }
        let before = measured_refresh_ms();

        // 一个 33ms 长帧（2 个周期）
        note_frame_interval(Duration::from_millis(33));
        assert!(
            (before - measured_refresh_ms()).abs() < 0.5,
            "单个长帧不得改变刷新率估计（{} → {}）",
            before,
            measured_refresh_ms()
        );
        assert_eq!(LONG_FRAMES.load(Ordering::Relaxed), 1);
    }
}

/// 报表窗口长度（毫秒）与失真上限：超过 [`STALE_WINDOW_MS`] 的窗口是
/// "长时间空闲后恢复"造成的，速率无意义，丢弃但仍清零计数。
const REPORT_WINDOW_MS: u64 = 1_000;
const STALE_WINDOW_MS: u64 = 3_000;

/// 报表窗口判定：`(是否输出摘要, 是否重置基线)`。纯函数，可合成时间戳测试。
///
/// 拆出来的原因：`now_ms()` 是**进程启动以来**的毫秒数，进程刚起来时它
/// 本身可能只有几十——把基线"推到 1500ms 之前"这种测试手法在进程启动
/// 不足 1.5 秒时根本无法表达（`saturating_sub` 会夹到 0，窗口永不到达）。
#[inline]
fn report_window(last: u64, now: u64) -> (bool, bool) {
    if last == REPORT_UNINIT {
        return (false, true); // 惰性建基线，本帧不输出
    }
    let elapsed = now.saturating_sub(last);
    if elapsed < REPORT_WINDOW_MS {
        return (false, false);
    }
    (true, true)
}

/// 报表通道自身的健康计数：`(尝试输出次数, 真正输出次数, 因窗口失真被丢弃)`。
///
/// 诊断通道自己也会坏，而它坏掉时**没有任何外部症状**——只是"日志里少了
/// 某一行"。这几个数让这种静默失败可见：尝试远多于输出即说明
/// [`emit_window`] 在提前 return。
static REPORT_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static REPORT_EMITTED: AtomicU64 = AtomicU64::new(0);
static REPORT_DROPPED_STALE: AtomicU64 = AtomicU64::new(0);
/// 帧与预算指标的**非破坏性**读取。
///
/// 刻意与 [`snapshot`] 分开：`snapshot()` 是"取走并清零"的窗口语义，而
/// 诊断 HUD / `[rate]` 这类旁路观察者若复用它，就会把 [`end_frame`] 的窗口
/// 偷走（表现为报表里各项恒为 0）。旁路观察一律走这里。
#[allow(clippy::type_complexity)]
/// 呈现帧的来源分类（诊断用：定位"为什么还在超面板帧率"）。
///
/// 面板对齐闸门只装在输入路径（`App::paced_redraw`），而重绘请求还有别的来源：
/// 摄像头消息（`view()` 变化由 iced 自行安排重绘）、窗口事件、以及 iced/winit
/// 内部。闸门外的那些无从拦阻，所以必须能**区分**"这一帧是我们请求的"与
/// "这一帧是别处来的"，否则只能猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedrawSource {
    /// 无标记——不是本 crate 任何请求点发起的。
    Unknown,
    /// 输入路径（经 `App::paced_redraw` 的面板闸门）。
    Input,
    /// 惯性动画 `step_momentum`。
    Momentum,
    /// 后台笔画几何计算完成。
    StrokeComputed,
}

static REDRAW_SOURCE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static FRAMES_BY_SOURCE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// 标记"下一次重绘的来源"。请求点调用，`App::draw` 消费。
#[inline]
pub fn set_redraw_source(source: RedrawSource) {
    REDRAW_SOURCE.store(source as u8, Ordering::Relaxed);
}

/// 消费并记账当前帧的来源（`App::draw` 每帧一次）。
pub fn note_frame_source() {
    let raw = REDRAW_SOURCE.swap(0, Ordering::Relaxed);
    let idx = match raw {
        1 => 1,
        2 => 2,
        3 => 3,
        _ => 0,
    };
    FRAMES_BY_SOURCE[idx].fetch_add(1, Ordering::Relaxed);
}

/// 本窗口内各来源的帧数，顺序为 `[未知, 输入, 惯性, 后台几何]`。
pub fn frames_by_source() -> [u64; 4] {
    std::array::from_fn(|i| take(&FRAMES_BY_SOURCE[i]))
}

#[allow(clippy::type_complexity)]
pub fn observe() -> (u64, u64, f64, f64, u64, f64, f64) {
    let view_calls = VIEW_CALLS.load(Ordering::Relaxed);
    let prep_calls = PREPARE_CALLS.load(Ordering::Relaxed).max(1);
    let work_calls = FRAME_WORK_CALLS.load(Ordering::Relaxed).max(1);
    let ms = |nanos: u64, calls: u64| nanos as f64 / calls as f64 / 1_000_000.0;
    (
        FRAMES.load(Ordering::Relaxed),
        LONG_FRAMES.load(Ordering::Relaxed),
        MAX_FRAME_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        measured_refresh_ms(),
        view_calls,
        ms(FRAME_WORK_NANOS.load(Ordering::Relaxed), work_calls),
        ms(PREPARE_NANOS.load(Ordering::Relaxed), prep_calls),
    )
}
/// 每渲染帧调用一次；满 1 秒输出窗口摘要并清零。
pub fn end_frame() {
    FRAMES.fetch_add(1, Ordering::Relaxed);

    // 节流：距上次输出满 1 秒才继续（无 Mutex、无锁竞争）。
    let now = now_ms();
    let last = REPORT_AT_MS.load(Ordering::Relaxed);
    let (emit, rebaseline) = report_window(last, now);
    if rebaseline {
        REPORT_AT_MS.store(now, Ordering::Relaxed);
    }
    if !emit {
        return;
    }
    REPORT_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    emit_window(now.saturating_sub(last));
}

/// 输出并清零一个完整窗口（`end_frame` 的窗口内部分，拆出以便直接测试）。
fn emit_window(elapsed_ms: u64) {
    // 取窗口**必须**在失真判断之前：失真窗口也要清零，否则下一次窗口的计数
    // 会被上一次残留污染，速率全部失真（`end_frame_throttles_without_mutex`
    // 锁定此契约）。
    let window = snapshot();

    // 长时间空闲后恢复渲染时窗口失真，直接丢弃该窗口
    if elapsed_ms > STALE_WINDOW_MS {
        REPORT_DROPPED_STALE.fetch_add(1, Ordering::Relaxed);
        return;
    }
    REPORT_EMITTED.fetch_add(1, Ordering::Relaxed);

    let secs = elapsed_ms as f64 / 1000.0;
    let mb_per_s = |bytes: u64| bytes as f64 / secs / 1_048_576.0;
    let refresh_ms = measured_refresh_ms();

    log::info!(
        "[rstat] {secs:.1}s {window} \
         | fps={fps:.1}/s {fps_judge} cam={cam_fps:.1}/s view={view_fps:.1}/s \
         up={upload_mb:.1}MB/s staged_fill={staged_mb:.1}MB/s \
         static_kb={static_kb:.0} dyn_kb={dyn_kb:.0} cursor_kb={cursor_kb:.0} \
         {touch_chain}",
        fps = window.frames as f64 / secs,
        // 稳定性判读：60Hz 面板上 fps 应 ≈60 且 long=0
        fps_judge = if refresh_ms > 0.0 {
            format!(
                "(vsync {:.0}Hz long={} worst={:.1}ms prep={:.2}ms work={:.2}ms)",
                1000.0 / refresh_ms,
                window.long_frames,
                window.max_frame_ns as f64 / 1_000_000.0,
                window.prepare_avg_ms(),
                window.frame_work_avg_ms(),
            )
        } else {
            "(vsync ?)".to_string()
        },
        cam_fps = window.camera_messages as f64 / secs,
        view_fps = window.view_calls as f64 / secs,
        upload_mb = mb_per_s(window.upload_bytes),
        staged_mb = mb_per_s(window.staged_bytes),
        static_kb = window.static_upload_bytes as f64 / 1024.0,
        dyn_kb = window.dynamic_upload_bytes as f64 / 1024.0,
        cursor_kb = window.cursor_upload_bytes as f64 / 1024.0,
        // 输入通路：触控 / 笔由 Windows 提升后走 iced 原生鼠标事件（框架自带
        // `Event::Touch`，见 winit 的 WM_POINTER* 分支），因此**不再有**自定义
        // hook / WM_POINTER / WM_TOUCH 计数可言——原那组计数会恒为
        // 「未装 / 0」，是纯粹的误导性噪声，故已移除。
        touch_chain = {
            let (frames, _long, _worst, refresh_ms, view_calls, work, prep) = observe();
            format!(
                "| observe frames={frames} refresh={refresh_ms:.1}ms view_calls={view_calls} \
                 work={work:.3}ms prep={prep:.3}ms"
            )
        },
    );
}



