//! 撤销历史：仅支持 Draw/Erase 两类命令（清空走 history.clear() 不入栈）。
//!
//! [`HistoryManager::undo`] 就地改写权威列表（`strokes` / [`EraserSet`]），
//! 并返回被撤销命令的内容，供调用方做**定向**渲染缓存失效，避免全量重网格化。
//! 两者的分工：列表是真相、缓存是派生数据——撤销必须同时改写两者。
//!
//! `Erase` 命令入栈的「旧橡皮擦集」是 [`EraserSet::shared`] 的 `Arc` 克隆，
//! 零深拷贝——`EraserSet` 的写时复制保证这份快照此后永不被改动。

use std::sync::Arc;

use crate::ink::eraser::EraserSet;
use crate::ink::stroke::DrawStroke;

const MAX_HISTORY_STEPS: usize = 50;

#[derive(Debug, Clone)]
pub enum DrawCommand {
    Draw {
        stroke: DrawStroke,
    },
    Erase {
        eraser_stroke: DrawStroke,
        /// 追加本次橡皮擦**之前**的橡皮擦集（共享只读快照，非深拷贝）。
        saved_erasers: Arc<Vec<DrawStroke>>,
    },
}

#[derive(Clone)]
pub struct HistoryManager {
    undo_stack: Vec<DrawCommand>,
}

/// undo 弹出的命令内容（供调用方做定向缓存失效）。
#[derive(Debug, Clone)]
pub enum UndoneCommand {
    /// 撤销一笔绘制（返回被移除的笔画 id）
    Draw(u64),
    /// 撤销一次擦除（返回被撤销的橡皮擦；与其相交的笔画网格需重建）
    Erase(DrawStroke),
}

impl HistoryManager {
    pub fn new() -> Self {
        Self {
            undo_stack: Vec::new(),
        }
    }

    pub fn push(&mut self, cmd: DrawCommand) {
        self.undo_stack.push(cmd);

        // 上限裁剪：超过 2 倍上限时丢弃最旧的 1 倍
        let hard_limit = MAX_HISTORY_STEPS * 2;
        if self.undo_stack.len() > hard_limit {
            let excess = self.undo_stack.len() - MAX_HISTORY_STEPS;
            self.undo_stack.drain(0..excess);
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// 撤销栈顶命令。就地改写 `strokes` / [`EraserSet`]，返回被撤销命令的
    /// 内容，供调用方做**定向**缓存失效（避免全量重网格化）。
///
/// - `Draw`：从 `strokes` 中移除该笔画（`rposition` 命中最后同 id 项——
///   绘制撤销是后进先出，故最后一项即目标）。必须真的移除：仅从渲染缓存
///   里删掉是不够的——`strokes` 是权威列表，任何一次全量重建
///   （切换信号源恢复、清空后重算）或橡皮擦的 targeted rebuild 遍历都会
///   把它捞回来，撤销的笔迹会「复活」。
/// - `Erase`：恢复入栈时保存的旧橡皮擦集快照（`Arc` 快照，不深拷贝权威
///   列表；`EraserSet::replace` 内部会同步刷新共享快照）。
pub fn undo(
        &mut self,
        strokes: &mut Vec<DrawStroke>,
        erasers: &mut EraserSet,
    ) -> Option<UndoneCommand> {
        let cmd = self.undo_stack.pop()?;
        let undone = match cmd {
            DrawCommand::Draw { stroke } => {
                let id = stroke.id;
                if let Some(index) = strokes.iter().rposition(|s| s.id == id) {
                    strokes.remove(index);
                }
                UndoneCommand::Draw(id)
            }
            DrawCommand::Erase {
                eraser_stroke,
                saved_erasers,
            } => {
                // `replace` 内部同步刷新共享快照 —— 撤销后后台线程看到的
                // 必须是恢复后的集合，否则下一次绘制的扣除几何会用到过期集。
                let restored = match Arc::try_unwrap(saved_erasers) {
                    Ok(items) => items,
                    // 快照被多个 Erase 命令共享时不能就地取走，退回整体替换
                    // （仍然零深拷贝权威列表，只是多留一份旧快照）。
                    Err(shared) => (*shared).clone(),
                };
                erasers.replace(restored);
                UndoneCommand::Erase(eraser_stroke)
            }
        };
        Some(undone)
    }

    pub fn clear(&mut self) {
        self.undo_stack.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::stroke::StrokeBounds;

    fn stroke(id: u64) -> DrawStroke {
        let mut s = DrawStroke::new("#ff0000".into(), 5.0, id, 0.4, 30.0);
        s.add_point(0.0, 0.0, 10.0, 10.0);
        s.bounds = StrokeBounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 10.0,
            max_y: 10.0,
        };
        s
    }

    fn eraser(id: u64) -> DrawStroke {
        let mut s = DrawStroke::new_eraser(30.0, id);
        s.add_point(0.0, 0.0, 5.0, 5.0);
        s
    }

    #[test]
    fn undo_draw_removes_stroke_from_authoritative_list() {
        let mut h = HistoryManager::new();
        let mut strokes = vec![stroke(1), stroke(2)];
        h.push(DrawCommand::Draw {
            stroke: stroke(2),
        });

        let undone = h.undo(&mut strokes, &mut EraserSet::new()).expect("undo");
        assert!(matches!(undone, UndoneCommand::Draw(2)));
        assert_eq!(
            strokes.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1],
            "撤销绘制必须从 strokes 中移除，否则全量重建会让笔迹复活"
        );
    }

    #[test]
    fn undo_draw_removes_last_occurrence_not_first() {
        // id 重复（不应发生，但要求 rposition 语义稳定）时撤销最后一项，
        // 保证后进先出的绘制顺序不被破坏。
        let mut h = HistoryManager::new();
        let mut strokes = vec![stroke(7), stroke(8), stroke(7)];
        h.push(DrawCommand::Draw { stroke: stroke(7) });
        assert!(h.undo(&mut strokes, &mut EraserSet::new()).is_some());
        assert_eq!(
            strokes.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![7, 8]
        );
    }

    #[test]
    fn undo_draw_on_missing_id_is_noop_but_reports_id() {
        let mut h = HistoryManager::new();
        let mut strokes = vec![stroke(1)];
        h.push(DrawCommand::Draw { stroke: stroke(99) });
        let undone = h.undo(&mut strokes, &mut EraserSet::new()).expect("undo");
        assert!(matches!(undone, UndoneCommand::Draw(99)));
        assert_eq!(strokes.len(), 1, "id 不存在时不得误删其它笔画");
    }

    #[test]
    fn undo_erase_restores_previous_eraser_set() {
        let mut h = HistoryManager::new();
        let mut erasers = EraserSet::new();
        erasers.push(eraser(1));
        // 入栈快照走 shared()：零深拷贝，且此后不被后续 push 改动
        h.push(DrawCommand::Erase {
            eraser_stroke: eraser(2),
            saved_erasers: erasers.shared(),
        });
        erasers.push(eraser(2));

        let undone = h
            .undo(&mut Vec::new(), &mut erasers)
            .expect("undo");
        assert!(matches!(undone, UndoneCommand::Erase(ref e) if e.id == 2));
        assert_eq!(
            erasers.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1],
            "撤销擦除必须恢复旧橡皮擦集"
        );
    }

