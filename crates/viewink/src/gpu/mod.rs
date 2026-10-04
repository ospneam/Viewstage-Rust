//! 批注/摄像头共用的 GPU 原语。
//!
//! 分层 + GPU 扩边架构：CPU 只产出「形状定义」——画布坐标线段表
//! （中心线端点 + 半宽 + 两端颜色），四边形扩展、圆头关节、端帽与
//! 抗锯齿全部由核显着色器完成（annotation.wgsl 胶囊 SDF）。
//! 相比旧 CPU 三角化（每段 ~744B 顶点数据），上传量降至 64B/段，
//! 关节 overdraw 消除，且获得解析级抗锯齿。
//!
//! 本模块只放「与具体渲染对象无关的通用 GPU 类型与辅助」；摄像头专用的
//! 平面上传器（PaddedPlaneUploader）留在主包 render 模块，不进入本 crate。

use iced_wgpu::graphics::Viewport;
use iced_wgpu::wgpu;

/// 单条线段记录（GPU 胶囊扩展的输入，64 字节）。
///
/// - `a` / `b`：xy = 画布坐标端点，z = 该端半宽（线宽的一半），w = 对齐保留
/// - `ca` / `cb`：两端直通（非预乘）RGBA，支持沿段渐变（触控轨迹衰减等）
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct StrokeSeg {
    pub a: [f32; 4],
    pub b: [f32; 4],
    pub ca: [f32; 4],
    pub cb: [f32; 4],
}

impl StrokeSeg {
    /// 等宽等色段（最常见情形）。
    pub fn new(ax: f32, ay: f32, bx: f32, by: f32, half_w: f32, color: [f32; 4]) -> Self {
        Self {
            a: [ax, ay, half_w, 0.0],
            b: [bx, by, half_w, 0.0],
            ca: color,
            cb: color,
        }
    }
}

/// 颜色打包为 RGBA8（`0xAABBGGRR` 内存序，与 WGSL `unpack4x8unorm` 一致）。
///
/// 量化到 8bit **不是精度妥协**：目标 framebuffer 本身就是 8bit/通道，
/// 更高位在混合后必然被截掉。附带收益是 interning 键离散，条目数有界。
#[inline]
pub fn pack_color(color: [f32; 4]) -> u32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    q(color[0]) | (q(color[1]) << 8) | (q(color[2]) << 16) | (q(color[3]) << 24)
}

/// [`pack_color`] 的逆（着色器 `unpack4x8unorm` 的宿主侧镜像）。
///
/// 存在的意义是**可测**：段表里只有索引，没有颜色，任何"索引指错了"的
/// bug 在肉眼下只是"颜色有点不对"，而 round-trip 断言能当场抓住。
#[inline]
pub fn unpack_color(packed: u32) -> [f32; 4] {
    let q = |shift: u32| ((packed >> shift) & 0xFF) as f32 / 255.0;
    [q(0), q(8), q(16), q(24)]
}

/// GPU 段表条目：**32 字节**（[`StrokeSeg`] 的一半）。
///
/// 段表是顶点带宽的主体（20 万段 × 64B = 12.8MB/帧），而每段真正变化的
/// 只有 4 个 f32 + 2 个颜色——颜色在文档里高度重复（一支笔一个色，轨迹
/// 只在 alpha 上分档），故改为「4B 索引 + 一张很小的调色板」。
///
/// - `a`：`(ax, ay, half_a, half_b)`
/// - `b`：`(bx, by, ca_idx, cb_idx)`
///
/// 索引以 `u32` 存放（着色器 `palette[idx]` 取出后 `unpack4x8unorm`）。
/// 两端各一个索引是为了保住「沿段渐变」（触控轨迹的 alpha 衰减）。
///
/// 内存序与 `annotation.wgsl` 的 `Seg` 一一对应，由
/// `packed_seg_matches_shader_layout` 用 naga 交叉验证。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PackedSeg {
    pub a: [f32; 4],
    pub b: [f32; 2],
    pub ca: u32,
    pub cb: u32,
}

impl PackedSeg {
    /// 由源段 + 调色板索引构造。索引由 [`ColorPalette::intern`] 给出。
    #[inline]
    pub fn new(seg: &StrokeSeg, ca: u32, cb: u32) -> Self {
        Self {
            a: [seg.a[0], seg.a[1], seg.a[2], seg.b[2]],
            b: [seg.b[0], seg.b[1]],
            ca,
            cb,
        }
    }
}

