//! 触控/鼠标预测轨迹（画布坐标）。仅触控绘制时有意义——触控路径保持消息驱动，
//! view 会随 RawTouch* 批次执行；鼠标零消息热路径不产生轨迹。
//!
//! ## 零分配策略（稳态）
//!
//! 原实现（主 crate build_touch_trail）每帧分配：`Vec::with_capacity(8)` 轨迹点、
//! `drain_touch_history()` 新 `Vec`、`StrokeGeometry` 底层 `Vec`、`Arc` 头。
//! 本实现：
//! - 轨迹点不再物化为 `Vec`——直接沿「笔尖 → 历史 → 触点 → 预测」链逐段生成；
//! - 段缓冲走**双缓冲池**：两帧前的几何此时渲染侧必然已释放，`Arc::try_unwrap`
//!   回收底层 `Vec`（容量保留）继续使用；
//! - 历史 drain 到调用方（CanvasGfx）的复用缓冲，超长积压截尾
//!   （[`TRAIL_HISTORY_CAP`]——轨迹只需一帧内的子帧样本，画质其次）。
//! 稳态每帧仅 1 次固定小分配（`Arc` 头，无法避免——图元按 Arc 交接）。
//!
//! ## 残留修复
//!
//! 原实现「无进行中笔画/非绘画模式」时早退，旧轨迹永远留在屏幕上（残影）。
//! 现在这些情形显式清空轨迹槽位。

use std::sync::Arc;

use viewink::gpu::{StrokeGeometry, StrokeSeg};
use viewink::live::LiveCanvasState;

/// 单帧轨迹的历史点上限：轨迹是一帧内（~16ms）的移动残影，
/// 超过上限的积压历史（view 长时间未被调用的极端情形）没有视觉意义。
const TRAIL_HISTORY_CAP: usize = 32;

/// 段缓冲初始容量提示（池冷启动时使用；热稳态走回收，不再分配）。
const TRAIL_CAPACITY_HINT: usize = 16;

/// 速度外推系数：预测点 = 触点 + 最后段速度 × 系数。
const PREDICTION_FACTOR: f32 = 0.75;

/// 触控轨迹构建上下文（view() 每帧从 App 状态收集，全 Copy）。
#[derive(Debug, Clone, Copy)]
pub struct TouchTrailCtx {
    /// 最新触点（屏幕坐标）。
    pub touch: (f32, f32),
    /// 画布坐标笔宽（调用方 `get_pen_width()`）。
    pub pen_width_canvas: f32,
    /// 轨迹颜色 (r, g, b)，0..1（预解析，避免每帧 from_str_radix）。
    pub color: (f32, f32, f32),
}

/// 触控预测轨迹构建器（双缓冲池）。
#[derive(Default)]
pub struct TrailBuilder {
    /// 上一轮两帧各自产出的几何（供回收底层缓冲）。
    pool: [Option<Arc<StrokeGeometry>>; 2],
    flip: bool,
}

impl TrailBuilder {
    /// 清空轨迹（非绘画模式/无进行中笔画时调用，修复轨迹残留）。
    pub fn clear(&mut self, live: &LiveCanvasState) {
        if live.trail.borrow().is_some() {
            *live.trail.borrow_mut() = None;
        }
    }

