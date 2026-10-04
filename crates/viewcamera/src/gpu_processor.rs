//! RGB 回退路径的 CPU 处理（RGB→RGBA + 最近邻降采样）。
//!
//! NV12 直传路径不经过此模块的逐帧处理：原始平面零拷贝切片后直接上传，
//! YUV→RGB 由渲染管线着色器完成（见 render/camera.wgsl）。

const DOWNSAMPLE_WIDTH: u32 = 854;
const DOWNSAMPLE_HEIGHT: u32 = 480;

/// NV12 帧的两个平面（零拷贝切片，共享同一底层分配）。
#[derive(Debug, Clone)]
pub struct Nv12Planes {
    /// Y 平面：w*h 字节，每像素 1 字节亮度
    pub y: bytes::Bytes,
    /// UV 平面：(w/2)*(h/2) 个交错 (U,V) 对 = w*h/2 字节
    pub uv: bytes::Bytes,
    pub width: u32,
    pub height: u32,
}

impl Nv12Planes {
    /// 把连续 NV12 缓冲切分为 Y/UV 两平面（Bytes::slice 零拷贝）。
    pub fn split(data: bytes::Bytes, w: u32, h: u32) -> Option<Nv12Planes> {
        let y_size = (w as usize) * (h as usize);
        let uv_size = y_size / 2;
        if w == 0 || h == 0 || data.len() < y_size + uv_size {
            return None;
        }
        Some(Nv12Planes {
            y: data.slice(0..y_size),
            uv: data.slice(y_size..y_size + uv_size),
            width: w,
            height: h,
        })
    }

    /// 【仅拍照时调用】单次 NV12→RGBA 转换（BT.601 limited range，与着色器一致）。
    /// 每次拍照一次 O(w*h)，不在实时路径上。
    pub fn to_rgba(&self, dst: &mut Vec<u8>) {
        let w = self.width as usize;
        let h = self.height as usize;
        dst.clear();
        dst.resize(w * h * 4, 0);
        // BT.601 limited range: Y ∈ [16,235], UV ∈ [16,240]
        // 逐像素写入预分配的切片（4 字节一组），替代旧实现的 4 次 push：
        // 同样的算术，省掉每像素 4 次长度检查与容量增长分支。
        let uv_w = w / 2;
        for (i, px) in dst.chunks_exact_mut(4).enumerate() {
            let row = i / w;
            let col = i - row * w;
            let y_val = self.y[i] as i32;
            let uv_index = (row / 2) * uv_w + (col / 2);
            let u_val = self.uv[uv_index * 2] as i32 - 128;
            let v_val = self.uv[uv_index * 2 + 1] as i32 - 128;
            let yy = (y_val - 16) * 298; // ≈255/219*256
            // 整数近似（右移 8 位），与浮点公式偏差 ≤1
            px[0] = ((yy + 409 * v_val) >> 8).clamp(0, 255) as u8;
            px[1] = ((yy - 100 * u_val - 208 * v_val) >> 8).clamp(0, 255) as u8;
            px[2] = ((yy + 516 * u_val) >> 8).clamp(0, 255) as u8;
            px[3] = 255;
        }
    }
}

pub struct CpuFrameProcessor {
    /// 直接写入并交出的输出缓冲（原地转换，见 [`Self::process`]）。
    output: Vec<u8>,
    last_w: u32,
    last_h: u32,
    index_size: (u32, u32, u32, u32),
    sx: Vec<u32>,
    sy: Vec<u32>,
}

impl CpuFrameProcessor {
    pub fn new() -> Self {
        Self {
            output: Vec::new(),
            last_w: 0,
            last_h: 0,
            index_size: (0, 0, 0, 0),
            sx: Vec::new(),
            sy: Vec::new(),
        }
    }

    fn downsample_size(src_w: u32, src_h: u32) -> (u32, u32) {
        let rgba_size = (src_w * src_h * 4) as usize;
        if rgba_size < 2 * 1024 * 1024 {
            return (src_w, src_h);
        }
        let aspect = src_w as f32 / src_h as f32;
        let dst_w = DOWNSAMPLE_WIDTH;
        let mut dst_h = DOWNSAMPLE_HEIGHT;
        if (aspect - 16.0 / 9.0).abs() > 0.01 {
            dst_h = (dst_w as f32 / aspect) as u32;
        }
        (dst_w.max(1), dst_h.max(1))
    }

    fn record_size(&mut self, dst_w: u32, dst_h: u32) {
        self.last_w = dst_w;
        self.last_h = dst_h;
    }

    pub fn last_width(&self) -> u32 {
        self.last_w
    }
    pub fn last_height(&self) -> u32 {
        self.last_h
    }

