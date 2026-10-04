mod app;
mod sys;
mod views;

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

use iced::alignment::{Horizontal, Vertical};
use iced::futures::channel::mpsc;
use iced::futures::sink::SinkExt;
use iced::futures::StreamExt;
use iced::mouse;
use iced::widget::canvas::{Action, Event, Geometry};
use iced::widget::{button, canvas, column, container, image, row, stack, text};
use iced::{
    Border, Color, Element, Length, Padding, Point, Rectangle, Renderer, Shadow, Size,
    Subscription, Task, Theme,
};
use tokio::sync::mpsc as tokio_mpsc;
use views::icons;

use app::config::Config;
use app::state::DrawMode;
use std::time::Instant;

// ==================
// ImageEntry
// ==================

#[derive(Debug, Clone)]
struct ImageEntry {
    handle: image::Handle,
    /// 预解码 RGBA 的共享句柄：信号源层背景纹理数据源。
    /// 以 Arc 指针作为图片身份（同图重入零重上传；列表持有全部 Arc，无 ABA）。
    rgba: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    source_id: Option<SourceId>,
}
use app::source_manager::{SourceId, SourceManager, SourceState};
use viewink::ink::history::{DrawCommand, HistoryManager, UndoneCommand};
use viewink::ink::tessellator::apply_start_taper;
use viewink::ink::stroke::DrawStroke;
use viewcore_touch::{Momentum, ScreenTransform};

// ==================
// 输入诊断计数器（并入 `viewink::stats` 的 1 秒报表）
// ==================
// 曾经还有一个独立的 `[rate]` 报表（`report_input_rate` + DRAW_FRAMES +
// RATE_LAST），现已删除：它的采样窗口是"每 30 帧"，于是**只有在帧率低于
// 30/s 时才发得出报表**，读数系统性偏低（实测它报 25fps 而同一时刻 rstat
// 报 55fps）。诊断速率只有一个权威来源——`stats::end_frame` 驱动的 1 秒窗口。

// ==================
// ToolButton
// ==================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolButton {
    Menu,
    Minimize,
    Move,
    Comment,
    Eraser,
    Undo,
    Photo,
    Light,
    Settings,
    Image,
}

// ==================
// CameraCommand (主线程→摄像头线程)
// ==================

#[derive(Debug, Clone)]
pub enum CameraCommand {
    Start {
        index: usize,
        width: u32,
        height: u32,
        frame_interval_ms: u64,
    },
    Stop,
    Capture,
}

// ==================
// CameraEvent (摄像头线程→主线程)
// 帧载荷直接复用 viewcamera 类型：Arc 包装在采集线程完成，
// 主线程零转换（原 CameraFramePixels/CameraFrameData 转换样板已删除）
// ==================

#[derive(Debug, Clone)]
pub enum CameraEvent {
    Frame(viewcamera::camera::CameraFrameData),
    Captured(viewcamera::camera::CameraFrameData),
}

// ==================
// 后台线程命令发送器（std::sync::mpsc 包装）
// 用阻塞通道替代轮询：工作线程 recv() 挂起零唤醒，替代 try_recv+sleep 空转
// ==================

/// 主线程 → 摄像头线程命令通道（std mpsc，Clone + 手动 Debug 以满足 Message 约束）。
#[derive(Clone)]
pub struct CameraCommandSender(std::sync::mpsc::Sender<CameraCommand>);

impl std::fmt::Debug for CameraCommandSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CameraCommandSender")
    }
}

impl CameraCommandSender {
    fn send(&self, cmd: CameraCommand) {
        let _ = self.0.send(cmd);
    }
}

/// 主线程 → 笔画计算线程任务通道。
#[derive(Clone)]
pub struct StrokeTaskSender(std::sync::mpsc::Sender<StrokeComputeTask>);

impl std::fmt::Debug for StrokeTaskSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StrokeTaskSender")
    }
}

impl StrokeTaskSender {
    fn send(&self, task: StrokeComputeTask) {
        let _ = self.0.send(task);
    }
}

// ==================
// StrokeComputeCommand (主线程→计算线程)
// 后台线程计算「扣除了已存在橡皮擦」的笔画网格，主线程先用未扣网格兜底 ≤1 帧
// ==================

#[derive(Debug, Clone)]
pub struct StrokeComputeTask {
    stroke_id: u64,
    stroke: DrawStroke,
    /// 共享的橡皮擦集快照：只克隆引用计数，**零深拷贝**。
    ///
    /// 由 [`viewink::ink::eraser::EraserSet::shared`] 提供，写时复制保证
    /// 本快照在任务排队与执行期间不会被主线程的后续擦除改动。
    erasers: Arc<Vec<DrawStroke>>,
    eraser_generation: u64,
}

// ==================
// StrokeComputeResult (计算线程→主线程)
// ==================

#[derive(Debug, Clone)]
pub struct StrokeComputeResult {
    stroke_id: u64,
    /// 扣除橡皮擦后的几何
    geometry: Option<viewink::gpu::StrokeGeometry>,
    eraser_generation: u64,
}

// ==================
// 逐笔画网格缓存条目
// 类型已收编为 viewgfx::CachedMesh（见 CanvasGfx 编排层）
// ==================

// ==================
// Message
// ==================

#[derive(Debug, Clone)]
pub enum Message {
    ToolClicked(ToolButton),
    ColorSelected(usize),
    SizeSelected(usize),
    EraserSizeSelected(usize),
    EraserClearSliderChanged(f64),
    EraserClearSliderReleased,
    RotateLeft,
    RotateRight,
    BrightnessChanged(f64),
    ContrastChanged(f64),
    GrayscaleToggled(bool),
    ResetCameraSettings,
    MenuSettingsClicked,
    SettingsSectionSelected(SettingsSection),
    FrameRateModeSelected(String),
    /// 批注热路径渲染帧率上限（30–240）。高刷屏上批注 60fps 已足够顺滑，
    /// 限帧可线性降低 GPU 占用与功耗——这是全应用最有效的性能旋钮。
    AnnotationFpsCapChanged(u32),
    MomentumSettingToggled(bool),
    /// 笔锋收尾最短宽度比（写 `canvas` 副本，见 P1 说明）。
    PenMinWidthRatioChanged(f32),
    /// 笔锋收尾时间窗（毫秒）：笔速衰减回溯窗口越长，笔尾越尖。
    PenTailDurationChanged(f32),
    PenSizePresetChanged(usize, f32),
    EraserSizePresetChanged(usize, f32),
    CameraIndexChanged(u32),
    CameraResolutionSelected((u32, u32)),
    DefaultRotationSelected(i32),
    PictureDirectorySelected,
    PresentModeSelected(String),
    IntegratedGpuToggled(bool),
    ExportSettings,
    ImportSettings,
    ResetAllSettings,
    MenuCloseClicked,
    SidebarImageSelected(usize),
    SidebarImageDelete(usize),
    SidebarImportImage,
    CanvasPressed(Point),
    CanvasReleased,
    CanvasWheelScroll {
        delta: f32,
        cursor: Point,
    },
    CameraReady(CameraCommandSender),
    CameraEvent(CameraEvent),
    StrokeComputeReady(StrokeTaskSender),
    StrokeComputed(StrokeComputeResult),
    WindowClosed,
    SettingsWindowOpened(iced::window::Id),
    SettingsWindowClosed(iced::window::Id),
    SettingsWindowDragRequested(iced::window::Id),
    SettingsWindowCloseRequested(iced::window::Id),
    WindowResized(iced::window::Id, Size),
    MainWindowOpened(iced::window::Id),
    // 开发者选项
    DevClickIncremented,
    /// 开发者滑块释放：统一落盘脏配置（拖动期间只改内存，避免每 tick 写盘）
    PersistDeveloperConfig,
    PerfMonitorToggled(bool),
    // OOBE
    OobeAction(OobeAction),
    OobeWindowOpened(iced::window::Id),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Canvas,
    Source,
    Storage,
    Renderer,
    Application,
    About,
}

/// 首次引导的最后一步序号（步骤 = 0..=OOBE_LAST_STEP）。
///
/// 引导只问**真正生效**的问题：批注帧率、摄像头预览帧率、摄像头本身。
/// 原先还有「语言」与「主题」两步——两者都是假设置（`language` 从未被读取，
/// `theme` 只决定哪个按钮高亮而所有 UI 颜色都是硬编码深色），已删除。
pub const OOBE_LAST_STEP: usize = 2;

#[derive(Debug, Clone)]
pub enum OobeAction {
    FrameRateModeSelected(String),
    AnnotationFpsCapChanged(u32),
    CameraIndexChanged(u32),
    CameraResolutionSelected((u32, u32)),
    NextStep,
    Complete,
    Skip,
}

// ==================
// App
// ==================

pub struct App {
    active_tool: ToolButton,
    pub draw_mode: DrawMode,
    strokes: Vec<DrawStroke>,
    frozen: bool,
    history: HistoryManager,
    pub selected_color: usize,
    pub selected_size: usize,
    pub selected_eraser_size: usize,
    sidebar_visible: bool,
    /// 右键菜单是否显示。`Cell`：零消息自有管线的 `handle_pointer_down`
    /// （`&self`）点画布时要立即关掉菜单，不能延迟到消息。
    menu_visible: Cell<bool>,
    light_on: bool,
    image_list: Vec<ImageEntry>,
    current_image_index: Option<usize>,
    showing_image: bool,
    settings_visible: bool,
    pub pen_panel_visible: bool,
    pub eraser_clear_slider: f64,
    pub camera_brightness: f64,
    pub camera_contrast: f64,
    pub camera_grayscale: bool,
    pub camera_rotation: i32,
    pub config: Config,
    // ---- 分层渲染通道：canvas 热路径（&self）与渲染图元共享的实时状态 ----
    // 变换(scale/offset)、进行中笔画、增量几何缓存、橡皮擦悬停光标等全部在此，
    // 交互期间零消息更新，UI widget 层不参与重建（见 render/live.rs）
    live: Arc<viewink::live::LiveCanvasState>,
    // ---- 图形编排门面（viewgfx）：背景层 dirty-cache、增量镶嵌编排、
    // 触控轨迹双缓冲池、静态快照复用缓冲。持有 live 的权威 Arc ----
    gfx: viewgfx::CanvasGfx,
    // ---- 帧驱动门面（viewframe）：呈现周期估计、vsync 跳帧档位、摄像头上屏
    // 网格、直通合成通道注册、每帧渲染计量与性能 HUD 采样 ----
    //
    // 这些原先是本结构体上的 15 个字段（last_frame_start / last_input_* /
    // perf_* / camera_pending / camera_grid_next / direct_channels_registered …）。
    // 它们与「用户点了哪个按钮」共用一个结构体，导致节拍标定只能靠真机看 HUD
    // 数字来自证。现在决策与其输入输出都在 viewframe crate 内，可单测。
    frame: viewframe::FrameDriver,
    /// 书写进行中（Down→Up / 按下→释放）。零消息路径（&self）写入 → Cell
    is_drawing: Cell<bool>,
    /// 鼠标左键按下中（含平移）。`Cell`：自有管线的 `handle_pointer_down` /
    /// `handle_pointer_up` 在 `&self` 下维护它，而橡皮擦悬停守卫与平移的
    /// 帧率豁免都要在零消息路径读它。
    mouse_left_pressed: Cell<bool>,
    /// 最近鼠标位置（pan 增量计算；&self 热路径可写）
    last_mouse_pos: std::cell::Cell<Option<(f64, f64)>>,
    /// 最近一次拖拽增量（释放时交给惯性）
    last_pan_delta: std::cell::Cell<(f64, f64)>,
    /// 惯性（viewcore-touch Momentum）：零消息路径（&self）也要操作 → RefCell
    momentum: std::cell::RefCell<Momentum>,
    background_image: Option<Arc<viewcamera::camera::CameraFrameData>>,
    camera_sender: Option<CameraCommandSender>,
    stroke_compute_sender: Option<StrokeTaskSender>,
    next_stroke_id: std::cell::Cell<u64>,
    // 逐笔画画布坐标几何缓存（key = 笔画 id）。笔画完成时插入未扣几何，
    // 后台线程返回扣除几何后替换；橡皮擦完成时只重建相交笔画（targeted rebuild）
    stroke_mesh_cache: RefCell<std::collections::HashMap<u64, viewgfx::CachedMesh>>,
    // 缓存代数：任意 mutation 点递增，静态快照同步用 O(1) 比较替代 O(n) 扫描
    mesh_cache_generation: Cell<u64>,
    // 橡皮擦代数：新增橡皮擦完成后自增，用于丢弃后台线程的过期结果
    eraser_generation: Cell<u64>,
    // 橡皮擦集：权威列表 + 供后台线程共享的只读快照（写时复制，见
    // `viewink::ink::eraser::EraserSet`）。字段私有，变更只走其方法，
    // 因此「快照过期」这个失效模式在类型层面就不可能发生。
    erasers: viewink::ink::eraser::EraserSet,
    // 信号源管理
    source_manager: SourceManager,
    // V2 捏合缩放中点锚点（开始时中点对应的画布坐标；零消息路径写入 → Cell）
    pinch_start_mid_cx: Cell<f32>,
    pinch_start_mid_cy: Cell<f32>,
    // 窗口尺寸
    window_size: Size,
    // 设置窗口
    open_settings_windows: std::collections::HashSet<iced::window::Id>,
    pub settings_section: SettingsSection,
    settings_feedback: Option<String>,
    // 主窗口 ID
    main_window_id: Option<iced::window::Id>,
    // 触控轨迹预览的预解析颜色 (r, g, b)，避免每帧 from_str_radix
    trail_color_rgb: Cell<(f32, f32, f32)>,
    // 配置脏标记：滑块拖动只改内存，释放/窗口关闭时统一落盘
    config_dirty: Cell<bool>,
    // 上一帧惯性积分时刻（帧率无关的惯性摩擦）。原由 `FrameTick` 消息更新，
    // 现由 `step_momentum` 在每个渲染帧上更新——与渲染节拍同源，比独立
    // 定时器更准。
    //
    // 注意这**不是**渲染帧起点（那个在 `viewframe::FrameMetrics` 里）：这里是
    // 惯性自己上一次积分的时刻，两者由不同的调用点推进。
    last_frame_tick_time: Cell<Instant>,
    // OOBE
    pub show_oobe: bool,
    pub oobe_step: usize,
    pub oobe_windows: std::collections::HashSet<iced::window::Id>,
    pub dev_click_count: u8,
    }

