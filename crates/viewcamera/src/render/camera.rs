//! 摄像头/图片背景层独立 wgpu 渲染管线。
//!
//! - 帧像素支持两种格式（[`FramePixels`]）：
//!   - `Rgba`：图片导入等 CPU 侧已有 RGBA 数据（旧行为）。
//!   - `Nv12`：摄像头原生 NV12 直传（零拷贝切片），YUV→RGB 在着色器完成，
//!     彻底消除每帧 CPU 解码/转换，上传带宽约 -63%；线性采样顺带完成缩放，
//!     替代旧版 CPU 最近邻降采样（画质提升，不再 854×480 上采样糊屏）。
//! - 纹理在帧变化时上传（generation 差量），尺寸不变则复用纹理仅重写。
//!   平面行宽不满足 256 对齐时走暂存缓冲 copy（跨帧复用 grow-only staging，
//!   稳态零 GPU 分配）。
//! - 图片以 0.9 适配居中绘制（与旧 CameraCanvas 一致），pan/zoom 只更新 uniform。

use iced::Rectangle;
use iced_wgpu::graphics::Viewport;
use iced_wgpu::wgpu;
use std::sync::{Arc, Mutex};

use crate::render::padded_plane::PaddedPlaneUploader;
use viewink::gpu::{
    create_write_buffer, triangle_primitive_state, write_buffer_belt, Globals, ViewTransform,
};

/// 帧像素数据。
#[derive(Debug, Clone)]
pub enum FramePixels {
    /// 已解码 RGBA（图片导入路径）
    Rgba(Arc<Vec<u8>>),
    /// NV12 原始平面零拷贝切片（摄像头直传路径）
    Nv12 {
        /// Y 平面：width * height 字节
        y: Arc<bytes::Bytes>,
        /// UV 交错平面：(width/2) * (height/2) 对 = width*height/2 字节
        uv: Arc<bytes::Bytes>,
    },
}

/// 摄像头/图片帧数据（采集线程→主线程→渲染管线的 Arc 快照）。
#[derive(Debug, Clone)]
pub struct CameraFrameData {
    pub pixels: FramePixels,
    pub width: u32,
    pub height: u32,
    pub generation: u64,
}

/// 摄像头/图片层图元。
#[derive(Debug, Clone, Default)]
pub struct CameraLayer {
    /// 共享实时视图变换句柄（信号源层唯一依赖——批注笔画状态不可见；
    /// pan/zoom 零消息热路径直读；None 时用默认变换）
    pub transform: Option<Arc<viewink::live::LiveTransform>>,
    /// 实时摄像头帧。
    pub frame: Option<Arc<CameraFrameData>>,
    /// 静态图片（优先于摄像头帧显示）。
    pub image: Option<Arc<CameraFrameData>>,
    /// 图像滤镜归一化参数 (brightness_delta, contrast, grayscale_bool, yuv_matrix)
    /// w 分量：NV12 路径的 YUV 矩阵选择（0=BT.601, 1=BT.709）
    pub brightness_contrast: [f32; 4],
    /// 旋转角度（度数，0/90/180/270）
    pub rotation_degrees: i32,
}

impl CameraLayer {
    /// NV12 路径使用的 YUV 转换矩阵标志：≥720p 用 BT.709（高清摄像头惯例），否则 BT.601。
    pub fn yuv_matrix_flag(width: u32, height: u32) -> f32 {
        if width >= 1280 || height >= 720 {
            1.0
        } else {
            0.0
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadVertex {
    position: [f32; 2],
    uv: [f32; 2],
}

const QUAD_VERTEX_LAYOUT: wgpu::VertexBufferLayout = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<QuadVertex>() as u64,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &wgpu::vertex_attr_array!(0 => Float32x2, 1 => Float32x2),
};

fn camera_sampler(device: &wgpu::Device) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("viewstage.camera.sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        min_filter: wgpu::FilterMode::Linear,
        mag_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Linear,
        // 覆盖最大 mip 链（4096px 纹理 12 层）；无链纹理自动钳制到单层
        lod_min_clamp: 0.0,
        lod_max_clamp: 16.0,
        ..Default::default()
    })
}

