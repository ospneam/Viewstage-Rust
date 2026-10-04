//! 静态层离屏光栅缓存：把「每帧重画整份可见文档」换成「脏了才重画，
//! 平时一次 blit」。
//!
//! # 为什么曾经否掉、现在又做
//!
//! 历史实现（`annotation.rs` 模块文档记录）实测结论是"退化为全屏 blit 且是
//! 净损失"。那个结论**只在稀疏墨迹下成立**：一次全屏 blit 的成本 ≈ 屏幕像素
//! 数，与墨迹量无关；而直接画胶囊的成本 ≈ 可见段数 × 每段片元数。墨迹稀疏
//! 时后者远小于前者，缓存必亏；墨迹铺满屏幕时后者是前者的数倍，缓存才赚。
//!
//! 所以这里不是"要不要缓存"的二选一，而是**按密度切换**（见
//! [`StaticRasterCache::worth_caching`]）：低于阈值走原来的直接绘制路径
//! （行为与今天完全一致，零回归风险），高于阈值才启用缓存。
//!
//! # 只在"键稳定"时启用
//!
//! 缓存内容只由 (静态快照, 变换矩阵, 视口尺寸, 段表写入序号) 决定。
//! 平移/缩放时变换每帧都变 → 缓存每帧都要重画 → 比直接画更慢。所以要求
//! 该键**连续稳定若干帧**才启用：书写时静态快照与变换都不动（只有动态层在
//! 变，而动态层不在缓存里），于是缓存画一次、之后每帧只是一次 blit。
//!
//! # 合成正确性
//!
//! 缓存纹理用**非 sRGB** 格式（surface 格式去掉 sRGB 后缀）：着色器写入的
//! 是与直接渲染**同样的编码值**，采样时不发生额外解码，blit 是逐字节精确
//! 的拷贝。缓存里存的是**预乘 alpha 的墨迹叠在透明黑底**上，再以预乘混合
//! 贴到帧上——预乘的 source-over 可结合，等价于直接画在同一张帧上。

/// 每段平均片元数的估计：半宽 2px × 长度 8px ≈ 2·2·8。
///
/// 只是个量级估计，用来和屏幕像素数比大小，不需要精确。
pub const FRAGMENTS_PER_SEG: u64 = 32;

/// 启用缓存所需的收益倍数：估计片元数必须超过 blit 成本的这么多倍。
///
/// >1 的余量是给"缓存的固定开销"留的：一次 blit 除了逐像素取样外还有
/// 全屏的带宽（4K 下 33MB/帧纹理读），阈值太贴近 1 会让缓存变成负优化。
const CACHE_BENEFIT: u64 = 2;

/// 键连续稳定多少帧才允许启用（见模块文档）。
pub const STABLE_FRAMES: u8 = 2;

/// **像素可复用性**的键：任一项变化都意味着缓存纹理里已有的像素全部作废。
///
/// # 为什么不再用 `snapshot` / `write_serial` 参与判定
///
/// 旧键含快照 `Arc` 地址与写入序号，于是**每一次追加笔画**都会让键变化 →
/// 每次收笔都全量重画整层。追加并不影响已有段：一个只追加、不改历史内容的
/// 文档里，前 N 段的像素**完全可复用**，只需要把新增段补画进去。
///
/// 内容新鲜度改由 [`tail_ranges`] + 缓冲前缀共同表达：
///
/// ```text
/// 可增量  ⟺  params 未变 且 缓存已画到 rasted_upto
///           且 缓冲里 [0, rasted_upto) 与 CPU 快照一致
/// 增量范围 = tail_ranges(可见区间, rasted_upto)
/// ```
///
/// `view_rect` 必须在内：剔除用的矩形由 `viewport` + `transform` 推出，只比较
/// `matrix` 无法推出"可见集不变"——而可见集变了却复用旧像素，屏幕上就是漏画。
///
/// # NaN 的行为
///
/// `matrix` / `view_rect` 是浮点。若它们含 NaN，`PartialEq` 恒为 false →
/// 永远走全量重画。这是**安全退化**（结果正确，只是慢），不会漏画。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RasterParams {
    /// 画布→NDC 变换矩阵。
    pub matrix: [f32; 16],
    /// 物理像素尺寸（尺寸变了纹理必须重建）。
    pub size: (u32, u32),
    /// 本帧剔除用的画布矩形（`None` = 视口未知，不剔除）。
    pub view_rect: Option<[f32; 4]>,
}