impl Default for App {
    fn default() -> Self {
        let config = Config::load();
        let show_oobe = !config.developer.first_run_completed;
        // 图形编排门面先建：live 的权威 Arc 在其内部，App 克隆同一句柄
        let gfx = viewgfx::CanvasGfx::new(1.0);
        let live = Arc::clone(&gfx.live);

        App {
            active_tool: ToolButton::Move,
            draw_mode: DrawMode::Move,
            strokes: Vec::new(),
            frozen: false,
            history: HistoryManager::new(),
            selected_color: 0,
            selected_size: 1,
            selected_eraser_size: 1,
            sidebar_visible: false,
            menu_visible: Cell::new(false),
            light_on: false,
            image_list: Vec::new(),
            current_image_index: None,
            showing_image: false,
            settings_visible: false,
            pen_panel_visible: false,
            eraser_clear_slider: 0.0,
            camera_brightness: 0.0,
            camera_contrast: 100.0,
            camera_grayscale: false,
            camera_rotation: config.source.default_rotation,
            live,
            gfx,
            frame: viewframe::FrameDriver::new(),
            config,
            is_drawing: Cell::new(false),
            mouse_left_pressed: Cell::new(false),
            last_mouse_pos: std::cell::Cell::new(None),
            last_pan_delta: std::cell::Cell::new((0.0, 0.0)),
            momentum: std::cell::RefCell::new(Momentum::new()),
            background_image: None,
            camera_sender: None,
            stroke_compute_sender: None,
            next_stroke_id: std::cell::Cell::new(0),
            stroke_mesh_cache: RefCell::new(std::collections::HashMap::new()),
            mesh_cache_generation: Cell::new(0),
            eraser_generation: Cell::new(0),
            erasers: viewink::ink::eraser::EraserSet::new(),
            source_manager: {
                let mut sm = SourceManager::new();
                sm.enter(SourceId::Camera);
                sm
            },
            pinch_start_mid_cx: Cell::new(0.0),
            pinch_start_mid_cy: Cell::new(0.0),
            window_size: Size::new(1280.0, 720.0),
            open_settings_windows: std::collections::HashSet::new(),
            settings_section: SettingsSection::Application,
            settings_feedback: None,
            main_window_id: None,
            trail_color_rgb: Cell::new((1.0, 1.0, 1.0)),
            config_dirty: Cell::new(false),
            last_frame_tick_time: Cell::new(Instant::now()),
            show_oobe,
            oobe_step: 0,
            oobe_windows: std::collections::HashSet::new(),
            dev_click_count: 0,
        }
    }
}

impl App {
    fn hex_to_f32_rgb(hex: &str) -> (f32, f32, f32) {
        let hex = hex.trim_start_matches('#');
        if hex.len() != 6 {
            return (1.0, 1.0, 1.0);
        }
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(255) as f32 / 255.0;
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(255) as f32 / 255.0;
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(255) as f32 / 255.0;
        (r, g, b)
    }

    fn get_pen_color(&self) -> String {
        self.config
            .canvas
            .pen_colors
            .get(self.selected_color)
            .cloned()
            .unwrap_or_else(|| "#ef4444".into())
    }

    fn get_pen_width(&self) -> f32 {
        let base = self
            .config
            .canvas
            .pen_size_presets
            .get(self.selected_size)
            .copied()
            .unwrap_or(5.0);
        base / self.live.transform.scale.get()
    }

    fn screen_to_canvas(&self, screen_x: f32, screen_y: f32) -> (f32, f32) {
        ScreenTransform::new(
            self.live.transform.offset_x.get(),
            self.live.transform.offset_y.get(),
            self.live.transform.scale.get(),
        )
        .screen_to_canvas(screen_x, screen_y)
    }

    fn send_camera_command(&mut self, cmd: CameraCommand) {
        if let Some(sender) = &self.camera_sender {
            sender.send(cmd);
        }
    }

    fn start_camera(&mut self) {
        let frame_interval_ms =
            viewframe::grid::display_period(&self.config.canvas.frame_rate_mode).as_millis() as u64;
        self.send_camera_command(CameraCommand::Start {
            index: self.config.source.camera_index,
            width: self.config.source.camera_width,
            height: self.config.source.camera_height,
            frame_interval_ms,
        });
    }

    fn restart_camera(&mut self) {
        if !self.showing_image {
            self.send_camera_command(CameraCommand::Stop);
            self.start_camera();
        }
    }

    fn persist_config(&mut self) {
        if let Err(error) = self.config.save() {
            self.settings_feedback = Some(format!("保存失败：{error}"));
        }
    }

    /// 配置脏标记落盘：滑块等高频消息只标脏内存，释放/窗口关闭时统一写入，
    /// 避免 JSON 序列化+写盘跟随每个拖动 tick（状态驱动持久化）。
    fn flush_config_if_dirty(&mut self) {
        if self.config_dirty.replace(false) {
            self.persist_config();
        }
    }

    fn alloc_stroke_id(&self) -> u64 {
        let id = self.next_stroke_id.get();
        self.next_stroke_id.set(id + 1);
        id
    }

    /// 将完成的笔画加入列表，并提交到后台计算扣除橡皮擦后的网格
    fn finalize_stroke(&mut self, stroke: DrawStroke) {
        let stroke_id = stroke.id;
        self.history.push(DrawCommand::Draw {
            stroke: stroke.clone(),
        });
        // 立即用「未扣几何」兜底插入缓存（下一帧即可渲染，避免闪一帧），
        // 后台线程返回更精确的扣除几何后再替换。
        if let Some(uncut) = viewink::ink::mesh::tessellate_stroke_canvas(&stroke, &[], None) {
            self.stroke_mesh_cache.borrow_mut().insert(
                stroke_id,
                viewgfx::CachedMesh::new(uncut, self.eraser_generation.get()),
            );
            self.touch_mesh_cache();
        }
        // 后台线程需要一份只读副本（它要重算扣除版几何，无法复用已插入缓存的
        // 兜底几何）。每笔一次，非帧热路径，保持直接克隆。
        let stroke_for_compute = stroke.clone();
        // 橡皮擦集快照：`Arc` 克隆（**零深拷贝**）。旧实现是
        // `Arc::new(self.eraser_strokes.clone())`——深拷贝发生在包进 Arc
        // 之前，所以每个任务都拿到一份独立的完整拷贝，Arc 一点忙都没帮上。
        // 成本随橡皮擦数线性增长、触发频率却是每写一笔；用过 50 次橡皮擦
        // （每条 500 点）时，每晾干一笔就是主线程上 200 次堆分配 + 1 MB
        // 拷贝。改为向 `EraserSet` 索取共享快照后降为一次原子自增。
        let erasers = self.erasers.shared();
        self.strokes.push(stroke);

        if let Some(sender) = &self.stroke_compute_sender {
            sender.send(StrokeComputeTask {
                stroke_id,
                stroke: stroke_for_compute,
                erasers,
                eraser_generation: self.eraser_generation.get(),
            });
        }
    }

    /// 橡皮擦完成：作为持久化批注存储，并把相交的已缓存笔画 targeted rebuild
    /// 任务发到后台线程（不再主线程 tessellate）。代数自增丢弃过期结果。
    fn finalize_eraser(&mut self, eraser_stroke: DrawStroke) {
        let eraser_id = eraser_stroke.id;
        let eraser_bounds = eraser_stroke.bounds;
        // undo 所需的「旧橡皮擦集」快照：**先取共享快照再 push**——零深拷贝。
        // `EraserSet::push` 是整体替换权威列表，所以这份已交出去的 Arc 快照
        // 此后永不被改动，可安全入撤销栈。
        let saved_erasers = self.erasers.shared();
        self.erasers.push(eraser_stroke.clone());
        self.history.push(DrawCommand::Erase {
            eraser_stroke,
            saved_erasers,
        });
        self.eraser_generation.set(self.eraser_generation.get() + 1);

        // 本轮 targeted rebuild 任务共享同一份快照：Arc 只克隆引用计数。
        let erasers = self.erasers.shared();
        let new_generation = self.eraser_generation.get();

        // targeted rebuild：相交且 id 更小的绘制笔画发到后台线程重算
        let eb = eraser_bounds;
        if let Some(sender) = &self.stroke_compute_sender {
            for stroke in &self.strokes {
                if stroke.id < eraser_id
                    && stroke.bounds.intersects(
                        eb.min_x,
                        eb.min_y,
                        eb.max_x - eb.min_x,
                        eb.max_y - eb.min_y,
                    )
                {
                    sender.send(StrokeComputeTask {
                        stroke_id: stroke.id,
                        stroke: stroke.clone(),
                        erasers: Arc::clone(&erasers),
                        eraser_generation: new_generation,
                    });
                }
            }
        }
    }

    /// 从当前 App 字段构建 SourceState
    fn capture_source_state(&self) -> SourceState {
        SourceState {
            strokes: self.strokes.clone(),
            // 共享快照而非深拷贝：切源期间可能有后台任务持有这份快照，
            // `EraserSet` 的写时复制保证它不会被后续变更改动。
            eraser_strokes: self.erasers.shared(),
            scale: self.live.transform.scale.get(),
            offset_x: self.live.transform.offset_x.get(),
            offset_y: self.live.transform.offset_y.get(),
            current_image_index: self.current_image_index,
            showing_image: self.showing_image,
            history: self.history.clone(),
            timestamp: 0,
        }
    }

    /// 用 SourceState 恢复 App 字段
    fn apply_source_state(&mut self, state: SourceState) {
        self.strokes = state.strokes;
        // `replace` 顺带刷新共享快照，后台线程下一次取到的是恢复后的集合。
        // 不动 `eraser_generation`：切源本来就会走 `rebuild_stroke_mesh_cache`
        // 全量重算，代数语义保持不变。
        self.erasers.replace(state.eraser_strokes.as_ref().clone());
        self.live.transform.scale.set(state.scale);
        self.live.transform.offset_x.set(state.offset_x);
        self.live.transform.offset_y.set(state.offset_y);        self.current_image_index = state.current_image_index;
        self.showing_image = state.showing_image;
        self.history = state.history;
        self.rebuild_stroke_mesh_cache();
    }

    /// 全量重建逐笔画几何缓存（Clear/Snapshot 撤销、切换源恢复时调用）。
    fn rebuild_stroke_mesh_cache(&mut self) {
        let generation = self.eraser_generation.get();
        // 橡皮擦空间索引建一次、被全部笔画复用：旧实现每条笔画都从零重建
        // 一遍索引 → O(笔画数 × 橡皮擦数 × 橡皮擦点数)，索引构建反客为主。
        let index = viewink::ink::mesh::build_eraser_index(self.erasers.as_slice(), None);
        let mut cache = self.stroke_mesh_cache.borrow_mut();
        cache.clear();
        for stroke in &self.strokes {
            if let Some(mesh) =
                viewink::ink::mesh::tessellate_stroke_indexed(stroke, &index)
            {
                cache.insert(stroke.id, viewgfx::CachedMesh::new(mesh, generation));
            }
        }
        drop(cache);
        self.touch_mesh_cache();
    }

    /// 缓存内容变化后递增代数（view() 的 O(1) 失效判断依据）。
    fn touch_mesh_cache(&self) {
        self.mesh_cache_generation
            .set(self.mesh_cache_generation.get() + 1);
    }

    /// 笔画集已清空时的缓存清理（无可渲染对象，无需任何重网格化）。
    fn clear_stroke_mesh_cache(&mut self) {
        self.stroke_mesh_cache.borrow_mut().clear();
        self.touch_mesh_cache();
    }

    /// 橡皮擦被撤销后定向重建：仅 id 更小且与其 bounds 相交的笔画，
    /// 用恢复后的橡皮擦集重算缓存（替代全量重网格化）。
    fn retessellate_affected_by_eraser(&mut self, eraser: &DrawStroke) {
        let generation = self.eraser_generation.get();
        let eb = eraser.bounds;
        let intersects = |s: &DrawStroke| {
            s.id < eraser.id
                && s.bounds.intersects(
                    eb.min_x,
                    eb.min_y,
                    eb.max_x - eb.min_x,
                    eb.max_y - eb.min_y,
                )
        };
        // 无相交笔画直接早退：省掉索引构建与缓存借用（旧实现先克隆整个橡皮擦集
        // 再判空，橡皮擦多时这笔克隆纯属浪费）。
        if !self.strokes.iter().any(intersects) {
            return;
        }
        // 橡皮擦集已恢复，索引建一次、被全部相交笔画复用。
        let index = viewink::ink::mesh::build_eraser_index(self.erasers.as_slice(), None);
        // 单趟完成判定与重算：旧实现先收集 `affected: Vec<u64>` 再用
        // `contains` 过滤，第二次遍历是 O(笔画数 × 相交数) 的线性查找。
        let mut cache = self.stroke_mesh_cache.borrow_mut();
        for stroke in self.strokes.iter().filter(|s| intersects(s)) {
            if let Some(mesh) =
                viewink::ink::mesh::tessellate_stroke_indexed(stroke, &index)
            {
                cache.insert(stroke.id, viewgfx::CachedMesh::new(mesh, generation));
            }
        }
        drop(cache);
        self.touch_mesh_cache();
    }

    /// 保存当前源、切换到目标源 id；如果目标源有缓存状态则恢复
    fn switch_source(&mut self, id: SourceId) {
        self.source_manager
            .save_current(self.capture_source_state());
        if let Some(prev) = self.source_manager.switch_to(id) {
            self.apply_source_state(prev);
        } else {
            // 首次进入该源，重置为默认
            self.strokes.clear();
            self.erasers.clear();
            self.live.transform.scale.set(1.0);
            self.live.transform.offset_x.set(0.0);
            self.live.transform.offset_y.set(0.0);
            self.current_image_index = None;
            self.showing_image = false;
            self.history.clear();
            self.clear_stroke_mesh_cache();
        }
    }

    /// 切换到 camera 源，清除图像覆盖层，启动摄像头
    fn enter_camera(&mut self) {
        self.switch_source(SourceId::Camera);
        self.showing_image = false;
        self.current_image_index = None;
        self.frozen = false;
        self.background_image = None;
        self.start_camera();
    }