/// 直接映射查找表槽位数（power of two）。
///
/// [`ColorPalette::intern`] 的快路径：一笔一个色 → 同一支笔的几千个段
/// 全部命中同一槽，查找退化成一次数组访问 + 一次比较。整表重写路径
/// （撤销/擦除，20 万段）要过 20 万次，这里是 2ns 与 20ns 的差别。
const PALETTE_CACHE_SLOTS: usize = 256;

/// 调色板条目上限。超出即重置并递增 [`ColorPalette::epoch`]。
///
/// 现实用量：笔色数（个位数）× 轨迹 alpha 档（≤32）+ 橡皮擦/光标若干，
/// 实测几十到几百条。1024 是安全网，正常永不触发；一旦触发，调用方必须
/// 凭 epoch 变化触发整层重写（否则已驻留段的索引会指向重置后的表）。
const PALETTE_CAP: usize = 1024;

/// 单调追加的颜色表：段表只存索引，重复颜色只占 4 字节。
///
/// **只追加**是刻意的：已驻留 GPU 的段其索引永不失效 → 增量上传（前缀追加）
/// 不需要因为「新笔用了新颜色」而重传旧段。
#[derive(Debug)]
pub struct ColorPalette {
    entries: Vec<u32>,
    index: std::collections::HashMap<u32, u32>,
    /// 直接映射缓存：`[键, 索引+1]`，`0` 表示空槽。
    cache: Box<[[u32; 2]; PALETTE_CACHE_SLOTS]>,
    epoch: u64,
}

impl Default for ColorPalette {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            index: std::collections::HashMap::new(),
            // 数组长度 256 > 32，没有 `Default` 实现，手写
            cache: Box::new([[0; 2]; PALETTE_CACHE_SLOTS]),
            epoch: 0,
        }
    }
}

impl ColorPalette {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已用条目数（= 需上传的 u32 个数）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 表内容（`pack_color` 的 RGBA8，顺序即索引）。
    pub fn entries(&self) -> &[u32] {
        &self.entries
    }

    /// 重置纪元：每次**重置**递增（追加不递增）。
    ///
    /// 调用方（上传路径）比较它即可知道"已驻留的索引是否还有效"。
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 取色索引（必要时追加）。索引在表重置前保持稳定。
    #[inline]
    pub fn intern(&mut self, color: [f32; 4]) -> u32 {
        let key = pack_color(color);
        let slot = ((key.wrapping_mul(2_654_435_761) >> 24) as usize) & (PALETTE_CACHE_SLOTS - 1);
        let cached = self.cache[slot];
        if cached[0] == key && cached[1] != 0 {
            return cached[1] - 1;
        }
        let index = match self.index.get(&key) {
            Some(&i) => i,
            None => {
                if self.entries.len() >= PALETTE_CAP {
                    self.reset();
                    // 重置后本颜色必然是新条目
                    return self.intern(color);
                }
                let i = self.entries.len() as u32;
                self.entries.push(key);
                self.index.insert(key, i);
                i
            }
        };
        self.cache[slot] = [key, index + 1];
        index
    }

    /// 清空并递增纪元：已驻留段里的索引全部失效，调用方须整层重写。
    pub fn reset(&mut self) {
        self.entries.clear();
        self.index.clear();
        self.cache = Box::new([[0; 2]; PALETTE_CACHE_SLOTS]);
        self.epoch += 1;
    }
}

/// 画布坐标笔画几何：线段表（GPU 侧扩展为胶囊带）。
#[derive(Debug, Clone, Default)]
pub struct StrokeGeometry {
    pub segs: Vec<StrokeSeg>,
}
impl StrokeGeometry {
    pub fn push(&mut self, seg: StrokeSeg) {
        self.segs.push(seg);
    }

    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    /// 画布坐标 AABB（端点坐标按该端半宽外扩）：`[min_x, min_y, max_x, max_y]`。
    ///
    /// 渲染侧视口剔除的依据——O(段数) 扫描，调用方（静态快照构建）
    /// 每笔只算一次并随引用缓存，不进入帧热路径。空几何返回零矩形
    /// （零矩形与任何视口都不相交，等价于被剔除）。
    pub fn bounds(&self) -> [f32; 4] {
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for seg in &self.segs {
            let (ax, ay, aw) = (seg.a[0], seg.a[1], seg.a[2]);
            let (bx, by, bw) = (seg.b[0], seg.b[1], seg.b[2]);
            min_x = min_x.min(ax - aw).min(bx - bw);
            min_y = min_y.min(ay - aw).min(by - bw);
            max_x = max_x.max(ax + aw).max(bx + bw);
            max_y = max_y.max(ay + aw).max(by + bw);
        }
        if min_x > max_x {
            [0.0; 4]
        } else {
            [min_x, min_y, max_x, max_y]
        }
    }
}

