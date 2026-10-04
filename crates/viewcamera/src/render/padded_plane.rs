//! 行对齐的纹理平面上传器（摄像头 NV12 等原生平面专用，非笔迹代码）。
//!
//! 从原 `render::common` 抽出：仅摄像头渲染路径使用，不属于 viewink 笔迹引擎。

use iced_wgpu::wgpu;
use viewink::stats::{add_plane, add_staged_bytes};

/// 帧纹理平面上传器（挂 iced 主帧 encoder + staging belt）。
///
/// 数据直接写进 [`wgpu::util::StagingBelt`] 的映射暂存切片（**单次 CPU
/// 拷贝**，替代旧 write_buffer 内部再拷贝的双拷贝路径），拷贝命令记入
/// 调用方帧 encoder，随 iced 主帧统一 finish → submit → recall——上传
/// 不再自建 encoder 独立提交（对齐 Chromium：上传并入帧提交 + 映射
/// transfer buffer 单拷贝写）。
///
/// belt 的映射切片偏移由 [`wgpu::util::StagingBelt::allocate`] 按 512
/// 对齐分配（满足 copy 校验的 4 字节 `COPY_BUFFER_ALIGNMENT` 并留裕量）。
#[derive(Debug, Default)]
pub struct PaddedPlaneUploader;

impl PaddedPlaneUploader {
    pub fn new() -> Self {
        Self
    }

    /// 上传单个平面（拷贝命令记入调用方帧 encoder，无独立 submit）。
    ///
    /// - `data`：连续像素，长度必须为 `width_bytes * height`
    /// - `width_bytes`：每行字节数
    /// - `width_texels`：纹素宽度（extent 用，与格式相关）
    #[allow(clippy::too_many_arguments)]
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        texture: &wgpu::Texture,
        data: &[u8],
        width_bytes: u32,
        width_texels: u32,
        height: u32,
    ) {
        debug_assert_eq!(data.len(), (width_bytes as usize) * (height as usize));
        let size = wgpu::Extent3d {
            width: width_texels,
            height,
            depth_or_array_layers: 1,
        };
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64;
        let aligned = (width_bytes as u64).is_multiple_of(align);
        let width_bytes = width_bytes as usize;
        // 行距 256 对齐（行尾 padding 区不被 GPU 读——copy 按 size.width
        // 只取每行前 width_bytes 字节，无需清零）。
        let pitch = if aligned {
            width_bytes
        } else {
            ((width_bytes as u64).div_ceil(align) * align) as usize
        };
        let staged_len = pitch * height as usize;
        add_plane(aligned, data.len() as u64);

        // 映射暂存切片：分配（belt 内部 MAP_WRITE|COPY_SRC，稳态循环
        // 复用）→ 直接写入（单次 CPU 拷贝）→ 视图丢弃后记拷贝命令。
        let slice = belt.allocate(
            wgpu::BufferSize::new(staged_len as u64).expect("non-zero staged length"),
            wgpu::BufferSize::new(512).expect("constant non-zero"),
            device,
        );
        {
            let mut view = slice.get_mapped_range_mut();
            if pitch == width_bytes {
                // 行距已对齐：整块连续写（单 memcpy）
                view[..data.len()].copy_from_slice(data);
            } else {
                // 逐行拷入填充布局
                for (row, chunk) in data.chunks_exact(width_bytes).enumerate() {
                    view[row * pitch..row * pitch + width_bytes].copy_from_slice(chunk);
                }
            }
        } // 视图在 belt finish 前丢弃

        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: slice.buffer(),
                layout: wgpu::TexelCopyBufferLayout {
                    offset: slice.offset(),
                    bytes_per_row: Some(pitch as u32),
                    rows_per_image: None,
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            size,
        );
        add_staged_bytes(staged_len as u64);
    }
}