    /// 切换到指定索引的图片源，显示该图片，关闭摄像头。
    ///
    /// 图片身份 = 其 rgba Arc 指针：同图重入时代数不变 → 渲染管线
    /// 代数门控命中，零重上传、零纹理重建；批注层独立刷新，纯叠加。
    fn enter_image(&mut self, index: usize) {
        if self.showing_image && self.current_image_index == Some(index) {
            return; // 同图重入：信号源层已就绪，直接复用
        }
        let (pic_id, rgba, w, h) = {
            let entry = self.image_list.get(index).expect("invalid image index");
            (
                entry.source_id.clone(),
                Arc::clone(&entry.rgba),
                entry.width,
                entry.height,
            )
        };
        if let Some(ref id) = pic_id {
            self.switch_source(id.clone());
        }
        self.current_image_index = Some(index);
        self.showing_image = true;
        self.frame.camera().clear();
        // 图片身份 = Arc 指针（列表全程持有该 Arc，指针稳定无 ABA）
        let generation = Arc::as_ptr(&rgba) as usize as u64;
        self.background_image = Some(Arc::new(viewcamera::camera::CameraFrameData {
            pixels: viewcamera::camera::FramePixels::Rgba(rgba),
            width: w,
            height: h,
            generation,
        }));
        self.send_camera_command(CameraCommand::Stop);
    }


    // ==============================
    // 共享绘制助手：统一鼠标/触控的笔刷/橡皮擦启动与点追加逻辑
    // (消除 CanvasPressed/Moved 与 RawTouchStart/Move 两处 ~100 行重复)
    // ==============================

    fn begin_pen_stroke(&self, screen_x: f32, screen_y: f32) {
        let live = &self.live;
        let color = self.get_pen_color();
        let width = self.get_pen_width();
        let (cx, cy) = self.screen_to_canvas(screen_x, screen_y);
        let id = self.alloc_stroke_id();
        self.trail_color_rgb.set(Self::hex_to_f32_rgb(&color));
        live.pen_speed_buffer.set([0.0; 3]);
        live.pen_speed_count.set(0);
        live.pen_speed_write.set(0);
        live.pen_stroke_start_time.set(Some(Instant::now()));
        live.pen_last_move_time.set(Some(Instant::now()));
        live.pen_total_segments.set(1);
        let mut s = DrawStroke::new(
            color,
            width,
            id,
            self.config.canvas.pen_min_width_ratio,
            self.config.canvas.pen_tail_duration,
        );
        s.points.reserve(200);
        s.variable_widths.reserve(200);
        s.variable_widths.push(apply_start_taper(width, width, 0));
        s.segment_times = Some(vec![0.0]);
        s.add_point(cx, cy, cx, cy);
        *live.current_stroke.borrow_mut() = Some(s);
    }

    fn begin_eraser_stroke(&self, screen_x: f32, screen_y: f32) {
        let live = &self.live;
        let (cx, cy) = self.screen_to_canvas(screen_x, screen_y);
        let base_eraser_size = self.config.canvas.eraser_size / live.transform.scale.get();
        let id = self.alloc_stroke_id();
        let mut eraser_stroke = DrawStroke::new_eraser(base_eraser_size, id);
        eraser_stroke.points.reserve(200);
        eraser_stroke.variable_widths.reserve(200);
        eraser_stroke.add_point(cx, cy, cx, cy);
        eraser_stroke.variable_widths.push(base_eraser_size);
        *live.current_eraser_stroke.borrow_mut() = Some(eraser_stroke);
    }

    /// 橡皮擦悬停光标：屏幕坐标 → 画布坐标 + 半径（GPU 动态层消费）
    fn update_eraser_cursor(&self, screen_x: f32, screen_y: f32) {
        let (cx, cy) = self.screen_to_canvas(screen_x, screen_y);
        let scale = self.live.transform.scale.get();
        self.live.eraser_cursor.set(Some(viewink::live::EraserCursor {
            cx,
            cy,
            radius: self.config.canvas.eraser_size / 2.0 / scale,
            border: 2.0 / scale,
        }));
    }


    /// 渲染节流：把重绘请求翻译成 iced 的 `Action`。
    ///
    /// 决策本身在 [`viewframe::FrameDriver::paced_redraw`]——为什么是"跳 vsync"
    /// 而不是"定时器"、以及那道被删掉的「面板对齐闸门」为什么拦掉了 100% 的输入
    /// 重绘，都记在那里。这里的职责只有一件：把框架无关的 [`viewframe::Redraw`]
    /// 映射成本应用的 `Message` 类型。
    fn paced_redraw(&self) -> Action<Message> {
        match self.frame.paced_redraw(viewframe::RedrawInputs {
            stroke_rendering: self.is_stroke_rendering(),
            direct_manipulation: self.is_direct_manipulation(),
            annotation_fps_cap: self.config.canvas.annotation_fps_cap,
        }) {
            viewframe::Redraw::Now => Action::request_redraw(),
            viewframe::Redraw::At(at) => Action::request_redraw_at(at),
        }
    }

    /// 是否处于"直接操作"中：这些场合一律满 vsync，不受帧率上限约束。
    ///
    /// 书写与平移是连续跟手的交互，60fps 帧率上限会让笔尖/画布出现可见的
    /// 阶梯感；批注层的动态上传是 KB/s 级（增量管线），满刷新率的 GPU 增量
    /// 成本可忽略。
    fn is_direct_manipulation(&self) -> bool {
        // 书写 / 橡皮擦**不再豁免**帧率上限：GPU 占用率是第一约束，宁可要
        // 一个稳定的 60-72fps 也不要 144fps 的满刷新率把占用率推到 2 倍。
        // 由此引入的额外延迟由笔尖外推补偿（见 `effective_frame_period_ms`），
        // 两处改动是配套的，不能只回退其中一个。
        //
        // 平移 / 惯性 / 触摸仍然豁免：它们没有"笔尖"可外推，且平移的
        // GPU 成本与书写不可比（无增量镶嵌、无光标外推）。
        self.momentum.borrow().is_active()
            // 鼠标左键按住（平移中）：由 `Message::CanvasPressed` 维护。平移
            // 同样是逐帧重建的连续交互，不能被限帧。
            || self.mouse_left_pressed.get()
    }

    /// 书写 / 橡皮擦轨迹期间是否在重建动态几何（走 GPU 增量管路）。
    ///
    /// 与 `is_drawing` 的区别：后者是"笔已按下"，这里还包含橡皮擦悬停
    /// 圆环（它同样每帧重建动态几何、同样吃 GPU）。
    fn is_stroke_rendering(&self) -> bool {
        self.is_drawing.get() || self.draw_mode == DrawMode::Eraser
    }

    /// 当前**有效**的帧量化周期（毫秒）：帧内容实际上屏的间隔。
    ///
    /// 笔尖外推的预测时域。决策在
    /// [`viewframe::FrameDriver::effective_frame_period_ms`]：必须反映**节流后**
    /// 的节奏而不是面板刷新率——144Hz 面板上跳 1 个 vsync → 实际 72fps →
    /// 量化周期 13.9ms。若仍按面板的 6.9ms 外推，就只补了一半延迟，笔尖照样
    /// 落后指针。
    fn effective_frame_period_ms(&self) -> f32 {
        self.frame
            .effective_frame_period_ms(self.is_stroke_rendering())
    }

    /// 摄像头内容上屏（`canvas::Program::update` 的 `RedrawRequested` 分支）。
    ///
    /// 把**摄像头内容的上屏时刻**钉在一个绝对网格上。网格的动机、均匀性保证
    /// 与延迟代价都记在 [`viewframe::CameraGrid`]——这里只负责把「下一个格点
    /// 时刻」翻译成本应用的 `Action`。
    fn step_camera_grid(&self) -> Option<Action<Message>> {
        self.frame
            .step_camera_grid(self.frozen, &self.config.canvas.frame_rate_mode)
            .map(Action::request_redraw_at)
    }

    /// 惯性积分 + 续帧请求。**由重绘帧自身驱动**（见
    /// `canvas::Program::update` 的 RedrawRequested 分支），不再依赖
    /// `iced::time::every` 定时器。
    ///
    /// 旧实现用固定 16ms 定时器发 `FrameTick`：该周期与 60Hz 的 16.667ms
    /// 存在拍频，每个拍周期里总有一帧落在 vsync 截止线之后 → 惯性动画
    /// 周期性地多等一帧，肉眼可见为"顿一下"。且每帧多一条
    /// `Message::FrameTick` 往返（消息 → update → 强制 view() 重建）。
    ///
    /// 改由渲染帧驱动后：① 积分时钟改用**相邻两帧的真实间隔**
    /// （`last_frame_tick_time` 现在每渲染帧更新一次，与渲染节拍同源，
    /// 比独立定时器更准）；② 每帧锁定 vsync，惯性连续；③ 少一条消息、
    /// 少一次 widget 树重建。惯性停止时不再请求重绘，帧流自然停住。
    ///
    /// 返回 `None` 表示惯性已停（本帧无重绘需求）。
    fn step_momentum(&self) -> Option<Action<Message>> {
        if !self.config.canvas.momentum_enabled {
            return None;
        }
        // 未按下手势的惯性不应该存在；start() 只在松手时调用，这里兜底
        if !self.momentum.borrow().is_active() {
            return None;
        }
        // dt = 相邻两渲染帧的真实间隔（上一帧由本函数末行的时间戳给出）。
        // 上限 0.1s 与原实现一致：窗口拖动/断点续等长间隔会把画面瞬移出去。
        let now = Instant::now();
        let dt = now
            .duration_since(self.last_frame_tick_time.get())
            .as_secs_f32()
            .min(0.1);
        self.last_frame_tick_time.set(now);
        if let Some((dx, dy)) = self.momentum.borrow_mut().tick(dt) {
            self.live.transform.offset_x.set(self.live.transform.offset_x.get() + dx);
            self.live.transform.offset_y.set(self.live.transform.offset_y.get() + dy);
        }
        viewink::stats::set_redraw_source(viewink::stats::RedrawSource::Momentum);
        Some(Action::request_redraw())
    }

    /// 向当前笔/橡皮擦追加点的方法已迁入 viewgfx::CanvasGfx
    /// （append_pen_point / append_eraser_point，增量镶嵌编排随迁）。
    ///
    /// **单点笔画（点按）必须入栈。** 原先这里要求 `points.len() >= 2`，
    /// 动机是"只有一个点不构成笔画"——但在**只有边界消息、没有位移流**的
    /// 输入设备上（实测：本机触摸屏只投递 `WM_POINTERDOWN`/`UP`，
    /// `WM_POINTERUPDATE` 与 `WM_TOUCH*` 均为 0），一次交互**永远只有 1 个
    /// 点**，于是每一笔都在这里被丢掉：用户看到的是"触控完全没反应"。
    ///
    /// 单点不会画不出东西：`mesh::build_path` 对 n=1 产出 `[P→P]` 的退化
    /// 路径，胶囊扩展后就是一个直径 = 笔宽的圆点。笔锋（`apply_speed_taper`）
    /// 对 <2 点自行早退，不受影响。
    fn finalize_current_drawing(&mut self) {
        // 先固化笔尖外推点，再取笔画：`reset_current_stroke` 会把
        // `tip_predict` 清零，晚一步笔尾就会朝真实笔尖回缩（"笔画闪一下"）。
        self.gfx.commit_tip_prediction();
        let stroke = self.live.current_stroke.borrow_mut().take();
        if let Some(mut stroke) = stroke {
            self.live.reset_current_stroke();
            if !stroke.points.is_empty() && !stroke.variable_widths.is_empty() {
                let mut sw = stroke.variable_widths.clone();
                if let Some(ref times) = stroke.segment_times {
                    if times.len() >= sw.len() {
                        viewink::ink::tessellator::apply_speed_taper(
                            &mut sw,
                            &stroke.points,
                            stroke.line_width,
                            times,
                            stroke.pen_min_width_ratio,
                            stroke.pen_tail_duration,
                        );
                    }
                }
                stroke.stored_widths = Some(sw);
            }
            self.finalize_stroke(stroke);
        }
        let eraser_stroke = self.live.current_eraser_stroke.borrow_mut().take();
        if let Some(eraser_stroke) = eraser_stroke {
            self.live.reset_current_eraser();
            self.finalize_eraser(eraser_stroke);
        }
        self.is_drawing.set(false);
    }