/// 图片纹理的完整 mip 链层数：floor(log2(max(w,h))) + 1。
#[inline]
fn mip_level_count(width: u32, height: u32) -> u32 {
    let max_dim = width.max(height).max(1);
    32 - (max_dim - 1).leading_zeros()
}

const MIP_BLIT_WGSL: &str = r#"
// 图片纹理 mip 链生成：上一级 → 下一级双线性下采样（全屏三角）
@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_smp: sampler;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // 全屏三角形，覆盖 [-1,3]²
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = corners[vi];
    var out: VsOut;
    out.position = vec4<f32>(xy, 0.0, 1.0);
    // NDC y+ 为视口顶部 = 纹理 v0；uv.y 取反保持方向一致
    out.uv = vec2<f32>((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src_tex, src_smp, in.uv);
}
"#;

/// 图片纹理 mip 链生成器：blit 下采样管线（创建一次，跨图片复用）。
///
/// 仅用于静态图片（进入/切换时一次性毫秒级成本）；相机动态帧不生成
/// （内容逐帧变化且采样比 ≈1:1，生成反而增加带宽）。
/// 收益：大图缩放显示时每像素触碰 texel 数从最多 16 降到 ~4，
/// 消除缓存抖动与缩小摩尔纹——图片模式批注期间 GPU 带宽的主导项。
#[derive(Debug)]
struct MipGenerator {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
}

impl MipGenerator {
    fn new(device: &wgpu::Device) -> Self {
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("viewstage.camera.mip_blit.bgl"),
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
            });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("viewstage.camera.mip_blit"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(MIP_BLIT_WGSL)),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewstage.camera.mip_blit.pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("viewstage.camera.mip_blit"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: triangle_primitive_state(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        Self {
            pipeline,
            bind_group_layout,
        }
    }

    /// 对已写入 level-0 的纹理生成完整 mip 链（记入调用方帧 encoder）。
    ///
    /// 必须与内容上传拷贝记入**同一 encoder**：encoder 内命令按序执行，
    /// 保证 上传 → mip blit → 帧采样 的依赖顺序（若 mip 用独立 encoder
    /// 提前 submit，会先于帧 encoder 里的上传执行，读到空纹理）。
    fn generate(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        sampler: &wgpu::Sampler,
        mip_level_count: u32,
    ) {
        if mip_level_count <= 1 {
            return;
        }
        let mut src_view = texture.create_view(&wgpu::TextureViewDescriptor {
            base_mip_level: 0,
            mip_level_count: Some(1),
            ..Default::default()
        });

        for level in 1..mip_level_count {
            let dst_view = texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("viewstage.camera.mip_blit.bg"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&src_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            });
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewstage.camera.mip_blit.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rpass.set_pipeline(&self.pipeline);
            rpass.set_bind_group(0, &bind_group, &[]);
            rpass.draw(0..3, 0..1);
            drop(rpass);
            src_view = dst_view; // 下一级源 = 本级输出
            viewink::stats::add_mip_pass();
        }
    }
}

/// RGBA 单纹理 GPU 资源。
struct SingleTextureGpu {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    generation: u64,
    width: u32,
    height: u32,
    /// 静态图片 = 完整 mip 链层数；相机回退帧 = 1
    mip_level_count: u32,
    uploader: PaddedPlaneUploader,
    /// 图片是否完全不透明（每个像素 alpha=255）。不透明时 draw 走 REPLACE
    /// 管线旁路混合 dst 读（对齐 Chromium 不透明 quad 走 BlendMode::kNone）。
    /// 内容上传时扫描一次（generation 门控），扫描结果与该代内容绑定。
    opaque: bool,
}

