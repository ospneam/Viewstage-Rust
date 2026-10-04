//! # viewgfx — 画布图形编排层（性能优先，画质其次）
//!
//! 从主 crate（main.rs）隔离出的「每帧图形编排」逻辑：摄像头/图片背景层
//! 图元构建、批注层静态快照同步、进行中笔画增量镶嵌调度、触控预测轨迹。
//! 职责边界：只做**编排与缓存**——像素处理在 viewcamera，笔迹算法在 viewink，
//! wgpu 管线实现分别在两个引擎 crate 内，本 crate 不持有任何 GPU 设备对象。
//!
//! ## 性能预算（帧热路径内逐项兑现）
//!
//! | 路径 | 预算 |
//! | --- | --- |
//! | [`CanvasGfx::camera_layer`]（每帧） | 滤镜/帧未变 → 命中 dirty-cache，返回同一 `Arc`，零重建零分配 |
//! | [`CanvasGfx::annotation_layer`]（每帧） | 静态快照代数未变 → O(1) 早退；变化 → 复用 scratch 缓冲重建（无中间 `Vec` 分配） |
//! | [`CanvasGfx::append_pen_point`]（每点） | O(1)：3 槽速度环形缓冲 + 增量宽度，零分配 |
//! | [`CanvasGfx::append_eraser_point`]（每点） | 16ms 节流 + O(1) 追加，零分配 |
//! | [`CanvasGfx::refresh_live_geometry`]（**每帧一次**） | 增量：仅新增点与尾部段重算，满批（16 段）块常驻 GPU，零分配 |
//! | 触控预测轨迹（每帧） | 双缓冲池回收段缓冲 + 历史 drain 复用缓冲，稳态仅 1 次固定小分配（`Arc` 头） |
//!
//! 帧热路径（view() 与 canvas 零消息通道）内禁止：
//! - 稳态下的 `Vec::new()` / `format!` / 字符串解析等分配操作；
//! - 全量重建任何可增量维护的几何（静态快照、满批块、轨迹缓冲均按代数/差量维护）。
//!
//! ## 与主 crate 的分工
//!
//! - 主 crate：消息路由、手势识别（viewcore-touch）、平台输入 hook（sys），
//!   调用本 crate 的门面方法完成每帧编排；
//! - 本 crate：把「状态 → 每帧图元」的全部决策与缓存收敛到一处，
//!   `CanvasGfx` 持有 [`viewink::live::LiveCanvasState`] 的权威 `Arc`，
//!   主 crate 通过克隆句柄在零消息热路径直写同一状态。

mod background;
mod live_geo;
mod trail;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use viewcamera::camera::CameraLayer;
use viewink::live::{LiveCanvasState, StaticStrokeRef};

pub use background::BackgroundInputs;
pub use trail::TouchTrailCtx;

/// 逐笔画网格缓存条目（主 crate `stroke_mesh_cache` 的值类型）。
/// 笔画完成时插入未扣几何兜底，后台计算线程返回扣除几何后按代数门控替换。
pub struct CachedMesh {
    pub geometry: Arc<viewink::gpu::StrokeGeometry>,
    /// 计算该网格时的橡皮擦代数（过期结果丢弃依据）。
    pub eraser_generation: u64,
    /// 几何的画布坐标 AABB（`[min_x, min_y, max_x, max_y]`，含半宽外扩）。
    ///
    /// 与几何**同代**产出并随缓存条目存放，供快照构建零成本取用。
    /// 旧实现让快照构建现场调 `geometry.bounds()` 重扫全部段——快照每次
    /// 缓存代数变化都整表重建，于是每加一笔要重扫**所有**笔画的全部段，
    /// 整场书写累计 O(n²)。视口剔除的判定本身在帧热路径内是 O(1)，
    /// 但它的输入必须是 O(1) 拿得到的。
    pub bounds: [f32; 4],
}