    /// 触控事件（iced 原生 `Event::Touch`）→ 起笔 / 追加点 / 收笔。
    ///
    /// 与鼠标路径同构，差异只在事件来源。`FingerLost`（系统取消）按抬起处理，
    /// 否则 `is_drawing` 永远为真，后续输入全部错位。
    ///
    /// 收笔走 [`Message::CanvasReleased`]：finalize 需要 `&mut self`，而
    /// `canvas::Program::update` 只有 `&self`，复用现成消息即可，不必再造一条
    /// 延迟通道。
    /// 记录一次输入事件（供"输入→帧起点"延迟计量）。
    fn mark_input(&self) {
        self.frame.metrics().mark_input();
    }

fn on_touch(&self, touch: &iced::touch::Event, position: Point) -> Option<Action<Message>> {
        viewink::stats::add_touch_event();
        self.mark_input();
        match *touch {
            iced::touch::Event::FingerPressed { .. } => {
                if self.menu_visible.get() {
                    self.menu_visible.set(false);
                }
                match self.draw_mode {
                    DrawMode::Comment => {
                        self.is_drawing.set(true);
                        viewink::stats::add_touch_stroke_started();
                        self.begin_pen_stroke(position.x, position.y);
                    }
                    DrawMode::Eraser => {
                        self.is_drawing.set(true);
                        viewink::stats::add_touch_stroke_started();
                        self.begin_eraser_stroke(position.x, position.y);
                    }
                    DrawMode::Move => {
                        self.last_pan_delta.set((0.0, 0.0));
                    }
                }
                self.last_mouse_pos.set(Some((position.x as f64, position.y as f64)));
                Some(self.paced_redraw())
            }
            iced::touch::Event::FingerMoved { .. } => {
                let (x, y) = (position.x, position.y);
                if self.is_drawing.get() {
                    let (cx, cy) = self.screen_to_canvas(x, y);
                    if self.live.current_stroke.borrow().is_some() {
                        self.gfx.append_pen_point(cx, cy);
                    } else if self.live.current_eraser_stroke.borrow().is_some() {
                        self.gfx
                            .append_eraser_point(self.config.canvas.eraser_size, cx, cy);
                    }
                    return Some(self.paced_redraw());
                }
                match self.draw_mode {
                    DrawMode::Move => {
                        if let Some((px, py)) = self.last_mouse_pos.get() {
                            let (dx, dy) = (x as f64 - px, y as f64 - py);
                            self.live.transform.offset_x.set(
                                self.live.transform.offset_x.get() + dx as f32,
                            );
                            self.live.transform.offset_y.set(
                                self.live.transform.offset_y.get() + dy as f32,
                            );
                            self.last_pan_delta.set((dx, dy));
                        }
                        self.last_mouse_pos.set(Some((x as f64, y as f64)));
                        return Some(self.paced_redraw());
                    }
                    DrawMode::Eraser => {
                        self.update_eraser_cursor(x, y);
                        return Some(self.paced_redraw());
                    }
                    DrawMode::Comment => {}
                }
                None
            }
            iced::touch::Event::FingerLifted { .. } | iced::touch::Event::FingerLost { .. } => {
                Some(Action::publish(Message::CanvasReleased))
            }
        }
    }


    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::CameraReady(sender) => {
                self.camera_sender = Some(sender);
                self.start_camera();
            }
            Message::CameraEvent(event) => {
                match event {
                    CameraEvent::Frame(frame) => {
                        // `frozen` 在拍照后置位（相机线程同时转入冻结，不再推帧）；
                        // 这里兜底丢弃已经在途的迟到帧，避免一次无谓的纹理上传
                        // 与全帧重绘驱动。
                        viewink::stats::note_camera_frame(frame.width, frame.height);
                        if !self.frozen {
                            // **只入待上屏槽位，不直接上屏。** 帧序号（generation）
                            // 必须在这里盖上：上传侧（`viewgfx::update_camera_params`）
                            // 只靠它判断"有没有新帧"。曾经在重构时把它删掉过，结果
                            // 所有帧都带 0，`update_camera_params` 认为内容没变，
                            // 于是**一次都不上传**（实测 `up=0B/s`，画面全黑）。
                            //
                            // 帧载荷已是 viewcamera 类型（Arc 包装在采集线程完成），
                            // 主线程仅盖一个序号——零像素转换、零再包装。上屏由
                            // `step_camera_grid` 的固定网格决定（见
                            // `viewframe::CameraGrid::submit` 的文档）。
                            self.frame.camera().submit(frame);
                        }
                    }
                    CameraEvent::Captured(frame) => {
                        // 拍照帧已由采集线程统一为 RGBA（NV12 仅此一次 CPU 转换），
                        // Arc 已在采集线程包装，此处直接接管
                        let w = frame.width;
                        let h = frame.height;
                        let rgba_arc = match frame.pixels {
                            viewcamera::camera::FramePixels::Rgba(v) => v,
                            viewcamera::camera::FramePixels::Nv12 { .. } => unreachable!(
                                "captured frame is always converted to RGBA on capture thread"
                            ),
                        };
                        self.frozen = true;
                        self.draw_mode = DrawMode::Comment;
                        self.active_tool = ToolButton::Comment;
                        self.sidebar_visible = true;

                        // 拍照帧**立即**上屏，不走上屏网格——画面已冻结，用户在等结果。
                        // 帧序号由 CameraGrid 分配（与槽位内容同代写入）。
                        self.frame.camera().freeze_with(
                            viewcamera::camera::CameraFrameData {
                                pixels: viewcamera::camera::FramePixels::Rgba(Arc::clone(&rgba_arc)),
                                width: w,
                                height: h,
                                generation: 0,
                            },
                        );

                        // PNG 编码+写盘移到后台线程（消除主线程多毫秒卡顿）；
                        // 文件名主线程预生成，列表立即更新不等磁盘写入
                        let save_target =
                            calc_save_path("photo", "png", self.config.storage.picture_directory.as_deref());
                        match save_target {
                            Ok((save_path, _)) => {
                                let rgba_for_save = Arc::clone(&rgba_arc);
                                std::thread::spawn(move || {
                                    match ::image::save_buffer(
                                        &save_path,
                                        &rgba_for_save[..],
                                        w,
                                        h,
                                        ::image::ColorType::Rgba8,
                                    ) {
                                        Ok(()) => {
                                            log::info!("Photo saved: {}", save_path.display())
                                        }
                                        Err(e) => log::error!("Failed to save photo: {}", e),
                                    }
                                });
                                let entry =
                                    load_image_from_rgba((*rgba_arc).clone(), w, h);
                                self.image_list.push(entry);
                            }
                            Err(e) => {
                                log::error!("Failed to create save path: {}", e);
                                let entry = load_image_from_rgba((*rgba_arc).clone(), w, h);
                                self.image_list.push(entry);
                            }
                        }
                    }
                }
            }
            Message::StrokeComputeReady(sender) => {
                self.stroke_compute_sender = Some(sender);
            }
            Message::StrokeComputed(result) => {
                // 主线程已在 finalize_stroke 里插入未扣几何兜底；
                // 后台的扣除几何替换兜底（erasure 更精确的版本）
                let mut touched = false;
                {
                    let mut cache = self.stroke_mesh_cache.borrow_mut();
                    if let Some(geometry) = result.geometry {
                        let stale = cache
                            .get(&result.stroke_id)
                            .is_some_and(|c| c.eraser_generation > result.eraser_generation);
                        if !stale && self.strokes.iter().any(|s| s.id == result.stroke_id) {
                            cache.insert(
                                result.stroke_id,
                                viewgfx::CachedMesh::new(geometry, result.eraser_generation),
                            );
                            touched = true;
                        }
                    }
                }
                if touched {
                    self.touch_mesh_cache();
                }
                // 几何换新必须重画：静态层要按新段表重新上传 + 剔除。
                viewink::stats::set_redraw_source(
                    viewink::stats::RedrawSource::StrokeComputed,
                );
            }
            Message::ToolClicked(tool) => {
                let was_same_tool = self.active_tool == tool;
                self.active_tool = tool;
                match tool {
                    // Menu: 切换菜单弹窗（1:1 from main_handle_menu_toggle）
                    ToolButton::Menu => {
                        self.menu_visible.set(!self.menu_visible.get());
                    }
                    // Minimize: 最小化窗口
                    ToolButton::Minimize => {
                        self.menu_visible.set(false);
                        if let Some(id) = self.main_window_id {
                            return iced::window::minimize(id, true)
                                .map(|_: ()| Message::WindowClosed);
                        }
                    }
                    // Move: 切换到移动模式（不改变当前信号源）
                    ToolButton::Move => {
                        self.draw_mode = DrawMode::Move;
                        self.pen_panel_visible = false;
                        self.menu_visible.set(false);
                        *self.live.current_eraser_stroke.borrow_mut() = None;
                        self.live.eraser_cursor.set(None);
                    }
                    // Comment: 首次切换模式，再次点击显隐面板（1:1 from btnComment click）
                    ToolButton::Comment => {
                        self.draw_mode = DrawMode::Comment;
                        self.pen_panel_visible = if was_same_tool {
                            !self.pen_panel_visible
                        } else {
                            false
                        };
                        self.menu_visible.set(false);
                        *self.live.current_eraser_stroke.borrow_mut() = None;
                        self.live.eraser_cursor.set(None);
                    }
                    // Eraser: 首次切换模式，再次点击显隐面板（1:1 from btnEraser click）
                    ToolButton::Eraser => {
                        self.draw_mode = DrawMode::Eraser;
                        self.pen_panel_visible = if was_same_tool {
                            !self.pen_panel_visible
                        } else {
                            false
                        };
                        self.menu_visible.set(false);
                    }
                    // Undo: 撤销（1:1 from btnUndo click → main_handle_undo）
                    // 定向缓存失效：Draw 撤销 O(1) 移除、Erase 撤销仅重建相交笔画，
                    // 替代全量重网格化（笔画多时主线程卡顿明显）
                    ToolButton::Undo => {
                        self.pen_panel_visible = false;
                        self.menu_visible.set(false);
                        // `history.undo` 已就地改写权威列表（strokes /
                        // erasers，`EraserSet` 内部已同步刷新共享快照）；
                        // 此处只做渲染缓存的**定向**失效：
                        // Draw = O(1) 移除一项，Erase = 仅重建相交笔画。
                        match self
                            .history
                            .undo(&mut self.strokes, &mut self.erasers)
                        {
                            Some(UndoneCommand::Draw(id)) => {
                                if self.stroke_mesh_cache.borrow_mut().remove(&id).is_some() {
                                    self.touch_mesh_cache();
                                }
                            }
                            Some(UndoneCommand::Erase(eraser)) => {
                                self.retessellate_affected_by_eraser(&eraser);
                            }
                            None => {}
                        }
                    }
                    // Photo: 在 camera 模式下拍照；在 image 模式下切回 camera（不拍照）
                    ToolButton::Photo => {
                        self.pen_panel_visible = false;
                        self.menu_visible.set(false);
                        self.sidebar_visible = true;
                        let was_in_image = self.showing_image;
                        self.enter_camera();
                        if !was_in_image {
                            self.send_camera_command(CameraCommand::Capture);
                        }
                    }
                    // Light: 展台灯 toggle（1:1 from btnLight click → main_handle_light_toggle）
                    ToolButton::Light => {
                        self.light_on = !self.light_on;
                        viewcamera::light::toggle(self.light_on);
                    }
                    // Settings: 切换设置面板（1:1 from btnSettings click → main_show_settings）
                    ToolButton::Settings => {
                        self.pen_panel_visible = false;
                        self.menu_visible.set(false);
                        self.settings_visible = !self.settings_visible;
                    }
                    // Image: 切换图片库侧栏（1:1 from btnExpand click → main_handle_sidebar_toggle）
                    ToolButton::Image => {
                        self.pen_panel_visible = false;
                        self.menu_visible.set(false);
                        self.sidebar_visible = !self.sidebar_visible;
                    }
                }
            }
            Message::ColorSelected(idx) => {
                self.selected_color = idx;
            }
            Message::SizeSelected(idx) => {
                self.selected_size = idx;
            }
            Message::EraserSizeSelected(idx) => {
                self.selected_eraser_size = idx;
                if let Some(&size) = self.config.canvas.eraser_size_presets.get(idx) {
                    self.config.canvas.eraser_size = size;
                }
                self.persist_config();
            }
            // 菜单"设置"按钮（1:1 from menuSettings click → main_show_settings_window）
            Message::MenuSettingsClicked => {
                self.menu_visible.set(false);
                let (id, task) = iced::window::open(iced::window::Settings {
                    size: iced::Size::new(960.0, 540.0),
                    position: iced::window::Position::Centered,
                    decorations: false,
                    ..Default::default()
                });
                self.open_settings_windows.insert(id);
                return task.map(Message::SettingsWindowOpened);
            }
            Message::SettingsSectionSelected(section) => {
                self.settings_section = section;
                self.settings_feedback = None;
            }
            Message::FrameRateModeSelected(mode) => {
                self.config.canvas.frame_rate_mode = mode;
                self.persist_config();
                self.restart_camera();
            }
            // 批注帧率上限：只改内存 + 标脏，滑块释放时统一落盘（避免每 tick 写盘）
            Message::AnnotationFpsCapChanged(fps) => {
                self.config.canvas.annotation_fps_cap = fps.clamp(30, 240);
                self.config_dirty.set(true);
            }
            Message::MomentumSettingToggled(enabled) => {
                self.config.canvas.momentum_enabled = enabled;
                if !enabled {
                    self.momentum.borrow_mut().stop();
                }
                self.persist_config();
            }
            // 笔锋参数：写 `canvas` 副本——`begin_pen_stroke` 读的就是这一份。
            // 旧实现误写 `developer` 副本，滑块拖到底笔锋也不变。
            Message::PenMinWidthRatioChanged(ratio) => {
                self.config.canvas.pen_min_width_ratio = ratio.clamp(0.05, 1.0);
                self.config_dirty.set(true);
            }
            Message::PenTailDurationChanged(ms) => {
                self.config.canvas.pen_tail_duration = ms.clamp(0.0, 400.0);
                self.config_dirty.set(true);
            }
            Message::PenSizePresetChanged(index, value) => {
                if let Some(slot) = self.config.canvas.pen_size_presets.get_mut(index) {
                    *slot = value.clamp(0.5, 200.0);
                    self.persist_config();
                }
            }
            Message::EraserSizePresetChanged(index, value) => {
                if let Some(slot) = self.config.canvas.eraser_size_presets.get_mut(index) {
                    *slot = value.clamp(0.5, 200.0);
                    self.persist_config();
                }
            }
            Message::CameraIndexChanged(index) => {
                self.config.source.camera_index = index as usize;
                self.persist_config();
                self.restart_camera();
            }
            Message::CameraResolutionSelected((width, height)) => {
                self.config.source.camera_width = width;
                self.config.source.camera_height = height;
                self.persist_config();
                self.restart_camera();
            }
            Message::DefaultRotationSelected(rotation) => {
                let rotation = rotation.rem_euclid(360);
                self.config.source.default_rotation = rotation;
                self.camera_rotation = rotation;
                self.persist_config();
            }
            Message::PictureDirectorySelected => {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("选择照片存储目录")
                    .pick_folder()
                {
                    self.config.storage.picture_directory = Some(path);
                    self.persist_config();
                }
            }
            Message::PresentModeSelected(mode) => {
                // 呈现模式在 `main()` 启动时读一次并写进 ICED_PRESENT_MODE，
                // 运行期改动必须重启才生效——设置页已明确标注。
                self.config.renderer.present_mode = mode;
                self.persist_config();
                self.settings_feedback = Some("呈现模式将在下次启动时生效".to_string());
            }
            Message::IntegratedGpuToggled(enabled) => {
                // 同上：WGPU_POWER_PREF 必须在创建 wgpu 实例前设置。
                self.config.renderer.prefer_integrated_gpu = enabled;
                self.persist_config();
                self.settings_feedback = Some("核显偏好将在下次启动时生效".to_string());
            }
            Message::ExportSettings => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("ViewStage 设置", &["json"])
                    .set_file_name("viewstage-settings.json")
                    .save_file()
                {
                    self.settings_feedback = Some(match self.config.export(&path) {
                        Ok(()) => "设置已导出".to_string(),
                        Err(error) => format!("导出失败：{error}"),
                    });
                }
            }
            Message::ImportSettings => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("ViewStage 设置", &["json"])
                    .pick_file()
                {
                    match Config::import(&path) {
                        Ok(config) => {
                            self.config = config;
                            self.camera_rotation = self.config.source.default_rotation;
                            self.persist_config();
                            self.restart_camera();
                            self.settings_feedback = Some("设置已导入并应用".to_string());
                        }
                        Err(error) => self.settings_feedback = Some(format!("导入失败：{error}")),
                    }
                }
            }
            Message::ResetAllSettings => {
                self.config = Config::default();
                self.camera_rotation = self.config.source.default_rotation;
                self.persist_config();
                self.restart_camera();
                self.settings_feedback = Some("全部设置已恢复默认值".to_string());
            }
            Message::SettingsWindowOpened(id) => {
                self.open_settings_windows.insert(id);
            }
            Message::SettingsWindowClosed(id) => {
                self.open_settings_windows.remove(&id);
                self.oobe_windows.remove(&id);
                // 设置窗口关闭时兜底落盘（滑块未释放就关窗的边界情况）
                self.flush_config_if_dirty();
            }
            Message::SettingsWindowDragRequested(id) => {
                if self.open_settings_windows.contains(&id) {
                    return iced::window::drag(id);
                }
            }
            Message::SettingsWindowCloseRequested(id) => {
                if self.open_settings_windows.contains(&id) {
                    return iced::window::close(id).map(|_: ()| Message::WindowClosed);
                }
            }
            // 菜单"关闭"按钮
            Message::MenuCloseClicked => {
                self.menu_visible.set(false);
                if let Some(id) = self.main_window_id {
                    return iced::window::close(id).map(|_: ()| Message::WindowClosed);
                }
            }
            Message::WindowClosed => {}
            Message::WindowResized(_id, size) => {
                self.window_size = size;
            }
            Message::MainWindowOpened(id) => {
                // open_events 对每个窗口（含设置/引导窗口）都会触发；
                // 仅当尚未记录主窗口且该 id 不属于次级窗口时，才记录为主窗口。
                if self.main_window_id.is_none()
                    && !self.open_settings_windows.contains(&id)
                    && !self.oobe_windows.contains(&id)
                {
                    self.main_window_id = Some(id);
                }
            }
            Message::DevClickIncremented => {
                if self.config.developer.developer_mode {
                    // 已解锁时点击关闭
                    self.config.developer.developer_mode = false;
                    self.dev_click_count = 0;
                    self.persist_config();
                } else {
                    self.dev_click_count += 1;
                    if self.dev_click_count >= 5 {
                        self.config.developer.developer_mode = true;
                        self.dev_click_count = 0;
                        self.persist_config();
                    }
                }
            }
            Message::PersistDeveloperConfig => {
                self.flush_config_if_dirty();
            }
            Message::PerfMonitorToggled(enabled) => {
                self.config.developer.perf_monitor_enabled = enabled;
                self.persist_config();
            }
            Message::OobeAction(action) => {
                match action {
                    OobeAction::FrameRateModeSelected(mode) => {
                        self.config.canvas.frame_rate_mode = mode;
                    }
                    OobeAction::AnnotationFpsCapChanged(fps) => {
                        self.config.canvas.annotation_fps_cap = fps.clamp(30, 240);
                    }
                    OobeAction::CameraIndexChanged(index) => {
                        self.config.source.camera_index = index as usize;
                    }
                    OobeAction::CameraResolutionSelected((w, h)) => {
                        self.config.source.camera_width = w;
                        self.config.source.camera_height = h;
                    }
                    OobeAction::NextStep => {
                        if self.oobe_step < OOBE_LAST_STEP {
                            self.oobe_step += 1;
                        }
                    }
                    OobeAction::Complete => {
                        self.config.developer.first_run_completed = true;
                        self.persist_config();
                        self.show_oobe = false;
                        if let Some(id) = self.oobe_windows.iter().next().copied() {
                            return iced::window::close(id).map(|_: ()| Message::WindowClosed);
                        }
                    }
                    OobeAction::Skip => {
                        self.config.developer.first_run_completed = true;
                        self.persist_config();
                        self.show_oobe = false;
                        if let Some(id) = self.oobe_windows.iter().next().copied() {
                            return iced::window::close(id).map(|_: ()| Message::WindowClosed);
                        }
                    }
                }
                self.persist_config();
            }
            Message::OobeWindowOpened(id) => {
                self.oobe_windows.insert(id);
            }
            Message::EraserClearSliderChanged(val) => {
                if val >= 95.0 {
                    self.strokes.clear();
                    self.history.clear();
                    self.clear_stroke_mesh_cache();
                    self.eraser_clear_slider = 0.0;
                } else {
                    self.eraser_clear_slider = val;
                }
            }
            Message::EraserClearSliderReleased => {
                self.eraser_clear_slider = 0.0;
            }
            Message::RotateLeft => {
                self.camera_rotation = (self.camera_rotation - 90).rem_euclid(360);
            }
            Message::RotateRight => {
                self.camera_rotation = (self.camera_rotation + 90).rem_euclid(360);
            }
            Message::BrightnessChanged(val) => {
                self.camera_brightness = val;
            }
            Message::ContrastChanged(val) => {
                self.camera_contrast = val;
            }
            Message::GrayscaleToggled(val) => {
                self.camera_grayscale = val;
            }
            Message::ResetCameraSettings => {
                self.camera_brightness = 0.0;
                self.camera_contrast = 100.0;
                self.camera_grayscale = false;
                self.camera_rotation = 0;
            }
            Message::SidebarImageSelected(index) => {
                // 再次点击已选中的图片，返回摄像头
                if self.current_image_index == Some(index) {
                    self.enter_camera();
                    return iced::Task::none();
                }
                // 确保图片有 SourceId
                let entry = self.image_list.get_mut(index).expect("invalid image index");
                if entry.source_id.is_none() {
                    entry.source_id = Some(self.source_manager.alloc_pic_id());
                }
                self.enter_image(index);
            }
            Message::SidebarImageDelete(index) => {
                // 如果删除的是当前显示的图片，先保存其源状态
                if self.current_image_index == Some(index) {
                    self.source_manager
                        .save_current(self.capture_source_state());
                }
                self.image_list.remove(index);

                if self.current_image_index == Some(index) {
                    // 当前显示的图片被删除，切换到邻近或 camera
                    if index < self.image_list.len() {
                        // 切换到 shift 到该位置的下一张图片
                        self.enter_image(index);
                    } else if !self.image_list.is_empty() {
                        self.enter_image(self.image_list.len() - 1);
                    } else {
                        self.enter_camera();
                    }
                } else if let Some(cur) = self.current_image_index {
                    if index < cur {
                        self.current_image_index = Some(cur - 1);
                    }
                }
            }
            Message::SidebarImportImage => {
                let files = rfd::FileDialog::new()
                    .add_filter("图片", &["png", "jpg", "jpeg", "gif", "bmp", "webp"])
                    .set_title("导入图片")
                    .pick_files();
                if let Some(paths) = files {
                    for path in &paths {
                        match load_image_from_path(path) {
                            Ok(mut entry) => {
                                entry.source_id = Some(self.source_manager.alloc_pic_id());
                                self.image_list.push(entry);
                            }
                            Err(e) => {
                                log::error!("Failed to import image {}: {}", path.display(), e);
                            }
                        }
                    }
                    if !self.image_list.is_empty() {
                        let idx = self.image_list.len() - 1;
                        self.enter_image(idx);
                    }
                }
            }
            Message::CanvasPressed(pos) => {
                self.mouse_left_pressed.set(true);
                if self.menu_visible.get() {
                    self.menu_visible.set(false);
                }
                if self.draw_mode == DrawMode::Comment {
                    self.is_drawing.set(true);
                    self.begin_pen_stroke(pos.x, pos.y);
                } else if self.draw_mode == DrawMode::Eraser {
                    self.is_drawing.set(true);
                    self.begin_eraser_stroke(pos.x, pos.y);
                }
                self.last_mouse_pos.set(Some((pos.x as f64, pos.y as f64)));
                self.momentum.borrow_mut().stop();
            }
            Message::CanvasReleased => {
                self.mouse_left_pressed.set(false);
                if self.is_drawing.get() {
                    self.finalize_current_drawing();
                } else if self.draw_mode == DrawMode::Move && self.config.canvas.momentum_enabled {
                    let (dx, dy) = self.last_pan_delta.get();
                    self.momentum.borrow_mut().start(dx as f32, dy as f32);
                }
                self.last_pan_delta.set((0.0, 0.0));
                self.last_mouse_pos.set(None);
            }
            Message::CanvasWheelScroll { delta, cursor } => {
                let zoom = if delta > 0.0 { 1.1 } else { 0.9 };
                let new_scale = (self.live.transform.scale.get() * zoom).clamp(0.1, 10.0);
                let world_x = (cursor.x - self.live.transform.offset_x.get()) / self.live.transform.scale.get();
                let world_y = (cursor.y - self.live.transform.offset_y.get()) / self.live.transform.scale.get();
                self.live.transform.offset_x.set(cursor.x - world_x * new_scale);
                self.live.transform.offset_y.set(cursor.y - world_y * new_scale);
                self.live.transform.scale.set(new_scale);
                // 缩放只需更新 GPU transformation，不需要重建 mesh
            }
        }

        Task::none()
    }

    // ---- 每帧图形编排（背景层构建 / 静态快照 / 增量镶嵌 / 触控轨迹）
    // 已整体迁入 viewgfx::CanvasGfx（camera_layer / annotation_layer），
    // 本体只负责收集输入并转调；性能预算见 viewgfx crate 文档。----

    /// 直通合成通道注册（一次性）。调用点为主窗口 canvas widget 的 draw
    /// 副作用——**主窗口专属路径**（OOBE/设置窗口无此 canvas，其 renderer
    /// 永远不会误抢）。注册与同帧 prepare 的接管发生在同一窗口 redraw
    /// 原子段内，其他窗口的 renderer 无插入接管的窗口期。
    ///
    /// 注册内容与顺序（白板底色 → 摄像头背景 → 批注笔迹）记在
    /// [`viewframe::channels`]。幂等由 `Channels` 自身保证。
    fn register_direct_channels_once(&self) {
        self.frame.channels().ensure_registered(&self.gfx);
    }

    fn view(&self) -> Element<'_, Message> {
        let start = Instant::now();
        let element = self.view_inner();
        viewink::stats::add_view(start.elapsed());
        element
    }

    fn view_inner(&self) -> Element<'_, Message> {
        // 底层：摄像头/图片背景（直通合成通道渲染，无 widget）。本方法仅
        // 提交参数快照（viewgfx dirty-cache：滤镜/帧未变时零重建）到共享槽，
        // 摄像头通道 prepare 阶段取走上传，合成序列直接画进 frame。
        // 诊断开关：VIEWSTAGE_NO_CAMERA=1 时跳过该层（分层定位 GPU 占用）。
        if !viewframe::channels::diag_no_camera() {
            self.gfx.update_camera_params(viewgfx::BackgroundInputs {
                camera_frame: self.frame.camera().current().as_ref(),
                background_image: self.background_image.as_ref(),
                brightness: self.camera_brightness,
                contrast: self.camera_contrast,
                grayscale: self.camera_grayscale,
                rotation: self.camera_rotation,
                fallback_dims: (
                    self.config.source.camera_width,
                    self.config.source.camera_height,
                ),
            });
        }

        // 批注层（直通合成通道渲染，无 widget）：每帧编排（静态快照同步 /
        // 增量镶嵌）全部写进共享 LiveCanvasState，批注通道 prepare 直读。
        let mesh_cache = self.stroke_mesh_cache.borrow();
        self.gfx.annotation_frame(
            viewgfx::FrameInputs {
                mesh_cache: &mesh_cache,
                mesh_generation: self.mesh_cache_generation.get(),
                touch_trail: None,
            },
            &mut |_out| {},
        );
        drop(mesh_cache);

        // 顶层：浮动 toolbar
        let toolbar_el = views::toolbar::view(self);
        let toolbar_floating = container(toolbar_el)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Bottom);

        // 设置面板浮动层（1:1 from .settings-panel: 动态定位在设置按钮正上方）
        let settings_floating = if self.settings_visible {
            let settings_el = views::settings::view(self);
            // 动态计算面板位置：设置按钮在中心组最右侧，中心组居中
            let panel_width: f32 = 400.0;
            let center_group_width: f32 = 394.0; // 8个按钮(40*8=320) + 分隔线(~14) + 8个spacing(2*8=16)
            let toolbar_content_width = self.window_size.width - 20.0;
            let center_group_left = (toolbar_content_width - center_group_width) / 2.0 + 10.0;
            let settings_btn_center = center_group_left + center_group_width - 20.0;
            let panel_left = (settings_btn_center - panel_width / 2.0).max(10.0);

            Some(
                container(
                    container(settings_el)
                        .width(panel_width)
                        .height(Length::Shrink),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Left)
                .align_y(Vertical::Bottom)
                .padding(Padding {
                    top: 0.0,
                    bottom: 82.0,
                    left: panel_left,
                    right: 0.0,
                }),
            )
        } else {
            None
        };

        // 画笔控制面板浮动层
        let pen_panel_floating = if self.pen_panel_visible {
            let panel = views::pen_panel::view(self);
            Some(
                container(panel)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(Horizontal::Center)
                    .align_y(Vertical::Bottom)
                    .padding(Padding {
                        top: 0.0,
                        bottom: 82.0,
                        left: 0.0,
                        right: 0.0,
                    }),
            )
        } else {
            None
        };

        // 图片侧栏浮动层
        let sidebar_floating = if self.sidebar_visible {
            let sidebar_el = views::sidebar::view(self);
            Some(
                container(sidebar_el)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(Horizontal::Right)
                    .align_y(Vertical::Top)
                    .padding(Padding {
                        top: 10.0,
                        bottom: 80.0,
                        left: 0.0,
                        right: 10.0,
                    }),
            )
        } else {
            None
        };

        // 菜单弹窗浮动层（1:1 from .menu-popup: absolute, bottom: 90px, left: 10px）
        let menu_floating = if self.menu_visible.get() {
            let menu_content = column![
                button(
                    row![
                        icons::icon("settings", 16.0, Color::from_rgb8(255, 255, 255)),
                        text("设置").size(13)
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                )
                .on_press(Message::MenuSettingsClicked)
                .padding([10, 16])
                .style(|_, status| {
                    let (bg, border_color) = match status {
                        button::Status::Hovered => {
                            (Color::from_rgb8(58, 58, 58), Color::from_rgb8(59, 130, 246))
                        }
                        _ => (Color::from_rgb8(42, 42, 42), Color::TRANSPARENT),
                    };
                    button::Style {
                        background: Some(bg.into()),
                        text_color: Color::from_rgb8(255, 255, 255),
                        border: Border {
                            radius: 8.0.into(),
                            width: 1.0,
                            color: border_color,
                        },
                        shadow: Shadow::default(),
                        snap: false,
                    }
                }),
                button(
                    row![icons::icon("close", 16.0, Color::from_rgb8(255, 255, 255)), text("关闭").size(13)]
                        .spacing(8)
                        .align_y(iced::Alignment::Center)
                )
                .on_press(Message::MenuCloseClicked)
                .padding([10, 16])
                .style(|_, status| {
                    let (bg, border_color) = match status {
                        button::Status::Hovered => {
                            (Color::from_rgb8(58, 58, 58), Color::from_rgb8(59, 130, 246))
                        }
                        _ => (Color::from_rgb8(42, 42, 42), Color::TRANSPARENT),
                    };
                    button::Style {
                        background: Some(bg.into()),
                        text_color: Color::from_rgb8(255, 255, 255),
                        border: Border {
                            radius: 8.0.into(),
                            width: 1.0,
                            color: border_color,
                        },
                        shadow: Shadow::default(),
                        snap: false,
                    }
                }),
            ]
            .spacing(4);

            Some(
                container(
                    container(menu_content)
                        .padding(8)
                        .style(|_| container::Style {
                            background: Some(Color::from_rgb8(26, 26, 26).into()),
                            border: Border {
                                radius: 12.0.into(),
                                width: 1.0,
                                color: Color::from_rgba8(255, 255, 255, 25.0 / 255.0),
                            },
                            shadow: Shadow {
                                color: Color::from_rgba8(0, 0, 0, 0.4),
                                offset: iced::Vector::new(0.0, 4.0),
                                blur_radius: 12.0,
                            },
                            ..container::Style::default()
                        })
                        .width(Length::Shrink)
                        .height(Length::Shrink),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Left)
                .align_y(Vertical::Bottom)
                .padding(Padding {
                    top: 0.0,
                    bottom: 90.0,
                    left: 10.0,
                    right: 0.0,
                }),
            )
        } else {
            None
        };

        // 橡皮擦光标提示已迁移至 GPU 动态层（annotation 管线按 live.eraser_cursor
        // 直接生成圆环几何）——悬停移动零消息刷新，不经过 UI widget 层。

        // 事件层（透明 canvas，捕获鼠标/触摸事件；渲染已迁移到独立批注管线）
        let event_eraser_layer = canvas(self).width(Length::Fill).height(Length::Fill);

        // 收集所有浮动层（摄像头/批注背景已由直通合成通道渲染，无 widget 层）
        let mut layers: Vec<Element<Message>> = Vec::with_capacity(1 + 4);
        layers.push(event_eraser_layer.into());

        if let Some(p) = pen_panel_floating {
            layers.push(p.into());
        }
        if let Some(s) = sidebar_floating {
            layers.push(s.into());
        }
        if let Some(m) = menu_floating {
            layers.push(m.into());
        }

        // 性能 HUD（开发者模式）：左上角显示 FPS/帧时间/内存/笔擦网格数量
        if self.config.developer.perf_monitor_enabled {
            // 采样在 `viewframe::FrameMetrics::sample_perf`（draw 路径，每帧），
            // 这里只读最近一次的窗口结果。
            let perf = self.frame.metrics().perf();
            let fps = perf.fps;
            let frame_ms = perf.frame_ms;
            let mem = perf.memory_mb;
            let stroke_count = self.strokes.len();
            let eraser_count = self.erasers.len();
            let mesh_count = self.stroke_mesh_cache.borrow().len();
            // 视口剔除效果：静态层实际绘制的笔画数 vs 总量
            let (visible, total) = viewink::stats::static_visible();
            // 驻留式上传：缓冲按整份文档增长（×64B 即显存），绘制按可见区间分批
            let (resident_segs, draw_ranges) = viewink::stats::static_resident();
            // 帧率稳定性：实测刷新周期 + 长帧数（>1.5 周期）+ 窗口内最坏帧。
            // `long > 0` 即表示存在肉眼可见的顿挫，是"稳定 60fps"的判定点。
            let (long_frames, worst_ms) = viewink::stats::frame_stability();
            // 帧预算：宿主在帧内可控的 CPU 开销（镶嵌 + 打包上传 + 剔除）
            // 对比实测刷新周期。逼近周期即必然出现长帧（错过 vsync 死线
            // = 整帧多等一个周期），这是"笔迹是否跟手"的唯一硬指标。
            let (prep_avg, prep_max, work_avg, work_max, refresh_ms) =
                viewink::stats::frame_budget();
            // 静态层离屏缓存是否命中（高密度文档才启用；0 = 密度不够或键在变）
            let raster = viewink::stats::raster_blit_used();
            // GPU 侧成本核算（估计值，非硬件计时）：墨迹片元 vs 全屏 blit 纹素。
            // 二者哪个大，决定了"还值不值得再往下压"。
            let (_, ink_frag, blit_texels, _) = viewink::stats::raster_cost();
            // 触控诊断：经框架原生 `Event::Touch` 真正起笔的次数（自定义 hook 管线已
            // 删除，"hook 装没装 / WM_POINTER / 入队 / drain"那组计数恒为 0，
            // 是误导性噪声，故不再显示）。
            let touch_strokes = viewink::stats::touch_strokes_started();
            let info = format!(
                "FPS {:.0} | 帧 {:.1} ms\n内存 {:.1} MB\n笔画 {} | 擦除 {} | 网格 {}\n\
                 静态可见 {}/{}\n驻留 {} 段 {:.1} MB | 绘制 {} 批\n\
                 刷新 {:.1} Hz | 长帧 {} | 最坏 {:.1} ms\n\
                 帧预算 {:.2}/{:.2} ms（{:.0}%）\n\
                 离屏缓存 {} | 墨迹 {:.2} Mpx | blit {:.2} Mpx\n\
                 触控起笔 {}",
                fps,
                frame_ms,
                mem,
                stroke_count,
                eraser_count,
                mesh_count,
                visible,
                total,
                resident_segs,
                resident_segs as f64 * 32.0 / (1024.0 * 1024.0),
                draw_ranges,
                1000.0 / viewink::stats::measured_refresh_ms().max(0.001),
                long_frames,
                worst_ms,
                work_avg + prep_avg,
                work_max + prep_max,
                if refresh_ms > 0.0 {
                    (work_avg + prep_avg) * 100.0 / refresh_ms
                } else {
                    0.0
                },
                if raster { "命中" } else { "未启用" },
                ink_frag as f64 / 1_000_000.0,
                blit_texels as f64 / 1_000_000.0,
                touch_strokes,
            );
            let hud_box = container(
                text(info)
                    .size(12)
                    .color(Color::from_rgba8(140, 240, 140, 1.0)),
            )
            .padding(6)
            .style(|_| container::Style {
                background: Some(Color::from_rgba8(0, 0, 0, 0.55).into()),
                ..container::Style::default()
            })
            .width(Length::Shrink)
            .height(Length::Shrink);
            let hud_el = container(hud_box)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::Start)
                .align_y(iced::Alignment::Start)
                .padding(iced::Padding {
                    top: 12.0,
                    left: 12.0,
                    right: 0.0,
                    bottom: 0.0,
                });
            layers.push(hud_el.into());
        }

        layers.push(toolbar_floating.into());

        if let Some(sf) = settings_floating {
            layers.push(sf.into());
        }

        let main_stack = stack(layers).width(Length::Fill).height(Length::Fill);

        // 白板底色由 frame 的 LoadOp::Clear 提供（见
        // register_direct_channels_once）——容器必须保持透明：这里的
        // 不透明背景 quad 属于 iced Scene，绘制在直通通道之上会把
        // 摄像头/批注整层盖住。
        container(main_stack)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        // 原先这里有一条 `iced::time::every(16ms)` 的 FrameTick 订阅（按需启停）
        // 专门驱动惯性积分。已删除：惯性改由渲染帧自身驱动（`step_momentum`
        // 挂在 RedrawRequested 零消息路径上）。
        //
        // 固定 16ms 定时器与 60Hz vsync 存在拍频（16 vs 16.667ms），每个拍
        // 周期必有一帧错过 vsync 截止线 → 惯性动画周期性顿帧；Windows 默认
        // 15.625ms 计时器粒度还会把 16ms 睡眠拉长到 31.25ms（惯性只有 32Hz）。
        // 改由帧驱动后惯性逐帧锁 vsync，且每帧少一条消息往返与一次
        // widget 树重建。
        Subscription::batch(vec![
            Subscription::run(camera_worker),
            Subscription::run(stroke_compute_worker),
            iced::window::close_events().map(Message::SettingsWindowClosed),
            iced::window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
            iced::window::open_events().map(Message::MainWindowOpened),
        ])
    }
}

