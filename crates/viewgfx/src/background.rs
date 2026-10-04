//! 摄像头/图片背景层图元的 dirty-cache。
//!
//! 原实现（主 crate build_camera_layer）在 view() 每帧重建 [`CameraLayer`]
//! 并 `Arc::new` 包装——即使滤镜参数与帧完全未变。本模块引入参数指纹：
//!
//! - 指纹未命中才重建；命中时返回**同一个 `Arc<CameraLayer>`**（零分配）。
//! - pan/zoom 不参与指纹：变换是共享 [`LiveTransform`] 句柄，图元直读最新值，
//!   缩放/平移天然零重建。
//! - ABA 防护：缓存持有旧帧的 `Arc` 克隆，旧帧分配地址不可能被复用，
//!   `Arc::as_ptr` 指纹不会被新帧「撞号」；generation 作为第二道保险。

use std::cell::RefCell;
use std::sync::Arc;

use viewcamera::camera::{CameraFrameData, CameraLayer};
use viewink::live::LiveTransform;

/// 背景层构建输入（view() 每帧从 App 状态收集，全部 Copy/引用，零分配）。
pub struct BackgroundInputs<'a> {
    /// 实时摄像头帧（摄像头源）。
    pub camera_frame: Option<&'a Arc<CameraFrameData>>,
    /// 静态图片（图片源，优先于摄像头帧显示）。
    pub background_image: Option<&'a Arc<CameraFrameData>>,
    /// 亮度 -100..100（设置面板原值）。
    pub brightness: f64,
    /// 对比度 0..300（设置面板原值）。
    pub contrast: f64,
    /// 黑白模式。
    pub grayscale: bool,
    /// 旋转角度（度数，0/90/180/270）。
    pub rotation: i32,
    /// 无任何帧时的分辨率兜底（YUV 矩阵选择依据）。
    pub fallback_dims: (u32, u32),
}

/// 背景层参数指纹。
#[derive(Debug, Clone, Copy, PartialEq)]
struct BgKey {
    /// 摄像头帧 Arc 指针（None = 0）。
    frame_ptr: usize,
    /// 摄像头帧代数（ptr 之外的第二道保险）。
    frame_gen: u64,
    /// 静态图片 Arc 指针（None = 0）。
    image_ptr: usize,
    /// 归一化亮度（f32 位模式，消除 f64→f32 舍入抖动）。
    brightness: u32,
    /// 归一化对比度（f32 位模式）。
    contrast: u32,
    grayscale: bool,
    rotation: i32,
    /// YUV 矩阵标志（f32 位模式）。
    yuv: u32,
}

/// 背景层 dirty-cache。
#[derive(Default)]
pub struct BackgroundCache {
    cached: RefCell<Option<(BgKey, Arc<CameraLayer>)>>,
}

