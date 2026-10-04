//! 指针事件词汇表与固定容量指针表。
//!
//! 设计约束（性能预算）：
//! - [`PointerEvent`] 为 16 字节 `Copy` 值，按值传递零开销；
//! - 指针表为固定 8 槽数组：无 `HashMap`、无堆分配、无哈希计算，缓存友好；
//!   单次事件处理为 O(MAX_POINTERS) 线性扫描（≤8 次比较，分支可预测）。

/// 指针来源（OS 层判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerSource {
    Touch,
    Pen,
    Mouse,
}

/// 指针相位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerPhase {
    Down,
    Move,
    Up,
}

/// 单个指针事件（`Copy`，按值传递零开销）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerEvent {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub phase: PointerPhase,
    pub source: PointerSource,
}

/// 同时跟踪的指针上限。触控设备实际 ≤10 点；手势消费 ≤2 点。
pub const MAX_POINTERS: usize = 8;

#[derive(Clone, Copy)]
struct Slot {
    id: u32,
    x: f32,
    y: f32,
}

/// 固定容量指针表：以 O(n)（n ≤ 8）数组扫描替代 `HashMap`，
/// 热路径零哈希、零分配、零 rehash。
#[derive(Clone)]
pub(crate) struct PointerTable {
    slots: [Option<Slot>; MAX_POINTERS],
    count: u8,
}

impl PointerTable {
    pub(crate) fn new() -> Self {
        Self {
            slots: [None; MAX_POINTERS],
            count: 0,
        }
    }

    /// 当前活跃指针数（维护的计数，O(1)）。
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.count as usize
    }

    /// Down：同 ID 原位覆盖（缺失 Up 的重复 Down），否则占用空槽；表满时忽略（防御兜底）。
    pub(crate) fn down(&mut self, id: u32, x: f32, y: f32) {
        // 先查同 ID（避免两遍扫描顺序导致同 ID 重复占槽）
        for slot in self.slots.iter_mut() {
            if let Some(s) = *slot {
                if s.id == id {
                    *slot = Some(Slot { id, x, y });
                    return;
                }
            }
        }
        for slot in self.slots.iter_mut() {
            if slot.is_none() {
                *slot = Some(Slot { id, x, y });
                self.count += 1;
                return;
            }
        }
    }

    /// Move：仅当指针已在表中才更新（未知 ID 的 Move 被忽略，与旧语义一致）。
    #[inline]
    pub(crate) fn move_pointer(&mut self, id: u32, x: f32, y: f32) -> bool {
        for slot in self.slots.iter_mut() {
            if let Some(s) = *slot {
                if s.id == id {
                    *slot = Some(Slot { id, x, y });
                    return true;
                }
            }
        }
        false
    }

    /// Up：移除指针；返回该 ID 是否存在（不存在则调用方跳过手势通知）。
    pub(crate) fn up(&mut self, id: u32) -> bool {
        for slot in self.slots.iter_mut() {
            if let Some(s) = *slot {
                if s.id == id {
                    *slot = None;
                    self.count -= 1;
                    return true;
                }
            }
        }
        false
    }

    /// 按 ID 查询位置。
    #[inline]
    pub(crate) fn position_of(&self, id: u32) -> Option<(f32, f32)> {
        for s in self.slots.iter().flatten() {
            if s.id == id {
                return Some((s.x, s.y));
            }
        }
        None
    }

    /// 第 n 个活跃指针（槽位序，确定性）。n 越界 → `None`。
    #[inline]
    pub(crate) fn nth(&self, n: usize) -> Option<(u32, f32, f32)> {
        self.slots
            .iter()
            .flatten()
            .nth(n)
            .map(|s| (s.id, s.x, s.y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_move_up_lifecycle() {
        let mut t = PointerTable::new();
        assert_eq!(t.len(), 0);
        t.down(1, 10.0, 20.0);
        t.down(2, 30.0, 40.0);
        assert_eq!(t.len(), 2);
        assert_eq!(t.position_of(1), Some((10.0, 20.0)));
        assert!(t.move_pointer(2, 31.0, 41.0));
        assert_eq!(t.position_of(2), Some((31.0, 41.0)));
        // 未知 ID 的 Move 被忽略
        assert!(!t.move_pointer(9, 0.0, 0.0));
        assert!(t.up(1));
        assert_eq!(t.len(), 1);
        // 重复 Up：已不存在
        assert!(!t.up(1));
    }

    #[test]
    fn duplicate_down_overwrites_in_place() {
        let mut t = PointerTable::new();
        t.down(1, 1.0, 1.0);
        t.down(2, 2.0, 2.0);
        // 缺 Up 的重复 Down：原位覆盖，不新增
        t.down(1, 5.0, 5.0);
        assert_eq!(t.len(), 2);
        assert_eq!(t.position_of(1), Some((5.0, 5.0)));
    }

    #[test]
    fn nth_is_slot_ordered_and_deterministic() {
        let mut t = PointerTable::new();
        t.down(7, 0.0, 0.0);
        t.down(3, 1.0, 1.0);
        assert_eq!(t.nth(0).map(|(id, ..)| id), Some(7));
        assert_eq!(t.nth(1).map(|(id, ..)| id), Some(3));
        assert!(t.nth(2).is_none());
    }
}