// ==================
// Program trait 实现（支持多窗口）
// ==================

struct ViewStageApp;

impl iced::Program for ViewStageApp {
    type State = App;
    type Message = Message;
    type Theme = Theme;
    type Renderer = Renderer;
    type Executor = iced::executor::Default;

    fn name() -> &'static str {
        "ViewStage"
    }

    fn settings(&self) -> iced::Settings {
        // MSAA 关闭的理由（每帧 4 倍采样 + resolve 读回是大屏核显的填充带宽大头）
        // 记在 viewframe::boot::settings。
        viewframe::boot::settings()
    }

    fn window(&self) -> Option<iced::window::Settings> {
        Some(iced::window::Settings {
            size: iced::Size::new(1280.0, 720.0),
            decorations: false,
            fullscreen: true,
            ..Default::default()
        })
    }

    fn boot(&self) -> (App, Task<Message>) {
        let mut app = App::default();
        let mut tasks = Vec::new();
        if app.show_oobe {
            let (id, task) = iced::window::open(iced::window::Settings {
                size: iced::Size::new(600.0, 440.0),
                position: iced::window::Position::Centered,
                decorations: true,
                ..Default::default()
            });
            app.oobe_windows.insert(id);
            tasks.push(task.map(Message::OobeWindowOpened));
        }
        (app, Task::batch(tasks))
    }

    fn update(&self, state: &mut App, message: Message) -> Task<Message> {
        state.update(message)
    }

    fn view<'a>(&self, state: &'a App, window: iced::window::Id) -> Element<'a, Message> {
        if state.oobe_windows.contains(&window) {
            let oobe_el = views::oobe::view(state);
            container(oobe_el)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else if state.open_settings_windows.contains(&window) {
            // 设置窗口：独立的设置页面
            let settings_el = views::app_settings::view(state, window);
            container(settings_el)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            // 主窗口
            state.view()
        }
    }

    fn title(&self, state: &App, window: iced::window::Id) -> String {
        if state.oobe_windows.contains(&window) {
            "ViewStage - 首次引导".to_string()
        } else if state.open_settings_windows.contains(&window) {
            "ViewStage - 设置".to_string()
        } else {
            "ViewStage".to_string()
        }
    }

    fn subscription(&self, state: &App) -> Subscription<Message> {
        state.subscription()
    }
}