/// 只画"尚未进缓存"的段：把可见区间裁到 `[from, ∞)`。
///
/// 纯函数，便于测试。**这是增量的核心**——必须严格裁到 `from`，否则已缓存的
/// 段会被**再画一遍**：预乘 source-over 叠加两次会让墨迹变深（alpha 复合），
/// 表现为"每收一笔颜色变浓一次"。这类错误不会崩、只会慢慢变深，很容易被漏掉。
pub fn tail_ranges(ranges: &[(u32, u32)], from: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(ranges.len());
    for &(start, len) in ranges {
        if len == 0 {
            continue;
        }
        let lo = start.max(from);
        let hi = start.saturating_add(len);
        if lo < hi {
            out.push((lo, hi - lo));
        }
    }
    out
}

/// 能否只补画尾部（三个条件缺一不可）。
///
/// 提成纯函数是因为**整份增量的正确性都压在这一个判断上**，而它原先内联在
/// 需要 GPU 设备才能跑的 `prepare_step` 里——等于没有测试覆盖。
///
/// 三个条件各自对应一种会让"复用已有像素"变成"漏画/错位"的情形：
///
/// | 条件 | 失效场景 | 漏画后果 |
/// | --- | --- | --- |
/// | `cache_valid` | 纹理刚重建、上一帧被禁用 | 尾部之外全无像素 |
/// | `params_match` | 平移/缩放/窗口尺寸/剔除矩形变 | 已有像素位置或可见集不对 |
/// | `buffer_prefix_segs >= rasted_upto` | 橡皮擦、撤销、清空、换快照 | 段表被重排，尾部偏移错位 |
pub fn extendable(
    cache_valid: bool,
    params_match: bool,
    buffer_prefix_segs: u32,
    rasted_upto: u32,
) -> bool {
    cache_valid && params_match && buffer_prefix_segs >= rasted_upto
}

/// 覆盖"启用缓存"所需的可见段数下限。`VIEWSTAGE_RASTER_MIN_SEGS=1000`。
///
/// # 为什么需要这个口
///
/// 默认阈值在 1080p 下约 **13 万可见段**（墨迹几乎铺满屏幕）。而增量光栅
/// 只在"缓存已启用 + 有新段追加"时才发生——也就是**只有高密度文档**才走得到。
/// 于是出现一个尴尬的组合：改动最需要被验证的那条分支，在正常使用中几乎不会
/// 被执行。
///
/// 有了这个口就能在**普通稀疏文档**上强制启用缓存，从而：
///
/// - 人工确认增量光栅的产物与全量一致（尤其"每收一笔颜色变浓"这类预乘叠加
///   错误——它不崩、只慢慢变色，测试抓不到，只能用眼睛）；
/// - 在低密度文档上对比 GPU 开销，量化这条路径到底值多少。
pub fn min_segs_override() -> Option<u64> {
    use std::sync::OnceLock;
    static OVERRIDE: OnceLock<Option<u64>> = OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        std::env::var("VIEWSTAGE_RASTER_MIN_SEGS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
    })
}

/// 离屏缓存的启用判定（纯函数，便于测试）。
///
/// `visible_segs` = 本帧剔除后可见段数；`physical` = 帧缓冲像素尺寸。
pub fn worth_caching(visible_segs: u64, physical: (u32, u32)) -> bool {
    let (w, h) = physical;
    if w == 0 || h == 0 {
        return false;
    }
    // `min_segs` = 使 blit 划算所需的可见段数：
    //   段数 × FRAGMENTS_PER_SEG > 像素数 × CACHE_BENEFIT
    let pixels = w as u64 * h as u64;
    let min_segs = match min_segs_override() {
        Some(v) => v,
        None => pixels.saturating_mul(CACHE_BENEFIT) / FRAGMENTS_PER_SEG,
    };
    visible_segs > min_segs
}