/// 视图变换（画布坐标 → 逻辑屏幕坐标）：p_screen = p_canvas * scale + offset。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewTransform {
    pub scale: f32,
    pub offset_x: f32,
    pub offset_y: f32,
}

impl Default for ViewTransform {
    fn default() -> Self {
        Self {
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }
}

impl ViewTransform {
    /// 画布坐标 → NDC 的 4x4 矩阵（与 iced mesh 管线投影约定一致）。
    ///
    /// M = Orthographic(physical) * Scale(scale_factor) * Translate(offset) * Scale(scale)
    pub fn to_matrix(&self, viewport: &Viewport) -> [f32; 16] {
        use iced::advanced::graphics::core::Transformation;

        let m = Transformation::orthographic(viewport.physical_width(), viewport.physical_height())
            * Transformation::scale(viewport.scale_factor())
            * Transformation::translate(self.offset_x, self.offset_y)
            * Transformation::scale(self.scale);

        m.into()
    }

    /// 画布坐标 → **物理像素**坐标。
    ///
    /// 与 [`Self::to_matrix`] 的合成顺序一致：
    /// `ortho * scale(sf) * translate(offset) * scale(scale)`，从右往左作用于点，
    /// 即 `physical = (canvas * scale + offset) * scale_factor`。
    ///
    /// 存在的理由：脏矩形 → scissor 必须走**同一条**映射。抄一份矩阵元素的
    /// 换算必然随 `to_matrix` 的改动一起腐烂，而这类不一致的症状是"局部重画
    /// 擦错位置"，画面上留下一块擦不掉的残影——不崩、只静静留着。
    #[inline]
    pub fn canvas_to_physical(&self, viewport: &Viewport, x: f32, y: f32) -> (f32, f32) {
        let sf = viewport.scale_factor();
        ((x * self.scale + self.offset_x) * sf, (y * self.scale + self.offset_y) * sf)
    }
}

/// 顶点着色器全局 uniform：画布→NDC 变换 + 图像滤镜参数。
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Globals {
    pub transform: [f32; 16],
    /// 滤镜参数对齐 camera.wgsl 的 brightness_contrast vec4:
    ///   x = brightness (delta, 0.0 = 正常),
    ///   y = contrast (range, 1.0 = 正常),
    ///   z = grayscale (bool as f32),
    ///   w = unused（旋转由顶点几何阶段处理）
    pub brightness_contrast: [f32; 4],
}

/// 经**帧 encoder + staging belt** 把数据写入 GPU 缓冲（单次 CPU 拷贝）。
///
/// 与 [`wgpu::Queue::write_buffer`] 的区别：后者在 wgpu 内部为**每次调用**创建临时
/// staging 缓冲并单独 `submit` 一次，动态几何/悬停光标这类每帧写入会凭空多出
/// 每帧数次驱动级 submit；本函数把拷贝命令记入调用方的帧 encoder，由 iced 主帧
/// `finish → submit → recall` 统一提交（与 `PaddedPlaneUploader` 同一模式）。
///
/// `data.len()` 必须是 [`wgpu::COPY_BUFFER_ALIGNMENT`]（4）的倍数——
/// 本管线的写入单元恒为 `Globals`(80B) / `StrokeSeg`(64B) / `QuadVertex`(16B)。
pub fn write_buffer_belt(
    encoder: &mut wgpu::CommandEncoder,
    belt: &mut wgpu::util::StagingBelt,
    device: &wgpu::Device,
    buffer: &wgpu::Buffer,
    offset: u64,
    data: &[u8],
) {
    if data.is_empty() {
        return;
    }
    debug_assert_eq!(
        data.len() % wgpu::COPY_BUFFER_ALIGNMENT as usize,
        0,
        "belt 写入长度必须按 COPY_BUFFER_ALIGNMENT 对齐"
    );
    let size = wgpu::BufferSize::new(data.len() as u64).expect("non-empty write");
    belt.write_buffer(encoder, buffer, offset, size, device)
        .copy_from_slice(data);
}

/// 创建可写 GPU 缓冲区并写入数据（空数据也分配最小 1 字节以免空缓冲报错）。
pub fn create_write_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    usage: wgpu::BufferUsages,
    data: &[u8],
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: data.len().max(1) as u64,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if !data.is_empty() {
        queue.write_buffer(&buffer, 0, data);
    }
    buffer
}

/// 通用的三角形列表绘制状态（与 iced 一致）。
pub fn triangle_primitive_state() -> wgpu::PrimitiveState {
    wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        front_face: wgpu::FrontFace::Cw,
        ..Default::default()
    }
}