    #[test]
    fn undo_erase_refreshes_shared_snapshot() {
        // 撤销后后台线程看到的必须是恢复后的集合——否则下一笔绘制的扣除
        // 几何会用到过期集，已擦内容悄悄复活。
        let mut h = HistoryManager::new();
        let mut erasers = EraserSet::new();
        erasers.push(eraser(1));
        h.push(DrawCommand::Erase {
            eraser_stroke: eraser(2),
            saved_erasers: erasers.shared(),
        });
        erasers.push(eraser(2));
        assert_eq!(erasers.shared().len(), 2);

        h.undo(&mut Vec::new(), &mut erasers).expect("undo");

        let shared = erasers.shared();
        assert_eq!(shared.len(), 1, "共享快照未随撤销刷新");
        assert_eq!(shared[0].id, 1);
    }

    #[test]
    fn undo_lifo_order() {
        let mut h = HistoryManager::new();
        let mut strokes = vec![stroke(1), stroke(2), stroke(3)];
        h.push(DrawCommand::Draw { stroke: stroke(3) });
        h.push(DrawCommand::Draw { stroke: stroke(2) });

        assert!(matches!(
            h.undo(&mut strokes, &mut EraserSet::new()),
            Some(UndoneCommand::Draw(2))
        ));
        assert_eq!(strokes.iter().map(|s| s.id).collect::<Vec<_>>(), vec![1, 3]);
        assert!(matches!(
            h.undo(&mut strokes, &mut EraserSet::new()),
            Some(UndoneCommand::Draw(3))
        ));
        assert_eq!(strokes.iter().map(|s| s.id).collect::<Vec<_>>(), vec![1]);
        assert!(h.undo(&mut strokes, &mut EraserSet::new()).is_none());
    }

    #[test]
    fn history_stack_is_bounded() {
        let mut h = HistoryManager::new();
        let total = MAX_HISTORY_STEPS * 2 + 5;
        for id in 0..total as u64 {
            h.push(DrawCommand::Draw { stroke: stroke(id) });
        }
        // 裁剪语义：栈长超过硬上限（2×MAX）时一次性回落到 MAX，
        // 因此最终可撤销步数落在 (MAX, 硬上限] 区间内。
        let mut strokes: Vec<DrawStroke> = (0..total as u64).map(stroke).collect();
        let mut count = 0;
        while h.undo(&mut strokes, &mut EraserSet::new()).is_some() {
            count += 1;
        }
        assert!(
            count > MAX_HISTORY_STEPS && count <= MAX_HISTORY_STEPS * 2,
            "可撤销步数 {count} 应落在 ({MAX_HISTORY_STEPS}, {}] 内",
            MAX_HISTORY_STEPS * 2
        );
        assert_eq!(
            strokes.len(),
            total - count,
            "每次撤销绘制都应从权威列表移除一笔"
        );
    }

    #[test]
    fn clear_drops_stack() {
        let mut h = HistoryManager::new();
        h.push(DrawCommand::Draw { stroke: stroke(1) });
        h.clear();
        let mut strokes = vec![stroke(1)];
        assert!(h.undo(&mut strokes, &mut EraserSet::new()).is_none());
        assert_eq!(strokes.len(), 1);
        assert!(!h.can_undo());
    }
}
