//! 直通合成通道的一次性注册。
//!
//! # 为什么不是普通 widget
//!
//! 白板底色、摄像头背景、批注笔迹这三层都不走 iced 的 widget/Scene 通路，而是
//! 作为「直通通道」直接画进 frame。这样做的原因写在 [`viewink::annotation`] 里：
//! 增量镶嵌的产物需要**当帧上传、当帧呈现**，而经过 iced Scene 会被归一化到它
//! 自己的批次与纹理生命周期里。
//!
//! 注册顺序 = 合成顺序：白板底色在底、摄像头背景在其上、批注笔迹再上、iced UI
//! 层与文字最上（与旧架构 z 序一致）。

use std::sync::Arc;

use viewgfx::CanvasGfx;

/// 诊断开关：跳过摄像头层（分层定位 GPU 占用）。
///
/// `VIEWSTAGE_NO_CAMERA=1` 时摄像头背景层完全不注册。
pub fn diag_no_camera() -> bool {
    static NO_CAMERA: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_CAMERA.get_or_init(|| std::env::var("VIEWSTAGE_NO_CAMERA").is_ok_and(|v| v != "0"))
}

/// 白板底色（`#1a1a1a`）。
const CLEAR_COLOR: [f32; 4] = [26.0 / 255.0, 26.0 / 255.0, 26.0 / 255.0, 1.0];

/// 注册全部直通合成通道。**每个进程只需调用一次**（由 [`Channels`] 幂等保证）。
///
/// 白板底色不是通道——它不画任何东西，只是声明直通 pass 的 `LoadOp::Clear`。
/// 早先建模成一个 45 行全空实现的 `DirectChannel`，每帧要多一次堆分配对象 +
/// 两次虚调用 + render 里一次 find_map。
///
/// 必须在**主窗口**的 canvas draw 副作用处调用：与同帧 `prepare` 接管处于同一
/// redraw 原子段，辅助窗口的 renderer 才没有"通道尚未注册"的窗口期。
pub fn register(gfx: &CanvasGfx) {
    iced_wgpu::set_frame_clear_color(CLEAR_COLOR);
    log::info!("[direct] frame clear color set");

    if !diag_no_camera() {
        iced_wgpu::register_direct_channel(Box::new(
            viewcamera::camera::CameraChannel::new(gfx.camera_params_handle()),
        ));
        log::info!("[direct] camera channel registered");
    }
    iced_wgpu::register_direct_channel(Box::new(
        viewink::annotation::AnnotationLayer::new(Arc::clone(&gfx.live)),
    ));
    log::info!("[direct] annotation channel registered");
}

/// 直通通道注册的一次性闸门。
///
/// `Cell<bool>` 而非裸字段：宿主在 `canvas::Program::draw(&self)` 里调用它，
/// 那里只有 `&self`。
pub struct Channels {
    registered: std::cell::Cell<bool>,
}

impl Channels {
    /// 尚未注册的状态。
    pub fn new() -> Self {
        Self {
            registered: std::cell::Cell::new(false),
        }
    }

    /// 若尚未注册则注册（幂等）。
    pub fn ensure_registered(&self, gfx: &CanvasGfx) {
        if self.registered.get() {
            return;
        }
        self.registered.set(true);
        register(gfx);
    }
}

impl Default for Channels {
    fn default() -> Self {
        Self::new()
    }
}
