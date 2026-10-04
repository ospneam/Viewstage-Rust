//! 批注层独立 wgpu 渲染管线：GPU 胶囊扩展。
//!
//! 分层渲染：图元仅持共享 [`LiveCanvasState`] 句柄——App 在 canvas 热路径
//! （零消息）写入最新几何，本层 prepare/draw 阶段直读，UI widget 层不参与重建。
//!
//! - 几何形态：CPU 只产出线段表（[`StrokeSeg`]，64B/段），四边形扩展、
//!   圆头关节、端帽与抗锯齿全部由核显着色器完成（annotation.wgsl）。
//!   相比旧 CPU 三角化，上传数据量约 12×↓、VS 负载 ~9×↓、关节 overdraw 消除。
//! - 静态笔画：全部可见段拼接进单个 grow-only storage 缓冲，一次 set_bind_group
//!   + 一次实例化 draw 覆盖全部。快照变化时做**最长公共前缀比对**：书写追加
//!   场景（新快照以旧快照为前缀）只把新增可见笔画的段以偏移增量写入缓冲，
//!   上传量与新增笔画成正比；橡皮擦/undo 等中间修改或视口移动走全量重写兜底。
//!   稳态（快照与视口未变）零打包、零上传、零分配。视口剔除只对"每次上传"生效
//!   （增量与全量均只上传可见段），不影响增量正确性：视口未动时前缀笔画的可见性
//!   与已持久化状态一致。
//! - 动态几何（进行中笔画增量块/橡皮擦轨迹/触控预测轨迹）：**两层上传**。稳定层
//!   （已满批 chunks ++ eraser_chunks）不可变、只追加，新块完成时按偏移增量写入
//!   一次后不再触碰；尾层（头部块 + 触控预测轨迹）每次追加重建。旧实现每帧重
//!   打包全部动态段（随笔画长度线性增长），本实现每移动仅上传尾层数 KB。
//! - 橡皮擦悬停光标：独立小缓冲独立 draw，与动态批解耦（互不触发重传）。
//! - pan/zoom：仅重写 1 个 uniform，零数据重传。
//! - 提交：所有缓冲写入经**帧 encoder + staging belt** 记录，随 iced 主帧统一
//!   `finish → submit → recall`（见 [`crate::gpu::write_buffer_belt`]），稳态每帧
//!   只有一次驱动级 submit。
//!
//! ## 为何不再有静态笔迹离屏光栅缓存
//!
//! 历史实现曾把已持久化的静态笔迹光栅化进一张与 surface 同尺寸的纹理，draw 阶段
//! 对「含笔迹瓦片」逐实例 blit。真机实测（1280×720，笔迹静止）证明该路径在本产品
//! 形态下**必然退化为全屏 blit 且是净损失**：
//!
//! - 瓦片覆盖集退化为全网格（`content=15`，即 5×3 全部命中）。笔迹 AABB 一旦横跨
//!   页面即铺满瓦片，「空白瓦片零成本」的前提不成立；
//! - 为此设计的按脏瓦片 `LoadOp::Load` 增量光栅化从未触发（`raster=2/0`、
//!   `pass=0`），每次全量光栅化反而多一次独立 submit；
//! - 笔迹是细长胶囊，几何重放的片段数与「稀疏细线覆盖面积」成正比，而全屏 blit
//!   是固定 `P` 片段 + `P×8B` 纹理带宽——对任何 GPU 都是负收益。
//!
//! 故移除该机制，回到几何重放（等价于旧 `VIEWSTAGE_STATIC_BLIT=0` 分支）。视口剔除、
//! 前缀增量、单缓冲单 draw 全部保留，它们才是静态层成本的真正来源。

use iced_wgpu::graphics::Viewport;
use iced_wgpu::wgpu;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

/// 段表不对齐告警的节流计数器（见 `check_segment_table_invariant`）。
static DEBUG_SEGTABLE_WARN: AtomicU64 = AtomicU64::new(0);

use crate::gpu::{
    triangle_primitive_state, write_buffer_belt, ColorPalette, Globals, PackedSeg, StrokeGeometry,
    StrokeSeg,
};
pub use crate::live::LiveCanvasState;
use crate::render::raster_cache::{self, RasterParams, STABLE_FRAMES};

/// 静态笔画引用（stroke id → 画布坐标几何）。
///
/// `bounds` 为几何的画布坐标 AABB（含半宽外扩），在快照构建时算一次并随
/// 引用缓存，供渲染侧做**视口剔除**——帧热路径内只做 O(1) 矩形相交判定，
/// 不再为视口外的笔画绘制任何段（段本身仍驻留缓冲，见 [`StaticUploadSlot`]）。
#[derive(Debug, Clone)]
pub struct StaticStrokeRef {
    pub id: u64,
    pub generation: u64,
    pub geometry: Arc<StrokeGeometry>,
    /// 画布坐标 AABB：`[min_x, min_y, max_x, max_y]`。
    pub bounds: [f32; 4],
}

impl StaticStrokeRef {
    /// 构造引用并按几何计算 AABB（O(段数)）。
    ///
    /// 仅适用于一次性/测试场景。**热路径请用 [`Self::with_bounds`]**：
    /// 快照每次缓存代数变化都整表重建，用本构造会让每加一笔都重扫全部
    /// 已存笔画的全部段（累计 O(n²)）。
    pub fn new(id: u64, generation: u64, geometry: Arc<StrokeGeometry>) -> Self {
        let bounds = geometry.bounds();
        Self::with_bounds(id, generation, geometry, bounds)
    }

    /// 用**预先算好**的 AABB 构造引用（O(1)）。
    ///
    /// `bounds` 必须与 `geometry` 同代——由 [`viewgfx::CachedMesh`] 在几何
    /// 产出时一并算出并随缓存条目存放。
    pub fn with_bounds(
        id: u64,
        generation: u64,
        geometry: Arc<StrokeGeometry>,
        bounds: [f32; 4],
    ) -> Self {
        Self {
            id,
            generation,
            geometry,
            bounds,
        }
    }

    /// 与给定矩形是否相交（零矩形恒为 false）。
    #[inline]
    pub fn intersects(&self, rect: &[f32; 4]) -> bool {
        self.bounds[0] <= rect[2]
            && self.bounds[1] <= rect[3]
            && self.bounds[2] >= rect[0]
            && self.bounds[3] >= rect[1]
    }
}

/// 批注层直通合成通道：仅持共享实时状态句柄与持久管线——
/// **不经 iced widget/Scene/primitive 队列**（实现 [`iced_wgpu::DirectChannel`]）。
/// App 启动时注册进 iced_wgpu 合成器：每帧 `prepare` 直读最新几何上传，
/// 合成序列在 iced 全部 UI 层之下直接画进最终 frame；热路径更新零 widget 参与。
#[derive(Debug)]
pub struct AnnotationLayer {
    pub live: Arc<LiveCanvasState>,
    /// 直通通道的持久管线（`DirectChannel::init` 幂等创建；原 iced
    /// Storage/TypeId 机制退役）。
    pipeline: Option<AnnotationPipeline>,
    /// 静态层离屏光栅缓存（高密度文档才启用，见 [`raster_cache`]）。
    cache: StaticRasterCache,
    /// 本帧是否走缓存 blit（`draw` 据此跳过静态层的直接绘制）。
    cache_active: bool,
}

impl AnnotationLayer {
    pub fn new(live: Arc<LiveCanvasState>) -> Self {
        Self {
            live,
            pipeline: None,
            cache: StaticRasterCache::default(),
            cache_active: false,
        }
    }

    /// 稳定层指纹：已满批块（chunks ++ eraser_chunks）的 ptr+len 混合。
    /// 这些块一旦生成就不可变（只追加），对应 GPU 稳定层的一次性上传。
    pub(crate) fn dyn_stable_fingerprint(&self) -> u64 {
        let live = &self.live;
        let mut h: u64 = 0x9463_a9e5_c1a2_e9d8;
        for g in live.chunks.borrow().iter() {
            mix_hash(&mut h, Arc::as_ptr(g) as usize as u64);
            mix_hash(&mut h, g.segs.len() as u64);
        }
        for g in live.eraser_chunks.borrow().iter() {
            mix_hash(&mut h, Arc::as_ptr(g) as usize as u64);
            mix_hash(&mut h, g.segs.len() as u64);
        }
        h
    }

    /// 尾层指纹：头部块 + 触控轨迹（每次追加重建的部分）。
    pub(crate) fn dyn_tail_fingerprint(&self) -> u64 {
        let live = &self.live;
        let mut h: u64 = 0xcbf2_92ce_4842_2235;
        match live.head.borrow().as_ref() {
            Some(head) => {
                mix_hash(&mut h, 0x1);
                mix_hash(&mut h, live.head_index.get());
                mix_hash(&mut h, Arc::as_ptr(head) as usize as u64);
                mix_hash(&mut h, head.segs.len() as u64);
            }
            None => mix_hash(&mut h, 0),
        }
        match live.trail.borrow().as_ref() {
            Some((gen, g)) => {
                mix_hash(&mut h, *gen);
                mix_hash(&mut h, Arc::as_ptr(g) as usize as u64);
                mix_hash(&mut h, g.segs.len() as u64);
            }
            None => mix_hash(&mut h, 0xdead_beef),
        }
        h
    }

    /// 橡皮擦悬停光标指纹（坐标/半径位级混合；None → 0）。
    pub(crate) fn cursor_fingerprint(&self) -> u64 {
        match self.live.eraser_cursor.get() {
            Some(c) => {
                let mut h: u64 = 0x9e37_79b9_7f4a_7c15;
                mix_hash(&mut h, c.cx.to_bits() as u64);
                mix_hash(&mut h, c.cy.to_bits() as u64);
                mix_hash(&mut h, c.radius.to_bits() as u64);
                mix_hash(&mut h, c.border.to_bits() as u64);
                h
            }
            None => 0,
        }
    }
}

#[inline]
fn mix_hash(h: &mut u64, v: u64) {
    *h = (*h ^ v).wrapping_mul(0x1000_0000_01b3);
    *h = h.rotate_left(27);
}

