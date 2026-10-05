//! 画布视图变换：缩放、平移、屏幕↔画布换算。
//!
//! # 为什么变换要收进渲染层
//!
//! 变换的**状态**本来就在渲染侧（`LiveCanvasState::transform` 的三个 `Cell`），
//! 但应用此前直接读写它们——15 处写入散落在 `main.rs`，其中 6 处是
//! "读出旧值、算一遍、写回去"的手工组合。这类代码的问题不是难看，而是
//! **不变量没有地方可查**：缩放钳位是 `0.1..=10.0`，它同时出现在滚轮缩放里，
//! 也存在于 `Viewport::MIN_SCALE/MAX_SCALE`，两处各写一份字面量。
//! 改其中一个，另一个静默不跟随——而缩放越界是那种"没人会发现直到某天
//! 画布缩到看不见"的问题。
//!
//! 所以本模块把变换的**决策**收成几个语义操作，状态仍留在渲染侧：
//!
//! | 操作 | 语义 |
//! |---|---|
//! | [`CanvasView::reset`] | 回到恒等变换（切源时） |
//! | [`CanvasView::set_view`] | 恢复到某个已知变换（源状态回放） |
//! | [`CanvasView::pan_by`] | 画布平移增量（拖拽 / 惯性 / 捏合共用） |
//! | [`CanvasView::zoom_at`] | 以某个屏幕点为视觉锚点缩放 |
//!
//! # 缩放锚点为什么复用 Viewport
//!
//! [`zoom_at`] 不自己实现锚点数学，而是构造 [`Viewport`] 后调
//! [`Viewport::apply_pinch`]。理由：那段数学（先求世界坐标、缩放、再由
//! 锚点反解偏移）已经被手写过一份，两者必须永远一致，而"两份实现"正是它们会
//! 漂移的原因。钳位也因此自动跟随 `MIN_SCALE`/`MAX_SCALE`。
//!
//! # 全部方法仅 `&self`
//!
//! 拖拽与惯性都在 `canvas::Program` 的零消息路径上调用，那里只有 `&self`。

use zephyra::input::Viewport;
use viewink::live::LiveCanvasState;

/// 画布视图变换的操作面。
///
/// 持共享实时状态的**引用**而非 `Arc`：它总是从 `App` 的 `gfx`/`live` 借来，
/// 不需要独立所有权。
pub struct CanvasView<'a> {
    live: &'a LiveCanvasState,
}

impl<'a> CanvasView<'a> {
    /// 绑定到共享实时状态。
    pub fn new(live: &'a LiveCanvasState) -> Self {
        Self { live }
    }

    /// 当前缩放。
    pub fn scale(&self) -> f32 {
        self.live.transform.scale.get()
    }

    /// 当前偏移 `(offset_x, offset_y)`。
    pub fn offset(&self) -> (f32, f32) {
        (
            self.live.transform.offset_x.get(),
            self.live.transform.offset_y.get(),
        )
    }

    /// 屏幕坐标 → 画布坐标。
    pub fn screen_to_canvas(&self, screen_x: f32, screen_y: f32) -> (f32, f32) {
        let (offset_x, offset_y) = self.offset();
        Viewport::new(offset_x, offset_y, self.scale()).screen_to_canvas(screen_x, screen_y)
    }

    /// 回到恒等变换。
    ///
    /// 切源时用：目标源的缩放/偏移不继承当前视图。
    pub fn reset(&self) {
        self.set_view(1.0, 0.0, 0.0);
    }

    /// 恢复到某个已知变换（源状态回放）。
    pub fn set_view(&self, scale: f32, offset_x: f32, offset_y: f32) {
        self.live.transform.scale.set(scale);
        self.live.transform.offset_x.set(offset_x);
        self.live.transform.offset_y.set(offset_y);
    }

    /// 画布平移增量。
    ///
    /// 拖拽、惯性滑动、捏合平移三者语义相同——都只是让画布跟着手走一段——
    /// 所以共用这一个入口，不各自重复"读出、加、写回"。
    pub fn pan_by(&self, dx: f32, dy: f32) {
        let t = &self.live.transform;
        t.offset_x.set(t.offset_x.get() + dx);
        t.offset_y.set(t.offset_y.get() + dy);
    }