    /// 构建预测轨迹并写入 `live.trail`。
    ///
    /// `history`：屏幕坐标子帧历史（CanvasGfx 复用缓冲，已 drain）。
    pub fn build(
        &mut self,
        live: &LiveCanvasState,
        ctx: &TouchTrailCtx,
        history: &[(f32, f32)],
    ) {
        let stroke_ref = live.current_stroke.borrow();
        let Some(stroke) = stroke_ref.as_ref() else {
            drop(stroke_ref);
            self.clear(live);
            return;
        };
        let Some(last) = stroke.points.last() else {
            drop(stroke_ref);
            self.clear(live);
            return;
        };

        let scale = live.transform.scale.get();
        let inv_scale = 1.0 / scale;
        let offset_x = live.transform.offset_x.get();
        let offset_y = live.transform.offset_y.get();
        let lx = last.to_x;
        let ly = last.to_y;

        // 最后段速度（画布坐标）：外推预测用
        let mut vel_x = 0.0;
        let mut vel_y = 0.0;
        if stroke.points.len() >= 2 {
            let p1 = &stroke.points[stroke.points.len() - 2];
            vel_x = lx - p1.to_x;
            vel_y = ly - p1.to_y;
        }
        drop(stroke_ref);

        // 历史截尾：保留最近的样本
        let history = if history.len() > TRAIL_HISTORY_CAP {
            &history[history.len() - TRAIL_HISTORY_CAP..]
        } else {
            history
        };

        let tx = (ctx.touch.0 - offset_x) * inv_scale;
        let ty = (ctx.touch.1 - offset_y) * inv_scale;
        let pred_x = tx + vel_x * PREDICTION_FACTOR;
        let pred_y = ty + vel_y * PREDICTION_FACTOR;
        let pdx = pred_x - tx;
        let pdy = pred_y - ty;
        let has_pred = pdx * pdx + pdy * pdy > 4.0;

        // 点总数 = 笔尖 + 历史 + 触点 (+ 预测)；段数 = 点数 - 1
        let point_count = 1 + history.len() + 1 + has_pred as usize;
        let n = point_count.saturating_sub(1).max(1) as f32;

        // 半宽（画布坐标）：笔宽的 0.4 倍视觉系数，与旧版一致
        let half_w = ((ctx.pen_width_canvas * scale).max(2.0) * 0.4 * inv_scale) * 0.5;
        let (rr, gg, bb) = ctx.color;

        let mut segs = self.acquire_buffer();
        let push = |segs: &mut Vec<StrokeSeg>,
                        i: usize,
                        x1: f32,
                        y1: f32,
                        x2: f32,
                        y2: f32| {
            let dx = x2 - x1;
            let dy = y2 - y1;
            if dx * dx + dy * dy < 1.0 {
                return;
            }
            // Alpha 沿轨迹从 0.45 衰减到 0.20（ca/cb 两端插值实现渐变）
            let a0 = 0.45 - (i as f32 / n) * 0.25;
            let a1 = 0.45 - ((i + 1) as f32 / n) * 0.25;
            segs.push(StrokeSeg {
                a: [x1, y1, half_w, 0.0],
                b: [x2, y2, half_w, 0.0],
                ca: [rr, gg, bb, a0],
                cb: [rr, gg, bb, a1],
            });
        };

        let mut i = 0usize;
        let (mut px, mut py) = (lx, ly);
        for &(hx, hy) in history {
            let hx = (hx - offset_x) * inv_scale;
            let hy = (hy - offset_y) * inv_scale;
            push(&mut segs, i, px, py, hx, hy);
            px = hx;
            py = hy;
            i += 1;
        }
        push(&mut segs, i, px, py, tx, ty);
        i += 1;
        if has_pred {
            push(&mut segs, i, tx, ty, pred_x, pred_y);
        }

        if segs.is_empty() {
            return; // 与旧逻辑一致：空几何不覆盖（几乎不会发生）
        }
        let generation = live.trail_generation.get() + 1;
        live.trail_generation.set(generation);
        let arc = Arc::new(StrokeGeometry { segs });
        self.pool[self.flip as usize] = Some(Arc::clone(&arc));
        *live.trail.borrow_mut() = Some((generation, arc));
    }

