//! 实时画布共享状态（分层渲染通道）。
//!
//! 分层设计：canvas 交互（绘画/拖拽/橡皮擦悬停）期间走零消息通道——
//! `canvas::Program::update`（&self）直接写入此处状态并返回
//! `Action::request_redraw()`，不发布任何 Message：
//! - 运行时不进入 program.update → **UI 层（widget 树）完全不重建**；
//! - 重绘帧仅执行 interface.draw + 图元 prepare/draw，
//!   本结构的最新值被直接上传 GPU，实现"只渲染 canvas 层，UI 静态叠加"。
//!
//! 层间解耦：
//! - 信号源层（摄像头/图片）只依赖 [`LiveTransform`]——pan/zoom 变换的
//!   独立句柄；批注笔画状态对它完全不可见。
//! - 批注层持完整 [`LiveCanvasState`]（内含同一变换句柄）。
//! 因此显示图片时绘画：批注层独立刷新，信号源层零上传零重建，纯 GPU 叠加。

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Instant;

use crate::gpu::StrokeGeometry;
use crate::ink::stroke::DrawStroke;

/// 静态笔画引用（stroke id → 画布坐标几何）。
/// 定义于 annotation 管线；此处 re-export 供快照槽位使用。
pub use crate::render::annotation::StaticStrokeRef;

/// 视图变换实时句柄：pan/zoom 的单一数据源。
///
/// 信号源层（相机/图片图元）与批注层共享同一 Arc；
/// 写入方为 App 消息路径与 canvas 零消息热路径。
#[derive(Debug)]
pub struct LiveTransform {
    pub scale: Cell<f32>,
    pub offset_x: Cell<f32>,
    pub offset_y: Cell<f32>,
}

impl LiveTransform {
    pub fn new(initial_scale: f32) -> Self {
        Self {
            scale: Cell::new(initial_scale),
            offset_x: Cell::new(0.0),
            offset_y: Cell::new(0.0),
        }
    }

    /// 当前视图变换快照（渲染 uniform 用）。
    pub fn view_transform(&self) -> crate::gpu::ViewTransform {
        crate::gpu::ViewTransform {
            scale: self.scale.get(),
            offset_x: self.offset_x.get(),
            offset_y: self.offset_y.get(),
        }
    }
}

// SAFETY: 同 LiveCanvasState——仅 winit 主线程读写（见下方不变量说明）。
unsafe impl Send for LiveTransform {}
unsafe impl Sync for LiveTransform {}

/// 实时画布状态：App（写）与渲染图元（读，prepare/draw 阶段）之间的共享层。
///
/// 全部使用 Cell/RefCell 以支持 `&self` 热路径写入；渲染侧仅在
/// prepare/draw 短暂借用，单线程 UI 前提下无争用。
pub struct LiveCanvasState {
    // ---- 视图变换（信号源层共享的唯一耦合点）----
    pub transform: Arc<LiveTransform>,

    // ---- 进行中笔画（笔）----
    pub current_stroke: RefCell<Option<DrawStroke>>,
    pub pen_speed_buffer: Cell<[f32; 3]>,
    pub pen_speed_count: Cell<usize>,
    pub pen_speed_write: Cell<usize>,
    pub pen_stroke_start_time: Cell<Option<Instant>>,
    pub pen_last_move_time: Cell<Option<Instant>>,
    pub pen_total_segments: Cell<usize>,

    // ---- 进行中笔画增量几何缓存 ----
    /// 已满批的稳定段（常驻 GPU，不重传）。
    pub chunks: RefCell<Vec<Arc<StrokeGeometry>>>,
    /// 下一个待整批镶嵌的段 index。
    pub batch_start: Cell<usize>,
    /// 尾部（未满批已定形段 + 笔尖段），每次追加重建。
    pub head: RefCell<Option<Arc<StrokeGeometry>>>,
    pub head_index: Cell<u64>,
    /// 已处理的点数（差量镶嵌依据）。
    pub point_count: Cell<usize>,
    /// 增量路径缓存 path[0]=points[0].from，path[k]=points[k-1].to。
    pub path: RefCell<Vec<(f32, f32, f32)>>,
    /// 实时笔锋逐点宽度（与 path 对齐）。
    pub widths: RefCell<Vec<f32>>,

    // ---- 进行中橡皮擦 ----
    pub current_eraser_stroke: RefCell<Option<DrawStroke>>,
    /// 橡皮擦轨迹段几何（红色半透明预览）。
    pub eraser_chunks: RefCell<Vec<Arc<StrokeGeometry>>>,
    pub eraser_point_count: Cell<usize>,

    // ---- 触控预测轨迹 ----
    pub trail_generation: Cell<u64>,
    /// (generation, geometry)；None 表示无轨迹。
    pub trail: RefCell<Option<(u64, Arc<StrokeGeometry>)>>,

    // ---- 笔尖外推（显示层补偿 vsync 一帧量化）----
/// **有效**帧量化周期（毫秒）；`0` = 未测得 → **不外推**（保守退化）。
    ///
    /// 宿主每渲染帧写入（`App::draw`）。笔尖外推的预测时域就是它：
    /// 指针采样发生在 vsync 边缘，帧内容在**下一个** vsync 才上屏，光标层
    /// 却用更新鲜的采样 → 笔尖系统性地落后指针约一帧的位移。
    ///
    /// 必须是**节流后**的有效周期而不是面板刷新率：书写时 GPU 占用率优先，
    /// 帧率被限到 60-72fps，量化周期因此翻倍。补偿要跟着一起翻倍。
    pub frame_period_ms: Cell<f32>,
    /// 笔尖外推向量（画布坐标）：`指针速度 × frame_period_ms`，屏幕空间限幅。
    ///
    /// **纯显示层**：只用于延长尾段的终点，**绝不写进 `stroke.points`**，
    /// 因此完成后的笔迹几何与未外推时逐段一致（外推只是把"这一帧该看到
    /// 的笔尖位置"提前画出来；真实点到达后立刻取代它）。
    pub tip_predict: Cell<[f32; 2]>,