/// 静态层上传槽：**所有静态笔画段拼接进单个 grow-only storage 缓冲**，
/// bind group 切换恒为 1（旧实现按 128 笔分批，随笔数线性增长的 bind group
/// 切换在此归零）。
///
/// 驻留式（residency-based）：缓冲内容不再随视口重排。
///
/// # 与旧设计的根本差别
///
/// 旧设计让 GPU 缓冲**只装当前可见的段**，靠"缓存矩形比视口每边外扩 25%"
/// 做迟滞。代价是：**平移每跨过 0.25 个视口宽，就要把全部可见段重新打包
/// 并整段重传**——一次几百 KB 的上传尖峰，正好砸在平移手势中间。
///
/// 新设计把两件事解耦：
/// - **驻留**（buffer 里有什么）：只由**笔画集合**决定。书写追加走偏移增量，
///   视口怎么动都不影响；橡皮擦/undo 才整表重传。
/// - **可见**（本帧画哪些）：每帧按笔画 AABB 算，产出**合并后的实例区间**，
///   用 `draw(0..6, range)` 逐区间绘制。着色器无需改动——`instance_index`
///   本就包含 `firstInstance` 偏移，`segments[instance_index]` 天然取到
///   正确的段。
///
/// 代价是绘制批次变多（可见**连续游程**数，不是笔画数），收益是平移期间
/// **零上传**，且每帧成本恒定、没有尖峰。相邻可见笔画会被合并成一个区间；
/// 区间数超过 [`MAX_STATIC_DRAWS`] 时退回单次全量，把最坏情况钉死在 1 批。
///
/// # 代价：显存
///
/// 驻留意味着缓冲按**整份文档**增长而非"当前视野"。10 万段 ≈ 6.4 MB——对任何
/// 近代 GPU 都是可忽略的量级，而它换来的是平移零上传。老实说这是本设计最
/// 需要被知晓的取舍，不是纯粹的免费午餐。
#[derive(Debug)]
struct StaticUploadSlot {
    buffer: wgpu::Buffer,
    capacity: u64,
    bind_group: wgpu::BindGroup,
    seg_count: u32,
    staging: Vec<PackedSeg>,
    /// 本槽调色板（段表只存 4B 索引，见 [`SlotPalette`]）。
    palette: SlotPalette,
    /// 段表写入序号：任何一次上传/缓冲重建都推进。离屏缓存据此判断内容
    /// 是否变化——比逐字节对比缓冲便宜得多。
    write_serial: u64,
    /// 已构建过（首帧必建）。
    built: bool,
    /// 每笔在 GPU 缓冲中的段区间 `(起始段, 段数)`，与快照**同序**。
    /// 逐笔追加，故第 i 项对应 `snapshot[i]`。绘制期按可见性挑区间。
    stroke_ranges: Vec<(u32, u32)>,
    /// 已持久化到 GPU 的快照引用（增量前缀比对的基准）。
    snapshot: Option<Arc<[StaticStrokeRef]>>,
    /// GPU 缓冲中的有效段总数（增量写入偏移 = 该值 × 段大小）。
    persisted_segs: u32,
    /// 本帧可见笔画数（HUD 统计）。
    visible_count: usize,
    /// 本帧可见段数（HUD 统计）。
    visible_segs: u64,
    /// GPU 段缓冲里 `[0, buffer_prefix_segs)` 与 CPU 快照**逐段一致**。
    ///
    /// 由 `upload_static` 在每条返回路径上写入。栅格缓存据此判断能否只补画
    /// 尾部：水位线以下的段既在缓冲里、又被缓存画过，无需重画。
    ///
    /// 任何非追加的改动（橡皮擦、撤销、清空、换快照）都会让这个值小于缓存
    /// 水位，从而强制全量重画——这是刻意的：那条路径上段表被重排，靠偏移
    /// 增量画必然错位。0 = 不知道（整表重传 / 缓冲重建）。
    buffer_prefix_segs: u32,
    /// 局部重画的脏矩形（画布空间 AABB，`None` = 无法界定 → 全量）。
    ///
    /// 由 `upload_static` 在同一次调用里算出——那里新旧两份快照同时在手，
    /// 是唯一能把"旧墨迹原来在哪"也算进去的地方（见那里的注释）。
    pub dirty_rect: Option<[f32; 4]>,
    /// 本帧剔除产物：绘制区间 + 统计（`ranges` 容量跨帧复用）。
    ///
    /// 视口未知（首帧/未布局）时为单区间 `0..seg_count`（不剔除）；已知但
    /// 全部移出视野时为空（零 draw）。
    visible: VisibleSet,
}

impl StaticUploadSlot {
    fn empty(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, uniform: &wgpu::Buffer) -> Self {
        let initial = 4096u64;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewstage.annotation.static"),
            size: initial,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let palette = SlotPalette::new(device, "viewstage.annotation.static.palette");
        let bind_group = make_segment_bind_group(device, layout, uniform, &buffer, &palette.buffer);
        Self {
            buffer,
            capacity: initial,
            bind_group,
            seg_count: 0,
            staging: Vec::new(),
            palette,
            write_serial: 0,
            built: false,
            stroke_ranges: Vec::new(),
            snapshot: None,
            persisted_segs: 0,
            visible_count: 0,
            visible_segs: 0,
    buffer_prefix_segs: 0,
    dirty_rect: None,
            visible: VisibleSet::default(),
        }
    }

    /// 容量足够时复用同一缓冲（稳态零 create_buffer）；返回是否重建了缓冲。
    fn ensure_capacity(&mut self, device: &wgpu::Device, size: u64) -> bool {
        if self.capacity < size {
            self.capacity = (size * 2).max(4096);
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("viewstage.annotation.static"),
                size: self.capacity,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            true
        } else {
            false
        }
    }

    fn refresh_bind_group(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniform: &wgpu::Buffer,
    ) {
        self.bind_group =
            make_segment_bind_group(device, layout, uniform, &self.buffer, &self.palette.buffer);
    }
}

/// grow-only 段缓冲槽（动态批与悬停光标共用形态）。
#[derive(Debug)]
struct SegUploadSlot {
    buffer: wgpu::Buffer,
    capacity: u64,
    bind_group: wgpu::BindGroup,
    seg_count: u32,
    staging: Vec<PackedSeg>,
    palette: SlotPalette,
}

impl SegUploadSlot {
    fn empty(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, uniform: &wgpu::Buffer) -> Self {
        let initial = 4096u64;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewstage.annotation.segments"),
            size: initial,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let palette = SlotPalette::new(device, "viewstage.annotation.segments.palette");
        let bind_group = make_segment_bind_group(device, layout, uniform, &buffer, &palette.buffer);
        Self {
            buffer,
            capacity: initial,
            bind_group,
            seg_count: 0,
            staging: Vec::new(),
            palette,
        }
    }

    /// 容量足够时仅覆盖写入同一缓冲（稳态零 create_buffer）；
    /// 返回是否重建了缓冲——重建时调用方必须刷新绑定组。
    fn ensure_capacity(&mut self, device: &wgpu::Device, size: u64) -> bool {
        if self.capacity < size {
            // 2 倍扩容减少反复重建；旧缓冲随赋值自动释放
            self.capacity = (size * 2).max(4096);
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("viewstage.annotation.segments"),
                size: self.capacity,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            true
        } else {
            false
        }
    }

    fn refresh_bind_group(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniform: &wgpu::Buffer,
    ) {
        self.bind_group =
            make_segment_bind_group(device, layout, uniform, &self.buffer, &self.palette.buffer);
    }
}

/// 槽位私有调色板：CPU 侧 [`ColorPalette`] + GPU 侧 `array<u32>` 缓冲。
///
/// 每个槽一张表，互不干扰：静态层的表随文档增长（笔色数级别），
/// 动态层只有当前笔画 + 轨迹（几十条），光标层 1~2 条。分表的好处是
/// 动态层每帧重打包尾层时，interning 命中率高且表不会因文档变大而抖动。
#[derive(Debug)]
struct SlotPalette {
    cpu: ColorPalette,
    buffer: wgpu::Buffer,
    capacity: u64,
    /// 已上传条目数（稳态只追加增量）。
    uploaded: usize,
    /// 上传时的 [`ColorPalette::epoch`]；不等即表示表被重置过。
    uploaded_epoch: u64,
}

impl SlotPalette {
    fn new(device: &wgpu::Device, label: &str) -> Self {
        // 最小 4B：空表也要有合法缓冲（绑定要求 size > 0）
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            cpu: ColorPalette::new(),
            buffer,
            capacity: 4,
            uploaded: 0,
            uploaded_epoch: 0,
        }
    }

    /// 把新增条目追加上传。**返回 `true` 表示表被重置过**——已驻留段里的
    /// 索引全部失效，调用方必须整层重写（否则会画出别人的颜色）。
    fn sync(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
    ) -> bool {
        let reset = self.cpu.epoch() != self.uploaded_epoch;
        if !reset && self.cpu.len() == self.uploaded {
            return false; // 无新增：零上传
        }
        // 追加时只写 [uploaded, len)；重置后从头全写
        let mut first = if reset { 0 } else { self.uploaded };
        let needed = ((self.cpu.len() * 4) as u64).max(4);
        if self.capacity < needed {
            self.capacity = needed.next_power_of_two().max(64);
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("viewstage.annotation.palette"),
                size: self.capacity,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            first = 0; // 换了缓冲，旧内容不在了
        }
        let mut bytes: Vec<u8> = Vec::with_capacity((self.cpu.len() - first) * 4);
        for entry in &self.cpu.entries()[first..] {
            bytes.extend_from_slice(&entry.to_le_bytes());
        }
        if !bytes.is_empty() {
            crate::gpu::write_buffer_belt(
                encoder,
                belt,
                device,
                &self.buffer,
                (first * 4) as u64,
                &bytes,
            );
        }
        self.uploaded = self.cpu.len();
        self.uploaded_epoch = self.cpu.epoch();
        reset
    }
}

/// 静态层离屏光栅缓存（按密度启用，见 [`raster_cache`] 模块文档）。
///
/// 生命周期：
/// - [`Self::prepare_step`] 在 `prepare` 里判定是否启用；启用且键失效时
///   **把静态层重画进缓存纹理**（独立 render pass，透明底），并记下键；
/// - [`Self::blit`] 在直通 pass 开头把缓存贴上来，随后动态层/光标照常画；
/// - 未启用时 [`Self::blit`] 不做事，静态层由 `draw` 走原来的直接绘制路径。
#[derive(Debug)]
struct StaticRasterCache {
    texture: Option<wgpu::Texture>,
    view: Option<wgpu::TextureView>,
    bind_group: Option<wgpu::BindGroup>,
    blit_pipeline: Option<wgpu::RenderPipeline>,
    /// 局部重画用：把脏矩形擦成透明（无混合，replace 语义）。
    clear_rect_pipeline: Option<wgpu::RenderPipeline>,
    /// blit 采样器（1:1 像素映射用最近邻；wgpu 27 的 `Sampler` 无 `Default`，
    /// 必须经 `device.create_sampler` 创建，故惰性持有）。
    sampler: Option<wgpu::Sampler>,
    /// 缓存纹理的编码格式（surface 去掉 sRGB 后缀）。
    format: Option<wgpu::TextureFormat>,
    size: (u32, u32),
    /// 缓存里**已画到的段水位**（段索引）。`[0, rasted_upto)` 的可见段像素
    /// 已在纹理里（`rasted_upto` 之前不可见的段不算——矩形不变时它们持续不可见）。
    ///
    /// 追加笔画只需从该水位往后补画；水位之下的像素永不重画。橡皮擦/撤销/
    /// 清空会让缓冲前缀断裂（`buffer_prefix_segs < rasted_upto`），从而退回全量。
    rasted_upto: u32,
    /// 画缓存时的**像素可复用性键**（`None` = 从未画过）。
    params: Option<RasterParams>,
    /// 参数键连续稳定的帧数（达到 [`STABLE_FRAMES`] 才允许启用）。
    stable_frames: u8,
    /// 本帧缓存是否有效（`prepare_step` 刚画好）。
    valid: bool,
    /// 上一帧是否真的用到了缓存（统计/HUD 用）。
    used_last_frame: bool,
    /// 缓存里没有任何可见墨迹（空文档 / 全部移出视野）：此时 blit 纯属
    /// 白读一张全屏纹理，直接跳过。
    empty: bool,
}

impl Default for StaticRasterCache {
    fn default() -> Self {
        Self {
            texture: None,
            view: None,
            sampler: None,
            bind_group: None,
            blit_pipeline: None,
            clear_rect_pipeline: None,
            format: None,
            size: (0, 0),
            rasted_upto: 0,
            params: None,
            stable_frames: 0,
            valid: false,
            used_last_frame: false,
            empty: true,
        }
    }
}