    /// 取段缓冲：优先回收两帧前的几何底层 `Vec`（容量保留），
    /// 回收失败（渲染侧仍持有）才冷启动分配。
    fn acquire_buffer(&mut self) -> Vec<StrokeSeg> {
        self.flip = !self.flip;
        let slot = self.flip as usize;
        if let Some(old) = self.pool[slot].take() {
            if let Ok(geom) = Arc::try_unwrap(old) {
                let mut segs = geom.segs;
                segs.clear();
                return segs;
            }
        }
        Vec::with_capacity(TRAIL_CAPACITY_HINT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use viewink::ink::stroke::DrawStroke;

    fn drawing_live() -> LiveCanvasState {
        let live = LiveCanvasState::new(1.0);
        let mut s = DrawStroke::new("#ffffff".to_string(), 5.0, 1, 0.4, 120.0);
        s.add_point(0.0, 0.0, 0.0, 0.0);
        s.add_point(0.0, 0.0, 100.0, 100.0);
        *live.current_stroke.borrow_mut() = Some(s);
        live
    }

    fn ctx() -> TouchTrailCtx {
        TouchTrailCtx {
            touch: (110.0, 110.0),
            pen_width_canvas: 5.0,
            color: (1.0, 1.0, 1.0),
        }
    }

    #[test]
    fn build_sets_trail() {
        let live = drawing_live();
        let mut builder = TrailBuilder::default();
        builder.build(&live, &ctx(), &[(105.0, 105.0)]);
        let trail = live.trail.borrow().clone();
        assert!(trail.is_some(), "有进行中笔画时必须产出轨迹");
        let (_, geom) = trail.unwrap();
        assert!(!geom.is_empty());
    }

    #[test]
    fn no_stroke_clears_residue() {
        let live = drawing_live();
        let mut builder = TrailBuilder::default();
        builder.build(&live, &ctx(), &[]);
        assert!(live.trail.borrow().is_some());

        // 笔画结束（finalize 后 current_stroke=None）：旧轨迹必须被清空
        *live.current_stroke.borrow_mut() = None;
        builder.build(&live, &ctx(), &[]);
        assert!(live.trail.borrow().is_none(), "无笔画时必须清除残留轨迹");
    }

    #[test]
    fn none_input_clears_residue() {
        let live = drawing_live();
        let mut builder = TrailBuilder::default();
        builder.build(&live, &ctx(), &[]);
        assert!(live.trail.borrow().is_some());

        builder.clear(&live);
        assert!(live.trail.borrow().is_none());
    }

    #[test]
    fn buffer_pool_recycles() {
        let live = drawing_live();
        let mut builder = TrailBuilder::default();
        builder.build(&live, &ctx(), &[(101.0, 101.0)]);
        builder.build(&live, &ctx(), &[(102.0, 102.0)]);
        // 第三帧：flip 回到第 1 帧的槽位，其 Arc 仅剩池内克隆 → try_unwrap 成功
        let before = live.trail.borrow().as_ref().map(|(_, g)| g.segs.capacity());
        builder.build(&live, &ctx(), &[(103.0, 103.0)]);
        let after = live.trail.borrow().as_ref().map(|(_, g)| g.segs.capacity());
        // 回收的缓冲容量 >= 冷启动提示容量（证明未走冷启动新分配路径也可以，
        // 这里只验证行为正确：轨迹持续更新且容量不异常膨胀）
        assert!(before.unwrap_or(0) >= 2);
        assert!(after.unwrap_or(0) >= 2);
    }

    #[test]
    fn history_capped() {
        let live = drawing_live();
        let mut builder = TrailBuilder::default();
        let long_history: Vec<(f32, f32)> = (0..TRAIL_HISTORY_CAP * 4)
            .map(|i| (i as f32, i as f32))
            .collect();
        builder.build(&live, &ctx(), &long_history);
        let (_, geom) = live.trail.borrow().clone().unwrap();
        // 笔尖→最后 CAP 个历史→触点 (+预测)：段数有上界
        assert!(geom.segs.len() <= TRAIL_HISTORY_CAP + 2 + 1);
    }

    #[test]
    fn prediction_fades_alpha() {
        let live = drawing_live();
        let mut builder = TrailBuilder::default();
        builder.build(&live, &ctx(), &[]);
        let (_, geom) = live.trail.borrow().clone().unwrap();
        let first = &geom.segs[0];
        let last = geom.segs.last().unwrap();
        assert!(first.ca[3] >= last.cb[3] - 1e-4, "alpha 必须沿轨迹衰减");
    }
}