/// RGBA 像素不透明性扫描：所有像素 alpha == 255（字节序第 4 字节）。
///
/// 一次性成本 O(字节数)（4K RGBA ≈ 8M 像素，毫秒级）；仅在该代内容
/// 首次上传时发生，静态显示期零扫描。
fn scan_opaque(rgba: &[u8]) -> bool {
    debug_assert_eq!(rgba.len() % 4, 0, "RGBA 数据必须按 4 字节对齐");
    rgba.chunks_exact(4).all(|px| px[3] == 255)
}

/// NV12 双平面 GPU 资源（Y=R8Unorm，UV=Rg8Unorm 半分辨率）。
struct Nv12TexturesGpu {
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    generation: u64,
    width: u32,
    height: u32,
    uploader: PaddedPlaneUploader,
}

/// 已上传到 GPU 的图像（按像素格式区分绘制管线与绑定组）。
enum GpuImage {
    Rgba(SingleTextureGpu),
    Nv12(Nv12TexturesGpu),
}

/// 绑定组条目（RGBA 与 NV12 共用布局：uniform + 双纹理槽 + 采样器）。
///
/// RGBA 路径将同一视图绑定到两个纹理槽（fs_main 只读槽 1，槽 2 占位）；
/// NV12 路径分别绑定 Y / UV 平面。
fn camera_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniform_buffer: &wgpu::Buffer,
    view_a: &wgpu::TextureView,
    view_b: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewstage.camera.bind_group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(view_a),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(view_b),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

impl SingleTextureGpu {
    #[allow(clippy::too_many_arguments)]
    fn upload(
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        layout: &wgpu::BindGroupLayout,
        uniform_buffer: &wgpu::Buffer,
        sampler: &wgpu::Sampler,
        mip_generator: Option<&MipGenerator>,
        rgba: &[u8],
        width: u32,
        height: u32,
        generation: u64,
        existing: Option<Self>,
        assume_opaque: bool,
    ) -> Self {
        // 不透明性判定：调用方已知内容恒不透明时（相机帧）跳过 O(字节数)
        // 扫描；否则扫描一次并绑定到该代内容（generation 门控）。
        let opaque_now = || {
            if assume_opaque {
                true
            } else {
                scan_opaque(rgba)
            }
        };
        // 同尺寸仅重写像素，复用既有纹理/bind group（避免每帧重建 GPU 对象）
        if let Some(mut t) = existing {
            if t.width == width && t.height == height {
                t.uploader
                    .upload(device, encoder, belt, &t.texture, rgba, width * 4, width, height);
                // 内容重写后 mip 链失效：静态图片一次性重建（毫秒级）。
                // 与上传同一 encoder：保证 上传 → mip blit 顺序。
                if let Some(gen) = mip_generator {
                    gen.generate(device, encoder, &t.texture, sampler, t.mip_level_count);
                }
                t.generation = generation;
                t.opaque = opaque_now();
                return t;
            }
        }

        // 静态图片建完整 mip 链（大图缩放显示时采样带宽降数倍）；
        // RENDER_ATTACHMENT 供 blit 下采样写入
        let mip_levels = mip_generator.map_or(1, |_| mip_level_count(width, height));
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewstage.camera.texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: mip_levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | (if mip_generator.is_some() {
                    wgpu::TextureUsages::RENDER_ATTACHMENT
                } else {
                    wgpu::TextureUsages::empty()
                }),
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut uploader = PaddedPlaneUploader::new();
        uploader.upload(device, encoder, belt, &texture, rgba, width * 4, width, height);
        if let Some(gen) = mip_generator {
            // 与上传同一 encoder：保证 上传 → mip blit 顺序
            gen.generate(device, encoder, &texture, sampler, mip_levels);
        }
        let bind_group =
            camera_bind_group(device, layout, uniform_buffer, &view, &view, sampler);

        SingleTextureGpu {
            texture,
            bind_group,
            generation,
            width,
            height,
            mip_level_count: mip_levels,
            uploader,
            opaque: opaque_now(),
        }
    }
}