impl StaticRasterCache {
    /// 纹理/管线按需创建（低密度文档永不分配那 33MB）。
    fn ensure_resources(
        &mut self,
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        size: (u32, u32),
    ) {
        // 缓存存「与帧一样的编码值」：去掉 sRGB 后缀，采样时不做额外解码
        let format = surface_format.remove_srgb_suffix();
        if self.size == size && self.format == Some(format) && self.texture.is_some() {
            return;
        }
        self.size = size;
        self.format = Some(format);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewstage.annotation.raster_cache"),
            size: wgpu::Extent3d {
                width: size.0.max(1),
                height: size.1.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // 采样器：blit 是 1:1 像素映射，用最近邻即可（少一次双线性取样）
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("viewstage.annotation.blit.sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let layout = raster_cache::blit_bind_group_layout(device);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewstage.annotation.blit.bind_group"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        self.sampler = Some(sampler);
        self.blit_pipeline = Some(raster_cache::blit_pipeline(device, &layout, format));
        // 零绑定组布局，可以独立于 blit 的 layout 创建。
        self.clear_rect_pipeline = Some(raster_cache::clear_rect_pipeline(device, format));
        self.texture = Some(texture);
        self.view = Some(view);
        self.bind_group = Some(bind_group);
        // **纹理刚换 = 里面一个像素都没有**。必须把水位清零，否则下一帧的增量
        // 判定会以为 `[0, rasted_upto)` 还在纹理里，于是只补画尾部 → 画面上
        // 凭空少一大片（旧纹理的内容从没进过新纹理）。同理 `valid` 归 false。
        self.rasted_upto = 0;
        self.valid = false;
    }

    /// 彻底放弃缓存里的**像素**（禁用、密度不足、密度变化等）。水位一并清零——
/// 留着会让下次启用时误以为还有可复用的像素。
    ///
    /// **不要清 `self.params`。** 它是稳定帧计数器的输入，不是像素复用凭据：
    /// 清掉会造成死循环——第一帧 `stable=1` → 判定未启用 → 清 `params` →
    /// 第二帧又读到 `None` → 又是 `stable=1`，永远出不来。
    /// （实测踩过：`stable=1 params_match=false` 每帧都成立，缓存从未启用。）
    ///
    /// `params` 该由"参数是否变化"自然淘汰，不需要这里手动清。
    fn invalidate_content(&mut self) {
        self.rasted_upto = 0;
        self.valid = false;
        self.used_last_frame = false;
        self.empty = true;
    }

    /// `prepare` 阶段：判定是否启用，必要时把静态层重画进缓存。
    ///
    /// 返回**本帧是否可以直接 blit 缓存**。注意重画缓存的那一帧返回
    /// `false`——那一帧已经在把静态层画进缓存纹理了，再 blit 回来等于
    /// 光栅化 + 全屏拷贝，比直接画更差。收益从下一帧才开始。
    #[allow(clippy::too_many_arguments)]
    fn prepare_step(
        &mut self,
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        size: (u32, u32),
        params: RasterParams,
        visible_segs: u64,
        pipeline: &wgpu::RenderPipeline,
        slot: &StaticUploadSlot,
        canvas_to_physical: &dyn Fn(f32, f32) -> (f32, f32),
        encoder: &mut wgpu::CommandEncoder,
    ) -> bool {
        // 参数稳定性：平移/缩放时变换每帧变 → 缓存每帧重画反而更慢。
        // 书写时矩阵、视口、剔除矩形都不动（只有动态层在变，而动态层不在缓存里），
        // 于是画一次、之后每帧一次 blit；追加新笔画时同样稳定，只是多补画一段。
        let prev_params = self.params;
        let stable = if prev_params == Some(params) {
            self.stable_frames.saturating_add(1)
        } else {
            1
        };
        self.stable_frames = stable;
        self.params = Some(params);

        let enabled = stable >= STABLE_FRAMES && raster_cache::worth_caching(visible_segs, size);
        // 门控探针。这个门曾让缓存在整个项目周期里**从未启用过**，而症状
        // （`stable=1`、缓存恒不命中）在别处完全看不出来——密度门有 HUD 数字，
        // 稳定帧门没有。默认关闭，`ICED_RASTER_TRACE=1` 打开。
        if std::env::var_os("ICED_RASTER_TRACE").is_some() {
            log::info!(
                "[raster_gate] stable={stable}/{} vs={visible_segs} 值得={} 参数匹配={}",
                STABLE_FRAMES,
                raster_cache::worth_caching(visible_segs, size),
                prev_params == Some(params),
            );
        }
        if !enabled {
            self.invalidate_content();
            return false;
        }

        // ---- 三档：纯 blit / 增量补画 / 局部重画 / 全量 ----
        //
        // | 档位 | 条件 | 代价 |
        // | --- | --- | --- |
        // | 纯 blit | 键未变、前缀覆盖水位、无新可见段 | 一次全屏拷贝 |
        // | 增量补画 | 键未变、前缀覆盖水位、有新段 | 只光栅新段（`Load`） |
        // | 局部重画 | 键未变、前缀断裂、但有脏矩形 | 脏矩形内清+重绘 |
        // | 全量 | 键变了 / 前缀为 0 / 脏矩形退化 | 整层清+重绘 |
        //
        // 局部重画是"橡皮擦 / 撤销 / 笔画被后台线程换成扣除版"这三条路径的
        // 解药：它们都会让 `common_prefix_len` 断裂，于是旧实现整层重画。
        // 而它们实际影响的只是一小块区域——重画整屏密度文档纯属浪费。
        let can_extend = raster_cache::extendable(
            self.valid,
            prev_params == Some(params),
            slot.buffer_prefix_segs,
            self.rasted_upto,
        );

        let from = if can_extend { self.rasted_upto } else { 0 };
        let tail = raster_cache::tail_ranges(&slot.visible.ranges, from);
        if can_extend && tail.is_empty() {
            // 没有新段要画 → 本帧纯 blit，不开 render pass。
            self.used_last_frame = true;
            return true;
        }

        self.ensure_resources(device, surface_format, size);
        let Some(view) = self.view.as_ref() else {
            self.invalidate_content();
            return false;
        };

        // ---- 选档 ----
        //
        // `scissor` 为 `Some` 即局部重画：脏矩形能裁到附件内，且参数键未变
        // （否则缓存里已有像素的位置全错了，局部重画只会留下错位的残影）。
        let scissor = if can_extend {
            None
        } else if self.valid && prev_params == Some(params) {
            slot.dirty_rect.and_then(|rect| {
                raster_cache::scissor_from_rect(rect, canvas_to_physical, size)
            })
        } else {
            None
        };
        let partial = scissor.is_some();
        // 局部重画重绘**全部可见段**（靠 scissor 限制光栅范围），不是只画
        // 脏矩形里的段 —— 后者需要按笔画 AABB 再剔一次，收益远小于复杂度。
        let draw_ranges = if partial {
            slot.visible.ranges.clone()
        } else {
            tail
        };

        // 把静态层画进缓存。
        //
        // - **全量**：`Clear` + 画全部可见段。
        // - **增量**：`Load` + 只画 `tail`（水位之后的新段）。绝不能画到水位
        //   之前——预乘 source-over 叠加两次会让墨迹变深，表现为"每收一笔
        //   颜色变浓一次"。
        // - **局部**：`Load` + scissor 限制 + 先把脏矩形擦成透明再重绘。
        //   scissor **不作用于 Clear**，所以擦除必须是"画一个透明矩形"。
        //
        // 录进**帧 encoder**（不另建 submit）：DirectChannel 的约定是
        // 所有命令随主帧统一提交，而缓存 pass 只在需要补画时出现。
        {
            let clear_rect = if partial {
                self.clear_rect_pipeline.as_ref()
            } else {
                None
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewstage.annotation.raster_cache.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if can_extend || partial {
                            wgpu::LoadOp::Load
                        } else {
                            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if let Some([x, y, w, h]) = scissor {
                pass.set_scissor_rect(x, y, w, h);
            }
            if let Some(clear) = clear_rect {
                pass.set_pipeline(clear);
                pass.draw(0..3, 0..1);
            }
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &slot.bind_group, &[]);
            for &(start, len) in &draw_ranges {
                if len > 0 {
                    pass.draw(0..6, start..start + len);
                }
            }
        }
        let drew_any = draw_ranges.iter().any(|&(_, l)| l > 0);
        // 统计：增量 / 局部 / 全量，以及补画了多少段。这是"增量与局部真的在跑"
        // 的唯一运行时证据——分支是否被走到，读代码看不出来。
        let tail_segs: u64 = draw_ranges.iter().map(|&(_, l)| l as u64).sum();
        crate::stats::note_raster_pass(can_extend, partial, tail_segs);
        self.valid = true;
        // 水位推进到总段数。不可见的新段也算推进：矩形不变时它们持续不可见，
        // 缓存里没有它们是正确的。
        self.rasted_upto = slot.seg_count;
        // 缓存里可能一笔墨都没有（空文档 / 全被剔除）：记下来让 blit 跳过。
        // 增量时只能"从空变非空"，不能让已有的非空退回空。
        self.empty = if can_extend { self.empty && !drew_any } else { !drew_any };
        self.used_last_frame = false; // 本帧走直接绘制，缓存从下一帧起用
        false
    }

    /// 直通 pass 开头：把缓存贴上来（未启用时无操作）。
    fn blit(&self, pass: &mut wgpu::RenderPass<'_>) {
        if !self.valid || !self.used_last_frame || self.empty {
            return;
        }
        let (Some(pipeline), Some(bind_group)) =
            (self.blit_pipeline.as_ref(), self.bind_group.as_ref())
        else {
            return;
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn make_segment_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniform: &wgpu::Buffer,
    buffer: &wgpu::Buffer,
    palette: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewstage.annotation.bind_group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: palette.as_entire_binding(),
            },
        ],
    })
}

/// 独立批注渲染管线（创建一次，跨帧常驻）。
#[derive(Debug)]
pub struct AnnotationPipeline {
    device: wgpu::Device,
    /// 直通 pass 的目标格式（缓存纹理据此去掉 sRGB 后缀，保证 blit 精确）。
    surface_format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    /// 静态层单缓冲上传槽（视口剔除 + 前缀增量 + 缓冲复用；持久化状态内聚于槽）。
    static_slot: StaticUploadSlot,
    dynamic_slot: SegUploadSlot,
    // ---- 动态段两层上传状态：稳定层（已满批块，只追加）+ 尾层（头部/轨迹，逐帧重传）----
    /// 稳定层指纹（chunks++eraser_chunks 的 ptr+len 混合）
    dyn_stable_fp: u64,
    /// 尾层指纹（head_index + head/trail 的 ptr+len+presence）
    dyn_tail_fp: u64,
    /// 已持久化到 GPU 的稳定段总数
    dyn_stable_segs: u32,
    /// 已持久化的 chunks / eraser_chunks 块数（增量追加判定）
    dyn_pen_persisted: usize,
    dyn_eraser_persisted: usize,
    /// 缓冲重建后强制全量重写
    dyn_full_rewrite: bool,
    cursor_slot: SegUploadSlot,
    cursor_slot_fingerprint: u64,
    cached_matrix: Option<[f32; 16]>,
    physical_size: (u32, u32),
}

/// 视口（逻辑像素）→ 画布坐标矩形；`scale <= 0` 或视口未知时返回 `None`
/// （退化为不剔除，行为与旧实现一致）。
pub fn view_to_canvas_rect(
    viewport: (f32, f32),
    transform: &crate::gpu::ViewTransform,
) -> Option<[f32; 4]> {
    let (vw, vh) = viewport;
    if vw <= 0.0 || vh <= 0.0 || transform.scale <= 0.0 {
        return None;
    }
    let inv = 1.0 / transform.scale;
    Some([
        (0.0 - transform.offset_x) * inv,
        (0.0 - transform.offset_y) * inv,
        (vw - transform.offset_x) * inv,
        (vh - transform.offset_y) * inv,
    ])
}

/// 静态层每帧 draw 区间数上限；超过则退化为单次全量 draw。
///
/// 区间数 = 可见笔画的**连续游程**数。相邻笔画在缓冲里本就连续，会被并成
/// 一个区间，所以正常笔迹密度下区间数远小于可见笔画数。但极端情形（大量
/// 零散标记）可能退化成"逐笔一区间"，此时逐区间 draw 的驱动开销会超过
/// 剔除省下的顶点开销——用这个上限把最坏情况钉死在 1 次 draw。
pub const MAX_STATIC_DRAWS: usize = 64;

/// 可见集：绘制区间 + 统计（`cull_visible` 的产物，容量复用）。
#[derive(Debug, Default)]
pub struct VisibleSet {
    /// 绘制实例区间 `(起始段, 段数)`，升序、不相交、段数非零。
    pub ranges: Vec<(u32, u32)>,
    /// 可见笔画数。
    pub strokes: usize,
    /// 可见段数（各笔画段数之和，不受区间合并影响）。
    pub segs: u64,
}

/// 驻留式静态层的剔除：按笔画 AABB 挑出该画的段区间。
///
/// GPU 缓冲装着**全部**笔画段（平移不再触发任何重传），这里每帧只算出该画
/// 哪几段。着色器无需改动——`@builtin(instance_index)` 本就包含
/// `firstInstance` 偏移，`segments[instance_index]` 天然取到正确的段。
///
/// - `ranges` 与 `snapshot` 同序（逐笔追加保证），按 `zip` 配对，长度不符
///   时以较短的为准（防御性）；
/// - `rect` 为 `None`（视口未知）→ 单区间 `0..总段数`，即不剔除；
/// - 相邻可见笔画自动并成一段，避免逐笔一次 draw；
/// - 区间数超过 [`MAX_STATIC_DRAWS`] → 退化为单区间全量（最坏 1 次 draw）。
///
/// `out` 的 `ranges` 会被清空重填（容量复用），`strokes`/`segs` 覆写。
pub fn cull_visible(
    ranges: &[(u32, u32)],
    snapshot: &[StaticStrokeRef],
    rect: Option<&[f32; 4]>,
    total_segs: u32,
    out: &mut VisibleSet,
) {
    out.ranges.clear();
    out.strokes = 0;
    out.segs = 0;

    let Some(rect) = rect else {
        if total_segs > 0 {
            out.ranges.push((0, total_segs));
        }
        return;
    };

    for (&(start, len), stroke) in ranges.iter().zip(snapshot) {
        if len == 0 || !stroke.intersects(rect) {
            continue;
        }
        out.strokes += 1;
        out.segs += len as u64;
        match out.ranges.last_mut() {
            // 与上一区间首尾相接 → 并入，避免逐笔一次 draw
            Some(last) if last.0 + last.1 == start => last.1 += len,
            _ => {
                if out.ranges.len() >= MAX_STATIC_DRAWS {
                    // 区间太多：逐区间 draw 的开销超过剔除收益，退化为全量。
                    // 统计仍按真实可见集（与绘制无关，仅供 HUD）。
                    out.ranges.clear();
                    if total_segs > 0 {
                        out.ranges.push((0, total_segs));
                    }
                    return;
                }
                out.ranges.push((start, len));
            }
        }
    }
}

/// 两快照的最长公共前缀长度：逐笔要求 id、generation、几何 `Arc` 三者一致。
///
/// 快照由 mesh cache 按 id 升序重建（`refresh_static_snapshot`），同一几何
/// clone 保持指针不变，因此：
/// - 书写追加 → 新快照严格以旧快照为前缀，返回 `prev.len()`；
/// - 橡皮擦 targeted rebuild / undo → 被改笔画的 generation 或几何变化，
///   前缀在该笔画处截断（返回值 < `prev.len()`）。
fn common_prefix_len(prev: &[StaticStrokeRef], next: &[StaticStrokeRef]) -> usize {
    let mut i = 0;
    while i < prev.len() && i < next.len() {
        if prev[i].id != next[i].id
            || prev[i].generation != next[i].generation
            || !Arc::ptr_eq(&prev[i].geometry, &next[i].geometry)
        {
            break;
        }
        i += 1;
    }
    i
}

/// 把源段（64B，带颜色）压成 GPU 段（32B，颜色走调色板索引）。
#[inline]
fn push_packed(out: &mut Vec<PackedSeg>, seg: &StrokeSeg, palette: &mut ColorPalette) {
    // 绝大多数段两端同色：只 intern 一次（A 端），B 端命中同一键。
    let ca = palette.intern(seg.ca);
    let cb = if seg.cb == seg.ca {
        ca
    } else {
        palette.intern(seg.cb)
    };
    out.push(PackedSeg::new(seg, ca, cb));
}

/// 打包稳定层段（`chunks` ++ `eraser_chunks`）：`pen_from`/`eraser_from`
/// 为各列表的起始下标，增量追加时跳过已持久化块，全量重写时传 0。
fn pack_stable_segs(
    live: &LiveCanvasState,
    pen_from: usize,
    eraser_from: usize,
    out: &mut Vec<PackedSeg>,
    palette: &mut ColorPalette,
) {
    for g in live.chunks.borrow().iter().skip(pen_from) {
        for seg in &g.segs {
            push_packed(out, seg, palette);
        }
    }
    for g in live.eraser_chunks.borrow().iter().skip(eraser_from) {
        for seg in &g.segs {
            push_packed(out, seg, palette);
        }
    }
}

/// 打包尾层段（头部块 + 触控预测轨迹）：每次追加重建。
fn pack_tail_segs(live: &LiveCanvasState, out: &mut Vec<PackedSeg>, palette: &mut ColorPalette) {
    if let Some(head) = live.head.borrow().as_ref() {
        for seg in &head.segs {
            push_packed(out, seg, palette);
        }
    }
    if let Some((_, trail)) = live.trail.borrow().as_ref() {
        for seg in &trail.segs {
            push_packed(out, seg, palette);
        }
    }
}

impl AnnotationLayer {
    /// 静态层上传：**驻留式** + 前缀增量。缓冲内容只由笔画集合决定。
    ///
    /// - 快照未变（含**平移**）：零打包、零上传、零分配；
    /// - 书写追加（新快照以上一片为前缀）：只把新增笔画的段写在
    ///   `persisted_segs` 偏移处，上传量与**新增段数**成正比；
    /// - 橡皮擦 / undo / 缓冲重建降级：整表重传（先打包成一段连续数据，
    ///   单次写入，摊薄 belt chunk 与拷贝命令数）。
    ///
    /// 视口剔除**不在这里做**——旧设计让缓冲只装可见段，于是平移每跨过
    /// 0.25 个视口宽就要整段重传一次，几百 KB 的尖峰正好砸在平移手势中间。
    /// 现在剔除改由 [`Self::update_static_cull`] 在绘制期每帧算，平移因此
    /// **零上传**。
    fn upload_static(
        &self,
        pipeline: &mut AnnotationPipeline,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
    ) {
        const SEG_SIZE: u64 = std::mem::size_of::<PackedSeg>() as u64;

        // 借用而非克隆：稳态下零 Arc 原子操作（旧实现每帧无条件 clone 一次）
        let strokes_borrow = self.live.static_snapshot.borrow();
        let strokes: &Arc<[StaticStrokeRef]> = &strokes_borrow;
        let slot = &mut pipeline.static_slot;

        // ---- 路径 1：快照未变 → 零工作。视口变化不参与判定（剔除在绘制期）。
        if slot.built
            && slot
                .snapshot
                .as_ref()
                .is_some_and(|prev| Arc::ptr_eq(prev, strokes))
        {
            // 快照未变 → 整表一致，水位就是全部段数。
            slot.buffer_prefix_segs = slot.seg_count;
            return;
        }

        // 调色板：新增条目只追加上传（几 KB），**表被重置**则本层必须整表
        // 重写——已驻留段里的索引会指向别人的颜色。
        let palette_reset = slot.palette.sync(device, encoder, belt);

        // ---- 规划本次写入：从第一个与上次不同的索引起重写 ----
        //
        // 原实现只有"尾部追加"（`is_prefix_append`）与"整表重传"两档，漏掉了
        // **中间某笔的几何被替换**：新增笔画时 `finalize_stroke` 先塞一份
        // "未扣橡皮擦"的兜底网格，后台线程回来后把该条目换成扣除版——该条目
        // 变了，前缀断裂，于是整份文档全量重传（实测 2.4MB→5.4MB，prepare
        // 单帧峰值 79ms，收笔必顿）。
        //
        // 通用做法：取 `common_prefix_len`（逐项比对 id / generation /
        // geometry 指针），保留 `[0, keep)` 的驻留字节，从 `keep` 起重写。
        // 这同时覆盖"尾部新增"与"中间替换"，且内容完全相同时（keep == len）
        // 一次字节都不写。
        //
        // `base_segs` = 缓冲里已驻留且仍有效的段数（整表重传时为 0，从偏移 0
        // 覆盖写）；`tail_segs` = 本次要写入的段数。容量上界取二者之和——两条
        // 路径都不会多申请（把整表路径也算成 `persisted + all` 会在 undo 缩表
        // 时触发一次无谓的 2 倍扩容）。
        let keep = if !palette_reset && slot.built {
            slot.snapshot
                .as_ref()
                .map(|prev| common_prefix_len(prev, strokes).min(strokes.len()))
                .unwrap_or(0)
        } else {
            0
        };
        let seg_count = |refs: &[StaticStrokeRef]| -> usize {
            refs.iter().map(|s| s.geometry.segs.len()).sum()
        };
        let base_segs = seg_count(&strokes[..keep]);
        let tail_from = keep;
        let tail_segs = seg_count(&strokes[tail_from..]);
        let target_bytes = (((base_segs + tail_segs) as u64) * SEG_SIZE).max(1);

        let rebuilt = slot.ensure_capacity(device, target_bytes);
        if rebuilt {
            // 缓冲已换新：旧前缀随旧缓冲失效，绑定组必须重指
            slot.refresh_bind_group(device, &pipeline.bind_group_layout, &pipeline.uniform_buffer);
        }
        // 重建后退化为整表重传（否则偏移前缀会写进新缓冲的空白区）
        let incremental = keep > 0 && !rebuilt;

        // 本次写入后，缓冲里与 CPU 快照一致的前缀段数。非追加路径（`keep == 0`
        // 或缓冲重建）只能承认"不知道"，即 0 —— 强制下游全量重画。
        slot.buffer_prefix_segs = if incremental { base_segs as u32 } else { 0 };

        // ---- 脏矩形：变化笔画在**新旧两份快照**里的 AABB 之并 ----
        //
        // 局部重画要擦掉脏矩形再重绘，所以矩形必须同时覆盖：
        //
        // - **新**快照 `[keep, len)` 的 bounds —— 变化后的墨迹在哪；
        // - **旧**快照 `[keep, prev.len)` 的 bounds —— 旧墨迹原来在哪。
        //
        // 第二个来源不能省：橡皮擦把一笔擦短/擦掉时，新 bounds 只覆盖剩下的
        // 部分，被擦掉的旧墨迹在矩形之外——漏掉它就会在屏幕上留一块擦不掉的
        // 残影，而且这种错误不会崩、只会静静留在画面上。
        //
        // `keep == 0`（整表不一致）时无法界定范围 → `None` → 退回全量。
        slot.dirty_rect = if keep == 0 {
            None
        } else {
            let prev = slot.snapshot.as_deref();
            let mut rect: Option<[f32; 4]> = None;
            for s in &strokes[keep..] {
                rect = raster_cache::union_rect(rect, s.bounds);
            }
            if let Some(prev) = prev {
                for s in prev.iter().skip(keep) {
                    rect = raster_cache::union_rect(rect, s.bounds);
                }
            }
            rect
        };

        if incremental {
            // ---- 路径 2：偏移增量（书写热路径，通常只有一笔）----
            if tail_segs == 0 {
                // 内容逐项相同：快照换了 Arc 但段表一个字节都不用动。
                // 此刻整表都与快照一致，水位推到末尾。
                slot.built = true;
                slot.snapshot = Some(Arc::clone(strokes));
                slot.buffer_prefix_segs = slot.seg_count;
                return;
            }
            // 保留的前缀在 staging / 段表里也只保留对应部分：被替换条目的
            // 旧打包数据必须丢弃，否则段表偏移与缓冲内容会错位。
            //
            // 单位是**元素个数**（`staging` 是 `Vec<PackedSeg>`），不是字节。
            // 写成 `base_segs * SEG_SIZE` 会把长度上限放大 32 倍，`truncate`
            // 变成空操作——旧打包数据就一直留在 staging 里无限增长。
            slot.staging.truncate(base_segs);
            slot.stroke_ranges.truncate(keep);
            let mut offset = base_segs as u32;
            let mut staged_from = slot.staging.len();
            for stroke in &strokes[tail_from..] {
                // 逐笔打包（调色板 append-only：旧段的索引不受影响）
                for seg in &stroke.geometry.segs {
                    push_packed(&mut slot.staging, seg, &mut slot.palette.cpu);
                }
                let len = stroke.geometry.segs.len() as u32;
                if len > 0 {
                    write_buffer_belt(
                        encoder,
                        belt,
                        device,
                        &slot.buffer,
                        offset as u64 * SEG_SIZE,
                        bytemuck::cast_slice(&slot.staging[staged_from..]),
                    );
                }
                slot.stroke_ranges.push((offset, len));
                offset += len;
                staged_from = slot.staging.len();
            }
            crate::stats::add_static_upload(tail_segs as u64 * SEG_SIZE);
            slot.persisted_segs = offset;
            slot.seg_count = offset;
        } else {
            // ---- 路径 3：整表重传（首帧 / 橡皮擦 / undo / 缓冲重建降级）----
            // 先打包再单次写入：逐笔一次 `write_buffer_belt` 会为每笔各申请
            // 一个 belt chunk 与一条拷贝命令，N 笔就是 N 倍驱动开销。
            slot.staging.clear();
            for stroke in strokes.iter() {
                for seg in &stroke.geometry.segs {
                    push_packed(&mut slot.staging, seg, &mut slot.palette.cpu);
                }
            }
            slot.stroke_ranges.clear();
            if !slot.staging.is_empty() {
                write_buffer_belt(
                    encoder,
                    belt,
                    device,
                    &slot.buffer,
                    0,
                    bytemuck::cast_slice(&slot.staging),
                );
                crate::stats::add_static_upload(slot.staging.len() as u64 * SEG_SIZE);
            }
            let mut offset = 0u32;
            for stroke in strokes.iter() {
                let len = stroke.geometry.segs.len() as u32;
                slot.stroke_ranges.push((offset, len));
                offset += len;
            }
            slot.persisted_segs = offset;
            slot.seg_count = offset;
        }

        slot.built = true;
        slot.snapshot = Some(Arc::clone(strokes));
        // 段表内容已变 → 离屏缓存必须重画
        slot.write_serial = slot.write_serial.wrapping_add(1);
    }