    /// 以屏幕点 `(cursor_x, cursor_y)` 为**视觉锚点**缩放，保持该点之下的
    /// 画布点不动。
    ///
    /// 缩放结果钳位到 [`Viewport::MIN_SCALE`]..=[`Viewport::MAX_SCALE`]。
    /// 之所以不自己算：这段锚点数学是 `zephyra::input::Viewport` 里的同一份实现，
    /// 缩放的钳位范围也由它单点定义，两处不可能漂移。
    pub fn zoom_at(&self, cursor_x: f32, cursor_y: f32, scale_ratio: f32) {
        let (offset_x, offset_y) = self.offset();
        let scale = self.scale();
        let mut transform = Viewport::new(offset_x, offset_y, scale);

        // 锚点 = 缩放前该屏幕点之下的画布点。
        let (anchor_cx, anchor_cy) = transform.screen_to_canvas(cursor_x, cursor_y);
        transform.apply_pinch(cursor_x, cursor_y, scale_ratio, anchor_cx, anchor_cy);

        self.live.transform.scale.set(transform.scale);
        self.live.transform.offset_x.set(transform.offset_x);
        self.live.transform.offset_y.set(transform.offset_y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 就地建一个状态并借出操作面——测试里不需要 `'static`。
    fn with_view(f: impl FnOnce(&CanvasView<'_>)) {
        let live = LiveCanvasState::new(1.0);
        f(&CanvasView::new(&live));
    }

    #[test]
    fn reset_restores_identity() {
        with_view(|v| {
            v.set_view(3.0, 40.0, -20.0);
            v.reset();
            assert_eq!(v.scale(), 1.0);
            assert_eq!(v.offset(), (0.0, 0.0));
        });
    }

    #[test]
    fn pan_by_accumulates() {
        with_view(|v| {
            v.pan_by(10.0, 5.0);
            v.pan_by(-3.0, 2.0);
            assert_eq!(v.offset(), (7.0, 7.0));
        });
    }

    #[test]
    fn pan_by_does_not_touch_scale() {
        with_view(|v| {
            v.set_view(2.5, 1.0, 2.0);
            v.pan_by(100.0, 100.0);
            assert_eq!(v.scale(), 2.5, "平移不得改动缩放");
        });
    }

    #[test]
    fn zoom_at_keeps_the_anchored_canvas_point_fixed() {
        with_view(|v| {
            v.set_view(1.0, 0.0, 0.0);
            let (cursor_x, cursor_y) = (400.0f32, 300.0f32);

            let before = v.screen_to_canvas(cursor_x, cursor_y);
            v.zoom_at(cursor_x, cursor_y, 1.5);
            let after = v.screen_to_canvas(cursor_x, cursor_y);

            assert!(
                (before.0 - after.0).abs() < 1e-3 && (before.1 - after.1).abs() < 1e-3,
                "锚点下的画布点应不动：{before:?} -> {after:?}"
            );
            assert!((v.scale() - 1.5).abs() < 1e-6);
        });
    }

    #[test]
    fn zoom_at_works_from_a_non_identity_transform() {
        with_view(|v| {
            // 带偏移与缩放时锚点仍须保持——否则"滚轮缩放会让画布漂移"。
            v.set_view(2.0, 130.0, -45.0);
            let (cursor_x, cursor_y) = (250.0f32, 480.0f32);
            let before = v.screen_to_canvas(cursor_x, cursor_y);
            v.zoom_at(cursor_x, cursor_y, 0.8);
            let after = v.screen_to_canvas(cursor_x, cursor_y);
            assert!(
                (before.0 - after.0).abs() < 1e-3 && (before.1 - after.1).abs() < 1e-3,
                "非恒等变换下锚点也应不动：{before:?} -> {after:?}"
            );
        });
    }

    #[test]
    fn zoom_at_clamps_to_the_gesture_scale_bounds() {
        with_view(|v| {
            v.set_view(1.0, 0.0, 0.0);
            for _ in 0..40 {
                v.zoom_at(0.0, 0.0, 1.5);
            }
            assert!(
                v.scale() <= Viewport::MAX_SCALE + 1e-6,
                "连续放大后应停在 MAX_SCALE，实得 {}",
                v.scale()
            );
            for _ in 0..80 {
                v.zoom_at(0.0, 0.0, 0.5);
            }
            assert!(
                v.scale() >= Viewport::MIN_SCALE - 1e-6,
                "连续缩小后应停在 MIN_SCALE，实得 {}",
                v.scale()
            );
        });
    }

    #[test]
    fn scale_bounds_live_in_one_place() {
        // 缩放钳位只有一处定义：应用层不得再写 0.1 / 10.0 字面量。
        assert_eq!(Viewport::MIN_SCALE, 0.1);
        assert_eq!(Viewport::MAX_SCALE, 10.0);
    }
}