impl Nv12TexturesGpu {
    #[allow(clippy::too_many_arguments)]
    fn upload(
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        layout: &wgpu::BindGroupLayout,
        uniform_buffer: &wgpu::Buffer,
        sampler: &wgpu::Sampler,
        y: &bytes::Bytes,
        uv: &bytes::Bytes,
        width: u32,
        height: u32,
        generation: u64,
        existing: Option<Self>,
    ) -> Self {
        let (uv_w, uv_h) = (width / 2, height / 2);
        // 同尺寸仅重写平面内容，复用纹理/bind group。
        // 双平面拷贝命令记入调用方帧 encoder，随主帧统一 submit
        // （不再自建 encoder 独立提交）。
        if let Some(mut p) = existing {
            if p.width == width && p.height == height {
                p.uploader
                    .upload(device, encoder, belt, &p.y_texture, y, width, width, height);
                p.uploader.upload(
                    device,
                    encoder,
                    belt,
                    &p.uv_texture,
                    uv,
                    uv_w * 2,
                    uv_w,
                    uv_h,
                );
                p.generation = generation;
                return p;
            }
        }

        let y_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewstage.camera.nv12.y"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uv_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewstage.camera.nv12.uv"),
            size: wgpu::Extent3d {
                width: uv_w,
                height: uv_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut uploader = PaddedPlaneUploader::new();
        // 首建路径：双平面同样记入帧 encoder（随主帧统一 submit）
        uploader.upload(device, encoder, belt, &y_texture, y, width, width, height);
        uploader.upload(device, encoder, belt, &uv_texture, uv, uv_w * 2, uv_w, uv_h);
        let bind_group = camera_bind_group(
            device,
            layout,
            uniform_buffer,
            &y_view,
            &uv_view,
            sampler,
        );

        Nv12TexturesGpu {
            y_texture,
            uv_texture,
            bind_group,
            generation,
            width,
            height,
            uploader,
        }
    }
}

/// 按帧数据格式上传（复用同格式旧资源；格式切换时旧资源直接释放重建）。
#[allow(clippy::too_many_arguments)]
fn gpu_image_upload(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    belt: &mut wgpu::util::StagingBelt,
    layout: &wgpu::BindGroupLayout,
    uniform_buffer: &wgpu::Buffer,
    sampler: &wgpu::Sampler,
    // 静态图片槽传 Some（生成 mip 链）；相机帧槽传 None（动态内容不生成）
    mip_generator: Option<&MipGenerator>,
    data: &CameraFrameData,
    existing: Option<GpuImage>,
    // 内容是否已知恒不透明（true = 直接标 opaque，跳过每帧 O(字节数)
    // 的 alpha 扫描）。相机帧恒为不透明；静态图片可能带透明像素，须扫描。
    assume_opaque: bool,
) -> GpuImage {
    match (&data.pixels, existing) {
        (FramePixels::Rgba(rgba), Some(GpuImage::Rgba(t))) => {
            GpuImage::Rgba(SingleTextureGpu::upload(
                device,
                encoder,
                belt,
                layout,
                uniform_buffer,
                sampler,
                mip_generator,
                rgba,
                data.width,
                data.height,
                data.generation,
                Some(t),
                assume_opaque,
            ))
        }
        (FramePixels::Nv12 { y, uv }, Some(GpuImage::Nv12(p))) => {
            GpuImage::Nv12(Nv12TexturesGpu::upload(
                device,
                encoder,
                belt,
                layout,
                uniform_buffer,
                sampler,
                y,
                uv,
                data.width,
                data.height,
                data.generation,
                Some(p),
            ))
        }
        (FramePixels::Rgba(rgba), _) => GpuImage::Rgba(SingleTextureGpu::upload(
            device,
            encoder,
            belt,
            layout,
            uniform_buffer,
            sampler,
            mip_generator,
            rgba,
            data.width,
            data.height,
            data.generation,
            None,
            assume_opaque,
        )),
        (FramePixels::Nv12 { y, uv }, _) => GpuImage::Nv12(Nv12TexturesGpu::upload(
            device,
            encoder,
            belt,
            layout,
            uniform_buffer,
            sampler,
            y,
            uv,
            data.width,
            data.height,
            data.generation,
            None,
        )),
    }
}