    /// 静态层每帧剔除：算绘制区间（O(笔画数)，无 memcpy 无上传）。
    ///
    /// 与 [`Self::upload_static`] 分开是必要的——上传路径在快照未变时**提前
    /// 返回**，而可见性每帧都在变（平移/缩放/窗口尺寸）。两者都只读同一份
    /// 借用，不产生冲突。
    fn update_static_cull(
        &self,
        pipeline: &mut AnnotationPipeline,
        transform: &crate::gpu::ViewTransform,
    ) {
        let strokes_borrow = self.live.static_snapshot.borrow();
        let strokes: &Arc<[StaticStrokeRef]> = &strokes_borrow;
        let view_rect = view_to_canvas_rect(self.live.viewport.get(), &transform);
        let slot = &mut pipeline.static_slot;

        cull_visible(
            &slot.stroke_ranges,
            strokes,
            view_rect.as_ref(),
            slot.seg_count,
            &mut slot.visible,
        );
        slot.visible_count = slot.visible.strokes;
        slot.visible_segs = slot.visible.segs;
        crate::stats::set_static_visible(slot.visible.strokes, strokes.len());
        crate::stats::set_static_resident(
            slot.seg_count as u64,
            slot.visible.ranges.len() as u64,
        );
        Self::check_segment_table_invariant(slot, strokes.len());
    }