impl CachedMesh {
    /// 由几何构造缓存条目：连带算出 AABB（同代产出，避免快照重建时重扫）。
    pub fn new(geometry: viewink::gpu::StrokeGeometry, eraser_generation: u64) -> Self {
        let bounds = geometry.bounds();
        Self {
            geometry: Arc::new(geometry),
            eraser_generation,
            bounds,
        }
    }
}

/// 触控子帧历史 drain 回调：平台触控 hook（主 crate sys 模块）把共享
/// history 追加进调用方复用缓冲。本 crate 不依赖平台层，由调用方注入。
pub type DrainTouchHistory<'a> = &'a mut dyn FnMut(&mut Vec<(f32, f32)>);

/// 每帧批注层编排输入（view() 路径）。
pub struct FrameInputs<'a> {
    /// 逐笔画网格缓存（id → [`CachedMesh`]）。
    pub mesh_cache: &'a HashMap<u64, CachedMesh>,
    /// 缓存代数：任意 mutation 点递增，与 `live.static_generation` O(1) 比较。
    pub mesh_generation: u64,
    /// 触控/鼠标预测轨迹上下文；`None` 表示当前帧不构建（并清除残留轨迹）。
    pub touch_trail: Option<TouchTrailCtx>,
}

/// 画布图形编排门面：主 crate 每帧调用的唯一入口。
///
/// 所有方法仅 `&self`——内部状态全部 `Cell`/`RefCell`/`Arc`，
/// 与 canvas 零消息热路径（`&self`）兼容。
pub struct CanvasGfx {
    /// 分层渲染共享实时状态（权威句柄）。主 crate 克隆后在
    /// canvas 热路径直写变换/进行中笔画；本结构的各方法读写同一状态。
    pub live: Arc<LiveCanvasState>,

    /// 摄像头参数槽：`update_camera_params` 每帧覆盖写（dirty-cache 产出），
    /// `viewcamera::camera::CameraChannel`（直通通道）prepare 时取走。
    camera_params: Arc<Mutex<Option<Arc<CameraLayer>>>>,

    background: background::BackgroundCache,
    /// 静态快照重建复用缓冲（仅代数变化时使用；稳态空闲）。
    static_scratch: RefCell<Vec<StaticStrokeRef>>,
    /// 橡皮擦追加点 16ms 节流时钟（原主 crate last_move_redraw）。
    eraser_throttle: Cell<Instant>,
    /// 触控子帧历史 drain 复用缓冲（稳态零分配）。
    touch_history: RefCell<Vec<(f32, f32)>>,
    /// 笔迹镶嵌采样点复用缓冲（尾部段每帧重建，跨帧保容量 → 零分配）。
    tess_scratch: RefCell<Vec<(f32, f32, f32)>>,
    /// 触控预测轨迹构建器（双缓冲池）。
    trail: RefCell<trail::TrailBuilder>,
}

impl CanvasGfx {
    /// 创建编排门面（`initial_scale` 为初始视图缩放）。
    pub fn new(initial_scale: f32) -> Self {
        Self {
            live: Arc::new(LiveCanvasState::new(initial_scale)),
            camera_params: Arc::new(Mutex::new(None)),
            background: background::BackgroundCache::default(),
            static_scratch: RefCell::new(Vec::new()),
            eraser_throttle: Cell::new(Instant::now()),
            touch_history: RefCell::new(Vec::new()),
            tess_scratch: RefCell::new(Vec::new()),
            trail: RefCell::new(trail::TrailBuilder::default()),
        }
    }

    /// 摄像头参数槽句柄（App 启动时交给 `CameraChannel::new`——直通通道
    /// prepare 阶段从此槽取最新快照）。
    pub fn camera_params_handle(&self) -> Arc<Mutex<Option<Arc<CameraLayer>>>> {
        Arc::clone(&self.camera_params)
    }