impl CameraLayer {
    /// 上传本帧参数到管线（原 iced `Primitive::prepare` 体）。
    ///
    /// 直通通道语义：画布即全窗口 → `bounds` 取视口逻辑尺寸
    /// （原 widget 布局 bounds 与其等价）。
    fn upload(
        &self,
        pipeline: &mut CameraPipeline,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        viewport: &Viewport,
    ) {
        let bounds = Rectangle::with_size(viewport.logical_size());

        // 变换 uniform（直读共享实时状态；变化才写，pan/zoom 零顶点重传）
        let transform: ViewTransform = self
            .transform
            .as_ref()
            .map(|l| l.view_transform())
            .unwrap_or_default();
        let matrix = transform.to_matrix(viewport);
        let bc = self.brightness_contrast;
        let needs_uniform = pipeline.cached_matrix != Some(matrix)
            || pipeline.cached_brightness_contrast != Some(bc);
        if needs_uniform {
            let globals = Globals {
                transform: matrix,
                brightness_contrast: bc,
            };
            // 经帧 encoder + belt 写入：`Queue::write_buffer` 每次调用都会
            // 自建 staging 缓冲并单独 submit 一次，pan/zoom 时每帧多一次
            // 驱动级提交。
            write_buffer_belt(
                encoder,
                belt,
                device,
                &pipeline.uniform_buffer,
                0,
                bytemuck::cast_slice(&[globals]),
            );
            pipeline.cached_matrix = Some(matrix);
            pipeline.cached_brightness_contrast = Some(bc);
        }
        pipeline.physical_size = (viewport.physical_width(), viewport.physical_height());

        // ============ 层解耦契约 ============
        // 信号源层（本图元）与批注层完全独立：
        // - 本层仅依赖 LiveTransform（pan/zoom）与 frame/image 的代数；
        //   批注笔画状态变化不会触碰此处任何上传/重建路径。
        // - 显示图片时在 canvas 上绘画：texture 代数不变 → 零重上传、
        //   零纹理重建；每帧仅一次全屏四边形合成（GPU 固定成本，
        //   与 DWM 每帧合成等价，无法也无需规避）。
        // - 图片身份 = rgba Arc 指针（App 侧 enter_image 设置），同图重入
        //   代数不变 → 纹理跨源切换持久复用。

        if let Some(frame) = &self.frame {
            if pipeline
                .frame
                .as_ref()
                .is_none_or(|t| t.generation_of() != frame.generation)
            {
                pipeline.frame = Some(gpu_image_upload(
                    device,
                    encoder,
                    belt,
                    &pipeline.bind_group_layout,
                    &pipeline.uniform_buffer,
                    &pipeline.sampler,
                    None, // 相机帧：动态内容不生成 mip
                    frame,
                    pipeline.frame.take(),
                    // 相机 RGB 回退路径由 CpuFrameProcessor 恒置 alpha=255，
                    // NV12 路径 fs_main_nv12 恒输出 alpha=1.0：均为不透明，
                    // 跳过每帧 O(字节数) 的 alpha 扫描。
                    false,
                ));
            }
        } else {
            pipeline.frame = None;
        }

        if let Some(image) = &self.image {
            if pipeline
                .image
                .as_ref()
                .is_none_or(|t| t.generation_of() != image.generation)
            {
                pipeline.image = Some(gpu_image_upload(
                    device,
                    encoder,
                    belt,
                    &pipeline.bind_group_layout,
                    &pipeline.uniform_buffer,
                    &pipeline.sampler,
                    Some(&pipeline.mip_generator), // 静态图片：完整 mip 链
                    image,
                    pipeline.image.take(),
                    true, // 图片可能带透明像素：扫描以选择 ALPHA / REPLACE 管线
                ));
            }
        } else {
            pipeline.image = None;
        }

        // 图片以 0.9 适配居中：目标矩形 = 预览变换前的画布坐标空间
        // 旋转 90°/270° 时交换图片宽高参与适配，得到旋转后应有的宽高比例矩形
        let rotation = self.rotation_degrees.rem_euclid(360);
        let active = self.image.as_ref().or(self.frame.as_ref());
        let rect = active.map(|data| {
            let (iw, ih) = if rotation == 90 || rotation == 270 {
                (data.height, data.width)
            } else {
                (data.width, data.height)
            };
            Self::fit_rect(&bounds, iw, ih)
        });
        let need_rebuild = rect != pipeline.cached_rect || rotation != pipeline.cached_rotation;
        if need_rebuild {
            pipeline.cached_rect = rect;
            pipeline.cached_rotation = rotation;
            if let Some((x, y, w, h)) = rect {
                let verts = Self::quad_uv_rotated(x, y, w, h, rotation);
                write_buffer_belt(
                    encoder,
                    belt,
                    device,
                    &pipeline.quad_vertex,
                    0,
                    bytemuck::cast_slice(&verts),
                );
            }
        }
    }
}