/// blit 用着色器：全屏三角形 + 一次采样，输出预乘色。
pub const BLIT_WGSL: &str = include_str!("raster_blit.wgsl");

/// **清矩形**用着色器：全屏三角形，输出全透明。
///
/// # 为什么需要它（而不是 `LoadOp::Clear` + scissor）
///
/// 局部重画要求"只把脏矩形擦成透明"。但 **scissor 不作用于 clear**——
/// `LoadOp::Clear` 永远清整张附件。唯一的办法是 `LoadOp::Load` + 用一条
/// `replace` 混合的管线把脏矩形写成透明，再在同一次 pass 里补画。
///
/// 顶点部分与 blit 完全一致（全屏三角形），所以视觉上必然覆盖同一块区域。
pub const CLEAR_RECT_WGSL: &str = r#"
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vertex_index << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(vertex_index & 2u) * 2.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

// 全透明 + 无混合（`replace`）= 把目标像素写成"什么都没有"。
@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 0.0, 0.0, 0.0);
}
"#;

/// 清矩形管线：无绑定组、无混合，写透明。
pub fn clear_rect_pipeline(
    device: &iced_wgpu::wgpu::Device,
    format: iced_wgpu::wgpu::TextureFormat,
) -> iced_wgpu::wgpu::RenderPipeline {
    use iced_wgpu::wgpu;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("viewstage.annotation.raster_clear_rect.shader"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(CLEAR_RECT_WGSL)),
    });
    // 空绑定组布局：这条管线不采样任何东西。
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("viewstage.annotation.raster_clear_rect.layout"),
        bind_group_layouts: &[],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("viewstage.annotation.raster_clear_rect.pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                // **不能有混合**：`replace` 才是"写透明"；任何 source-over
                // 都会把已有墨迹按 alpha 混一遍（越擦越浓）。
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

/// 把 `a` 与 `b` 求并集；`None` 视作空集。
pub fn union_rect(a: Option<[f32; 4]>, b: [f32; 4]) -> Option<[f32; 4]> {
    Some(match a {
        None => b,
        Some(r) => [
            r[0].min(b[0]),
            r[1].min(b[1]),
            r[2].max(b[2]),
            r[3].max(b[3]),
        ],
    })
}

/// 脏矩形（画布空间）→ 物理像素 scissor，并与附件尺寸求交。
///
/// 返回 `None` 表示矩形退化（空/非有限/完全在屏外）——此时调用方应当**退回全量
/// 重画**而不是跳过：跳过的后果是屏幕上留着一块永远擦不掉的旧墨迹。
pub fn scissor_from_rect(
    rect: [f32; 4],
    canvas_to_physical: &dyn Fn(f32, f32) -> (f32, f32),
    physical: (u32, u32),
) -> Option<[u32; 4]> {
    if !rect.iter().all(|v| v.is_finite()) {
        return None;
    }
    let (x0, y0) = canvas_to_physical(rect[0], rect[1]);
    let (x1, y1) = canvas_to_physical(rect[2], rect[3]);
    // 交换 + 裁边：画布 y 向上、帧缓冲 y 向下，跨轴时 min/max 会反。
    let min_x = x0.min(x1).floor().max(0.0);
    let min_y = y0.min(y1).floor().max(0.0);
    let max_x = x0.max(x1).ceil().min(physical.0 as f32);
    let max_y = y0.max(y1).ceil().min(physical.1 as f32);
    if max_x - min_x < 1.0 || max_y - min_y < 1.0 {
        return None;
    }
    Some([
        min_x as u32,
        min_y as u32,
        max_x.ceil().min(physical.0 as f32) as u32 - min_x as u32,
        max_y.ceil().min(physical.1 as f32) as u32 - min_y as u32,
    ])
}

/// blit 管线的绑定组布局（纹理 + 采样器）。
pub fn blit_bind_group_layout(
    device: &iced_wgpu::wgpu::Device,
) -> iced_wgpu::wgpu::BindGroupLayout {
    use iced_wgpu::wgpu;
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("viewstage.annotation.blit.layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

/// 创建 blit 渲染管线（目标格式 = 缓存的编码格式，与 surface 去掉 sRGB 后缀一致）。
pub fn blit_pipeline(
    device: &iced_wgpu::wgpu::Device,
    layout: &iced_wgpu::wgpu::BindGroupLayout,
    format: iced_wgpu::wgpu::TextureFormat,
) -> iced_wgpu::wgpu::RenderPipeline {
    use iced_wgpu::wgpu;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("viewstage.annotation.blit.shader"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(BLIT_WGSL)),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("viewstage.annotation.blit.pipeline_layout"),
        bind_group_layouts: &[layout],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("viewstage.annotation.blit.pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                // 预乘 source-over：把「透明底上的预乘墨迹」贴到帧上
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 密度闸门必须只在**确实划算**时才开：稀疏墨迹（几百分段）永远不进缓存，
    /// 否则就是历史上被否掉的那个"净损失"方案。
    #[test]
    fn gate_stays_off_for_sparse_ink_and_on_for_dense() {
        // 1080p：blit 成本 2.07M 像素
        let hd = (1920u32, 1080u32);
        assert!(!worth_caching(0, hd), "空文档不进缓存");
        assert!(!worth_caching(1_000, hd), "1000 段远不足以回本");
        assert!(!worth_caching(30_000, hd), "3 万段仍不划算（阈值附近）");
        // 阈值 = blit_cost * 2 / 32 = 2.07M*2/32 ≈ 129k 段
        assert!(!worth_caching(129_000, hd), "恰好在阈值下方应关闭");
        assert!(worth_caching(200_000, hd), "20 万段铺满屏幕应启用");
        assert!(worth_caching(1_000_000, hd), "超密文档必须启用");
    }

    /// 4K 下 blit 成本翻 4 倍，阈值同步抬高——否则会退化成"高分辨率下
    /// 缓存反而更贵"的那个陷阱。
    #[test]
    fn gate_scales_with_resolution() {
        let uhd = (3840u32, 2160u32);
        let threshold =
            |p: (u32, u32)| (p.0 as u64 * p.1 as u64) * CACHE_BENEFIT / FRAGMENTS_PER_SEG;
        assert_eq!(threshold(uhd), threshold((1920, 1080)) * 4);
        assert!(
            !worth_caching(threshold(uhd) as u64, uhd),
            "4K 阈值处应关闭"
        );
        assert!(worth_caching(threshold(uhd) as u64 + 1, uhd));
    }

    /// 退化尺寸不得除零；段数乘法溢出不得 panic/wrap。
    #[test]
    fn gate_handles_degenerate_size() {
        assert!(!worth_caching(u64::MAX, (0, 1080)), "零宽 → 不启用");
        assert!(!worth_caching(u64::MAX, (1920, 0)), "零高 → 不启用");
        assert!(!worth_caching(u64::MAX, (0, 0)), "零尺寸 → 不启用");
        assert!(!worth_caching(1, (10, 10)), "10×10 屏 + 1 段仍不划算");
        // 溢出必须饱和而非回绕：极小屏 + 海量段数，结论是「划算」而不是绕回成 0
        assert!(worth_caching(u64::MAX, (1, 1)));
    }

    /// 键的任一变化都必须让缓存失效。
    #[test]
    fn params_change_on_any_raster_input() {
        let base = RasterParams {
            matrix: [0.0; 16],
            size: (100, 100),
            view_rect: Some([0.0, 0.0, 100.0, 100.0]),
        };
        assert_eq!(base, base);
        assert_ne!(
            base,
            RasterParams {
                matrix: [1.0; 16],
                ..base
            },
            "变换变了 → 已有像素全部作废"
        );
        assert_ne!(
            base,
            RasterParams {
                size: (101, 100),
                ..base
            },
            "尺寸变了 → 纹理重建"
        );
        assert_ne!(
            base,
            RasterParams {
                view_rect: Some([0.0, 0.0, 200.0, 100.0]),
                ..base
            },
            "剔除矩形变了 → 可见集可能变，不能复用"
        );
        assert_ne!(
            base,
            RasterParams {
                view_rect: None,
                ..base
            },
            "视口未知（不剔除）≠ 已知矩形"
        );
    }

    /// NaN 必须让键比较失败（退化成全量重画），而**不是**意外相等。
    #[test]
    fn nan_params_never_compare_equal() {
        let a = RasterParams {
            matrix: [f32::NAN; 16],
            size: (100, 100),
            view_rect: None,
        };
        let b = a;
        assert_ne!(a, b, "含 NaN 时不得判为可复用（否则会漏画）");
    }

    /// 增量范围必须严格裁到 `from` 之后——重叠部分一旦重画，预乘 alpha 叠加
    /// 会让墨迹变深。
    #[test]
    fn tail_ranges_clips_strictly_after_from() {
        // from 在所有区间之前 → 原样返回
        assert_eq!(
            tail_ranges(&[(0, 10), (20, 5)], 0),
            vec![(0, 10), (20, 5)]
        );
        // from 落在第一个区间中间 → 裁掉前缀
        assert_eq!(tail_ranges(&[(0, 10), (20, 5)], 4), vec![(4, 6), (20, 5)]);
        // from 落在两个区间之间 → 只留第二个
        assert_eq!(tail_ranges(&[(0, 10), (20, 5)], 15), vec![(20, 5)]);
        // from 正好在区间边界
        assert_eq!(tail_ranges(&[(0, 10), (20, 5)], 10), vec![(20, 5)]);
        // from 超过全部 → 空（本帧纯 blit）
        assert!(tail_ranges(&[(0, 10), (20, 5)], 25).is_empty());
        // from == 0 且有零长区间 → 跳过零长
        assert_eq!(tail_ranges(&[(0, 0), (5, 3)], 0), vec![(5, 3)]);
        // 空输入
        assert!(tail_ranges(&[], 0).is_empty());
    }

    /// 增量判定的三种失效必须各自都能拦住——这是"不漏画"的全部保证。
    #[test]
    fn extendable_requires_all_three_conditions() {
        // 追加一笔：缓存有效、参数未变、缓冲前缀覆盖水位 → 可增量
        assert!(extendable(true, true, 100, 100));
        // 缓冲前缀**恰好**等于水位（无新增段）也算可增量 → 走"纯 blit"分支
        assert!(extendable(true, true, 40, 40));
        // 纹理刚重建 → 里面没有像素
        assert!(!extendable(false, true, 100, 100), "纹理重建后不得复用");
        // 平移/缩放/尺寸/剔除矩形变 → 已有像素位置不对
        assert!(!extendable(true, false, 100, 100), "参数变了不得复用");
        // 橡皮擦 / 撤销 / 清空 → 前缀断裂，必须全量
        assert!(
            !extendable(true, true, 50, 100),
            "缓冲前缀短于水位（段表被重排）必须全量"
        );
        // 两个都失效也仍然拦住
        assert!(!extendable(false, false, 0, 100));
    }

    /// 增量范围与已缓存部分**不得有交集**。有交集就是"重画一遍"，预乘 alpha
    /// 叠加会让墨迹变深——这是静默的错误（不崩、只慢慢变色），必须钉死。
    #[test]
    fn tail_never_overlaps_the_cached_prefix() {
        let visible = [(0, 50), (60, 20)];
        let cached_upto = 70; // 水位落在第二个区间**内部** —— 最容易写错的情形
        let tail = tail_ranges(&visible, cached_upto);
        for &(start, len) in &tail {
            let tail_end = start + len;
            for &(cs, cl) in &visible {
                let cached_end = (cs + cl).min(cached_upto);
                // 增量区间 [start, tail_end) 与已缓存 [cs, cached_end) 不得相交
                let overlap_start = start.max(cs);
                let overlap_end = tail_end.min(cached_end);
                assert!(
                    overlap_start >= overlap_end,
                    "增量区间 {start}..{tail_end} 与已缓存 {cs}..{cached_end} 重叠于 \
                     {overlap_start}..{overlap_end}"
                );
            }
        }
        // 第一区间整段已缓存 → 不该重画；第二区间只画水位之后的部分
        assert_eq!(tail, vec![(70, 10)]);
    }
}