    // ---- 静态笔画快照 ----
    /// stroke_mesh_cache 的 id 升序快照；mesh cache 变化时由 App 同步，
    /// 渲染侧按 Arc 内容差量上传。
    pub static_snapshot: RefCell<Arc<[StaticStrokeRef]>>,
    /// 快照对应的 mesh cache 代数（App 侧 O(1) 判断是否需要重建快照）。
    pub static_generation: Cell<u64>,

    // ---- 视口（逻辑像素；canvas draw 每帧写入）----
    /// 渲染侧据此把屏幕矩形逆变换为画布矩形，做静态笔画**视口剔除**：
    /// 视口外的笔画不上传、不绘制（高笔迹量下的主要开销来源）。
    pub viewport: Cell<(f32, f32)>,

    // ---- 橡皮擦悬停光标（画布坐标，GPU 动态层消费）----
    pub eraser_cursor: Cell<Option<EraserCursor>>,
}

impl LiveCanvasState {
    pub fn new(initial_scale: f32) -> Self {
        Self {
            transform: Arc::new(LiveTransform::new(initial_scale)),
            current_stroke: RefCell::new(None),
            pen_speed_buffer: Cell::new([0.0; 3]),
            pen_speed_count: Cell::new(0),
            pen_speed_write: Cell::new(0),
            pen_stroke_start_time: Cell::new(None),
            pen_last_move_time: Cell::new(None),
            pen_total_segments: Cell::new(1),
            chunks: RefCell::new(Vec::new()),
            batch_start: Cell::new(0),
            head: RefCell::new(None),
            head_index: Cell::new(0),
            point_count: Cell::new(0),
            path: RefCell::new(Vec::new()),
            widths: RefCell::new(Vec::new()),
            current_eraser_stroke: RefCell::new(None),
            eraser_chunks: RefCell::new(Vec::new()),
            eraser_point_count: Cell::new(0),
            trail_generation: Cell::new(0),
            trail: RefCell::new(None),
            viewport: Cell::new((0.0, 0.0)),
            frame_period_ms: Cell::new(0.0),
            tip_predict: Cell::new([0.0, 0.0]),
            static_snapshot: RefCell::new(Arc::from([])),
            static_generation: Cell::new(0),
            eraser_cursor: Cell::new(None),
        }
    }

    /// 当前视图变换快照（渲染 uniform 用）。
    pub fn view_transform(&self) -> crate::gpu::ViewTransform {
        self.transform.view_transform()
    }

    /// 清空进行中笔画的全部增量缓存（笔画结束/中断时调用）。
    pub fn reset_current_stroke(&self) {
        *self.chunks.borrow_mut() = Vec::new();
        self.batch_start.set(0);
        *self.head.borrow_mut() = None;
        *self.path.borrow_mut() = Vec::new();
        *self.widths.borrow_mut() = Vec::new();
        self.point_count.set(0);
        // 笔尖外推是逐帧重算的显示层状态：收笔后必须清零，否则下一笔的
        // 第一帧会带上上一笔末端的预测位移（笔尖凭空窜出去一截）。
        self.tip_predict.set([0.0, 0.0]);
    }

    /// 清空进行中橡皮擦的全部缓存。
    pub fn reset_current_eraser(&self) {
        *self.eraser_chunks.borrow_mut() = Vec::new();
        self.eraser_point_count.set(0);
    }
}

impl std::fmt::Debug for LiveCanvasState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 高频可变内部状态不做完整格式化（避免借用冲突），仅输出摘要
        f.debug_struct("LiveCanvasState")
            .field("scale", &self.transform.scale.get())
            .field(
                "offset",
                &(self.transform.offset_x.get(), self.transform.offset_y.get()),
            )
            .field(
                "drawing",
                &self.current_stroke.borrow().as_ref().map(|s| s.id),
            )
            .finish()
    }
}

/// 橡皮擦悬停光标（画布坐标，GPU 动态层直接生成圆环几何）。
#[derive(Debug, Clone, Copy)]
pub struct EraserCursor {
    pub cx: f32,
    pub cy: f32,
    /// 画布坐标半径（屏幕橡皮擦尺寸的一半 / scale）
    pub radius: f32,
    /// 画布坐标线宽（边框 2px / scale）
    pub border: f32,
}

// SAFETY: LiveCanvasState 仅在 winit 主线程访问——写入方为 canvas 事件处理
// （canvas::Program::update）与 App 消息路径，读取方为同一事件循环内执行的
// 图元 prepare/draw。Cell/RefCell 在此单线程前提下即普通可变性。
// 跨线程数据源（相机帧/触控原始队列）均经消息通道在主线程汇合后写入，
// 不直接触碰本结构。若未来渲染移至独立线程，此不变量必须重新审视。
unsafe impl Send for LiveCanvasState {}
unsafe impl Sync for LiveCanvasState {}