/// 摄像头/图片背景层直通合成通道：**不经 iced widget/Scene/primitive 队列**
/// （实现 [`iced_wgpu::DirectChannel`]）。
///
/// - 参数通路：App 每帧经 `viewgfx::CanvasGfx::update_camera_params` 把
///   dirty-cache 产出的最新 [`CameraLayer`] 快照写进共享参数槽，`prepare`
///   时取走——不经 view() 构建图元、不经 UI widget 层。
/// - `active`：最近一次快照（App 未提交新帧时复用，纹理/quad 缓冲稳态零上传）。
/// - `init` 幂等（compositor 重建等场景可能多次调用）。
pub struct CameraChannel {
    params: Arc<Mutex<Option<Arc<CameraLayer>>>>,
    active: Option<Arc<CameraLayer>>,
    pipeline: Option<CameraPipeline>,
}

impl CameraChannel {
    pub fn new(params: Arc<Mutex<Option<Arc<CameraLayer>>>>) -> Self {
        Self {
            params,
            active: None,
            pipeline: None,
        }
    }
}

impl iced_wgpu::DirectChannel for CameraChannel {
    fn init(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) {
        if self.pipeline.is_none() {
            self.pipeline = Some(CameraPipeline::new(device, queue, format));
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
        // 取走 App 提交的最新参数快照（无提交则复用 active）
        if let Ok(mut slot) = self.params.lock() {
            if let Some(latest) = slot.take() {
                self.active = Some(latest);
            }
        }

        let Some(layer) = self.active.as_ref() else {
            return;
        };
        let Some(pipeline) = self.pipeline.as_mut() else {
            return;
        };
        // 纹理上传挂帧 encoder + staging belt（随主帧统一 submit）
        layer.upload(pipeline, device, encoder, belt, viewport);
    }

    fn draw(&mut self, render_pass: &mut wgpu::RenderPass<'_>) {
        let Some(pipeline) = self.pipeline.as_ref() else {
            return;
        };

        let active = pipeline.image.as_ref().or(pipeline.frame.as_ref());
        let Some(img) = active else {
            return;
        };

        // 不透明 RGBA 图片 / NV12 帧 → REPLACE（旁路 dst 读）；透明图片 → ALPHA
        let (selected_pipeline, bind_group) = match img {
            GpuImage::Rgba(t) => {
                let p = if t.opaque {
                    &pipeline.pipeline_rgba_replace
                } else {
                    &pipeline.pipeline_rgba
                };
                (p, &t.bind_group)
            }
            GpuImage::Nv12(p) => (&pipeline.pipeline_nv12, &p.bind_group),
        };
        render_pass.set_pipeline(selected_pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.set_scissor_rect(0, 0, pipeline.physical_size.0, pipeline.physical_size.1);
        render_pass.set_vertex_buffer(0, pipeline.quad_vertex.slice(..));
        render_pass.set_index_buffer(pipeline.quad_index.slice(..), wgpu::IndexFormat::Uint32);
        render_pass.draw_indexed(0..6, 0, 0..1);
    }
}

impl GpuImage {
    fn generation_of(&self) -> u64 {
        match self {
            GpuImage::Rgba(t) => t.generation,
            GpuImage::Nv12(p) => p.generation,
        }
    }
}

impl CameraLayer {
    /// 0.9 适配居中矩形（画布坐标，与旧 CameraCanvas 完全一致）。
    fn fit_rect(bounds: &Rectangle, img_w: u32, img_h: u32) -> (f32, f32, f32, f32) {
        let canvas_w = bounds.width;
        let canvas_h = bounds.height;
        let img_aspect = img_w as f32 / img_h as f32;
        let canvas_aspect = canvas_w / canvas_h;

        let (base_w, base_h) = if img_aspect > canvas_aspect {
            let dw = canvas_w * 0.9;
            (dw, dw / img_aspect)
        } else {
            let dh = canvas_h * 0.9;
            (dh * img_aspect, dh)
        };

        let x = (canvas_w - base_w) / 2.0;
        let y = (canvas_h - base_h) / 2.0;
        (x, y, base_w, base_h)
    }

    /// 生成固定矩形 (x,y,w,h) 的四边形，按 rotation(0/90/180/270) 重排 UV 四角。
    /// 几何顶点不动，仅纹理采样角重排 → 内容旋转但几何保持适配矩形比例，无拉伸。
    fn quad_uv_rotated(x: f32, y: f32, w: f32, h: f32, rotation: i32) -> [QuadVertex; 4] {
        // 角点顺序：TL, TR, BR, BL
        let corners = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
        // 每行的 4 个 UV 对应 TL, TR, BR, BL（与 0° 基准一致）。
        // 纹理 uv 原点在左上角，而上传时缓冲行 0 即纹理行 0，故 0° 时屏幕顶取 v=0。
        let uv_table: [[[f32; 2]; 4]; 4] = [
            [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]], // 0°
            [[0.0, 1.0], [0.0, 0.0], [1.0, 0.0], [1.0, 1.0]], // 90°
            [[1.0, 1.0], [0.0, 1.0], [0.0, 0.0], [1.0, 0.0]], // 180°
            [[1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]], // 270°
        ];
        let idx = (rotation.rem_euclid(360) / 90) as usize % 4;
        let uvs = &uv_table[idx];
        let mut verts = [QuadVertex {
            position: [0.0, 0.0],
            uv: [0.0, 0.0],
        }; 4];
        for i in 0..4 {
            verts[i] = QuadVertex {
                position: corners[i],
                uv: uvs[i],
            };
        }
        verts
    }
}

/// 摄像头/图片独立渲染管线。
pub struct CameraPipeline {
    mip_generator: MipGenerator,
    pipeline_rgba: wgpu::RenderPipeline,
    pipeline_rgba_replace: wgpu::RenderPipeline,
    pipeline_nv12: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    frame: Option<GpuImage>,
    image: Option<GpuImage>,
    quad_vertex: wgpu::Buffer,
    quad_index: wgpu::Buffer,
    cached_matrix: Option<[f32; 16]>,
    cached_brightness_contrast: Option<[f32; 4]>,
    cached_rect: Option<(f32, f32, f32, f32)>,
    /// 上次生成 quad 顶点时使用的旋转角度（度），用于检测变化
    cached_rotation: i32,
    physical_size: (u32, u32),
}

impl CameraPipeline {
    /// 创建摄像头/图片渲染管线（创建一次，跨帧常驻；直通通道 `init` 调用）。
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewstage.camera.uniform"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &uniform_buffer,
            0,
            bytemuck::cast_slice(&[Globals {
                transform: [0.0; 16],
                brightness_contrast: [0.0, 1.0, 0.0, 0.0],
            }]),
        );