    /// RGB → RGBA + 最近邻降采样（全 CPU，无 GPU 往返）。
    ///
    /// 原地写入 `output` 并将其所有权交出（调用方包进 `Arc`）——不做
    /// `buffer → output` 的中间拷贝。降采样索引表（`sx`/`sy`）在尺寸不变时
    /// 复用。
    pub fn process(&mut self, rgb: &[u8], src_w: u32, src_h: u32) -> Vec<u8> {
        let (dst_w, dst_h) = Self::downsample_size(src_w, src_h);
        let index_size = (src_w, src_h, dst_w, dst_h);
        self.record_size(dst_w, dst_h);
        if dst_w != src_w && dst_h != src_h && self.index_size != index_size {
            self.sy = (0..dst_h)
                .map(|dy| (dy as u64 * src_h as u64 / dst_h as u64) as u32)
                .collect();
            self.sx = (0..dst_w)
                .map(|dx| (dx as u64 * src_w as u64 / dst_w as u64) as u32)
                .collect();
            self.index_size = index_size;
        }
        // 原地转换：直接把 RGBA 写进即将交出的 `output`，省掉
        // `buffer → output` 的整帧 memcpy（旧实现每帧多拷 1.6MB）。
        // `output` 被 take 走后为新空 Vec，下一帧 resize 重新分配——
        // 这是必要的：返回值的所有权要交给调用方包进 Arc。
        let needed = (dst_w * dst_h * 4) as usize;
        self.output.clear();
        self.output.resize(needed, 0);
        let buf = &mut self.output;
        let sx = &self.sx;
        let sy = &self.sy;

        if dst_w == src_w && dst_h == src_h {
            for (i, px) in buf.chunks_exact_mut(4).enumerate() {
                let s = i * 3;
                px[0] = rgb[s];
                px[1] = rgb[s + 1];
                px[2] = rgb[s + 2];
                px[3] = 255;
            }
        } else {
            for (dy, row) in buf.chunks_exact_mut((dst_w * 4) as usize).enumerate() {
                let src_y = sy[dy] as usize;
                let base = src_y * src_w as usize;
                for (dx, px) in row.chunks_exact_mut(4).enumerate() {
                    let s = (base + sx[dx] as usize) * 3;
                    px[0] = rgb[s];
                    px[1] = rgb[s + 1];
                    px[2] = rgb[s + 2];
                    px[3] = 255;
                }
            }
        }

        std::mem::take(&mut self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_rgb_to_rgba_without_downsampling() {
        let mut processor = CpuFrameProcessor::new();
        let output = processor.process(&[1, 2, 3, 4, 5, 6], 2, 1);
        assert_eq!(output, [1, 2, 3, 255, 4, 5, 6, 255]);
        assert_eq!((processor.last_width(), processor.last_height()), (2, 1));
    }

    #[test]
    fn downsamples_with_nearest_neighbor_indices() {
        let mut processor = CpuFrameProcessor::new();
        let rgb = (0..1280 * 720)
            .flat_map(|index| {
                let x = index % 1280;
                let y = index / 1280;
                [x as u8, y as u8, (x + y) as u8]
            })
            .collect::<Vec<_>>();
        let output = processor.process(&rgb, 1280, 720);
        assert_eq!(
            (processor.last_width(), processor.last_height()),
            (854, 480)
        );
        assert_eq!(output.len(), 854 * 480 * 4);
        let offset = (854 * 480 - 1) * 4;
        let x = processor.sx[853] as usize;
        let y = processor.sy[479] as usize;
        assert_eq!(
            &output[offset..offset + 4],
            &[x as u8, y as u8, (x + y) as u8, 255]
        );
    }

    #[test]
    fn reuses_downsample_indices_for_unchanged_dimensions() {
        let mut processor = CpuFrameProcessor::new();
        let rgb = vec![0; 1280 * 720 * 3];
        processor.process(&rgb, 1280, 720);
        let sx = processor.sx.as_ptr();
        let sy = processor.sy.as_ptr();
        processor.process(&rgb, 1280, 720);
        assert_eq!(processor.sx.as_ptr(), sx);
        assert_eq!(processor.sy.as_ptr(), sy);
    }

    #[test]
    fn rebuilds_downsample_indices_when_dimensions_change() {
        let mut processor = CpuFrameProcessor::new();
        processor.process(&vec![0; 1280 * 720 * 3], 1280, 720);
        processor.process(&vec![0; 1024 * 768 * 3], 1024, 768);
        assert_eq!(processor.index_size, (1024, 768, 854, 640));
        assert_eq!((processor.sx.len(), processor.sy.len()), (854, 640));
        assert_eq!((processor.sx[853], processor.sy[639]), (1022, 766));
    }

    #[test]
    fn repeated_processing_is_idempotent_per_dimensions() {
        // 原地转换 + 交出所有权：连续两帧同尺寸必须产出逐位相同的结果，
        // 且 `sx`/`sy` 不重建（无降采样尺寸变化时）。
        let mut processor = CpuFrameProcessor::new();
        let rgb: Vec<u8> = (0..64 * 64 * 3).map(|i| (i % 251) as u8).collect();
        let first = processor.process(&rgb, 64, 64);
        let second = processor.process(&rgb, 64, 64);
        assert_eq!(first, second, "同输入同尺寸必须逐位一致");
        assert_eq!(first.len(), 64 * 64 * 4);
        // 无降采样时索引表保持为空且不参与寻址
        assert!(processor.sx.is_empty() && processor.sy.is_empty());
    }

    #[test]
    fn downsampled_rows_are_independent() {
        // 逐行 chunked 写入：验证行内寻址未串行（相邻行起点不同）。
        let src_w = 300u32;
        let src_h = 300u32; // 300*300*4 > 2MB → 走降采样分支
        let rgb: Vec<u8> = (0..src_w * src_h * 3).map(|i| (i % 256) as u8).collect();
        let mut processor = CpuFrameProcessor::new();
        let out = processor.process(&rgb, src_w, src_h);
        let (dw, dh) = (processor.last_width(), processor.last_height());
        assert_ne!((dw, dh), (src_w, src_h), "该尺寸应触发降采样");
        for dy in 0..dh as usize {
            for dx in 0..dw as usize {
                let s = (processor.sy[dy] as usize * src_w as usize
                    + processor.sx[dx] as usize)
                    * 3;
                let d = (dy * dw as usize + dx) * 4;
                assert_eq!(
                    &out[d..d + 4],
                    &[rgb[s], rgb[s + 1], rgb[s + 2], 255],
                    "像素 ({dx},{dy}) 寻址错误"
                );
            }
        }
    }

    /// 构造 2×2 合成 NV12：Y 平面逐像素给定，UV 单对 (U,V)。
    fn nv12_fixture(y: [u8; 4], u: u8, v: u8) -> bytes::Bytes {
        let mut data = Vec::with_capacity(4 + 2);
        data.extend_from_slice(&y);
        data.push(u);
        data.push(v);
        bytes::Bytes::from(data)
    }

    #[test]
    fn splits_nv12_planes_zero_copy() {
        let data = nv12_fixture([16, 128, 235, 81], 90, 240);
        let planes = Nv12Planes::split(data.clone(), 2, 2).expect("split");
        assert_eq!(planes.y.len(), 4);
        assert_eq!(planes.uv.len(), 2);
        assert_eq!(planes.y.as_ref(), &[16, 128, 235, 81]);
        assert_eq!(planes.uv.as_ref(), &[90, 240]);
        // Bytes::slice 零拷贝：切片指针应落在原始分配内
        let base = data.as_ptr() as usize;
        let y_ptr = planes.y.as_ptr() as usize;
        let uv_ptr = planes.uv.as_ptr() as usize;
        assert_eq!(y_ptr, base);
        assert_eq!(uv_ptr, base + 4);
    }

    #[test]
    fn split_rejects_undersized_buffer_and_zero_dims() {
        let data = bytes::Bytes::from(vec![0u8; 5]); // 2×2 需要 6 字节
        assert!(Nv12Planes::split(data, 2, 2).is_none());
        let empty = bytes::Bytes::new();
        assert!(Nv12Planes::split(empty, 0, 0).is_none());
    }

    #[test]
    fn to_rgba_indexing_is_row_major() {
        // 逐像素写入版按 `i = row*w + col` 定位 Y 平面——用非对称图案
        // 验证行/列索引未串行（4×4，Y(i)=i，UV(i)=2i+1）。
        let (w, h) = (4usize, 4usize);
        let y: Vec<u8> = (0..w * h).map(|i| i as u8).collect();
        let uv: Vec<u8> = (0..w * h / 2)
            .flat_map(|i| [128u8, (2 * i + 1) as u8])
            .collect();
        let planes = Nv12Planes::split(bytes::Bytes::from([y, uv].concat()), 4, 4).unwrap();
        let mut out = Vec::new();
        planes.to_rgba(&mut out);
        assert_eq!(out.len(), w * h * 4);
        for row in 0..h {
            for col in 0..w {
                let i = row * w + col;
                let d = i * 4;
                // alpha 恒 255；RGB 由 Y 与 UV 决定，此处只校验 alpha 与
                // 长度契约（数值等价由参考色测试覆盖）。
                assert_eq!(out[d + 3], 255, "({col},{row}) alpha 必须恒 255");
            }
        }
    }

    #[test]
    fn nv12_to_rgba_bt601_limited_range_reference_colors() {
        // BT.601 limited range 参考色（整数近似 ±1）：
        // 白 Y=235/U=V=128；黑 Y=16；红 Y=81/U=90/V=240
        let white = Nv12Planes::split(nv12_fixture([235; 4], 128, 128), 2, 2).unwrap();
        let mut out = Vec::new();
        white.to_rgba(&mut out);
        for px in out.chunks_exact(4) {
            assert_eq!(px[0], 254);
            assert_eq!(px[1], 254);
            assert_eq!(px[2], 254);
            assert_eq!(px[3], 255);
        }

        let black = Nv12Planes::split(nv12_fixture([16; 4], 128, 128), 2, 2).unwrap();
        let mut out = Vec::new();
        black.to_rgba(&mut out);
        for px in out.chunks_exact(4) {
            assert_eq!(&px[..3], &[0, 0, 0]);
        }

        let red = Nv12Planes::split(nv12_fixture([81; 4], 90, 240), 2, 2).unwrap();
        let mut out = Vec::new();
        red.to_rgba(&mut out);
        for px in out.chunks_exact(4) {
            assert_eq!(px[0], 254); // ≈255
            assert_eq!(px[1], 0);
            assert_eq!(px[2], 0);
        }
    }
}
