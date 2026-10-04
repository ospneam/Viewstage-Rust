//! 橡皮擦集：权威列表 + 供后台线程共享的只读快照（写时复制）。
//!
//! # 为什么需要这个类型
//!
//! 每一笔绘制完成时，主线程要把「当前橡皮擦集」交给后台计算线程，让它算出
//! 扣除橡皮擦后的几何。旧实现是 `Arc::new(self.eraser_strokes.clone())`：
//!
//! - **`Arc` 一点忙都没帮上**。深拷贝在包进 `Arc` **之前**就发生了，所以每个
//!   任务都拿到一份独立的完整拷贝——代码注释里"多个任务共享同一份数据"的意图
//!   在 `finalize_stroke` 路径上从未成立（只有 `finalize_eraser` 内部的那批
//!   任务真的共享了）。
//! - 成本随**橡皮擦数**线性增长，而触发频率是**每写一笔**。`DrawStroke` 每次
//!   克隆要 4 次堆分配（`points` / `color` / `variable_widths` /
//!   `segment_times`）。用过 50 次橡皮擦、每条 500 点时，每晾干一笔就在主线程
//!   上多做 200 次分配 + 1 MB 拷贝。
//!
//! # 契约：`items` 私有，变更只走方法
//!
//! 修法可以是"在每个变更点记得刷新快照"，但那是个**靠记忆的纪律**：漏一处就
//! 让后台线程拿到过期橡皮擦集，算出本该被擦掉的笔迹——已擦内容悄悄复活，
//! 而且极难定位。所以这里让 `items` **私有**，所有变更只能经
//! [`EraserSet::push`] / [`EraserSet::replace`] / [`EraserSet::clear`]，
//! 每个方法内部同步刷新 `shared`。编译器兜住这个不变量。
//!
//! `push` 的实现是**整体替换** `items`（而非 `Vec::push`），这带来第二个好处：
//! [`EraserSet::shared`] 交出去的那份快照**永远不会被后续操作改动**，所以它可
//! 以直接用作撤销栈里的历史快照，零深拷贝。

use std::sync::Arc;

use crate::ink::stroke::DrawStroke;

/// 橡皮擦集。只经本类型的方法变更，见[模块文档](self)的契约说明。
#[derive(Debug, Clone, Default)]
pub struct EraserSet {
    /// 权威列表。**私有**：变更必须走下方方法，以强制刷新 `shared`。
    items: Vec<DrawStroke>,
    /// 交给后台计算线程的只读快照。
    shared: Arc<Vec<DrawStroke>>,
}

impl EraserSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// 权威列表（只读借用）。
    #[inline]
    pub fn as_slice(&self) -> &[DrawStroke] {
        &self.items
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 迭代（只读借用）。
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, DrawStroke> {
        self.items.iter()
    }

    /// 交给后台线程的只读快照。**只克隆引用计数**。
    ///
    /// 返回的 `Arc` 内容此后永不改变（所有变更都是整体替换 `items` 后另建
    /// `shared`），因此可安全地长期持有——撤销栈正是这么用的。
    #[inline]
    pub fn shared(&self) -> Arc<Vec<DrawStroke>> {
        Arc::clone(&self.shared)
    }

    /// 追加一条橡皮擦（整体替换 `items`，旧快照不受影响）。
    pub fn push(&mut self, eraser: DrawStroke) {
        let mut items = std::mem::take(&mut self.items);
        items.push(eraser);
        self.replace(items);
    }

    /// 整体替换列表（Clear / 撤销恢复 / 源状态恢复）。
    pub fn replace(&mut self, items: Vec<DrawStroke>) {
        self.shared = Arc::new(items.clone());
        self.items = items;
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.replace(Vec::new());
    }

    /// 取回权威列表的所有权（整体转移，非逐项克隆）。
    ///
    /// 供"把这个源的全部批注搬到别处"这类场景用（例如整源导出）。返回后
    /// 两半都清空，下一次 `shared()` 会给出空集——因此调用方**必须**自行
    /// `replace` 才能让后台线程看到新内容。
    #[inline]
    pub fn take(&mut self) -> Vec<DrawStroke> {
        let items = std::mem::take(&mut self.items);
        self.shared = Arc::new(Vec::new());
        items
    }
}

impl std::ops::Deref for EraserSet {
    type Target = [DrawStroke];

    #[inline]
    fn deref(&self) -> &[DrawStroke] {
        &self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eraser(id: u64, points: usize) -> DrawStroke {
        let mut s = DrawStroke::new("#000000".into(), 8.0, id, 0.0, 0.0);
        for k in 0..points {
            s.add_point(k as f32, 0.0, 0.0, 0.0);
        }
        s
    }

    #[test]
    fn push_refreshes_snapshot() {
        let mut set = EraserSet::new();
        set.push(eraser(1, 3));
        set.push(eraser(2, 3));

        assert_eq!(set.len(), 2);
        // 后台线程看到的必须是**当前**集，而不是漏刷新的旧集
        assert_eq!(set.shared().len(), 2);
        assert_eq!(set.shared()[1].id, 2);
    }

    #[test]
    fn snapshot_is_immutable_across_later_pushes() {
        let mut set = EraserSet::new();
        set.push(eraser(1, 3));
        let snapshot = set.shared();
        assert_eq!(snapshot.len(), 1);

        // 追加/清空都不得改动已交出去的快照（撤销栈依赖这个性质）
        set.push(eraser(2, 3));
        set.clear();

        assert_eq!(snapshot.len(), 1, "旧快照被后续操作改动了");
        assert_eq!(snapshot[0].id, 1);
    }

    #[test]
    fn clear_and_replace_refresh_snapshot() {
        let mut set = EraserSet::new();
        set.push(eraser(1, 3));
        set.replace(vec![eraser(7, 2), eraser(8, 2)]);
        assert_eq!(set.shared().len(), 2);
        assert_eq!(set.shared()[0].id, 7);

        set.clear();
        assert!(set.is_empty());
        assert!(set.shared().is_empty(), "clear 必须同步刷新快照");
    }

    #[test]
    fn shared_is_refcount_clone_not_deep_copy() {
        let mut set = EraserSet::new();
        set.push(eraser(1, 64));
        let a = set.shared();
        let b = set.shared();
        assert!(Arc::ptr_eq(&a, &b), "两次 shared() 必须是同一份数据");
        assert_eq!(Arc::strong_count(&a), 3); // set.shared + a + b
    }

    #[test]
    fn take_resets_both_halves() {
        let mut set = EraserSet::new();
        set.push(eraser(1, 3));
        let items = set.take();
        assert_eq!(items.len(), 1);
        assert!(set.is_empty());
        assert!(set.shared().is_empty());
    }

    #[test]
    fn deref_exposes_authoritative_list() {
        let mut set = EraserSet::new();
        set.push(eraser(5, 3));
        assert_eq!(set[0].id, 5);
        assert_eq!(set.iter().count(), 1);
        assert_eq!(set.as_slice().len(), 1);
    }
}
