//! 逐笔画几何缓存：画布坐标网格 + 代数失效。
//!
//! # 为什么缓存要带代数
//!
//! 笔画的网格（[`CachedMesh`]）在两种情况下会变：笔画完成时插入、橡皮擦完成
//! 或撤销后重算。渲染侧每帧要判断"缓存是否比上一帧新"，若靠遍历全部条目比较，
//! 就是 O(笔画数) 每帧；一个长文档写到第 500 笔时每帧多 500 次比较。代数把
//! 这个判断压成一次整数比较。
//!
//! # 关键不变量：任何变更都要 touch
//!
//! 代数只在 [`MeshCache::touch`] 里自增，而**每个** mutation 方法在返回前都调用
//! 它。这条约束此前是靠 8 个调用点各自记得调用 `touch_mesh_cache()` 来维持的
//! ——漏一处就是一个"画面不更新但没有任何报错"的 bug。收进类型之后，调用点
//! 无法忘记。
//!
//! # 几何与 bounds 必须同代产出
//!
//! [`CachedMesh`] 在构造时就把 AABB 一起算好存下，而不是让快照构建时现场调
//! `geometry.bounds()` 重扫全部段。旧实现让快照每次代数变化都整表重建，于是
//! 每加一笔要重扫**所有**笔画的全部段，整场书写累计 O(n²)。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use viewink::ink::eraser::EraserSet;
use viewink::ink::mesh::{build_eraser_index, tessellate_stroke_canvas, tessellate_stroke_indexed};
use viewink::ink::stroke::DrawStroke;

use crate::CachedMesh;

/// 逐笔画画布坐标几何缓存。
///
/// 全部方法仅 `&self`（`RefCell`/`Cell`）——渲染侧在 `&self` 热路径上读它。
pub struct MeshCache {
    entries: RefCell<HashMap<u64, CachedMesh>>,
    /// 缓存代数：任意 mutation 点递增，与 `live.static_generation` O(1) 比较。
    generation: Cell<u64>,
}

impl MeshCache {
    /// 空缓存（代数 0）。
    pub fn new() -> Self {
        Self {
            entries: RefCell::new(HashMap::new()),
            generation: Cell::new(0),
        }
    }

    /// 缓存代数，供渲染侧 O(1) 失效判断。
    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    /// 可渲染条目数（性能 HUD）。
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    /// 只读快照，供每帧静态快照同步（`FrameInputs::mesh_cache`）。
    pub fn snapshot(&self) -> std::cell::Ref<'_, HashMap<u64, CachedMesh>> {
        self.entries.borrow()
    }

    /// 笔画完成：用「未扣几何」兜底插入（下一帧即可渲染，避免闪一帧）。
    ///
    /// 后台线程返回更精确的扣除几何后再由 [`MeshCache::replace`] 替换。
    pub fn insert_uncut(&self, stroke: &DrawStroke, eraser_generation: u64) {
        if let Some(uncut) = tessellate_stroke_canvas(stroke, &[], None) {
            self.entries
                .borrow_mut()
                .insert(stroke.id, CachedMesh::new(uncut, eraser_generation));
            self.touch();
        }
    }

    /// 后台线程返回扣除几何：仅当它不比缓存里现有的更旧时才替换。
    ///
    /// 返回是否真的替换了。**替换成功时内部已 touch**，所以调用方不需要、也不
    /// 应该再自行递增代数——漏掉这一步就是"画面不更新且无任何报错"。
    pub fn replace(
        &self,
        id: u64,
        geometry: viewink::gpu::StrokeGeometry,
        eraser_generation: u64,
    ) -> bool {
        let mut cache = self.entries.borrow_mut();
        let stale = cache
            .get(&id)
            .is_some_and(|c| c.eraser_generation > eraser_generation);
        if stale {
            return false;
        }
        cache.insert(id, CachedMesh::new(geometry, eraser_generation));
        drop(cache);
        self.touch();
        true
    }

    /// 移除单条（撤销笔画）。
    ///
    /// 返回是否确实移除过——没移除就不 touch，避免无谓的快照重建。
    pub fn remove(&self, id: u64) -> bool {
        let removed = self.entries.borrow_mut().remove(&id).is_some();
        if removed {
            self.touch();
        }
        removed
    }

    /// 全量重建：笔画集已清空或切源后调用。
    ///
    /// 橡皮擦空间索引建一次、被全部笔画复用。旧实现每条笔画都从零重建一遍索引
    /// → O(笔画数 × 橡皮擦数 × 橡皮擦点数)，索引构建反客为主。
    pub fn rebuild(&self, strokes: &[DrawStroke], erasers: &EraserSet, eraser_generation: u64) {
        let index = build_eraser_index(erasers.as_slice(), None);
        let mut cache = self.entries.borrow_mut();
        cache.clear();
        for stroke in strokes {
            if let Some(mesh) = tessellate_stroke_indexed(stroke, &index) {
                cache.insert(stroke.id, CachedMesh::new(mesh, eraser_generation));
            }
        }
        drop(cache);
        self.touch();
    }

    /// 笔画集已清空时的缓存清理（无可渲染对象，无需任何重网格化）。
    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
        self.touch();
    }

    /// 橡皮擦被撤销后定向重建：仅 id 更小且与其 bounds 相交的笔画，
    /// 用恢复后的橡皮擦集重算缓存（替代全量重网格化）。
    pub fn retessellate_affected_by_eraser(
        &self,
        strokes: &[DrawStroke],
        erasers: &EraserSet,
        eraser_generation: u64,
        eraser: &DrawStroke,
    ) {
        let eb = eraser.bounds;
        let intersects = |s: &DrawStroke| {
            s.id < eraser.id
                && s.bounds
                    .intersects(eb.min_x, eb.min_y, eb.max_x - eb.min_x, eb.max_y - eb.min_y)
        };
        // 无相交笔画直接早退：省掉索引构建与缓存借用（旧实现先克隆整个橡皮擦集
        // 再判空，橡皮擦多时这笔克隆纯属浪费）。
        if !strokes.iter().any(intersects) {
            return;
        }
        // 橡皮擦集已恢复，索引建一次、被全部相交笔画复用。
        let index = build_eraser_index(erasers.as_slice(), None);
        // 单趟完成判定与重算：旧实现先收集 `affected: Vec<u64>` 再用 `contains`
        // 过滤，第二次遍历是 O(笔画数 × 相交数) 的线性查找。
        let mut cache = self.entries.borrow_mut();
        for stroke in strokes.iter().filter(|s| intersects(s)) {
            if let Some(mesh) = tessellate_stroke_indexed(stroke, &index) {
                cache.insert(stroke.id, CachedMesh::new(mesh, eraser_generation));
            }
        }
        drop(cache);
        self.touch();
    }

    /// 缓存内容变化后递增代数（`view()` 的 O(1) 失效判断依据）。
    fn touch(&self) {
        self.generation.set(self.generation.get() + 1);
    }
}

impl Default for MeshCache {
    fn default() -> Self {
        Self::new()
    }
}
