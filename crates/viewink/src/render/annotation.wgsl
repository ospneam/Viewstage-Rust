//! 批注层 wgpu 顶点/片元着色器：GPU 胶囊扩展管线。
//!
//! CPU 仅上传线段表（中心线端点+半宽+两端直通色，见 gpu::StrokeSeg），
//! 本着色器完成全部拓扑工作：
//! - VS：无顶点缓冲，`vertex_index` 取四边形角点、`instance_index` 取段，
//!   沿法线扩展并按两端半宽渐变；端点沿轴向延伸半宽以容纳圆头。
//! - FS：点到中心线段的距离场 → `smoothstep` 解析抗锯齿；
//!   圆头关节/端帽由胶囊距离场天然覆盖；颜色沿投影参数插值，
//!   预乘在 AA 之后执行（PREMULTIPLIED_ALPHA_BLENDING 目标）。
//!
//! # 性能：FS 不再触碰 storage buffer
//!
//! 批注层的成本几乎全在**片元**上：每段覆盖 ≈ 2r·L 像素，一份 20 万段的
//! 文档在 144Hz 下是 ~7 亿片元/秒，而顶点只有 1.2M/帧。旧实现在 FS 里
//! `segments[seg_index]` **每片元重读整条 64B 记录**（拆成 4 次 16B 载入，
//! 虽有 L1 命中仍是实打实的取指/地址计算开销），再加上两个 `fwidth` 导数
//! 指令——而这些量全是**逐实例常量**，本可在 VS 算一次。
//!
//! 现改为：VS 读一次段记录，把它需要的一切以 **flat（不插值）varying**
//! 交给 FS。FS 于是零次内存访问，只剩 `dot`/`clamp`/`length`/`mix`。
//! 附带两处改进：
//! - 传给 FS 的是 `world - a`（段起点为原点）而非画布绝对坐标。画布可达
//!   10⁵ 量级，f32 在那里分辨率仅 ~0.01，直接插值绝对坐标会让 SDF 距离
//!   量化 → 缩小视图时抗锯齿抖动。段内相对坐标是 O(段长) 的小量。
//! - AA 尺度由 VS 从变换矩阵列长求得（世界单位/像素），FS 不再求导。
//!   与旧 `max(fwidth(x), fwidth(y))` **逐位等价**（见 vs_main 注释），
//!   只是从"每片元两条导数指令"变成"每顶点一次点积"。
//!
//! 权衡记录：片元插值寄存器共 5 个 location / 16 个分量（flat 不参与
//! 插值方程，只占寄存器），远低于 D3D11 的 32 分量上限。
//!
//! # 段表 32B：颜色走间接索引
//!
//! 段表是顶点带宽主体（20 万段）。旧记录 64B/段里有一半是颜色，而颜色在
//! 文档里高度重复（一笔一个色，触控轨迹只在 alpha 上分档），故段内只留
//! 4B×2 的调色板索引，真实颜色放一张几十条的小表（4B/条，RGBA8 打包）。
//! 20 万段：12.8MB/帧 → 6.4MB/帧。
//!
//! 颜色仍**在 VS 取出**并以 flat varying 交给 FS：调色板 binding 只对
//! VERTEX 可见，片元阶段对 buffer 的访问为零（同上一节的结论）。
//! 两端各一个索引是为了保住沿段渐变（轨迹 alpha 衰减）。