    /// 段表自洽性检查（每帧 O(1)，只读，不扰动时序）。
    ///
    /// "绘制完成后画面闪一下"最可能的结构性成因就是**收笔那一帧该笔画不在绘制
    /// 列表里**——动态层已清空、静态层还没画上，画面上就是一闪。这个不变量
    /// 正是该假设的直接检验：上传后段表必须与快照**逐笔对齐**（长度相等、
    /// 各段长度之和等于已驻留段数）。任何一帧对不齐，说明那一帧确实漏笔画。
    ///
    /// 之所以在这里查而不是查"最新笔画是否可见"：后者需要把最新 stroke id
    /// 一路传进来；而长度/求和对齐是等价且更便宜的判据——漏掉任何一笔都会
    /// 让两者不等。
    ///
    /// 正常帧不该触发，故按 60 帧节流告警，避免刷屏。
    fn check_segment_table_invariant(slot: &StaticUploadSlot, snapshot_len: usize) {
        let ranges = slot.stroke_ranges.len();
        let sum: u64 = slot
            .stroke_ranges
            .iter()
            .map(|&(_, len)| u64::from(len))
            .sum();
        let segs = u64::from(slot.seg_count);
        let persisted = u64::from(slot.persisted_segs);
        if ranges == snapshot_len && sum == segs && persisted == segs {
            return;
        }
        const THROTTLE: u64 = 60;
        let n = DEBUG_SEGTABLE_WARN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n % THROTTLE == 0 {
            log::warn!(
                "[segtable] 段表与快照不对齐：snapshot={snapshot_len} ranges={ranges} \
                 sum_len={sum} seg_count={segs} persisted={persisted}（累计 {n} 次）"
            );
        }
    }

    /// 动态几何两层上传。稳定层（已满批 chunks ++ 橡皮擦轨迹批）不可变、
    /// 只追加，新块完成时以偏移增量写入一次后不再触碰；尾层（头部块 +
    /// 触控预测轨迹）每次追加重写。旧实现每帧重打包全部动态段（随笔画
    /// 长度线性增长），本实现每移动仅上传尾层数 KB + 新满批块增量。
    fn upload_dynamic(
        &self,
        pipeline: &mut AnnotationPipeline,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
    ) {
        let live = &self.live;
        let stable_fp = self.dyn_stable_fingerprint();
        let tail_fp = self.dyn_tail_fingerprint();

        // 缓冲重建后强制全量重写
        if stable_fp == pipeline.dyn_stable_fp
            && tail_fp == pipeline.dyn_tail_fp
            && !pipeline.dyn_full_rewrite
        {
            return; // 无变化：零打包、零上传
        }

        const SEG_SIZE: u64 = std::mem::size_of::<PackedSeg>() as u64;
        let slot = &mut pipeline.dynamic_slot;

        // 调色板增量上传；表被重置 → 整层重写（已驻留索引失效）
        let palette_reset = slot.palette.sync(&pipeline.device, encoder, belt);
        if palette_reset {
            pipeline.dyn_full_rewrite = true;
        }

        // 稳定层判定：块数只增不减（reset 走全量分支）。
        // 三种情形：
        // - 稳定层无变化（fp 与块数均与已持久化一致）：前缀原样保留，只写尾层
        //   ——书写时绝大多数帧属于此情形，上传量恒为尾层数 KB；
        // - 稳定层追加（新满批块）：偏移增量写入新增块；
        // - 其余（首帧 / reset / 缓冲重建 / 调色板重置）：稳定层整体重写。
        let pen_len = live.chunks.borrow().len();
        let er_len = live.eraser_chunks.borrow().len();
        let stable_unchanged = !pipeline.dyn_full_rewrite
            && pen_len == pipeline.dyn_pen_persisted
            && er_len == pipeline.dyn_eraser_persisted
            && stable_fp == pipeline.dyn_stable_fp;
        let append_only = !pipeline.dyn_full_rewrite
            && !stable_unchanged
            && pen_len >= pipeline.dyn_pen_persisted
            && er_len >= pipeline.dyn_eraser_persisted
            && stable_fp != pipeline.dyn_stable_fp;

        // ---- 打包写入段（staging = [新增稳定段?] + [尾层]）----
        slot.staging.clear();
        let mut write_offset_segs: u32 = 0;

        if stable_unchanged {
            // 前缀已持久化：只打包尾层（见下），写在已持久化段数之后
            write_offset_segs = pipeline.dyn_stable_segs;
        } else if append_only {
            // 增量：只打包新增稳定块，写在已持久化前缀之后
            write_offset_segs = pipeline.dyn_stable_segs;
            pack_stable_segs(
                live,
                pipeline.dyn_pen_persisted,
                pipeline.dyn_eraser_persisted,
                &mut slot.staging,
                &mut slot.palette.cpu,
            );
        } else {
            // 全量：稳定层整体重写
            pack_stable_segs(live, 0, 0, &mut slot.staging, &mut slot.palette.cpu);
        }
        pipeline.dyn_pen_persisted = pen_len;
        pipeline.dyn_eraser_persisted = er_len;

        // 加上前缀即稳定层段总数（增量分支的 staging 只含新增块）
        let mut stable_segs = write_offset_segs + slot.staging.len() as u32;

        // ---- 尾层打包（head + trail）----
        pack_tail_segs(live, &mut slot.staging, &mut slot.palette.cpu);

        // ---- 容量 + 上传 ----
        // 缓冲内段总数 = 已持久化前缀 + 本帧打包段：增量写在偏移之后，
        // 容量必须按总数判定（前缀只增不减，按打包量判定迟早越界）。
        let mut total_segs = write_offset_segs + slot.staging.len() as u32;
        if slot.ensure_capacity(&pipeline.device, total_segs as u64 * SEG_SIZE) {
            // 缓冲重建 → 绑定组刷新；旧缓冲里的前缀内容随之失效，
            // 增量帧改为整层重写（从偏移 0 起打包全部段）。
            slot.refresh_bind_group(
                &pipeline.device,
                &pipeline.bind_group_layout,
                &pipeline.uniform_buffer,
            );
            if write_offset_segs != 0 {
                slot.staging.clear();
                pack_stable_segs(live, 0, 0, &mut slot.staging, &mut slot.palette.cpu);
                stable_segs = slot.staging.len() as u32;
                pack_tail_segs(live, &mut slot.staging, &mut slot.palette.cpu);
                write_offset_segs = 0;
                total_segs = slot.staging.len() as u32;
            }
        }
        if !slot.staging.is_empty() {
            write_buffer_belt(
                encoder,
                belt,
                &pipeline.device,
                &slot.buffer,
                write_offset_segs as u64 * SEG_SIZE,
                bytemuck::cast_slice(&slot.staging),
            );
            crate::stats::add_dynamic_upload(slot.staging.len() as u64 * SEG_SIZE);
        }

        // draw 覆盖缓冲前缀 + 本帧段；staging 为空时前缀仍需绘制
        slot.seg_count = total_segs;
        pipeline.dyn_stable_segs = stable_segs;
        pipeline.dyn_tail_fp = tail_fp;
        pipeline.dyn_stable_fp = stable_fp;
        pipeline.dyn_full_rewrite = false;
    }

