//! 每帧渲染计量：帧间隔、空档分类、输入→帧延迟、性能 HUD 采样。
//!
//! # 为什么与「应用状态」分开
//!
//! 这些量此前是 `App` 的 11 个字段。它们的问题不是占内存，而是**没法单独验证**：
//! 「长帧」「帧预算占比」「刷新率估计对不对」这些问题只能靠真机跑起来看 HUD 上
//! 的数字，而 HUD 数字本身又来自同一批字段——自证。移到这里之后，计量的输入
//! （两个 `Instant`）与输出（`viewink::stats`）都在一个模块里，可以直接断言。
//!
//! # 全部方法仅 `&self`
//!
//! `canvas::Program::draw` 只有 `&self`，所以帧起点、输入时刻、HUD 采样率状态
//! 全部是 `Cell`。

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::pacing::{panel_period, STREAM_CONTINUITY_GAP};

/// 帧起点与性能 HUD 的跨帧状态。
pub struct FrameMetrics {
    /// 最近一次实际渲染帧起点（批注热路径节流依据 + 惯性 dt 时基，draw 每帧更新）
    last_frame_start: Cell<Instant>,
    /// 最近一次输入事件的时刻。用于量"输入→帧起点"延迟——帧间隔无法区分"事件到了
    /// 但没画"与"压根没事件"，这个量能。
    last_input_at: Cell<Instant>,
    /// 上上一个输入事件的时刻。用来判断输入是否**连续**到达：手指停顿时，第一个
    /// 事件到下一个事件之间会有很长一段空档，把它算进"输入→帧起点"延迟会得到一个
    /// 由停顿长度决定的假数字（实测 90~143ms，而连续书写时只有一帧 11ms）。只在上
    /// 一个事件也很近时才采样。
    last_input_prev_at: Cell<Instant>,
    /// 性能 HUD：0.5s 窗口内累计的帧数。
    perf_frames: Cell<u32>,
    /// 性能 HUD：实测渲染帧率。
    perf_fps: Cell<f64>,
    /// 性能 HUD：帧耗时（与 FPS 同窗口降频）。
    perf_frame_ms: Cell<f64>,
    /// 性能 HUD：上一次 FPS 窗口的起点。
    perf_last_fps_time: Cell<Instant>,
    /// 性能 HUD：上一次帧时间采样的时刻。
    perf_last_frame_start: Cell<Instant>,
    /// 性能 HUD：进程内存占用。
    perf_memory_mb: Cell<f64>,
    /// 性能 HUD：上一次内存采样的时刻（初始值往前推 2s，使首帧即可出数）。
    perf_last_mem_time: Cell<Instant>,
}

/// 性能 HUD 的一帧采样结果（供 `view()` 读；`Cell` 内部字段不直接暴露）。
#[derive(Debug, Clone, Copy, Default)]
pub struct PerfSnapshot {
    /// 实测渲染帧率。
    pub fps: f64,
    /// 帧耗时（毫秒）。
    pub frame_ms: f64,
    /// 进程内存占用（MB）。
    pub memory_mb: f64,
}