// ==================
// 摄像头后台工作线程
// ==================

// ==================
// 摄像头后台工作线程
// ==================

/// 取一帧实时预览：NV12 直传（零拷贝切片）优先，MJPEG-only 等设备回退 RGB 软件处理。
/// 帧载荷直接产 viewcamera 类型，Arc 包装在本线程（采集侧）完成——主线程零转换。
fn poll_live_frame(
    cam: &mut viewcamera::capture::CameraManager,
    cpu_processor: &mut viewcamera::gpu_processor::CpuFrameProcessor,
) -> Option<viewcamera::camera::CameraFrameData> {
    if cam.is_native_nv12() {
        let (data, w, h, capture_ts) = cam.poll_frame_nv12()?;
        // 源端节拍诊断：与"我们收到帧的间隔"分开量。
        // 两者一致 → 抖动来自设备/驱动；源端均匀而我们不均匀 → 我们丢了样本。
        if let Some(ts) = capture_ts {
            let prev = CAPTURE_SRC_LAST.with(|s| s.get());
            if let Some(prev) = prev {
                if ts > prev {
                    viewink::stats::note_camera_source_interval(ts - prev);
                }
            }
            CAPTURE_SRC_LAST.with(|s| s.set(Some(ts)));
        }
        // 尺寸校验失败时 poll_frame_nv12 已自动降级（下次走 RGB 路径）
        return viewcamera::gpu_processor::Nv12Planes::split(data, w, h).map(
            |planes| viewcamera::camera::CameraFrameData {
                pixels: viewcamera::camera::FramePixels::Nv12 {
                    y: Arc::new(planes.y),
                    uv: Arc::new(planes.uv),
                },
                width: planes.width,
                height: planes.height,
                // generation 由主线程写入时覆盖
                generation: 0,
            },
        );
    }
    let (w, h) = cam.poll_frame()?;
    Some(viewcamera::camera::CameraFrameData {
        pixels: viewcamera::camera::FramePixels::Rgba(Arc::new(cpu_processor.process(
            cam.frame_rgb(),
            w,
            h,
        ))),
        width: cpu_processor.last_width(),
        height: cpu_processor.last_height(),
        generation: 0,
    })
}

/// 拍照取帧：NV12 时做一次性 RGBA 转换（仅拍照瞬间，不在实时路径），
/// 主线程拿到统一 RGBA 用于冻结显示与后台 PNG 保存。
fn poll_captured_frame(
    cam: &mut viewcamera::capture::CameraManager,
    cpu_processor: &mut viewcamera::gpu_processor::CpuFrameProcessor,
) -> Option<CameraEvent> {
    if cam.is_native_nv12() {
        let (data, w, h, _capture_ts) = cam.poll_frame_nv12()?;
        let planes = viewcamera::gpu_processor::Nv12Planes::split(data, w, h)?;
        let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
        planes.to_rgba(&mut rgba);
        return Some(CameraEvent::Captured(viewcamera::camera::CameraFrameData {
            pixels: viewcamera::camera::FramePixels::Rgba(Arc::new(rgba)),
            width: w,
            height: h,
            generation: 0,
        }));
    }
    let (w, h) = cam.poll_frame()?;
    Some(CameraEvent::Captured(viewcamera::camera::CameraFrameData {
        pixels: viewcamera::camera::FramePixels::Rgba(Arc::new(cpu_processor.process(
            cam.frame_rgb(),
            w,
            h,
        ))),
        width: cpu_processor.last_width(),
        height: cpu_processor.last_height(),
        generation: 0,
    }))
}

/// 摄像头线程状态 + 命令处理（集中一处避免分支重复）。
struct CameraWorkerState {
    camera: Option<viewcamera::capture::CameraManager>,
    should_freeze: bool,
    do_capture: bool,
    /// **用户请求**的采集间隔（来自 `frame_rate_mode`）。只由 `Start` 写入。
    ///
    /// 刻意与实际节拍分开：不分开的话，每次循环把实测值写回同一个字段就成
    /// 了**棘轮**——请求值被自己的历史结论吞掉，设备节拍恢复正常后拉不回来。
    requested_frame_interval: Duration,
}