    /// 橡皮擦悬停光标：圆盘（退化段）+ 边框环（弧段带），独立小缓冲。
    fn upload_cursor(
        &self,
        pipeline: &mut AnnotationPipeline,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
    ) {
        let fingerprint = self.cursor_fingerprint();
        if pipeline.cursor_slot_fingerprint == fingerprint {
            return;
        }
        let slot = &mut pipeline.cursor_slot;
        slot.palette.sync(&pipeline.device, encoder, belt);
        slot.staging.clear();
        if let Some(cursor) = self.live.eraser_cursor.get() {
            build_eraser_cursor_segments(cursor, &mut slot.staging, &mut slot.palette.cpu);
        }
        let bytes_len = (slot.staging.len() * std::mem::size_of::<PackedSeg>()) as u64;
        // 仅缓冲重建时才刷新绑定组；绘画期几何变化走稳态路径零绑定组开销
        if slot.ensure_capacity(&pipeline.device, bytes_len) {
            slot.refresh_bind_group(
                &pipeline.device,
                &pipeline.bind_group_layout,
                &pipeline.uniform_buffer,
            );
        }
        if !slot.staging.is_empty() {
            write_buffer_belt(
                encoder,
                belt,
                &pipeline.device,
                &slot.buffer,
                0,
                bytemuck::cast_slice(&slot.staging),
            );
            crate::stats::add_cursor_upload(bytes_len);
        }
        slot.seg_count = slot.staging.len() as u32;
        pipeline.cursor_slot_fingerprint = fingerprint;
    }
}

/// 橡皮擦悬停光标：半透明圆盘 + 实线边框环（画布坐标）。
/// 圆盘 = 中心到圆周的 24 条胶囊段；边框环 = 内外半径间的 24 条切向短段。
fn build_eraser_cursor_segments(
    cursor: crate::live::EraserCursor,
    out: &mut Vec<PackedSeg>,
    palette: &mut ColorPalette,
) {
    const SEGMENTS: usize = 24;
    let fill: [f32; 4] = [239.0 / 255.0, 68.0 / 255.0, 68.0 / 255.0, 0.12];
    let border: [f32; 4] = [239.0 / 255.0, 68.0 / 255.0, 68.0 / 255.0, 0.85];
    // 两种颜色各 intern 一次（整圈复用）
    let fill_idx = palette.intern(fill);
    let border_idx = palette.intern(border);

    let mut prev = (cursor.cx + cursor.radius, cursor.cy);
    for i in 1..=SEGMENTS {
        let angle = (i as f32) / (SEGMENTS as f32) * std::f32::consts::TAU;
        let next = (cursor.cx + cursor.radius * angle.cos(), cursor.cy + cursor.radius * angle.sin());
        // 圆盘填充：把多边形的边向内缩半个弦长，填出接近圆盘的多边形
        let chord_half = (2.0 * cursor.radius * (std::f32::consts::PI / SEGMENTS as f32 / 2.0).sin())
            .min(cursor.radius)
            * 0.5;
        out.push(PackedSeg {
            a: [cursor.cx, cursor.cy, chord_half.max(0.5), chord_half.max(0.5)],
            b: [(prev.0 + next.0) * 0.5, (prev.1 + next.1) * 0.5],
            ca: fill_idx,
            cb: fill_idx,
        });
        // 边框环：沿外缘的细带，半宽 = border
        let ring_half = cursor.border.max(0.5);
        out.push(PackedSeg {
            a: [prev.0, prev.1, ring_half, ring_half],
            b: [next.0, next.1],
            ca: border_idx,
            cb: border_idx,
        });
        prev = next;
    }
}

impl iced_wgpu::DirectChannel for AnnotationLayer {
    fn init(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) {
        // 幂等：compositor 重建等场景可能多次 init。
        if self.pipeline.is_none() {
            self.pipeline = Some(AnnotationPipeline::new(device, queue, format));
        }
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        viewport: &Viewport,
    ) {
        // 帧预算计量：AutoVsync 下错过 vsync 死线 = 整帧多等一个周期，
        // 故本段（打包 + belt 写入 + 剔除）必须可观测，见 `stats::frame_budget`。
        let started = Instant::now();
        let mut pipeline = self
            .pipeline
            .take()
            .expect("annotation pipeline not initialized");

        // 1. 变换 uniform（直读共享实时状态；变化才写，pan/zoom 零数据重传）
        let transform: crate::gpu::ViewTransform = self.live.view_transform();
        let matrix = transform.to_matrix(viewport);
        if pipeline.cached_matrix != Some(matrix) {
            let globals = Globals {
                transform: matrix,
                brightness_contrast: [0.0; 4],
            };
            write_buffer_belt(
                encoder,
                belt,
                device,
                &pipeline.uniform_buffer,
                0,
                bytemuck::cast_slice(&[globals]),
            );
            pipeline.cached_matrix = Some(matrix);
        }
        pipeline.physical_size = (viewport.physical_width(), viewport.physical_height());

        // 2. 静态笔画：**驻留式**上传（快照变化才写）+ 每帧剔除（画哪些）。
        //    上传与剔除分离是关键：上传在快照未变时提前返回，而可见性每帧都
        //    在变（pan/zoom/窗口尺寸），所以剔除不能挂在上传的早退路径上。
        self.upload_static(&mut pipeline, device, encoder, belt);
        self.update_static_cull(&mut pipeline, &transform);

        // 2.5 静态层离屏缓存（仅高密度 + 参数稳定时启用，见 `raster_cache`）。
        //     必须在剔除之后（要拿本帧可见段数判密度），且录进同一 encoder。
        {
            let surface_format = pipeline.surface_format;
            // 剔除用的矩形就是"可见集"的唯一决定因素，必须进键：矩阵不变但
            // 矩形变了（视口变化）时，可见集可能变大，而缓存里没有新可见的段。
            let view_rect = view_to_canvas_rect(self.live.viewport.get(), &transform);
            let params = RasterParams {
                matrix,
                size: pipeline.physical_size,
                view_rect,
            };
            let slot = &pipeline.static_slot;
            let blit = self.cache.prepare_step(
                device,
                surface_format,
                pipeline.physical_size,
                params,
                slot.visible_segs,
                &pipeline.pipeline,
                slot,
                &|x, y| transform.canvas_to_physical(viewport, x, y),
                encoder,
            );
            self.cache_active = blit;
            // GPU 侧成本核算（纯算术，零开销）：让「墨迹 vs 全屏 blit 谁更贵」
            // 在 HUD 上可读，而不是靠猜。数值口径见 `raster_cache`。
            let (w, h) = pipeline.physical_size;
            let blit_texels = w as u64 * h as u64;
            let ink_fragments = if blit {
                // 走缓存：静态层已烘进纹理，帧内只剩一次全屏拷贝
                0
            } else {
                slot.visible_segs * raster_cache::FRAGMENTS_PER_SEG
            };
            crate::stats::set_raster_cost(
                blit,
                ink_fragments,
                blit_texels,
                slot.visible_segs,
            );
        }

        // 3. 动态几何 + 悬停光标：指纹门控的独立小上传
        self.upload_dynamic(&mut pipeline, encoder, belt);
        self.upload_cursor(&mut pipeline, encoder, belt);

        crate::stats::end_frame();
        crate::stats::note_prepare_work(started.elapsed());

        // 归还管线（take/put-back 化解 self.pipeline 与 self.upload_* 的借用冲突）
        self.pipeline = Some(pipeline);
    }

    fn draw(&mut self, render_pass: &mut wgpu::RenderPass<'_>) {
        let pipeline = self
            .pipeline
            .as_ref()
            .expect("annotation pipeline not initialized");

        render_pass.set_scissor_rect(0, 0, pipeline.physical_size.0, pipeline.physical_size.1);

        // 静态层缓存：命中时一次全屏 blit 代替「重画整份可见文档」。
        // blit 用的是自己的纹理绑定组，draw 完要切回胶囊管线的绑定组。
        self.cache.blit(render_pass);
        crate::stats::note_raster_blit(self.cache_active);

        // 三个槽共用同一条 GPU 胶囊管线：一次 set_pipeline，随后只切 bind group
        render_pass.set_pipeline(&pipeline.pipeline);

        // 静态层：按本帧剔除出的实例区间逐段 draw。相邻可见笔画已合并，
        // 故批次数通常是"可见游程数"而非"可见笔画数"；区间过多时会退化为
        // 单次全量（见 `MAX_STATIC_DRAWS`）。着色器无改动——`instance_index`
        // 自带 firstInstance 偏移。
        let mut static_draws: u64 = 0;
        let slot = &pipeline.static_slot;
        if slot.seg_count > 0 && !self.cache_active {
            render_pass.set_bind_group(0, &slot.bind_group, &[]);
            for &(start, len) in &slot.visible.ranges {
                render_pass.draw(0..6, start..start + len);
                static_draws += 1;
            }
        }

        let mut dynamic_draws: u64 = 0;
        if pipeline.dynamic_slot.seg_count > 0 {
            render_pass.set_bind_group(0, &pipeline.dynamic_slot.bind_group, &[]);
            render_pass.draw(0..6, 0..pipeline.dynamic_slot.seg_count);
            dynamic_draws += 1;
        }

        // 悬停光标（最上层）
        let mut cursor_draws: u64 = 0;
        if pipeline.cursor_slot.seg_count > 0 {
            render_pass.set_bind_group(0, &pipeline.cursor_slot.bind_group, &[]);
            render_pass.draw(0..6, 0..pipeline.cursor_slot.seg_count);
            cursor_draws += 1;
        }
        crate::stats::add_annotation_draws(static_draws, dynamic_draws, cursor_draws);
    }
}

impl AnnotationPipeline {
    /// 创建批注渲染管线（创建一次，跨帧常驻；直通通道 `init` 调用）。
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewstage.annotation.uniform"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &uniform_buffer,
            0,
            bytemuck::cast_slice(&[Globals {
                transform: [0.0; 16],
                brightness_contrast: [0.0; 4],
            }]),
        );