impl FrameMetrics {
    /// 初始状态。
    ///
    /// `perf_last_mem_time` 往前推 2s：否则启用 HUD 后要空等一秒才出第一个读数。
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            last_frame_start: Cell::new(now),
            last_input_at: Cell::new(now),
            last_input_prev_at: Cell::new(now),
            perf_frames: Cell::new(0),
            perf_fps: Cell::new(0.0),
            perf_frame_ms: Cell::new(0.0),
            perf_last_fps_time: Cell::new(now),
            perf_last_frame_start: Cell::new(now),
            perf_memory_mb: Cell::new(0.0),
            perf_last_mem_time: Cell::new(now - Duration::from_secs(2)),
        }
    }

    /// 最近一次渲染帧起点。
    ///
    /// 节流唤醒偏移量以它为基准（见 `crate::pacing::throttle_wake_offset_us`），
    /// 惯性积分的 dt 时基也是它。
    pub fn last_frame_start(&self) -> Instant {
        self.last_frame_start.get()
    }

    /// 记录一次输入事件，并把"上一个输入事件"推到 `last_input_prev_at`。
    ///
    /// 两个时刻都要留：`last_input_at` 量"事件→下一帧"，`last_input_prev_at` 量
    /// "事件与事件之间的间隔"，后者用来判断这串输入是否**连续**（见
    /// [`FrameMetrics::begin_frame`]）。
    pub fn mark_input(&self) {
        let now = Instant::now();
        self.last_input_prev_at.set(self.last_input_at.replace(now));
    }

    /// 帧入口：取本帧起点，并采样输入→帧起点延迟。
    ///
    /// 返回的 `Instant` 是本函数**第一行**的时刻，后续两处计量都以此为基准——
    /// ① 相邻两帧起点之差 = 实测刷新周期；② 本帧属主 CPU 工作耗时。
    ///
    /// 延迟**只在输入连续时采样。** 手指停顿后落笔，第一个事件距上一个事件已经
    /// 隔了整段停顿，此时量到的"延迟"其实是停顿长度而不是管线延迟——实测未过滤
    /// 时是 90~143ms，而连续书写下只有一帧（~11ms）。判据用"上一个事件也很近"
    /// （`input_stream_gap`），而不是给 `since_input` 设上限：后者会把真正的管线
    /// 延迟也一并砍掉。
    pub fn begin_frame(&self) -> Instant {
        let frame_started = Instant::now();
        // 记账本帧的重绘来源：闸门装在输入路径上，但重绘还有别的来源（摄像头消息
        // 导致的 view 变化、窗口事件、iced/winit 内部）。不分类就无法判断"超出的帧"
        // 到底来自哪里。
        viewink::stats::note_frame_source();

        let since_input = self.last_input_at.get().elapsed();
        let input_stream_gap = self
            .last_input_at
            .get()
            .duration_since(self.last_input_prev_at.get());
        if input_stream_gap < STREAM_CONTINUITY_GAP
            && since_input < Duration::from_millis(250)
        {
            viewink::stats::add_input_latency(since_input);
        }
        frame_started
    }

    /// 帧出口：推进帧起点并做间隔分类。
    ///
    /// 这里的间隔是 **layout 间隔**，不是呈现间隔。iced 的 canvas widget 在 layout
    /// 阶段就调 `Program::draw` 来构建几何，所以调用次数**大于**呈现次数（实测
    /// 74/s vs 37.7/s）。面板刷新周期的估计因此**不能**在这里做——它由
    /// `iced_winit` 在成功 `present` 之后喂入（见 [`crate::pacing::note_present`]）。
    /// 曾经的错法（拿这里的间隔当刷新率）让笔尖外推的量化周期短了近一半，笔尖因此
    /// 落后指针。
    pub fn end_frame(&self, frame_started: Instant) {
        let previous_start = self.last_frame_start.replace(frame_started);
        let gap = frame_started.duration_since(previous_start);
        // 仅作诊断：`rstat` 里的 `frames=` 是 **layout 次数**，不是帧率。真实呈现速率
        // 看 `[frame_pacing] 呈现=N`。
        viewink::stats::note_frame_interval(gap);

        // 但帧间隔里混着"没人请求重绘、我们等输入"的空档。把它单列出来，否则空闲会
        // 被读成长帧停顿（实测误报 `long=9 worst=53ms`，实为 9 段无输入的安静期）。
        //
        // 阈值取面板刷新周期的 1.5 倍（60Hz 上 25ms），**不是** 4 倍：第一版用 4 倍
        // （67ms），而实测那些"停顿"恰恰是 49~57ms——阈值比要分类的对象还大，于是
        // `idle_gap` 恒为 0，空档照样被算进 `long`/`worst`。一帧的正常节奏是 ~11ms
        // （90fps），1.5 个 vsync 足以区分。
        let idle_threshold = panel_period()
            .map(|p| p.mul_f64(1.5))
            .unwrap_or(Duration::from_millis(25));
        viewink::stats::note_idle_gap(gap, idle_threshold.as_nanos() as u64);

        // 帧计数与 1 秒窗口报表。`end_frame` 原本只在批注通道 prepare 里调，而直通
        // 模式下报表不出现（几轮排查都被这个坑过）。放在这里才是与真实呈现帧一一
        // 对应的位置。
        viewink::stats::end_frame();
    }

    /// 性能 HUD 采样（仅在 `enabled` 时统计，开销极小）。
    ///
    /// `memory_mb` 是**闭包**而非直接读数：本函数对内存的调用频率是 1/s，而 FPS
    /// 采样是 0.5/s，两道闸门各自独立。传闭包让平台相关的内存查询留在调用方
    /// （`sys::memory` 是平台工具，不属于渲染层），且两道闸门都没放行时它一次也
    /// 不会被调用。
    pub fn sample_perf(&self, enabled: bool, memory_mb: impl FnOnce() -> f64) {
        if !enabled {
            return;
        }
        let now = Instant::now();
        let last_start = self.perf_last_frame_start.replace(now);
        let elapsed = now.duration_since(last_start).as_secs_f64();
        let frames = self.perf_frames.get() + 1;
        self.perf_frames.set(frames);
        let since_fps = now
            .duration_since(self.perf_last_fps_time.get())
            .as_secs_f64();
        if since_fps >= 0.5 {
            self.perf_fps.set(frames as f64 / since_fps);
            // 帧耗时显示与 FPS 同窗口降频（0.5s）：数字每帧变化会触发文字重新
            // shape/栅格化与 glyph atlas 更新——书写热路径上的持续 CPU/GPU 开销，
            // 与观测价值不成比例
            self.perf_frame_ms.set(elapsed * 1000.0);
            self.perf_frames.set(0);
            self.perf_last_fps_time.set(now);
        }
        if now.duration_since(self.perf_last_mem_time.get()).as_secs_f64() >= 1.0 {
            self.perf_memory_mb.set(memory_mb());
            self.perf_last_mem_time.set(now);
        }
    }

    /// 性能 HUD 当前读数。
    pub fn perf(&self) -> PerfSnapshot {
        PerfSnapshot {
            fps: self.perf_fps.get(),
            frame_ms: self.perf_frame_ms.get(),
            memory_mb: self.perf_memory_mb.get(),
        }
    }

    /// 帧预算记账：本帧属主 CPU 工作耗时。
    ///
    /// 与 `stats::note_prepare_work`（批注层打包上传 + 剔除）相加才是宿主在一帧
    /// 内可控的全部开销——逼近刷新周期即意味着要丢帧。
    ///
    /// 同时记 `add_draw_cost`：`draw` 函数体自身的耗时——回答"我们的代码慢不慢"
    /// 的唯一量。帧间隔（含等待输入）回答不了，它一直是本项目最大的误读来源。
    pub fn note_frame_work(&self, frame_started: Instant) {
        let work = frame_started.elapsed();
        viewink::stats::note_frame_work(work);
        viewink::stats::add_draw_cost(work);
    }
}

impl Default for FrameMetrics {
    fn default() -> Self {
        Self::new()
    }
}