    /// 摄像头/图片背景层参数提交（原 `camera_layer` 图元构建改为写共享槽）：
    /// dirty-cache 判定输入未变时返回同一 `Arc`（零重建零分配）并覆盖写入
    /// 参数槽，由直通通道 prepare 阶段取走上传。
    pub fn update_camera_params(&self, input: BackgroundInputs) {
        let layer = self.background.camera_layer(&self.live.transform, input);
        *self
            .camera_params
            .lock()
            .expect("camera params slot poisoned") = Some(layer);
    }

    /// 批注层每帧编排：静态快照同步 → 触控轨迹（**不构建图元**
    /// ——几何全部写进共享 `LiveCanvasState`，由批注直通通道 prepare 直读）。
    ///
    /// **进行中笔画的镶嵌不在这里**：它由宿主在 `Program::draw` 里每帧调用
    /// [`CanvasGfx::refresh_live_geometry`] 一次——`view()` 只在有消息时
    /// 重建，而绘制帧大多是无消息帧（UI 静止、仅批注层刷新）。放在 draw
    /// 里同时保证「每帧恰好一次」与「用最新输入点」。
    ///
    /// `drain_touch_history`：平台触控 hook（主 crate sys 模块）的历史
    /// drain 回调——本 crate 不依赖平台层，由调用方注入（写入内部复用缓冲）。
    pub fn annotation_frame(
        &self,
        input: FrameInputs,
        drain_touch_history: DrainTouchHistory<'_>,
    ) {
        live_geo::refresh_static_snapshot(
            &self.live,
            input.mesh_cache,
            input.mesh_generation,
            &mut self.static_scratch.borrow_mut(),
        );
        self.update_touch_trail(input.touch_trail, drain_touch_history);
    }

    /// 触控预测轨迹更新（消息热路径直接驱动点）。
    ///
    /// 运行时对「视图不变消息批」跳过 view()/layout 重建后，原 view() 内的
    /// 轨迹构建不再逐输入事件执行——本方法由 RawTouch* 消息处理路径直接
    /// 调用：drain 子帧历史后按 `ctx` 构建预测轨迹；`None` = 清除残留轨迹。
    /// 两种情形都 drain 共享历史缓冲，避免 hook 侧 history 无界积压。
    pub fn update_touch_trail(
        &self,
        ctx: Option<TouchTrailCtx>,
        drain_touch_history: DrainTouchHistory<'_>,
    ) {
        let mut history = self.touch_history.borrow_mut();
        drain_touch_history(&mut history);
        match ctx {
            Some(ctx) => self.trail.borrow_mut().build(&self.live, &ctx, &history),
            None => self.trail.borrow_mut().clear(&self.live),
        }
    }

    /// 进行中笔画/橡皮擦的增量镶嵌。**每渲染帧调用一次**（宿主在
    /// `Program::draw` 里调用，见 [`Self::annotation_frame`] 的说明）：
    /// 此刻本帧输入已全部并入共享状态，故一次重建即最新。
    pub fn refresh_live_geometry(&self) {
        live_geo::refresh_live_geometry(&self.live, &mut self.tess_scratch.borrow_mut());
    }

    /// 向当前笔快速追加一个画布坐标点（含笔锋速度→宽度计算）。
    pub fn append_pen_point(&self, cx: f32, cy: f32) {
        live_geo::append_pen_point(&self.live, cx, cy);
    }

    /// 收笔前把笔尖外推点固化为真实点（见 `live_geo::commit_tip_prediction`）。
    ///
    /// 必须在 `reset_current_stroke` **之前**调用——那一步会把 `tip_predict`
    /// 清零，晚一步笔尾就会回缩。
    pub fn commit_tip_prediction(&self) {
        live_geo::commit_tip_prediction(&self.live);
    }

    /// 向当前橡皮擦追加一个画布坐标点（16ms 节流；`eraser_size_screen`
    /// 为屏幕坐标橡皮擦尺寸，内部按当前缩放换算为画布宽度）。
    pub fn append_eraser_point(&self, eraser_size_screen: f32, cx: f32, cy: f32) {
        live_geo::append_eraser_point(
            &self.live,
            eraser_size_screen,
            cx,
            cy,
            &self.eraser_throttle,
        );
    }
}