        // 绑定组布局：uniform（VS 变换）+ 只读段表存储（VS 扩展 / FS SDF）
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewstage.annotation.bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // 调色板：只在 VS 查（颜色以 flat varying 交给 FS），
                // 故 VERTEX 可见即可 —— FS 保持零 buffer 访问。
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("viewstage.annotation.shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "annotation.wgsl"
            ))),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewstage.annotation.pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("viewstage.annotation.pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                // 无顶点缓冲：角点由 vertex_index 推导，段数据经 storage 读取
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: triangle_primitive_state(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let static_slot = StaticUploadSlot::empty(device, &bind_group_layout, &uniform_buffer);
        let dynamic_slot = SegUploadSlot::empty(device, &bind_group_layout, &uniform_buffer);
        let cursor_slot = SegUploadSlot::empty(device, &bind_group_layout, &uniform_buffer);

        Self {
            device: device.clone(),
            surface_format: format,
            pipeline,
            uniform_buffer,
            bind_group_layout,
            static_slot,
            dynamic_slot,
            dyn_stable_fp: 0,
            dyn_tail_fp: 0,
            dyn_stable_segs: 0,
            dyn_pen_persisted: 0,
            dyn_eraser_persisted: 0,
            dyn_full_rewrite: true, // 首次必走全量
            cursor_slot,
            cursor_slot_fingerprint: u64::MAX,
            cached_matrix: None,
            physical_size: (0, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 静态层增量写入的规划：必须是"从第一个差异索引起"，而不是"仅尾部追加"。
    ///
    /// 回归用例：新增笔画时 `finalize_stroke` 先塞一份"未扣橡皮擦"的兜底网格，
    /// 后台线程回来后把该条目换成扣除版——**中间条目变了**。旧实现只认"尾部
    /// 追加"，前缀一断就整份文档全量重传（实测 2.4MB→5.4MB，prepare 单帧
    /// 峰值 79ms，收笔必顿）。

    fn stroke(id: u64, positions: &[[f32; 2]]) -> StaticStrokeRef {
        let color = [1.0f32, 1.0, 1.0, 1.0];
        let mut geometry = StrokeGeometry::default();
        for pair in positions.windows(2) {
            geometry.push(StrokeSeg::new(
                pair[0][0],
                pair[0][1],
                pair[1][0],
                pair[1][1],
                1.0,
                color,
            ));
        }
        StaticStrokeRef::new(id, 0, Arc::new(geometry))
    }

    /// 构造"驻留段区间"：逐笔顺序累加，与 `upload_static` 的打包顺序一致。
    fn ranges_of(strokes: &[StaticStrokeRef]) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut off = 0u32;
        for s in strokes {
            let n = s.geometry.segs.len() as u32;
            out.push((off, n));
            off += n;
        }
        out
    }

    /// WGSL 离线校验：着色器只会在运行期 `create_shader_module` 时才编译，
    /// 没有这道闸，语法/类型/绑定错误会一路溜到用户机器上才炸成黑屏。
    /// naga 与 wgpu 27 同版本，走同一棵依赖树，无额外编译成本。
    /// 段表布局必须与着色器逐字段一致（合计 32B）。
    ///
    /// 这是**跨语言边界**：Rust 的 `#[repr(C)]` 和 WGSL 的 struct 各写一遍。
    /// 任何一侧改字段顺序/类型而忘了另一侧，症状是"墨迹颜色错乱或几何
    /// 扭曲"——只在真机上可见。这里用 naga 读出着色器侧的成员偏移，与
    /// 宿主 `PackedSeg` 的字段偏移逐个对拍。
    ///
    /// （naga 27 不再保留类型 size，故按成员偏移 + 已知基类型宽度推算。）
    #[test]
    fn packed_seg_matches_shader_layout() {
        assert_eq!(
            std::mem::size_of::<PackedSeg>(),
            32,
            "GPU 段表必须是 32B（64B 的一半 = 顶点带宽减半的前提）"
        );
        // 宿主侧：字段偏移（#[repr(C)]，无重排）
        let host = {
            let base = std::mem::MaybeUninit::<PackedSeg>::uninit();
            let ptr = base.as_ptr();
            unsafe {
                [
                    ("a", std::ptr::addr_of!((*ptr).a) as usize - ptr as usize),
                    ("b", std::ptr::addr_of!((*ptr).b) as usize - ptr as usize),
                    ("ca", std::ptr::addr_of!((*ptr).ca) as usize - ptr as usize),
                    ("cb", std::ptr::addr_of!((*ptr).cb) as usize - ptr as usize),
                ]
            }
        };
        assert_eq!(
            host,
            [("a", 0), ("b", 16), ("ca", 24), ("cb", 28)],
            "宿主字段偏移被重排了"
        );

        // 着色器侧：Seg 成员偏移
        let source = include_str!("annotation.wgsl");
        let module = naga::front::wgsl::parse_str(source).expect("解析");
        let size_of = |ty: naga::Handle<naga::Type>| -> u32 {
            match module.types[ty].inner {
                naga::TypeInner::Vector { size, .. } => size as u32 * 4,
                _ => 4,
            }
        };
        let seg_ty = module
            .global_variables
            .iter()
            .find(|(_, var)| var.name.as_deref() == Some("segments"))
            .and_then(|(_, var)| match module.types[var.ty].inner {
                naga::TypeInner::Array { base, .. } => Some(base),
                _ => None,
            })
            .expect("segments 必须是 array");
        let members = match &module.types[seg_ty].inner {
            naga::TypeInner::Struct { members, .. } => members.clone(),
            _ => panic!("segments 元素必须是 struct"),
        };
        // 着色器把 ca/cb 两个 u32 塞进 b_pos_idx 的 zw（按位复用），
        // 故着色器侧是 2 个 vec4：偏移 0 与 16，合计 32B。
        let shader_offsets: Vec<u32> = members.iter().map(|m| m.offset).collect();
        assert_eq!(
            shader_offsets,
            vec![0, 16],
            "着色器 Seg 成员偏移应与宿主 a/b 一致（ca/cb 复用 b 的 zw）"
        );
        let total = members
            .last()
            .map(|m| m.offset + size_of(m.ty))
            .unwrap_or(0);
        assert_eq!(
            total,
            std::mem::size_of::<PackedSeg>() as u32,
            "Rust/WGSL 段表步长不一致：着色器 {total}B vs 宿主 {}B",
            std::mem::size_of::<PackedSeg>()
        );
    }

    #[test]
    fn shaders_compile_and_validate() {
        // 两个着色器都只在运行期编译：一个笔误会一路溜到用户机器上才炸成黑屏
        for (name, source) in [
            ("annotation.wgsl", include_str!("annotation.wgsl")),
            ("raster_blit.wgsl", include_str!("raster_blit.wgsl")),
        ] {
            let module = naga::front::wgsl::parse_str(source).unwrap_or_else(|e| {
                panic!("{name} 解析失败:\n{e:?}\n{}", e.emit_to_string(source))
            });
            let mut validator = naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::empty(),
            );
            validator.validate(&module).unwrap_or_else(|e| {
                panic!("{name} 校验失败:\n{e:?}\n{}", e.emit_to_string(source))
            });
        }
    }

    /// FS 不得再访问 storage buffer——批注层的成本几乎全在片元上，
    /// 每片元重读 64B 段记录是这个层最大的单点开销。
    ///
    /// 这条用「IR 级」断言守住：naga 校验通过只说明着色器合法，说明不了
    /// 它没退化。有人把 `segments[...]` 加回 FS 时这里立刻红，而不是等到
    /// 真机掉帧才发现。
    ///
    /// 判据不能简单写成「FS 里没有 `Load`」：naga 把**入口参数**（即
    /// varying）也放在 global arena 里，函数体里对 `input.xxx` 的每次访问
    /// 都会展开成一条 `Load`。所以这里沿指针链回溯到根，看它落在哪个
    /// 地址空间——落在 `Storage`（段表）或 `Uniform`（每帧 Globals）才是
    /// 我们要禁的逐片元内存访问。
    #[test]
    fn fragment_shader_does_not_read_buffers() {
        let source = include_str!("annotation.wgsl");
        let module = naga::front::wgsl::parse_str(source).expect("解析");
        let fragment = module
            .entry_points
            .iter()
            .find(|e| e.name == "fs_main")
            .expect("必须保留 fs_main 入口（管线按名引用）");

        // 指针链：Load.pointer → (Access/AccessIndex).base → … → GlobalVariable
        // 注意表达式句柄属于**函数自己的 arena**（naga 的 `Handle` 只是下标，
        // 跨 arena 同值不同义），只有 `GlobalVariable` 才指向模块级 arena。
        let function_expressions = &fragment.function.expressions;
        let root_space = |mut expr: naga::Handle<naga::Expression>| -> Option<naga::AddressSpace> {
            loop {
                match function_expressions[expr] {
                    naga::Expression::GlobalVariable(var) => {
                        return Some(module.global_variables[var].space)
                    }
                    naga::Expression::Access { base, .. }
                    | naga::Expression::AccessIndex { base, .. } => expr = base,
                    _ => return None,
                }
            }
        };

        let buffer_reads: Vec<&str> = fragment
            .function
            .expressions
            .iter()
            .filter_map(|(_, expr)| match expr {
                naga::Expression::Load { pointer } => root_space(*pointer),
                _ => None,
            })
            .filter(|space| {
                matches!(
                    space,
                    naga::AddressSpace::Storage { .. } | naga::AddressSpace::Uniform
                )
            })
            .map(|space| match space {
                naga::AddressSpace::Storage { .. } => "segments（storage）",
                _ => "globals（uniform）",
            })
            .collect();
        assert!(
            buffer_reads.is_empty(),
            "fs_main 不得逐片元读 buffer（逐实例数据应由 flat varying 传入）：\
             发现 {buffer_reads:?}"
        );
    }

    /// 旧/新 FS 的覆盖度必须**数值等价**——优化把「每片元重读段表 + 求导」
    /// 换成了「flat varying + 顶点侧求 AA 尺度」，若等价性被破坏，症状是
    /// 笔迹变粗/变细或缩小视图时闪烁，只能在真机上看见。
    ///
    /// 这里把两版公式都搬成 CPU 参考实现，在参数空间上扫描比对：
    /// 段长、端点/中点/端帽外侧的取样点、半宽、画布缩放。
    #[test]
    fn fragment_coverage_is_numerically_equivalent_to_the_previous_formulation() {
        // 与 WGSL 保持一致：退化段判据、AA 半像素过渡
        const DEGENERATE_LEN_SQ: f32 = 1e-12;
        const MIN_AA: f32 = 1e-6;

        /// 旧实现：`aa = max(fwidth(x), fwidth(y))`（像素/世界单位）
        fn coverage_old(
            local: [f32; 2],
            dir: [f32; 2],
            len_sq: f32,
            half: [f32; 2],
            ppu: f32,
        ) -> f32 {
            // 等比缩放下 fwidth(local) == ppu；为可比，这里取 max 的等价形式
            let aa = ppu.max(MIN_AA);
            let t = if len_sq > DEGENERATE_LEN_SQ {
                (local[0] * dir[0] + local[1] * dir[1]) / len_sq
            } else {
                0.0
            };
            let t = t.clamp(0.0, 1.0);
            let dv = [local[0] - dir[0] * t, local[1] - dir[1] * t];
            let dist = (dv[0] * dv[0] + dv[1] * dv[1]).sqrt();
            let radius = half[0] + (half[1] - half[0]) * t;
            ((radius - dist) / aa + 0.5).clamp(0.0, 1.0)
        }

        /// 新实现：FS 用 `world - a` 的段内相对坐标 + 顶点侧算好的 aa_world
        fn coverage_new(
            local: [f32; 2],
            dir: [f32; 2],
            len_sq: f32,
            half: [f32; 2],
            ppu: f32,
        ) -> f32 {
            let aa_world = 1.0 / ppu.max(MIN_AA);
            let t = if len_sq > DEGENERATE_LEN_SQ {
                (local[0] * dir[0] + local[1] * dir[1]) / len_sq
            } else {
                0.0
            };
            let t = t.clamp(0.0, 1.0);
            let dv = [local[0] - dir[0] * t, local[1] - dir[1] * t];
            let dist = (dv[0] * dv[0] + dv[1] * dv[1]).sqrt();
            let radius = half[0] + (half[1] - half[0]) * t;
            ((radius - dist) * aa_world + 0.5).clamp(0.0, 1.0)
        }

        let mut checked = 0usize;
        for &ppu in &[0.05f32, 0.25, 1.0, 2.0, 4.0, 8.0] {
            for &len in &[0.0f32, 0.3, 4.0, 37.0, 500.0] {
                for &half_a in &[0.05f32, 1.5, 12.0] {
                    let half_b = half_a * 0.6;
                    let dir = [len, 0.0];
                    let len_sq = len * len;
                    // 取样：段内 0/0.25/0.5/0.75/1（后者含端帽外侧），
                    // 再加法向外侧，逼近 SDF 的拐点
                    for &t_axis in &[-1.5f32, -0.5, 0.0, 0.25, 0.5, 0.75, 1.0, 1.5] {
                        for &off in &[-2.0f32, -0.5, 0.0, 0.5, 2.0] {
                            let local = [len * t_axis, off];
                            let old = coverage_old(local, dir, len_sq, [half_a, half_b], ppu);
                            let new = coverage_new(local, dir, len_sq, [half_a, half_b], ppu);
                            assert!(
                                (old - new).abs() < 1e-6,
                                "覆盖度不等价：ppu={ppu} len={len} half={half_a} \
                                 local={local:?} old={old} new={new}"
                            );
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 500, "扫描点太少（{checked}），起不到回归护栏作用");
    }

    #[test]
    fn cull_visible_without_rect_draws_everything() {
        let strokes = vec![
            stroke(1, &[[0.0, 0.0], [1.0, 0.0]]),
            stroke(2, &[[100000.0, 0.0], [100001.0, 0.0]]),
        ];
        let ranges = ranges_of(&strokes);
        let mut out = VisibleSet::default();

        // rect = None（视口未知）→ 不剔除，单区间覆盖全部
        cull_visible(&ranges, &strokes, None, 2, &mut out);
        assert_eq!(out.ranges, vec![(0, 2)], "未剔除时必须一段画完");
        assert_eq!(out.strokes, 0, "未剔除路径不做统计（无相交判定）");
    }

    #[test]
    fn cull_visible_coalesces_contiguous_strokes() {
        // 三笔全在视野内、缓冲里首尾相接 → 必须并成**一个** draw 区间
        let strokes = vec![
            stroke(1, &[[0.0, 0.0], [1.0, 0.0]]),                 // 1 段
            stroke(2, &[[2.0, 0.0], [3.0, 0.0], [4.0, 0.0]]),     // 2 段
            stroke(3, &[[5.0, 0.0], [6.0, 0.0]]),                 // 1 段
        ];
        let ranges = ranges_of(&strokes);
        let mut out = VisibleSet::default();

        cull_visible(&ranges, &strokes, Some(&[-1.0, -1.0, 10.0, 10.0]), 4, &mut out);
        assert_eq!(out.ranges, vec![(0, 4)], "连续可见必须合并为一次 draw");
        assert_eq!(out.strokes, 3);
        assert_eq!(out.segs, 4);
    }

    #[test]
    fn cull_visible_splits_on_offscreen_stroke() {
        // 中间一笔远在视口外 → 前后可见笔画各自成段
        let strokes = vec![
            stroke(1, &[[0.0, 0.0], [1.0, 0.0]]),                 // 起始 0，长 1
            stroke(2, &[[100000.0, 0.0], [100001.0, 0.0]]),       // 起始 1，长 1（不可见）
            stroke(3, &[[2.0, 0.0], [3.0, 0.0]]),                 // 起始 2，长 1
        ];
        let ranges = ranges_of(&strokes);
        let mut out = VisibleSet::default();

        cull_visible(&ranges, &strokes, Some(&[-1.0, -1.0, 10.0, 10.0]), 3, &mut out);
        assert_eq!(out.ranges, vec![(0, 1), (2, 1)], "被剔除笔画的段必须跳过");
        assert_eq!(out.strokes, 2);
        assert_eq!(out.segs, 2);
    }

    #[test]
    fn cull_visible_all_offscreen_yields_zero_draws() {
        let strokes = vec![
            stroke(1, &[[0.0, 0.0], [1.0, 0.0]]),
            stroke(2, &[[2.0, 0.0], [3.0, 0.0]]),
        ];
        let ranges = ranges_of(&strokes);
        let mut out = VisibleSet::default();

        cull_visible(&ranges, &strokes, Some(&[500.0, 500.0, 600.0, 600.0]), 2, &mut out);
        assert!(out.ranges.is_empty(), "全部移出视野 → 零 draw");
        assert_eq!(out.strokes, 0);
        assert_eq!(out.segs, 0);
    }

    #[test]
    fn cull_visible_skips_empty_strokes_without_breaking_coalescing() {
        // 中间一笔 0 段（几何被擦空的笔画）：既不计入可见，也不该断开区间
        let strokes = vec![
            stroke(1, &[[0.0, 0.0], [1.0, 0.0]]),   // 起始 0，长 1
            stroke(2, &[]),                          // 起始 1，长 0
            stroke(3, &[[2.0, 0.0], [3.0, 0.0]]),   // 起始 1，长 1
        ];
        let ranges = ranges_of(&strokes);
        assert_eq!(ranges, vec![(0, 1), (1, 0), (1, 1)]);
        let mut out = VisibleSet::default();

        cull_visible(&ranges, &strokes, Some(&[-1.0, -1.0, 10.0, 10.0]), 2, &mut out);
        assert_eq!(out.ranges, vec![(0, 2)], "0 段笔画不应把区间切成两段");
        assert_eq!(out.strokes, 2, "0 段笔画不算可见笔画");
        assert_eq!(out.segs, 2);
    }

    #[test]
    fn cull_visible_falls_back_to_single_draw_past_the_cap() {
        // 交替可见 → 每笔一个区间；笔数取到 2×上限以上，确保可见游程数真的
        // 突破 MAX_STATIC_DRAWS（否则测不到退化分支）
        let n = MAX_STATIC_DRAWS as u32 * 2 + 10;
        let mut strokes = Vec::new();
        let mut ranges = Vec::new();
        let mut off = 0u32;
        for i in 0..n {
            let x = if i % 2 == 0 { 0.0 } else { 100000.0 };
            let s = stroke(i as u64, &[[x, 0.0], [x + 1.0, 0.0]]);
            ranges.push((off, 1));
            off += 1;
            strokes.push(s);
        }
        let mut out = VisibleSet::default();

        cull_visible(&ranges, &strokes, Some(&[-1.0, -1.0, 10.0, 10.0]), off, &mut out);
        assert_eq!(
            out.ranges,
            vec![(0, off)],
            "区间数超上限必须退化为单次全量 draw"
        );
        assert!(out.strokes > MAX_STATIC_DRAWS, "统计仍按真实可见集");
        assert_eq!(out.segs, out.strokes as u64);
    }

    #[test]
    fn cull_visible_ranges_shorter_than_snapshot_is_safe() {
        // 防御性：ranges 与 snapshot 长度不一致时按 zip 配对，不越界
        let strokes = vec![
            stroke(1, &[[0.0, 0.0], [1.0, 0.0]]),
            stroke(2, &[[0.0, 0.0], [1.0, 0.0]]),
            stroke(3, &[[0.0, 0.0], [1.0, 0.0]]),
        ];
        let mut out = VisibleSet::default();
        cull_visible(&[(0, 1)], &strokes, Some(&[-1.0, -1.0, 10.0, 10.0]), 3, &mut out);
        assert_eq!(out.ranges, vec![(0, 1)]);
        assert_eq!(out.strokes, 1);
    }

    #[test]
    fn cull_visible_empty_input_is_noop() {
        let mut out = VisibleSet::default();
        // 上一次留下的内容必须被清空，不能残留导致幽灵绘制
        out.ranges.push((7, 3));
        out.strokes = 99;
        out.segs = 99;

        cull_visible(&[], &[], Some(&[0.0, 0.0, 1.0, 1.0]), 0, &mut out);
        assert!(out.ranges.is_empty());
        assert_eq!(out.strokes, 0);
        assert_eq!(out.segs, 0);

        // total_segs = 0 时即便 rect = None 也不产出 draw
        cull_visible(&[], &[], None, 0, &mut out);
        assert!(out.ranges.is_empty(), "零段不得产生空 draw");
    }

    #[test]
    fn view_to_canvas_rect_inverts_transform() {
        let t = crate::gpu::ViewTransform {
            scale: 2.0,
            offset_x: 100.0,
            offset_y: 50.0,
        };
        let rect = view_to_canvas_rect((800.0, 600.0), &t).unwrap();
        // 屏幕 (0,0) → 画布 (-50,-25)；屏幕 (800,600) → 画布 (350,275)
        assert!((rect[0] + 50.0).abs() < 1e-4);
        assert!((rect[1] + 25.0).abs() < 1e-4);
        assert!((rect[2] - 350.0).abs() < 1e-4);
        assert!((rect[3] - 275.0).abs() < 1e-4);

        // 视口未知 / 缩放非法 → 退化为不剔除
        assert!(view_to_canvas_rect((0.0, 0.0), &t).is_none());
        let bad = crate::gpu::ViewTransform {
            scale: 0.0,
            ..Default::default()
        };
        assert!(view_to_canvas_rect((800.0, 600.0), &bad).is_none());
    }

    #[test]
    fn stroke_ref_bounds_include_half_width() {
        let s = stroke(1, &[[0.0, 0.0], [10.0, 0.0]]);
        // 段半宽 1.0 → AABB 应在 y 方向外扩 1.0
        assert!((s.bounds[1] + 1.0).abs() < 1e-4, "min_y 应含半宽外扩");
        assert!((s.bounds[3] - 1.0).abs() < 1e-4, "max_y 应含半宽外扩");
        assert!(s.intersects(&[-5.0, -5.0, 5.0, 5.0]));
        assert!(!s.intersects(&[100.0, 100.0, 200.0, 200.0]));
    }

    /// 静态层写入规划：**从第一个与上次不同的索引起**重写。
    ///
    /// 回归用例（真实 bug，代价 79ms/帧）：新增笔画时 `finalize_stroke` 先塞一份
    /// "未扣橡皮擦"的兜底网格、后台线程再换回扣除版；橡皮擦也会改写**中间**笔画
    /// （generation 变化）。这些都是**中间条目**变了——旧实现只认"尾部追加"，
    /// 前缀一断就整份文档全量重传（实测 2.4MB→5.4MB，`prep` 单帧峰值 79ms，
    /// `worst=152ms`，收笔必顿）。
    #[test]
    fn static_write_starts_at_first_divergence() {
        let a = stroke(1, &[[0.0, 0.0], [1.0, 0.0]]);
        let b = stroke(2, &[[10.0, 0.0], [11.0, 0.0]]);
        let c = stroke(3, &[[20.0, 0.0], [21.0, 0.0]]);
        let prev: Arc<[StaticStrokeRef]> = Arc::from(vec![a.clone(), b.clone()]);
        let keep = |p: &[StaticStrokeRef], n: &[StaticStrokeRef]| {
            common_prefix_len(p, n).min(n.len())
        };

        // 尾部追加（新笔画）→ 从 2 起
        let appended: Arc<[StaticStrokeRef]> = Arc::from(vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(keep(&prev, &appended), 2);

        // 等长同内容（快照重建但内容一致）→ keep == len，一个字节都不写
        let same: Arc<[StaticStrokeRef]> = Arc::from(prev.to_vec());
        assert_eq!(keep(&prev, &same), 2);

        // 中间条目被替换（generation 变化）→ 从 1 起，**不是**整表重传
        let erased = StaticStrokeRef::new(2, 1, Arc::clone(&b.geometry));
        let mid_edit: Arc<[StaticStrokeRef]> = Arc::from(vec![a.clone(), erased, c.clone()]);
        assert_eq!(keep(&prev, &mid_edit), 1);
        assert!(
            keep(&prev, &mid_edit) < mid_edit.len(),
            "中间修改也必须保住前缀，旧实现在此退化为整表重传"
        );

        // undo 尾部删除 → keep 截到新长度
        let undone: Arc<[StaticStrokeRef]> = Arc::from(vec![a.clone()]);
        assert_eq!(keep(&prev, &undone), 1);

        // 首笔即不同 → keep = 0
        let other = stroke(9, &[[50.0, 0.0], [51.0, 0.0]]);
        let unrelated: Arc<[StaticStrokeRef]> = Arc::from(vec![other, b.clone(), c.clone()]);
        assert_eq!(keep(&prev, &unrelated), 0);

        // 空前缀（首帧）→ keep = 0，整表写
        let empty: Arc<[StaticStrokeRef]> = Arc::from([]);
        assert_eq!(keep(&empty, &appended), 0);
    }

    #[test]
    fn cursor_segments_fill_when_present_and_empty_absent() {
        let mut out: Vec<PackedSeg> = Vec::new();
        let mut palette = ColorPalette::new();
        build_eraser_cursor_segments(
            crate::live::EraserCursor {
                cx: 10.0,
                cy: 20.0,
                radius: 8.0,
                border: 2.0,
            },
            &mut out,
            &mut palette,
        );
        // 圆盘扇面段 + 边框环段
        assert_eq!(out.len(), 24 * 2);
        assert!((out[0].a[0] - 10.0).abs() < 1e-4);
        assert!((out[0].a[1] - 20.0).abs() < 1e-4);
    }

    /// 段表里只有调色板索引，颜色必须能被索引**取回**——否则症状只是
    /// "颜色有点不对"，极难肉眼定位。
    #[test]
    fn packed_segments_resolve_their_color_through_the_palette() {
        use crate::gpu::unpack_color;
        let red = [1.0, 0.0, 0.0, 1.0];
        let blue = [0.0, 0.0, 1.0, 0.5];
        let mut palette = ColorPalette::new();
        let mut out: Vec<PackedSeg> = Vec::new();

        let seg_a = StrokeSeg::new(0.0, 0.0, 1.0, 1.0, 2.0, red);
        let seg_b = StrokeSeg::new(1.0, 1.0, 2.0, 0.0, 2.0, blue);
        push_packed(&mut out, &seg_a, &mut palette);
        push_packed(&mut out, &seg_b, &mut palette);
        // 同色再压一段：必须复用同一索引（表只追加，不增长）
        push_packed(&mut out, &seg_a, &mut palette);

        assert_eq!(palette.len(), 2, "两种颜色 → 两条目");
        assert_eq!(out[0].ca, out[2].ca, "同色段必须共享索引");
        assert_ne!(out[0].ca, out[1].ca, "异色段索引必须不同");

        for (seg, want) in [(&out[0], red), (&out[1], blue)] {
            let got = unpack_color(palette.entries()[seg.ca as usize]);
            for c in 0..4 {
                assert!(
                    (got[c] - want[c]).abs() <= 1.0 / 255.0,
                    "通道 {c}: 期望 {want:?} 实得 {got:?}"
                );
            }
        }
        // 几何必须无损搬进 32B 布局
        assert_eq!(out[0].a, [0.0, 0.0, 2.0, 2.0], "A/B 两端半宽都在 a 里");
        assert_eq!(out[0].b, [1.0, 1.0]);
    }

    /// 调色板重置后索引全部失效：已驻留段绝不能继续用旧索引。
    #[test]
    fn palette_reset_bumps_epoch_so_callers_rewrite() {
        let mut palette = ColorPalette::new();
        let before = palette.intern([1.0, 0.0, 0.0, 1.0]);
        assert_eq!(palette.epoch(), 0);
        palette.reset();
        assert_eq!(palette.epoch(), 1, "重置必须递增纪元");
        assert!(palette.is_empty());
        // 重置后同一颜色拿到新索引（可能与旧值相同，但纪元已变，
        // 调用方据此整层重写 —— 正确性不依赖索引值本身）
        let after = palette.intern([1.0, 0.0, 0.0, 1.0]);
        assert_eq!(palette.len(), 1);
        let _ = (before, after);
    }

    /// 8bit 量化：误差 ≤ 1/255，且量化后同色合并为一条目。
    #[test]
    fn color_quantization_is_within_one_step_and_merges_near_duplicates() {
        use crate::gpu::{pack_color, unpack_color};
        let base = [0.2, 0.4, 0.6, 0.8];
        for c in 0..4 {
            assert!((unpack_color(pack_color(base))[c] - base[c]).abs() <= 1.0 / 255.0);
        }
        // 量化格内（±1/512）的两个颜色必须 intern 到同一索引
        let mut palette = ColorPalette::new();
        let a = palette.intern([0.5, 0.5, 0.5, 1.0]);
        let b = palette.intern([0.5 + 1.0 / 1024.0, 0.5, 0.5, 1.0]);
        assert_eq!(a, b, "同一量化格内不应产生新条目");
        assert_eq!(palette.len(), 1);
    }
}