struct Globals {
    transform: mat4x4<f32>,
    /// 占位对齐（与 camera.wgsl / host Globals 结构一致，未使用）
    _padding: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> segments: array<Seg>;
/// 颜色表：`pack_color` 的 RGBA8（`unpack4x8unorm` 的内存序）。
/// **只对 VERTEX 可见** —— FS 不读任何 buffer。
@group(0) @binding(2) var<storage, read> palette: array<u32>;

/// 线段记录（与 gpu::PackedSeg 布局一致：32B）
struct Seg {
    a_pos_half: vec4<f32>, // xy=A 端画布坐标, z=A 端半宽, w=B 端半宽
    b_pos_idx: vec4<f32>,  // xy=B 端画布坐标, zw=两端调色板索引（f32 承载 u32）
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    /// 段内相对坐标 `world - a`（参与插值）
    @location(0) local: vec2<f32>,
    /// flat：`(b-a).xy`、`|b-a|²`、AA 尺度（世界单位/像素）
    @location(1) @interpolate(flat) ab_len_aa: vec4<f32>,
    /// flat：两端半宽
    @location(2) @interpolate(flat) halfs: vec2<f32>,
    /// flat：两端直通色（沿投影参数插值）
    @location(3) @interpolate(flat) ca: vec4<f32>,
    @location(4) @interpolate(flat) cb: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    let seg = segments[instance_index];
    let a = seg.a_pos_half.xy;
    let b = seg.b_pos_idx.xy;
    let half_a = max(seg.a_pos_half.z, 0.0);
    let half_b = max(seg.a_pos_half.w, 0.0);
    // 索引以 f32 承载（u32 精确表示到 2^24，条目数上限 1024 远在其内）
    let ca = unpack4x8unorm(palette[u32(seg.b_pos_idx.z)]);
    let cb = unpack4x8unorm(palette[u32(seg.b_pos_idx.w)]);

    // 轴向与法线；退化段（点画）任取固定轴
    let dir = b - a;
    let len = length(dir);
    var axis = vec2<f32>(1.0, 0.0);
    if len > 1e-6 {
        axis = dir / len;
    }
    let normal = vec2<f32>(-axis.y, axis.x);

    // 四边形角点表：(t 沿扩展轴向, 法线侧)
    // 覆盖圆头需要把两端各向外延伸半宽
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let corner = corners[vertex_index % 6u];

    let ext_a = a - axis * half_a;
    let ext_b = b + axis * half_b;
    let hw = mix(half_a, half_b, corner.x);
    let world = mix(ext_a, ext_b, corner.x) + normal * hw * corner.y;

    // AA 尺度：变换矩阵把世界单位映射到裁空，**列长**即每世界单位多少像素
    // （纯缩放+平移，故各向同性；取两列较大者则非等比缩放下为保守上界）。
    //
    // 与旧 FS 里 `aa = max(fwidth(x), fwidth(y))` 逐位等价：fwidth 的量纲
    // 是「像素/世界单位」，而覆盖度用的是 (radius-dist)/aa —— 即
    // 「世界单位/像素」，正是本处的倒数。`max` 的次序也在取倒数后保持不变。
    let ppu = max(length(globals.transform[0].xy), length(globals.transform[1].xy));
    let aa_world = 1.0 / max(ppu, 1e-6);

    var out: VertexOutput;
    out.position = globals.transform * vec4<f32>(world, 0.0, 1.0);
    out.local = world - a;
    out.ab_len_aa = vec4<f32>(dir, dot(dir, dir), aa_world);
    out.halfs = vec2<f32>(half_a, half_b);
    out.ca = ca;
    out.cb = cb;
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // 全部输入来自 varying，本函数不访问任何 buffer
    let ab = input.ab_len_aa.xy;
    let len_sq = input.ab_len_aa.z;
    let aa_world = input.ab_len_aa.w;

    // 到中心线段的最近点（钳制投影 → 天然圆头）
    var t = 0.0;
    if len_sq > 1e-12 {
        t = clamp(dot(input.local, ab) / len_sq, 0.0, 1.0);
    }
    let dist = length(input.local - ab * t);
    let radius = mix(input.halfs.x, input.halfs.y, t);

    // 解析抗锯齿：±0.5 像素内线性过渡（AA 尺度逐实例恒定，VS 已给出）
    let coverage = clamp((radius - dist) * aa_world + 0.5, 0.0, 1.0);

    if (coverage <= 0.0) {
        discard;
    }

    // 颜色沿投影插值，AA 后预乘
    var color = mix(input.ca, input.cb, t);
    color.a *= coverage;
    color = vec4<f32>(color.xyz * color.a, color.a);
    return color;
}
