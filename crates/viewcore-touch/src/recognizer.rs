//! 统一手势状态机：指针表 + 两指捏合，单入口 [`Recognizer::on_pointer`]。
//!
//! 每个 OS 指针事件恰好一次 O(MAX_POINTERS) 调用，产出 [`Gesture`]（`Copy`，
//! 零分配）。消费方无需再自行维护指针表或串联多个识别器。

use crate::pinch::{PinchDelta, PinchState};
use crate::pointer::{PointerEvent, PointerPhase, PointerTable};

/// 单个指针事件的识别输出（`Copy`）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Gesture {
    /// 捏合增量：Down 激活时为 1.0 锚点增量（消费方仅取 center）；
    /// Move 为增量比；Up 恒为 `None`。
    pub pinch: Option<PinchDelta>,
    /// 本次事件是否刚激活捏合（仅 Down 可能）。
    pub pinch_started: bool,
    /// 事件处理后的捏合状态。
    pub is_pinching: bool,
    /// 事件处理后的活跃指针数。
    pub pointers: u8,
}

/// 手势识别器：固定指针表 + 两指捏合（V2 增量语义）。
#[derive(Clone)]
pub struct Recognizer {
    table: PointerTable,
    pinch: PinchState,
}

impl Default for Recognizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Recognizer {
    pub fn new() -> Self {
        Self {
            table: PointerTable::new(),
            pinch: PinchState::new(),
        }
    }

    /// 当前是否处于捏合中。
    #[inline]
    pub fn is_pinching(&self) -> bool {
        self.pinch.is_pinching()
    }

    /// 当前活跃指针数。
    #[inline]
    pub fn pointer_count(&self) -> usize {
        self.table.len()
    }

    /// 单一入口：喂入一个指针事件，返回识别结果。
    ///
    /// - Down：登记指针；活跃 ≥2 时激活捏合并产出 1.0 锚点增量；
    /// - Move：仅更新已登记指针（未知 ID 忽略，与旧语义一致）；
    /// - Up：注销指针；捏合指抬起时结束，剩余 ≥2 指则无缝 regrab。
    #[inline]
    pub fn on_pointer(&mut self, ev: PointerEvent) -> Gesture {
        match ev.phase {
            PointerPhase::Down => {
                self.table.down(ev.id, ev.x, ev.y);
                let pinch = self.pinch.on_down(&self.table);
                Gesture {
                    pinch,
                    pinch_started: pinch.is_some(),
                    is_pinching: self.pinch.is_pinching(),
                    pointers: self.table.len() as u8,
                }
            }
            PointerPhase::Move => {
                let tracked = self.table.move_pointer(ev.id, ev.x, ev.y);
                let pinch = if tracked {
                    self.pinch.on_move(&self.table, ev.id)
                } else {
                    None
                };
                Gesture {
                    pinch,
                    pinch_started: false,
                    is_pinching: self.pinch.is_pinching(),
                    pointers: self.table.len() as u8,
                }
            }
            PointerPhase::Up => {
                let had = self.table.up(ev.id);
                if had {
                    self.pinch.on_up(&self.table, ev.id);
                }
                Gesture {
                    pinch: None,
                    pinch_started: false,
                    is_pinching: self.pinch.is_pinching(),
                    pointers: self.table.len() as u8,
                }
            }
        }
    }