impl CameraWorkerState {
    fn apply(&mut self, cmd: CameraCommand) {
        match cmd {
            CameraCommand::Start {
                index,
                width,
                height,
                frame_interval_ms,
            } => {
                self.requested_frame_interval = Duration::from_millis(frame_interval_ms);
                // "启动摄像头" 语义上即"要画面"：清掉拍照后的冻结。
                // （原为黑板模式留下的 Freeze/Unfreeze 命令对——黑板按钮移除后
                //   Unfreeze 只剩这一处用途，改为由 Start 隐含。）
                self.should_freeze = false;
                self.do_capture = false;
                if self.camera.is_some() {
                    return;
                }
                let mut cam = viewcamera::capture::CameraManager::new();
                let devices = cam.enumerate_devices();
                if !devices.is_empty() {
                    log::info!("Found {} camera(s)", devices.len());
                    let device_index = index.min(devices.len() - 1);
                    if let Err(e) = cam.open(device_index, width, height) {
                        log::error!("Failed to open camera: {}", e);
                    } else {
                        self.camera = Some(cam);
                    }
                } else {
                    log::warn!("No cameras found");
                }
            }
            CameraCommand::Stop => {
                if let Some(ref mut cam) = self.camera {
                    cam.close();
                }
                self.camera = None;
                self.should_freeze = false;
                self.do_capture = false;
            }
            CameraCommand::Capture => self.do_capture = true,
        }
    }
}

/// 采集侧节拍诊断的计数器（采集线程内）。
thread_local! {
    static CAMERA_DIAG: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn camera_worker() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(10, |mut output: mpsc::Sender<Message>| async move {
        // std 阻塞通道：工作线程用 recv()/recv_timeout() 挂起等待（零唤醒空转），
        // 替代旧版 try_recv+sleep 轮询（空闲时常驻 ~60-1000 次/秒唤醒，白耗 CPU）
        let (cmd_sender, cmd_receiver) = std::sync::mpsc::channel::<CameraCommand>();
        // 帧交接用"最新帧槽位 + 通知信号"，**不用**容量受限的队列。
        //
        // 队列（哪怕容量 1）在消费端落后时只能拒绝新帧，而拒绝发生在**采集
        // 节拍上**：跳过这一轮后要重新等摄像头的一个采样间隔，一拍变成两拍。
        // 实测每秒丢弃 30 次，`cam_gap` 的 p95 从 32.5ms 恶化到 48.8ms、最长
        // 197.6ms——这就是"画面一会更新快一会慢"。
        //
        // 槽位语义是"覆盖"：采集侧无条件写入最新帧并敲一次铃；消费端取走
        // 当前最新帧。采集节拍与消费进度**彻底解耦**，消费慢只会少显示一帧，
        // 不会让采集断拍。
        let latest_frame: Arc<std::sync::Mutex<Option<CameraEvent>>> =
            Arc::new(std::sync::Mutex::new(None));
        // 信号通道只承载"有新帧"这一位信息，允许合并：`try_send` 失败说明
        // 铃已经响过了（消费端还没来取），这正是我们要的——不排队。
        let (frame_sender, mut frame_receiver) = tokio_mpsc::channel::<()>(1);

        let _ = output
            .send(Message::CameraReady(CameraCommandSender(cmd_sender)))
            .await;

        // 在独立原生线程中运行摄像头操作（nokhwa 的 Camera 不是 Send）
        let latest_frame_for_thread = Arc::clone(&latest_frame);
        std::thread::spawn(move || {
            let latest_frame = latest_frame_for_thread;
            let mut state = CameraWorkerState {
                camera: None,
                should_freeze: false,
                do_capture: false,
                requested_frame_interval: viewframe::grid::mode_period("normal"),
            };
            let mut cpu_processor = viewcamera::gpu_processor::CpuFrameProcessor::new();
            // 采集节拍的绝对基准。见下面 `next_capture_at` 处的说明。
            let mut next_capture_at = std::time::Instant::now();

            loop {
                // 非阻塞排空主线程发来的命令
                loop {
                    match cmd_receiver.try_recv() {
                        Ok(cmd) => state.apply(cmd),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }

                // 拍照：取一帧后转入冻结
                if state.do_capture {
                    if let Some(ref mut cam) = state.camera {
                        if let Some(event) = poll_captured_frame(cam, &mut cpu_processor) {
                            publish_frame(&latest_frame, &frame_sender, event);
                        }
                    }
                    state.do_capture = false;
                    state.should_freeze = true;
                    continue;
                }

                match state.camera.as_mut() {
                    Some(cam) if !state.should_freeze => {
                        // **不再因为主线程没来得及消费就跳过整轮。**
                        //
                        // 旧实现是"容量 1 队列 + `try_reserve`"，预留失败就
                        // `sleep(1ms); continue`——注释写的是"避免为注定被
                        // 丢弃的帧做无效工作"。但它同时**破坏输出节拍**：跳过
                        // 这一轮后，下一次 `poll_live_frame` 要重新等摄像头的
                        // 一个采样间隔，于是本该 33.3ms 的一拍变成 66.6ms。
                        //
                        // 实测量化（此前）:
                        //
                        // ```text
                        // cam_gap min=30.0 中位=32.5 p95=48.8 max=197.6ms  丢弃=30/s
                        // ```
                        //
                        // **每秒丢弃 30 次**，p95 飙到 48.8ms、最大 197.6ms——
                        // 这就是"画面一会更新快一会慢"的直接来源。丢弃没有任何
                        // 好处：一次 NV12 解码远低于一次断拍造成的可见卡顿。
                        //
                        // 现在改成"最新帧槽位"（`latest_frame`）：采集侧永远
                        // 按自己的节拍产出，主线程落后只会**少看一帧**（少一次
                        // 显示），而不会让**采集**断拍。摄像头是内容源，显示才是
                        // 节拍——与浏览器把视频当内容、由合成器按固定节奏取最新
                        // 帧是同一个模型。
                        let frame_start = std::time::Instant::now();
                        // 采集侧节拍诊断：与上屏侧（`[frame_pacing]`）分开记，
                        // 才能判断抖动是摄像头/采集线程产生的，还是事件循环
                        // 与 present 路径产生的。两者的修法完全不同。
                        CAPTURE_LAST_AT.with(|slot| {
                            if let Some(prev) = slot.get() {
                                viewink::stats::note_camera_interval(
                                    frame_start.duration_since(prev),
                                );
                            }
                            slot.set(Some(frame_start));
                        });
                        if let Some(frame) = poll_live_frame(cam, &mut cpu_processor) {
                            publish_frame(
                                &latest_frame,
                                &frame_sender,
                                CameraEvent::Frame(frame),
                            );
                            // **按实测能力改节拍**：设备给 21fps 就别按 60Hz
                            // 空转 poll。实测值同时喂给显示侧（网格），两边
                            // 用同一个数，不会出现"采集 47.6ms、显示 33ms"这种
                            // 隔帧不同步。
                            viewframe::grid::publish_observed_period(cam.observed_period());
                            // 打印实测值本身，别靠推断——之前就是靠"轮次间隔
                            // ≈ 源端间隔"倒推，两者其实不等（poll 常在 1ms 内
                            // 返回排队样本），推断出的结论是错的。
                            if let Some(observed) = cam.observed_period() {
                                CAMERA_DIAG.with(|d| {
                                    let n = d.get() + 1;
                                    d.set(n);
                                    if n % 120 == 0 {
                                        log::info!(
                                            "[cam_pace] 设备声称={}fps 实测周期={:.1}ms \
                                             ({:.1}fps) 用户请求={:.0}ms 实际节拍={:.1}ms",
                                            cam.negotiated_fps(),
                                            observed.as_secs_f64() * 1000.0,
                                            1.0 / observed.as_secs_f64(),
                                            state.requested_frame_interval.as_secs_f64() * 1000.0,
                                            cam.pacing_period(state.requested_frame_interval)
                                                .as_secs_f64()
                                                * 1000.0,
                                        );
                                    }
                                });
                            }
                        }
                        // poll 自身耗时与"轮次间隔"必须分开量：间隔 = poll + sleep，
                        // 不分开就无法判断节拍是被**摄像头阻塞**拉长的，还是被
                        // 自己的 sleep 算错的。实测两种解释都出现过。
                        viewink::stats::note_camera_poll(frame_start.elapsed());
                        // **绝对时刻推进，不累积漂移。**
                        //
                        // 旧写法 `sleep(period - elapsed)` 以"本轮起点"为基准，
                        // 而 `poll_live_frame` 内部会阻塞等摄像头的下一个采样，
                        // 于是 `elapsed` 里混进了"等样本"的时间：一旦某轮超期
                        // （`elapsed >= period`）就不睡，下一轮紧接着开始，
                        // 误差不会自我纠正而是随机游走。
                        //
                        // 改成以 `next_capture_at` 为唯一基准向后推进：每轮
                        // `next_capture_at += period`，睡到该时刻；若已经落后
                        // （`now > next_capture_at`）就**不追赶**（避免突发补帧
                        // 又变成另一种抖动），直接把它对齐到现在。
                        let period = cam.pacing_period(state.requested_frame_interval);
                        let now = std::time::Instant::now();
                        if now < next_capture_at {
                            std::thread::sleep(next_capture_at - now);
                        }
                        next_capture_at = std::cmp::max(next_capture_at + period, now);
                    }
                    Some(_) => {
                        // 冻结：挂起等待命令或超时（零轮询空转）
                        match cmd_receiver.recv_timeout(Duration::from_millis(50)) {
                            Ok(cmd) => state.apply(cmd),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    None => {
                        // 无摄像头：挂起直到命令到达（零 CPU 消耗，替代 sleep(100ms) 轮询）
                        match cmd_receiver.recv() {
                            Ok(cmd) => state.apply(cmd),
                            Err(_) => return,
                        }
                    }
                }
            }
        });

        // 消费端：被铃唤醒后取走**当前最新**帧。
        //
        // 中间帧被覆盖是**预期行为**，不是缺陷：显示端要的是"此刻最新的画面"，
        // 而不是"每一帧都按序看到"。丢一帧显示的代价是一次重复画面（人眼几乎
        // 不可见），而按序排队会让画面越来越滞后——那才是可见的卡顿。
        while frame_receiver.recv().await.is_some() {
            let taken = latest_frame.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(event) = taken {
                let _ = output.send(Message::CameraEvent(event)).await;
            }
        }
    })
}

/// 把一帧写进"最新帧槽位"并敲铃。**永不阻塞、永不拒绝。**
///
/// 铃通道容量 1 且 `try_send` 失败是**正常**的：说明消费端还没来取上一帧，
/// 铃还响着，等它取走再敲即可。旧实现在这里整周期跳过采集节拍（见
/// `camera_worker` 内的注释），那才是"画面忽快忽慢"的根因。
fn publish_frame(
    slot: &Arc<std::sync::Mutex<Option<CameraEvent>>>,
    signal: &tokio_mpsc::Sender<()>,
    event: CameraEvent,
) {
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(event);
    let _ = signal.try_send(());
}

// ==================
// 摄像头采集节拍诊断
// ==================

thread_local! {
    /// 采集线程上一轮的起点。只用于统计采集侧间隔。
    static CAPTURE_LAST_AT: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
    /// MediaFoundation 报告的上一帧**源端**时间戳。只用于统计源端间隔。
    static CAPTURE_SRC_LAST: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

// ==================
// 笔画计算后台工作线程
// ==================

fn stroke_compute_worker() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(10, |mut output: mpsc::Sender<Message>| async move {
        // 任务侧用 std 阻塞通道：线程 recv() 挂起零唤醒，替代旧版
        // try_recv + 指数退避 sleep 轮询（空闲时常驻 ~60 次/秒唤醒）
        let (task_sender, task_receiver) = std::sync::mpsc::channel::<StrokeComputeTask>();
        let (mut result_sender, mut result_receiver) = mpsc::channel::<StrokeComputeResult>(10);

        let _ = output.send(Message::StrokeComputeReady(StrokeTaskSender(task_sender))).await;

        // 在独立原生线程中运行笔画网格计算（扣除已存在的橡皮擦）
        std::thread::spawn(move || {
            while let Ok(task) = task_receiver.recv() {
                // 只算扣除几何：未扣兜底已由主线程在 finalize_stroke 里
                // 同步算出（同一份 stroke、同一算法），后台重算一遍是
                // 纯重复 CPU 开销。`task.erasers` 为 Arc 快照，直接解引用。
                let geometry = viewink::ink::mesh::tessellate_stroke_canvas(
                    &task.stroke,
                    &task.erasers,
                    None,
                );
                let _ = result_sender.try_send(StrokeComputeResult {
                    stroke_id: task.stroke_id,
                    geometry,
                    eraser_generation: task.eraser_generation,
                });
            }
        });

        while let Some(result) = result_receiver.next().await {
            let _ = output.send(Message::StrokeComputed(result)).await;
        }
    })
}

// ==================
// 原始触控输入已迁移到零消息路径：hook（sys::touch）入队 + InvalidateRect
// 请求重绘 → RedrawRequested 内 drain_touch_zero_msg 消费。
// ==================

// ==================
// canvas::Program — 批注层（处理事件 + 绘制笔画，透明背景）
// 满批常量 LIVE_STROKE_BATCH 已迁入 viewgfx（live_geo 模块）
// ==================

impl canvas::Program<Message> for App {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        event: &Event,
        _bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<Message>> {
        // ============ 分层渲染热路径（零消息） ============
        // canvas 交互期间不发布任何 Message：直接写共享实时状态
        // （viewink::live），并请求重绘。运行时收到无消息的重绘请求时
        // 跳过 program.update/view 重建——UI 层（工具栏/侧栏等 widget）
        // 完全静止，仅画布动态层经 prepare/draw 刷新后叠加合成。

        // ============ 帧驱动节拍：摄像头网格 + 惯性积分 ============
        //
        // 惯性改由渲染帧自身驱动（取代原 16ms 定时器：拍频顿帧）。摄像头内容
        // 的上屏也挂在这里——见 `step_camera_grid`。
        if let Event::Window(iced::window::Event::RedrawRequested(_)) = event {
            if let Some(action) = self.step_camera_grid() {
                return Some(action);
            }
            return self.step_momentum();
        }

        // ============ 触控（框架原生通路） ============
        //
        // winit 把 `WM_POINTERDOWN/UPDATE/UP` 转成 `WindowEvent::Touch`
        // （`platform_impl/windows/event_loop.rs`），iced 再转成
        // `Event::Touch`（`conversion.rs::touch_event`）。**它不会**交给
        // `DefWindowProc` 去提升成鼠标消息——所以只处理 `Event::Mouse`
        // 的应用永远收不到触控，这就是"触控完全没反应"的原因。
        //
        // 位置已是逻辑坐标（iced 在转换时乘了 scale_factor），直接用。
        if let Event::Touch(touch) = event {
            let position = match *touch {
                iced::touch::Event::FingerPressed { position, .. }
                | iced::touch::Event::FingerMoved { position, .. }
                | iced::touch::Event::FingerLifted { position, .. }
                | iced::touch::Event::FingerLost { position, .. } => position,
            };
            return self.on_touch(touch, position);
        }

        // 橡皮擦悬停光标（未按下）：跟随移动，零消息刷新预览
        if matches!(self.draw_mode, DrawMode::Eraser) && !self.mouse_left_pressed.get() {
            if let Event::Mouse(iced::mouse::Event::CursorMoved { .. }) = event {
                if let Some(pos) = cursor.position() {
                    self.update_eraser_cursor(pos.x, pos.y);
                    return Some(self.paced_redraw());
                }
                return None;
            }
        }

        // 鼠标移动：绘画/拖拽期间直改状态（追加点 + 增量镶嵌 / pan 平移），
        // 与鼠标回报率（可达 1000Hz）完全解耦——每个事件仅一次 O(1) 状态写入，
        // 重绘频率由 Action::request_redraw 交还给框架按 vsync 调度。
        if let Event::Mouse(iced::mouse::Event::CursorMoved { .. }) = event {
            // 不再按 `is_touch_active()` 屏蔽鼠标：窗口已 `RegisterTouchWindow`，
            // Windows 不会把触控提升成鼠标消息，因此不存在"触控被当成鼠标重复
            // 处理"的风险。此前这道守卫的唯一实际效果是：一旦触点计数失配
            // （Up 丢失 / 掌压被过滤），**整个应用的鼠标输入全死**——UI 点不动、
            // 也画不了线。触控事件由 `drain_touch_zero_msg` 独占消费，根本不会
            // 走到这里。
            let pos = cursor.position()?;
            if self.is_drawing.get() {
                let (cx, cy) = self.screen_to_canvas(pos.x, pos.y);
                // 只追加点：镶嵌统一在 `App::draw` 每帧做一次（见那里的说明）
                if self.live.current_stroke.borrow().is_some() {
                    self.gfx.append_pen_point(cx, cy);
                } else if self.live.current_eraser_stroke.borrow().is_some() {
                    self.gfx.append_eraser_point(self.config.canvas.eraser_size, cx, cy);
                } else {
                    return None;
                }
                return Some(self.paced_redraw());
            }
            if self.mouse_left_pressed.get() && self.draw_mode == DrawMode::Move {
                if let Some((prev_x, prev_y)) = self.last_mouse_pos.get() {
                    let dx = pos.x as f64 - prev_x;
                    let dy = pos.y as f64 - prev_y;
                    self.live.transform.offset_x.set(self.live.transform.offset_x.get() + dx as f32);
                    self.live.transform.offset_y.set(self.live.transform.offset_y.get() + dy as f32);
                    self.last_pan_delta.set((dx, dy));
                    self.last_mouse_pos.set(Some((pos.x as f64, pos.y as f64)));
                    return Some(self.paced_redraw());
                }
            }
            return None;
        }

        // 与 `on_touch` 同源地记录输入时刻：延迟指标不能只覆盖触摸，鼠标书写走的
        // 是同一条 `paced_redraw` → `App::draw` 路径。
        if matches!(event, Event::Mouse(_)) {
            self.mark_input();
        }
        let message = match event {
            Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)) => {
                cursor.position().map(Message::CanvasPressed)
            }
            Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
                Some(Message::CanvasReleased)
            }
            Event::Mouse(iced::mouse::Event::WheelScrolled { delta }) => {
                let y = match delta {
                    iced::mouse::ScrollDelta::Lines { y, .. } => *y,
                    iced::mouse::ScrollDelta::Pixels { y, .. } => *y / 100.0,
                };
                cursor
                    .position()
                    .map(|cursor_pos| Message::CanvasWheelScroll {
                        delta: y,
                        cursor: cursor_pos,
                    })
            }
            _ => None,
        };

        message.map(Action::publish)
    }

    fn draw(
        &self,
        _state: &Self::State,
        _renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        // ---- 帧驱动：全部计量与状态推进都委托给 viewframe::FrameMetrics ----
        //
        // 起点取本函数**第一行**的时刻（`begin_frame` 内部第一件事就是取时钟），
        // 后续三处计量都以它为基准：① 相邻两帧起点之差 = layout 间隔；② 本帧属主
        // CPU 工作耗时；③ 输入→帧起点延迟。
        let metrics = self.frame.metrics();
        let frame_started = metrics.begin_frame();

        // 直通合成通道注册（一次性；主窗口专属副作用，见方法文档）
        self.register_direct_channels_once();

        // 视口尺寸写入共享状态：渲染侧据此把屏幕矩形逆变换为画布矩形，
        // 做静态笔画视口剔除（视口外笔画零上传零绘制）。
        self.live.viewport.set((bounds.width, bounds.height));

        // === 进行中笔迹/橡皮擦的增量镶嵌：每帧一次，且是本帧最后一步状态推进 ===
        //
        // 位置是刻意的：本函数在 iced 的 `interface.draw` 里被调用，而
        // `Program::update(RedrawRequested)`（笔/触控队列 drain、惯性积分）
        // 一定跑在它之前——同一轮事件循环内 `update → draw → prepare →
        // present` 的顺序，见 iced_winit/src/lib.rs 的 RedrawRequested 分支。
        // 于是「本帧收到的所有输入点」在镶嵌时已全部就位，紧接其后的
        // `DirectChannel::prepare` 直接上传这份几何：输入到像素**同帧直达**。
        //
        // 为什么不在输入事件里镶嵌：笔/触控回报率（240Hz 触控、1000Hz 鼠标）
        // 远高于帧率，逐事件镶嵌会把同一段尾部重算 N 次，其中 N-1 次的产物
        // 在渲染侧被直接丢弃。更糟的是这段 CPU 全部落在**帧起点之前**的关键
        // 路径上：1000Hz 鼠标 + 144Hz 屏幕 = 每帧 ~7 次尾部镶嵌，累积到把帧
        // 预算吃穿时就错过 vsync 死线 → 整帧多等一个周期（最刺眼的延迟）。
        // 每帧一次则恒为 O(新增点数)，且用的必然是最新点。
        self.gfx.refresh_live_geometry();

        // 帧间隔分类 + 1 秒窗口报表（layout 间隔 ≠ 呈现间隔，见 end_frame 文档）
        metrics.end_frame(frame_started);

        // 笔尖外推的预测时域 = **有效**帧量化周期（已含书写期的 60fps 节流），
        // 不是面板刷新率——否则 144Hz 面板跳帧后会欠补一半延迟。
        self.live
            .frame_period_ms
            .set(self.effective_frame_period_ms());

        // 性能 HUD 采样（仅在启用时统计）。内存查询作为闭包传入——两道闸门
        // （FPS 0.5s / 内存 1s）都没放行时它一次也不会被调用，且平台相关的
        // 内存 API 留在应用侧（`sys::memory` 是平台工具，不属于渲染层）。
        metrics.sample_perf(self.config.developer.perf_monitor_enabled, || {
            crate::sys::memory::get_memory_usage_mb()
        });

        // 渲染已迁移到独立批注管线（viewink::annotation），事件层仅捕获输入。
        // 帧预算计量：本函数内属宿主自身的 CPU 工作（镶嵌 + 记账）。与
        // `stats::note_prepare_work`（批注层打包上传 + 剔除）相加才是宿主
        // 在一帧内可控的全部开销——逼近刷新周期即意味着要丢帧。
        metrics.note_frame_work(frame_started);
        vec![]
    }
}