        // 单一布局同时覆盖 RGBA（tex_a）与 NV12（tex_a=Y, tex_b=UV）两条管线：
        // RGBA 路径把同一视图绑到双槽（fs_main 只读 tex_a），避免维护两套 shader/layout
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewstage.camera.bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("viewstage.camera.shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "camera.wgsl"
            ))),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewstage.camera.pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let make_pipeline = |fragment_entry: &'static str,
                             label: &'static str,
                             blend: wgpu::BlendState| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[QUAD_VERTEX_LAYOUT],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fragment_entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        // NV12 路径：fs_main_nv12 恒输出 alpha=1.0（不透明），
                        // 且本层画在 frame 底色（LoadOp::Clear）之上——混合语义
                        // 等价直写，REPLACE 旁路硬件混合的 dst 读（全显示
                        // 区域 × 60fps 的固定带宽，核显上可观）。
                        // RGBA 路径：图片可能带透明像素（col.a < 1 需与底层
                        // 混合），必须保持 ALPHA_BLENDING。
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: triangle_primitive_state(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let pipeline_rgba = make_pipeline(
            "fs_main",
            "viewstage.camera.pipeline",
            wgpu::BlendState::ALPHA_BLENDING,
        );
        // 不透明 RGBA 图片走 REPLACE：旁路混合 dst 读（Chromium 不透明
        // quad 等价做法）；含透明像素的图片回退 ALPHA 管线。
        let pipeline_rgba_replace = make_pipeline(
            "fs_main",
            "viewstage.camera.pipeline_replace",
            wgpu::BlendState::REPLACE,
        );
        let pipeline_nv12 = make_pipeline(
            "fs_main_nv12",
            "viewstage.camera.pipeline_nv12",
            wgpu::BlendState::REPLACE,
        );

        // 固定的 6 索引（两个三角形） + 4 顶点占位缓冲
        let quad_vertex = create_write_buffer(
            device,
            queue,
            "viewstage.camera.quad_vertex",
            wgpu::BufferUsages::VERTEX,
            &[0u8; 4 * std::mem::size_of::<QuadVertex>()],
        );
        let indices: [u32; 6] = [0, 1, 2, 0, 2, 3];
        let quad_index = create_write_buffer(
            device,
            queue,
            "viewstage.camera.quad_index",
            wgpu::BufferUsages::INDEX,
            bytemuck::cast_slice(&indices),
        );

        Self {
            mip_generator: MipGenerator::new(device),
            pipeline_rgba,
            pipeline_rgba_replace,
            pipeline_nv12,
            uniform_buffer,
            bind_group_layout,
            sampler: camera_sampler(device),
            frame: None,
            image: None,
            quad_vertex,
            quad_index,
            cached_matrix: None,
            cached_brightness_contrast: None,
            cached_rect: None,
            cached_rotation: 0,
            physical_size: (0, 0),
        }
    }
}

/// 白板底色已移出本模块：它不画任何东西，只是声明 frame 的
/// `LoadOp::Clear`，现由宿主经 `iced_wgpu::set_frame_clear_color` 设置
/// （见 `register_direct_channels_once`）。合成顺序 = 底色 → 摄像头 →
/// 批注 → iced UI 层，与旧架构 z 序一致。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_opaque_detects_fully_opaque_and_translucent() {
        // 全不透明：alpha（每 4 字节第 4 位）全为 255
        let opaque = [1u8, 2, 3, 255, 250, 250, 250, 255, 0, 0, 0, 255];
        assert!(scan_opaque(&opaque));
        // 任一像素 alpha < 255 → 不透明性不成立
        let translucent = [1u8, 2, 3, 255, 250, 250, 250, 200];
        assert!(!scan_opaque(&translucent));
        let transparent = [0u8, 0, 0, 0];
        assert!(!scan_opaque(&transparent));
        // 空数据 → 恒不透明（无像素即无混合需求，走 REPLACE 零成本）
        let empty: [u8; 0] = [];
        assert!(scan_opaque(&empty));
    }
}
