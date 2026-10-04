//! 屏幕坐标 ↔ 画布坐标变换（f32 全程，零分配，`Copy` 值）。
//!
//! 变换模型与渲染侧（viewink `ViewTransform` / `Globals`）一致：
//!
//! ```text
//! canvas = (screen - offset) / scale
//! screen = canvas * scale + offset
//! ```

/// 视图变换（12 字节 `Copy`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenTransform {
    pub offset_x: f32,
    pub offset_y: f32,
    pub scale: f32,
}

impl ScreenTransform {
    /// 缩放下限（滚轮 / 捏合共用）。
    pub const MIN_SCALE: f32 = 0.1;
    /// 缩放上限（滚轮 / 捏合共用）。
    pub const MAX_SCALE: f32 = 10.0;

    #[inline]
    pub const fn new(offset_x: f32, offset_y: f32, scale: f32) -> Self {
        Self {
            offset_x,
            offset_y,
            scale,
        }
    }

    /// 屏幕 → 画布。
    #[inline]
    pub fn screen_to_canvas(&self, x: f32, y: f32) -> (f32, f32) {
        (
            (x - self.offset_x) / self.scale,
            (y - self.offset_y) / self.scale,
        )
    }

    /// 画布 → 屏幕。
    #[inline]
    pub fn canvas_to_screen(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale + self.offset_x, y * self.scale + self.offset_y)
    }

    /// 捏合锚点换算：屏幕中点 → 画布锚点。
    ///
    /// 除法换倒数（1 次除法 + 2 次乘法）；scale 以 0.001 下限防退化，
    /// 与旧行为一致。
    #[inline]
    pub fn pinch_anchor(&self, center_x: f32, center_y: f32) -> (f32, f32) {
        let inv = 1.0 / self.scale.max(0.001);
        (
            (center_x - self.offset_x) * inv,
            (center_y - self.offset_y) * inv,
        )
    }

    /// 应用捏合增量：以屏幕点 `(center_x, center_y)` 为视觉锚点缩放，
    /// 保持画布锚点 `(anchor_cx, anchor_cy)` 始终位于该屏幕点之下。
    ///
    /// 缩放结果钳位到 [`Self::MIN_SCALE`]..=[`Self::MAX_SCALE`]。
    #[inline]
    pub fn apply_pinch(
        &mut self,
        center_x: f32,
        center_y: f32,
        scale_ratio: f32,
        anchor_cx: f32,
        anchor_cy: f32,
    ) {
        let new_scale = (self.scale * scale_ratio).clamp(Self::MIN_SCALE, Self::MAX_SCALE);
        self.offset_x = center_x - anchor_cx * new_scale;
        self.offset_y = center_y - anchor_cy * new_scale;
        self.scale = new_scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_canvas_roundtrip() {
        let t = ScreenTransform::new(30.0, -20.0, 2.5);
        let (cx, cy) = t.screen_to_canvas(105.0, 55.0);
        assert!((cx - 30.0).abs() < 1e-4);
        assert!((cy - 30.0).abs() < 1e-4);
        let (sx, sy) = t.canvas_to_screen(cx, cy);
        assert!((sx - 105.0).abs() < 1e-3);
        assert!((sy - 55.0).abs() < 1e-3);
    }

    #[test]
    fn apply_pinch_keeps_anchor_under_center() {
        let mut t = ScreenTransform::new(0.0, 0.0, 1.0);
        let center = (200.0, 150.0);
        let (ax, ay) = t.pinch_anchor(center.0, center.1);
        t.apply_pinch(center.0, center.1, 2.0, ax, ay);
        // 锚点画布点缩放后仍应位于同一屏幕点
        let (sx, sy) = t.canvas_to_screen(ax, ay);
        assert!((sx - center.0).abs() < 1e-3);
        assert!((sy - center.1).abs() < 1e-3);
        assert!((t.scale - 2.0).abs() < 1e-6);
    }

    #[test]
    fn apply_pinch_clamps_scale() {
        let mut t = ScreenTransform::new(0.0, 0.0, 9.5);
        t.apply_pinch(100.0, 100.0, 2.0, 0.0, 0.0);
        assert_eq!(t.scale, ScreenTransform::MAX_SCALE);
        t.apply_pinch(100.0, 100.0, 0.01, 0.0, 0.0);
        assert_eq!(t.scale, ScreenTransform::MIN_SCALE);
    }

    #[test]
    fn pinch_anchor_guards_zero_scale() {
        let t = ScreenTransform::new(10.0, 10.0, 0.0);
        let (ax, ay) = t.pinch_anchor(20.0, 20.0);
        assert!(ax.is_finite() && ay.is_finite(), "零 scale 不得产生 inf/nan");
    }
}