// ==================
// main
// ==================

// ==================
// Image I/O helpers
// ==================

fn pictures_viewstage_dir(custom: Option<&std::path::Path>) -> Result<std::path::PathBuf, String> {
    if let Some(path) = custom {
        return Ok(path.to_path_buf());
    }
    let base = dirs::picture_dir().ok_or_else(|| "No picture directory found".to_string())?;
    Ok(base.join("ViewStage"))
}

fn calc_save_path(
    prefix: &str,
    extension: &str,
    custom: Option<&std::path::Path>,
) -> Result<(std::path::PathBuf, String), String> {
    let base_dir = pictures_viewstage_dir(custom)?;
    let now = chrono::Local::now();
    let date_str = now.format("%Y-%m-%d").to_string();
    let time_str = now.format("%H-%M-%S").to_string();
    let date_dir = base_dir.join(&date_str);
    std::fs::create_dir_all(&date_dir).map_err(|e| format!("Failed to create dir: {}", e))?;
    let timestamp = now.format("%3f").to_string();
    let file_name = format!("{}_{}-{}.{}", prefix, time_str, timestamp, extension);
    Ok((date_dir.join(&file_name), file_name))
}

fn load_image_from_path(path: &std::path::Path) -> Result<ImageEntry, String> {
    let img = ::image::open(path).map_err(|e| format!("Failed to load image: {}", e))?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let rgba = Arc::new(rgba.to_vec());
    // Handle 持有独立副本（iced 要求所有权）；Arc 副本供背景纹理复用
    let handle = image::Handle::from_rgba(width, height, (*rgba).clone());
    Ok(ImageEntry {
        handle,
        rgba,
        width,
        height,
        source_id: None,
    })
}

fn load_image_from_rgba(rgba: Vec<u8>, width: u32, height: u32) -> ImageEntry {
    let rgba = Arc::new(rgba);
    let handle = image::Handle::from_rgba(width, height, (*rgba).clone());
    ImageEntry {
        handle,
        rgba,
        width,
        height,
        source_id: None,
    }
}

/// 分层诊断开关已迁至 `viewframe::channels::diag_no_camera`（与它所控制的通道
/// 注册放在一起——环境变量的语义就是"跳过这些通道"）。

/// 提升 Windows 计时器精度到 1ms。返回是否成功（供退出时配对还原）。
///
/// **这是本应用最大的一处延迟来源。** Windows 默认系统计时器粒度是
/// **15.625ms**，而 `std::thread::sleep` / `WaitForSingleObject` 都会向上
/// 取整到下一个系统节拍。不修正的话：
///
/// - `sleep(33ms)`（相机"自适应"档的推帧间隔）实际睡 ~46.9ms ⇒ 相机预览
///   从 30fps 掉到 ~21fps，且节拍抖动；
/// - `sleep(1ms)`（帧通道满时的重试退避）实际睡 ~15.6ms ⇒ 主线程稍有积压
///   就会把采集线程卡住两帧；
/// - `Action::request_redraw_at` 的定时补帧同样被 15.6ms 粒度量化，批注
///   帧率上限（默认 60）根本打不准。
///
/// `timeBeginPeriod` 是**引用计数**的：每次调用必须配一次 `timeEndPeriod`。
/// 若 winit/iced 内部已请求过，这里只是计数 +1，不冲突。
///
/// 代价：全局 1ms 粒度会略微增加电源管理器唤醒频率——对交互式应用可接受，
/// 且远小于上面几处延迟抖动。非 Windows 平台无此问题，恒返回 true（空操作）。
fn enable_high_resolution_timer() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Media::timeBeginPeriod;
        // 返回 0 表示失败；失败时保持默认粒度，功能不受影响
        if timeBeginPeriod(1) == 0 {
            log::warn!("timeBeginPeriod(1) failed: timer granularity stays at 15.625ms");
            false
        } else {
            log::info!("Windows timer resolution raised to 1ms");
            true
        }
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// 与 [`enable_high_resolution_timer`] 配对：还原系统计时器粒度。
fn restore_timer_resolution(enabled: bool) {
    if !enabled {
        return;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Media::timeEndPeriod;
        timeEndPeriod(1);
    }
}

fn main() {
    // 渲染器启动配置（改动需重启）。三件事都必须在窗口/适配器创建之前落实：
    // ① 呈现节拍基准（`set_present_hook`）——必须在第一帧呈现前注册好，否则前
    //    几帧的呈现周期估计缺失；
    // ② present_mode → `ICED_PRESENT_MODE`，由
    //    `iced_wgpu::settings::present_mode_from_env` 解析成 wgpu PresentMode；
    // ③ prefer_integrated_gpu → `WGPU_POWER_PREF=low`，双显卡机器锁定核显避免
    //    唤醒独显。
    // 语义与取值白名单都记在 `viewframe::boot`。
    viewframe::boot::install_present_hook();
    let boot_config = app::config::Config::load();
    viewframe::boot::apply_present_mode(&boot_config.renderer.present_mode);
    viewframe::boot::apply_power_pref(boot_config.renderer.prefer_integrated_gpu);

    // 日志写**文件**：原先用 TermLogger 输出到终端，而从 Explorer 启动时通常
    // 没有可见控制台——`RawTouch: hook installed` / `hook 安装失败` 这类唯一
    // 的诊断线索就这么丢了（触控链路整条断掉却毫无迹象的根因之一）。
    // 文件追加写，路径同时打到 stderr 与日志首行。
    let log_path = std::env::temp_dir().join("viewstage.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);
    match log_file {
        Ok(file) => {
            let _ = simplelog::WriteLogger::init(
                simplelog::LevelFilter::Info,
                simplelog::Config::default(),
                file,
            );
            eprintln!("[viewstage] log -> {}", log_path.display());
        }
        Err(e) => {
            // 落不了盘就退回终端（至少交互式运行还看得到）
            eprintln!("[viewstage] 日志文件打开失败（{}），退回终端", e);
            let _ = simplelog::TermLogger::init(
                simplelog::LevelFilter::Info,
                simplelog::Config::default(),
                simplelog::TerminalMode::Mixed,
                simplelog::ColorChoice::Auto,
            );
        }
    }

    // 必须在任何 sleep / 定时器建立之前请求，否则前几帧仍走粗粒度节拍
    let timer_raised = enable_high_resolution_timer();

    let _ = sys::memory::clean_working_set();
    log::info!("Memory usage: {:.1} MB", sys::memory::get_memory_usage_mb());

    let result = iced_winit::run(ViewStageApp);
    restore_timer_resolution(timer_raised);
    result.expect("Failed to run ViewStage");
}






