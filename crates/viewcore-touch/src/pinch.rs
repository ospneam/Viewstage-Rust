//! 两指捏合识别（V2 增量语义移植 + 热路径精简）。
//!
//! V2 语义（与旧实现一致）：
//! - `scale` 为增量比（相对上一帧指距），非累积比；无缩放死区；
//! - `center` 恒为两指中点；
//! - 激活容差：指距变化 < [`PINCH_TOLERANCE`] 且中点位移 < [`PINCH_TOLERANCE`]
//!   时不产出增量；
//! - 批次门控：仅当两指均有过位移后才产出增量，防单指先动撕裂参考距离。
//!
//! 相对旧实现的热路径改进：
//! - 激活容差判定全部在**平方距离域**完成（激活前 0 次 `sqrt`）；
//! - 每个实际产出的增量仅 1 次 `sqrt`；
//! - 移除旧实现只写不读的死状态（`first_finger_time` / `finger0` / `finger1`），
//!   以及从未被置位的 `is_pending` 分支（V1 遗留死代码）。

use crate::pointer::PointerTable;

/// 捏合激活容差（屏幕 px，逐轴 Chebyshev 判据）。
pub const PINCH_TOLERANCE: f32 = 8.0;

const NO_ID: u32 = u32::MAX;

/// 增量捏合输出（`Copy`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PinchDelta {
    /// 相对上一帧的指距比（1.0 = 无缩放）。
    pub scale: f32,
    /// 两指中点（屏幕坐标）。
    pub center_x: f32,
    /// 两指中点（屏幕坐标）。
    pub center_y: f32,
}

#[derive(Clone)]
pub(crate) struct PinchState {
    is_pinching: bool,
    ids: [u32; 2],
    /// 增量参考距离（上一产出帧或激活帧的指距）。
    prev_distance: f32,
    /// 激活时刻指距（容差判定基准）。
    initial_distance: f32,
    start_mid: (f32, f32),
    tolerance_sq: f32,
    beyond_tolerance: bool,
    moved: [bool; 2],
}

impl PinchState {
    pub(crate) fn new() -> Self {
        Self {
            is_pinching: false,
            ids: [NO_ID; 2],
            prev_distance: 0.0,
            initial_distance: 0.0,
            start_mid: (0.0, 0.0),
            tolerance_sq: 0.0,
            beyond_tolerance: false,
            moved: [false; 2],
        }
    }

    #[inline]
    pub(crate) fn is_pinching(&self) -> bool {
        self.is_pinching
    }

    /// Down：活跃指针 ≥2 时开始捏合。返回激活锚点增量（scale=1.0，center=中点），
    /// 供消费方换算画布锚点。
    pub(crate) fn on_down(&mut self, table: &PointerTable) -> Option<PinchDelta> {
        if self.is_pinching || table.len() < 2 {
            return None;
        }
        let (Some((id0, x0, y0)), Some((id1, x1, y1))) = (table.nth(0), table.nth(1)) else {
            return None;
        };
        self.ids = [id0, id1];
        self.start_pinch((x0, y0), (x1, y1));
        Some(PinchDelta {
            scale: 1.0,
            center_x: self.start_mid.0,
            center_y: self.start_mid.1,
        })
    }

    fn start_pinch(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        let dx = p0.0 - p1.0;
        let dy = p0.1 - p1.1;
        self.initial_distance = (dx * dx + dy * dy).sqrt();
        self.prev_distance = self.initial_distance;
        self.start_mid = ((p0.0 + p1.0) * 0.5, (p0.1 + p1.1) * 0.5);
        self.tolerance_sq = PINCH_TOLERANCE * PINCH_TOLERANCE;
        self.beyond_tolerance = false;
        self.moved = [false; 2];
        self.is_pinching = true;
    }

    /// Move：返回本事件产出的增量（仅当激活 + 批次门控通过）。
    pub(crate) fn on_move(&mut self, table: &PointerTable, moved_id: u32) -> Option<PinchDelta> {
        if !self.is_pinching {
            return None;
        }
        if table.len() < 2 {
            self.finish();
            return None;
        }
        let (Some((x0, y0)), Some((x1, y1))) = (
            table.position_of(self.ids[0]),
            table.position_of(self.ids[1]),
        ) else {
            self.finish();
            return None;
        };

        let dist_sq = {
            let dx = x0 - x1;
            let dy = y0 - y1;
            dx * dx + dy * dy
        };
        let mid = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);

        if self.prev_distance == 0.0 {
            // 两指同点退化：无参考距离，与旧实现一致直接忽略
            return None;
        }

        let was_beyond = self.beyond_tolerance;
        if !was_beyond {
            // 平方域激活判定（0 sqrt）：
            //   |dist - initial| < TOL  ⟺  dist_sq ∈ (lower_sq, upper_sq)
            let init = self.initial_distance;
            let lower_sq = if init <= PINCH_TOLERANCE {
                0.0
            } else {
                (init - PINCH_TOLERANCE) * (init - PINCH_TOLERANCE)
            };
            let upper_sq = (init + PINCH_TOLERANCE) * (init + PINCH_TOLERANCE);
            let mdx = mid.0 - self.start_mid.0;
            let mdy = mid.1 - self.start_mid.1;
            let mid_move_sq = mdx * mdx + mdy * mdy;
            if dist_sq > lower_sq && dist_sq < upper_sq && mid_move_sq < self.tolerance_sq {
                return None;
            }
            self.beyond_tolerance = true;
            // 首次超容差：以当前指距为增量参考（先于批次门控，与旧实现一致）
            self.prev_distance = dist_sq.sqrt();
        }

        // 批次门控：两指均有过位移才产出增量
        if moved_id == self.ids[0] {
            self.moved[0] = true;
        } else if moved_id == self.ids[1] {
            self.moved[1] = true;
        }
        if !(self.moved[0] && self.moved[1]) {
            return None;
        }
        self.moved = [false; 2];

        // 激活帧复用容差判定时的 sqrt；后续帧仅此一次 sqrt
        let dist = if was_beyond {
            dist_sq.sqrt()
        } else {
            self.prev_distance
        };
        let scale = dist / self.prev_distance;
        self.prev_distance = dist;

        Some(PinchDelta {
            scale,
            center_x: mid.0,
            center_y: mid.1,
        })
    }

    /// Up：若是捏合指则结束；屏幕仍 ≥2 指时用剩余手指重新开始。返回是否 regrab。
    pub(crate) fn on_up(&mut self, table: &PointerTable, id: u32) -> bool {
        if !self.is_pinching || !self.ids.contains(&id) {
            return false;
        }
        self.finish();
        if table.len() >= 2 {
            let (Some((id0, x0, y0)), Some((id1, x1, y1))) = (table.nth(0), table.nth(1)) else {
                return false;
            };
            self.ids = [id0, id1];
            self.start_pinch((x0, y0), (x1, y1));
            return true;
        }
        false
    }

    fn finish(&mut self) {
        if !self.is_pinching {
            return;
        }
        self.is_pinching = false;
        self.ids = [NO_ID; 2];
        self.moved = [false; 2];
    }
}