    /// 全量复位（窗口失焦等异常恢复路径用）。
    pub fn reset(&mut self) {
        self.table = PointerTable::new();
        self.pinch = PinchState::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pointer::PointerSource;

    fn ev(id: u32, x: f32, y: f32, phase: PointerPhase) -> PointerEvent {
        PointerEvent {
            id,
            x,
            y,
            phase,
            source: PointerSource::Touch,
        }
    }

    #[test]
    fn single_finger_never_pinches() {
        let mut r = Recognizer::new();
        let g = r.on_pointer(ev(1, 100.0, 100.0, PointerPhase::Down));
        assert_eq!(g.pointers, 1);
        assert!(!g.pinch_started);
        assert!(g.pinch.is_none());
        let g = r.on_pointer(ev(1, 200.0, 100.0, PointerPhase::Move));
        assert!(g.pinch.is_none());
        assert!(!g.is_pinching);
    }

    #[test]
    fn pinch_starts_on_second_down_with_anchor() {
        let mut r = Recognizer::new();
        r.on_pointer(ev(1, 100.0, 100.0, PointerPhase::Down));
        let g = r.on_pointer(ev(2, 200.0, 100.0, PointerPhase::Down));
        assert!(g.pinch_started);
        assert!(g.is_pinching);
        assert_eq!(g.pointers, 2);
        let d = g.pinch.expect("激活时应携带 1.0 锚点增量");
        assert_eq!(d.scale, 1.0);
        assert_eq!(d.center_x, 150.0);
        assert_eq!(d.center_y, 100.0);
    }

    #[test]
    fn within_tolerance_no_delta() {
        let mut r = Recognizer::new();
        r.on_pointer(ev(1, 100.0, 100.0, PointerPhase::Down));
        r.on_pointer(ev(2, 200.0, 100.0, PointerPhase::Down));
        // 双指对称移动 2px（< 8px 容差，指距 100→96，中点不动）→ 不产出增量
        let g = r.on_pointer(ev(1, 102.0, 100.0, PointerPhase::Move));
        assert!(g.pinch.is_none(), "容差内不得产出增量");
        let g = r.on_pointer(ev(2, 198.0, 100.0, PointerPhase::Move));
        assert!(g.pinch.is_none(), "容差内不得产出增量");
        // 批次门控通过但仍在容差内
        assert!(g.is_pinching);
    }

    #[test]
    fn incremental_ratio_with_batch_gate() {
        let mut r = Recognizer::new();
        r.on_pointer(ev(1, 100.0, 100.0, PointerPhase::Down));
        r.on_pointer(ev(2, 200.0, 100.0, PointerPhase::Down));
        // 指 1 大幅移动（超出容差）但指 2 未动 → 批次门控拦截
        let g = r.on_pointer(ev(1, 80.0, 100.0, PointerPhase::Move));
        assert!(g.pinch.is_none(), "单指先动不得产出增量");
        // 指 2 再动 → 两指均已动 → 产出增量（参考距离为激活帧 120）
        let g = r.on_pointer(ev(2, 205.0, 100.0, PointerPhase::Move));
        let d = g.pinch.expect("两指均动后应产出增量");
        assert!((d.scale - 125.0 / 120.0).abs() < 1e-5, "实际 scale={}", d.scale);
        assert!((d.center_x - 142.5).abs() < 1e-5);
    }

    #[test]
    fn up_ends_pinch_and_regrabs_with_remaining() {
        let mut r = Recognizer::new();
        r.on_pointer(ev(1, 100.0, 100.0, PointerPhase::Down));
        r.on_pointer(ev(2, 200.0, 100.0, PointerPhase::Down));
        r.on_pointer(ev(3, 300.0, 300.0, PointerPhase::Down)); // 已捏合中，忽略
        assert!(r.is_pinching());
        // 捏合指 1 抬起 → 结束并用剩余 {2,3} 无缝 regrab
        let g = r.on_pointer(ev(1, 100.0, 100.0, PointerPhase::Up));
        assert_eq!(g.pointers, 2);
        assert!(g.is_pinching, "剩余 ≥2 指应 regrab");
        assert!(g.pinch.is_none(), "Up 事件不产出增量");
        // 再抬指 2 → 仅剩 1 指 → 捏合结束
        let g = r.on_pointer(ev(2, 200.0, 100.0, PointerPhase::Up));
        assert_eq!(g.pointers, 1);
        assert!(!g.is_pinching);
    }

    #[test]
    fn unknown_pointer_up_is_ignored() {
        let mut r = Recognizer::new();
        r.on_pointer(ev(1, 100.0, 100.0, PointerPhase::Down));
        r.on_pointer(ev(2, 200.0, 100.0, PointerPhase::Down));
        let g = r.on_pointer(ev(99, 0.0, 0.0, PointerPhase::Up));
        assert_eq!(g.pointers, 2);
        assert!(g.is_pinching, "未知 ID 的 Up 不得影响捏合");
    }

    #[test]
    fn pinch_fends_when_finger_disappears() {
        let mut r = Recognizer::new();
        r.on_pointer(ev(1, 100.0, 100.0, PointerPhase::Down));
        r.on_pointer(ev(2, 200.0, 100.0, PointerPhase::Down));
        // 指针表只剩 1 指时（系统吞掉 Up 的兜底路径）
        let mut r2 = r.clone();
        r2.table = crate::pointer::PointerTable::new();
        r2.table.down(1, 100.0, 100.0);
        let g = r2.on_pointer(ev(1, 100.0, 100.0, PointerPhase::Move));
        assert!(!g.is_pinching, "活跃指针不足应结束捏合");
    }
}