impl BackgroundCache {
    /// 构建背景层图元：指纹命中返回缓存 `Arc`（零分配），未命中重建并缓存。
    pub fn camera_layer(
        &self,
        transform: &Arc<LiveTransform>,
        input: BackgroundInputs<'_>,
    ) -> Arc<CameraLayer> {
        // 激活源分辨率：图片优先，其次摄像头帧，最后配置兜底
        let active = input.background_image.or(input.camera_frame);
        let (w, h) = active
            .map(|f| (f.width, f.height))
            .unwrap_or(input.fallback_dims);
        let yuv = CameraLayer::yuv_matrix_flag(w, h);
        let brightness = (input.brightness as f32 / 100.0).to_bits();
        let contrast = (input.contrast as f32 / 100.0).to_bits();

        let key = BgKey {
            frame_ptr: input.camera_frame.map_or(0, |a| Arc::as_ptr(a) as usize),
            frame_gen: input.camera_frame.map_or(0, |f| f.generation),
            image_ptr: input.background_image.map_or(0, |a| Arc::as_ptr(a) as usize),
            brightness,
            contrast,
            grayscale: input.grayscale,
            rotation: input.rotation,
            yuv: yuv.to_bits(),
        };

        {
            let cached = self.cached.borrow();
            if let Some((k, layer)) = cached.as_ref() {
                if *k == key {
                    return Arc::clone(layer);
                }
            }
        }

        // 滤镜参数归一化（与设置面板显示一致）：
        //   brightness(-100..100) → /100 → [-1, 1]，0=正常
        //   contrast(0..300)      → /100 → [0, 3]，1=正常
        //   w = NV12 YUV 矩阵标志（按激活源分辨率选 BT.601/709）
        let layer = Arc::new(CameraLayer {
            transform: Some(Arc::clone(transform)),
            frame: input.camera_frame.cloned(),
            image: input.background_image.cloned(),
            brightness_contrast: [
                input.brightness as f32 / 100.0,
                input.contrast as f32 / 100.0,
                if input.grayscale { 1.0 } else { 0.0 },
                yuv,
            ],
            rotation_degrees: input.rotation,
        });
        *self.cached.borrow_mut() = Some((key, Arc::clone(&layer)));
        layer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use viewcamera::camera::FramePixels;

    fn dummy_frame(gen: u64) -> Arc<CameraFrameData> {
        Arc::new(CameraFrameData {
            pixels: FramePixels::Rgba(Arc::new(vec![0u8; 4])),
            width: 640,
            height: 480,
            generation: gen,
        })
    }

    fn input<'a>(
        frame: Option<&'a Arc<CameraFrameData>>,
        image: Option<&'a Arc<CameraFrameData>>,
        brightness: f64,
    ) -> BackgroundInputs<'a> {
        BackgroundInputs {
            camera_frame: frame,
            background_image: image,
            brightness,
            contrast: 100.0,
            grayscale: false,
            rotation: 0,
            fallback_dims: (1920, 1080),
        }
    }

    #[test]
    fn same_inputs_hit_cache() {
        let cache = BackgroundCache::default();
        let transform = Arc::new(LiveTransform::new(1.0));
        let frame = dummy_frame(1);

        let a = cache.camera_layer(&transform, input(Some(&frame), None, 0.0));
        let b = cache.camera_layer(&transform, input(Some(&frame), None, 0.0));
        assert!(Arc::ptr_eq(&a, &b), "输入未变必须命中缓存返回同一 Arc");
    }

    #[test]
    fn changed_filter_rebuilds() {
        let cache = BackgroundCache::default();
        let transform = Arc::new(LiveTransform::new(1.0));
        let frame = dummy_frame(1);

        let a = cache.camera_layer(&transform, input(Some(&frame), None, 0.0));
        let b = cache.camera_layer(&transform, input(Some(&frame), None, 25.0));
        assert!(!Arc::ptr_eq(&a, &b));
        assert!((b.brightness_contrast[0] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn changed_frame_rebuilds() {
        let cache = BackgroundCache::default();
        let transform = Arc::new(LiveTransform::new(1.0));
        let f1 = dummy_frame(1);
        let f2 = dummy_frame(2);

        let a = cache.camera_layer(&transform, input(Some(&f1), None, 0.0));
        let b = cache.camera_layer(&transform, input(Some(&f2), None, 0.0));
        assert!(!Arc::ptr_eq(&a, &b));
        // 新图元必须指向新帧（不许命中缓存拿到旧帧）
        assert!(b.frame.as_ref().is_some_and(|f| Arc::ptr_eq(f, &f2)));
    }

    #[test]
    fn yuv_flag_follows_active_dims() {
        let cache = BackgroundCache::default();
        let transform = Arc::new(LiveTransform::new(1.0));
        let small = Arc::new(CameraFrameData {
            pixels: FramePixels::Rgba(Arc::new(vec![0u8; 4])),
            width: 640,
            height: 480,
            generation: 0,
        });
        let layer = cache.camera_layer(&transform, input(Some(&small), None, 0.0));
        assert_eq!(layer.brightness_contrast[3], 0.0, "480p 应使用 BT.601");

        let big = Arc::new(CameraFrameData {
            pixels: FramePixels::Rgba(Arc::new(vec![0u8; 4])),
            width: 1920,
            height: 1080,
            generation: 1,
        });
        let layer = cache.camera_layer(&transform, input(Some(&big), None, 0.0));
        assert_eq!(layer.brightness_contrast[3], 1.0, "1080p 应使用 BT.709");
    }
}
