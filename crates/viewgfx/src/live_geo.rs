//! 进行中笔迹的增量几何编排（自主 crate main.rs 重写搬移）。
//!
//! 核心不变量：
//! - 笔：段 i 在 `points.len() >= i+3` 后形状不再改变（Catmull-Rom p3 已定），
//!   满批（[`LIVE_STROKE_BATCH`] 段）整批镶嵌并缓存常驻 GPU，尾部每帧重建；
//! - 橡皮擦：轨迹段几何按已处理点数差量追加；
//! - 静态快照：mesh cache 代数 O(1) 比较，变化时经**复用 scratch 缓冲**重建
//!   （原实现每次变化分配一个临时 `Vec`，本实现稳态零临时分配）。
//!
//! 全部函数仅 `&LiveCanvasState`（内部 Cell/RefCell），与 canvas 零消息
//! 热路径兼容；每点/每帧成本 O(1) 或 O(新增点数)，零分配。

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use viewink::gpu::StrokeSeg;
use viewink::ink::stroke::{DrawStroke, StrokePoint};
use viewink::live::{LiveCanvasState, StaticStrokeRef};

use super::CachedMesh;

/// 当前笔画稳定段分批大小：满一批才缓存一次（兼顾 GPU 块数与尾部重建成本）。
const LIVE_STROKE_BATCH: usize = 16;

/// 橡皮擦追加点节流间隔（毫秒）：橡皮擦是覆盖式擦除，无需笔级精度。
const ERASER_THROTTLE_MS: u64 = 16;

/// 笔迹采集抽稀阈值（**屏幕**像素，内部按缩放换算到画布坐标）。
///
/// 高回报率鼠标（可达 1000Hz）与高采样屏在慢速书写时会产生大量亚像素
/// 抖动点：它们不改变任何可见外观，却线性推高镶嵌、上传与绘制成本。
/// 低于该间距的输入点直接丢弃（不新增段、不推进笔锋状态）。
/// 0.35 屏幕像素在任意缩放下均不可辨，手感与外观保持不变。
const MIN_SAMPLE_DIST_SCREEN: f32 = 0.35;

/// 静态笔画快照同步：mesh cache 代数变化时重建 id 升序快照写入 live
/// （先画的在下，后画的覆盖；渲染侧对新旧快照做最长公共前缀比对，
/// 书写追加场景只增量上传新增可见段）。
///
/// AABB 取自缓存条目（几何产出时同代算出），**不重扫段表**——快照每次代数
/// 变化都整表重建，若在此现场 `geometry.bounds()`，每加一笔就要重扫所有
/// 笔画的全部段，整场书写累计 O(n²)。
///
/// `scratch`：调用方持有的复用缓冲（CanvasGfx 内），避免每次重建分配。
pub fn refresh_static_snapshot(
    live: &LiveCanvasState,
    cache: &HashMap<u64, CachedMesh>,
    generation: u64,
    scratch: &mut Vec<StaticStrokeRef>,
) {
    if live.static_generation.get() == generation {
        return;
    }
    scratch.clear();
    scratch.extend(cache.iter().map(|(id, c)| {
        StaticStrokeRef::with_bounds(*id, c.eraser_generation, c.geometry.clone(), c.bounds)
    }));
    scratch.sort_unstable_by_key(|r| r.id);
    *live.static_snapshot.borrow_mut() = Arc::from(scratch.as_slice());
    live.static_generation.set(generation);
}

/// 进行中笔画/橡皮擦的增量镶嵌：只有新增点与尾部段重算，每点成本 O(1)。
/// view 路径与 canvas 零消息热路径（每次追加点后）共用。
///
/// `scratch`：采样点复用缓冲（调用方持有，跨帧保容量）——尾部段每次追加
/// 都要重建，旧实现内部每次 `Vec::new()` 分配并多次 realloc。
pub fn refresh_live_geometry(live: &LiveCanvasState, scratch: &mut Vec<(f32, f32, f32)>) {
    // ---- 笔：增量镶嵌。满批缓存常驻 GPU，尾部每帧重建 ----
    let stroke_ref = live.current_stroke.borrow();
    if let Some(stroke) = stroke_ref.as_ref() {
        let current_pts = stroke.points.len();
        if current_pts != live.point_count.get() {
            let n = current_pts;
            let final_last = n as isize - 2; // 已定形段的最大 index，-1 表示无

            // 实时笔锋：增量维护逐点宽度，再对笔尾做速度衰减
            {
                let mut w = live.widths.borrow_mut();
                let prev_n = w.len();
                if prev_n > n {
                    w.truncate(n);
                }
                for k in w.len()..n {
                    w.push(viewink::ink::mesh::path_width(stroke, k));
                }
                if w.is_empty() && n > 0 {
                    w.push(viewink::ink::mesh::path_width(stroke, 0));
                }
                if let Some(ref times) = stroke.segment_times {
                    if times.len() >= w.len() && !w.is_empty() {
                        viewink::ink::tessellator::apply_live_tail_taper(
                            &mut w,
                            &stroke.points,
                            stroke.line_width,
                            times,
                            stroke.pen_min_width_ratio,
                            stroke.pen_tail_duration,
                        );
                    }
                }
            }

            // 路径增量维护：只追加尾部一项 + 同步新区间宽度
            {
                let mut path = live.path.borrow_mut();
                if path.len() > n + 1 {
                    path.truncate(n + 1);
                }
                if path.is_empty() && n > 0 {
                    path.push((
                        stroke.points[0].from_x,
                        stroke.points[0].from_y,
                        viewink::ink::mesh::path_width(stroke, 0),
                    ));
                }
                let prev_len = path.len();
                for k in prev_len..=n {
                    path.push((stroke.points[k - 1].to_x, stroke.points[k - 1].to_y, 0.0));
                }
                {
                    let w = live.widths.borrow();
                    let sync_start = prev_len.saturating_sub(1).min(n);
                    for k in sync_start..n.min(w.len()) {
                        path[k].2 = w[k];
                    }
                    if n < path.len() {
                        path[n].2 = stroke.line_width;
                    }
                }
                // 注意：`path[n]`（笔尖点）**不做**外推——它是样条末段的
                // p2 控制点，在这里加偏移会让末段切线偏转（直线被画歪，
                // 实测离轴可达 30px）。外推在镶嵌之后作用于末节胶囊的终点，
                // 见下方 `produced` 分支。
            }
            let path = live.path.borrow();

            // 满批的已定形段：整批一次镶嵌并缓存
            {
                let mut chunks = live.chunks.borrow_mut();
                let mut batch_start = live.batch_start.get();
                while (batch_start as isize) + (LIVE_STROKE_BATCH as isize) <= final_last + 1 {
                    if let Some(g) = viewink::ink::mesh::tessellate_live_range_into(
                        stroke,
                        &path,
                        batch_start,
                        batch_start + LIVE_STROKE_BATCH - 1,
                        scratch,
                    ) {
                        chunks.push(Arc::new(g));
                    }
                    batch_start += LIVE_STROKE_BATCH;
                }
                live.batch_start.set(batch_start);
            }

            // 尾部（未满批已定形段 + 笔尖段）：每次追加点重建，含笔尖端帽。
            //
            // **本函数是全局最热的 CPU 路径**（每个输入点执行一次，1000Hz
            // 鼠标 / 240Hz 触屏）。这里把稳态分配降到零：
            // 1. 先摘下上一帧的 head，若渲染侧已释放（无其他强引用）就用
            //    `Arc::try_unwrap` 收回它底层的 `Vec<StrokeSeg>`；
            // 2. 把收回的缓冲交给镶嵌函数复用，而非新建。
            //
            // 回收失败（渲染侧仍持有上一帧 head）不是错误：退回一次性新
            // 缓冲，行为与旧实现一致，仅少一次复用。
            let batch_start = live.batch_start.get();
            // 先在独立语句里摘下 head：`borrow_mut()` 的 RefMut 临时值活到
            // 所在语句结束；若直接写进 match 的判别式，Err 分支里再 borrow 会 panic。
            let previous_head = live.head.borrow_mut().take();
            let mut recycled: Vec<StrokeSeg> = match previous_head {
                Some(previous) => match Arc::try_unwrap(previous) {
                    Ok(geometry) => geometry.segs,
                    Err(previous) => {
                        // 渲染侧仍持有：放回去，不能破坏它的快照
                        *live.head.borrow_mut() = Some(previous);
                        Vec::new()
                    }
                },
                None => Vec::new(),
            };

            let produced = viewink::ink::mesh::tessellate_live_range_segs(
                stroke,
                &path,
                batch_start,
                n - 1,
                scratch,
                &mut recycled,
            );
            if produced {
                // ---- 笔尖外推：延长**末节胶囊**的终点 ----
                //
                // 必须作用在镶嵌**之后**的输出上，绝不能改 `path[n]`：
                // `path[n]` 是向心 Catmull-Rom 末段的 p2 控制点，改它等于
                // 改末段的**切线方向**——指针速度有噪声，切线就逐帧微转，
                // 直线会被画成歪的（且限帧把预测时域拉长后抖动加倍）。
                //
                // 末节胶囊的终点 = `curve.eval(1.0)` = `path[n]`（真实笔尖
                // 点），所以把它推到 `真实笔尖 + 速度×一帧`，得到"这一帧
                // 该看到的笔尖位置"，而样条形状与 `points` 完全不受影响。
                if live.frame_period_ms.get() > 0.0 {
                    let predict = live.tip_predict.get();
                    if predict[0] != 0.0 || predict[1] != 0.0 {
                        if let Some(last_seg) = recycled.last_mut() {
                            last_seg.b[0] += predict[0];
                            last_seg.b[1] += predict[1];
                        }
                    }
                }
                // 下一帧回收这一份（稳态下 refcount 恒为 1）
                *live.head.borrow_mut() =
                    Some(Arc::new(viewink::gpu::StrokeGeometry { segs: recycled }));
            }
            drop(path);
            live.head_index.set(live.head_index.get() + 1);
            live.point_count.set(current_pts);
        }
    }
    drop(stroke_ref);

    // ---- 橡皮擦轨迹增量缓存（红色半透明预览）----
    //
    // 分块策略：每次重建把「自上一块起的新点」镶嵌成一个新块追加进
    // `eraser_chunks`，而不是为整条轨迹重建一个几何。旧实现在每次追加点时
    // 构造临时 `DrawStroke`（含其 `points` / `variable_widths` 两个 Vec）
    // 并整条重镶嵌 → 每点 O(轨迹长度)，长轨迹橡皮擦会明显变慢。
    let es_ref = live.current_eraser_stroke.borrow();
    if let Some(es) = es_ref.as_ref() {
        let current_pts = es.points.len();
        let mut cached_pts = live.eraser_point_count.get();
        if current_pts < cached_pts {
            // 轨迹被截短（撤销/重入）：旧块描述的是已不存在的点，丢弃并
            // **同一次重建内**从头补齐（否则截短后的这一帧预览是空的）。
            live.eraser_chunks.borrow_mut().clear();
            cached_pts = 0;
        }
        if current_pts > cached_pts {
            let has_variable = !es.variable_widths.is_empty();
            let mut segs = Vec::new();
            // 段 i = **点 i 自身的 from→to 胶囊**（与整条镶嵌
            // `mesh::append_eraser_trail` 逐点同规则），i ∈ [0, n-1]。
            //
            // 旧实现按「点 i → 点 i+1」建模并只发射到 n-2，于是：① 最新点
            // 永不发射 → 预览笔尖**永远落后指针一个点**；② 为补边界重发
            // 点 cached-1 → 与上一块重复叠加（预乘 alpha 变深，出现深色
            // 接缝）。笔的点本就首尾相接，逐点胶囊即是连接段，无需桥接。
            for i in cached_pts..current_pts {
                emit_eraser_seg(&mut segs, &es.points[i], has_variable, es, i);
            }
            if !segs.is_empty() {
                live.eraser_chunks
                    .borrow_mut()
                    .push(Arc::new(viewink::gpu::StrokeGeometry { segs }));
            }
            live.eraser_point_count.set(current_pts);
        }
    }
}

/// 单个橡皮擦点 → 胶囊段（宽度不足则跳过；与 `append_eraser_trail` 同规则）。
#[inline]
fn emit_eraser_seg(
    out: &mut Vec<StrokeSeg>,
    pt: &StrokePoint,
    has_variable: bool,
    es: &DrawStroke,
    index: usize,
) {
    let w = if has_variable && index < es.variable_widths.len() {
        es.variable_widths[index]
    } else {
        es.line_width
    };
    if w < 0.001 {
        return;
    }
    // 画布坐标；退化段由胶囊 SDF 渲染为圆点
    out.push(StrokeSeg::new(
        pt.from_x,
        pt.from_y,
        pt.to_x,
        pt.to_y,
        w / 2.0,
        // 与 `mesh::append_eraser_trail` 共用同一常量：两条路径画在动态层
        // 同一位置，颜色不一致会在拼接处出现色差。
        viewink::ink::mesh::ERASER_TRAIL_COLOR,
    ));
}

/// 向当前笔快速追加一个画布坐标点，含笔锋速度→宽度计算（零消息热路径）。
///
/// 速度估计：3 槽环形缓冲平均（抑制抖动）→ 宽度平滑 → 起笔渐入。
pub fn append_pen_point(live: &LiveCanvasState, cx: f32, cy: f32) {
    let mut stroke_ref = live.current_stroke.borrow_mut();
    let stroke = match stroke_ref.as_mut() {
        Some(s) => s,
        None => return,
    };
    let (lx, ly) = stroke
        .points
        .last()
        .map(|p| (p.to_x, p.to_y))
        .unwrap_or((cx, cy));
    let dx = cx - lx;
    let dy = cy - ly;
    let dist2 = dx * dx + dy * dy;

    // 采集抽稀：亚像素抖动点直接丢弃（首点除外，否则笔画无法起笔）。
    // 阈值按缩放换算——保证在任意缩放下被丢弃的都是屏幕上不可辨的位移。
    if !stroke.points.is_empty() {
        let scale = live.transform.scale.get().max(1e-4);
        let min_dist = MIN_SAMPLE_DIST_SCREEN / scale;
        if dist2 < min_dist * min_dist {
            return;
        }
    }

    stroke.add_point(lx, ly, cx, cy);
    let now = Instant::now();
    let dt = live
        .pen_last_move_time
        .get()
        .map(|t| now.duration_since(t).as_secs_f32() * 1000.0)
        .unwrap_or(8.0)
        .max(1.0);
    let dist = dist2.sqrt();
    let raw_speed = dist / dt;
    // ---- 笔尖外推向量：指针速度 × 一个刷新周期 ----
    //
    // 补偿的是 vsync 的整帧量化：帧内容在下一个 vsync 才上屏，而系统光标由
    // 驱动用更新鲜的采样更新 → 笔尖稳定落后指针约一帧的位移。把这一帧的
    // 位移预先画出来，笔尖就"追上"了指针（真实点到达后立即取代它）。
    //
    // `frame_period_ms == 0`（未测得）时不外推，保守退化成原行为。
    // 限幅在**屏幕**空间做：缩得很小时画布坐标的距离会被放大，不限会窜出去。
    let period = live.frame_period_ms.get();
    if period > 0.0 {
        let scale = live.transform.scale.get().max(1e-4);
        let mut px = (dx / dt) * period;
        let mut py = (dy / dt) * period;
        let screen_len = (px * px + py * py).sqrt() * scale;
        const MAX_PREDICT_SCREEN_PX: f32 = 24.0;
        if screen_len > MAX_PREDICT_SCREEN_PX {
            let k = MAX_PREDICT_SCREEN_PX / screen_len;
            px *= k;
            py *= k;
        }
        live.tip_predict.set([px, py]);
    }
    let mut speed_buf = live.pen_speed_buffer.get();
    let write = live.pen_speed_write.get();
    speed_buf[write] = raw_speed;
    live.pen_speed_write.set((write + 1) % 3);
    if live.pen_speed_count.get() < 3 {
        live.pen_speed_count.set(live.pen_speed_count.get() + 1);
    }
    let count = live.pen_speed_count.get();
    let avg_speed: f32 = speed_buf[..count].iter().sum::<f32>() / count as f32;
    live.pen_speed_buffer.set(speed_buf);
    let last_width = stroke
        .variable_widths
        .last()
        .copied()
        .unwrap_or(stroke.line_width);
    let w = viewink::ink::tessellator::calc_pen_line_width(
        avg_speed,
        stroke.line_width,
        last_width,
        dist,
        stroke.pen_min_width_ratio,
    );
    let w = viewink::ink::tessellator::apply_start_taper(w, stroke.line_width, live.pen_total_segments.get());
    stroke.variable_widths.push(w);
    let elapsed = live
        .pen_stroke_start_time
        .get()
        .map(|t| now.duration_since(t).as_secs_f32() * 1000.0)
        .unwrap_or(0.0);
    if stroke.segment_times.is_none() {
        stroke.segment_times = Some(Vec::new());
    }
    if let Some(ref mut times) = stroke.segment_times {
        times.push(elapsed);
    }
    live.pen_total_segments.set(live.pen_total_segments.get() + 1);
    live.pen_last_move_time.set(Some(now));
}

/// 收笔前把**笔尖外推点固化为真实点**。
///
/// # 为什么必须固化
///
/// 书写时 [`append_pen_point`] 每帧把笔头段终点沿速度外推一个刷新周期
/// （`tip_predict`，屏幕上限 24px），用来抵消 vsync 的整帧量化——让墨迹当帧就
/// 追上指针。但收笔时 [`LiveCanvasState::reset_current_stroke`] 会把
/// `tip_predict` 清零，随后按**真实点**镶嵌最终几何。
///
/// 于是笔尾在抬笔那一帧朝真实笔尖位置**回缩最多 24 屏幕像素**——这就是
/// "有时候笔画会闪一下"。只在快速收笔时可见（外推量正比于速度），所以是
/// "有时候"而不是每次。
///
/// # 为什么固化进数据而不是只改渲染
///
/// 固化点之后，文档 / 网格缓存 / 后台线程重算 / undo 重建 / 存取盘**全部**
/// 看到同一个笔尾，无需任何一处再补偿。若只在渲染时拉伸笔尾，后台线程回来
/// 用未拉伸的笔画重算并替换缓存条目，那笔尾会在几百毫秒后再闪一次。
///
/// # 宽度取上一段的值
///
/// 实时的做法是"延长末节胶囊的终点、宽度不变"（见 `refresh_live_geometry`
/// 里对 `last_seg.b` 的位移），所以固化点沿用上一段宽度才是**逐像素等价**的
/// 做法，而不是按新距离重算。
///
/// 亚像素的预测点不固化：那会留下一段零长胶囊（退化几何）。
pub fn commit_tip_prediction(live: &LiveCanvasState) {
    let predict = live.tip_predict.get();
    if predict[0] == 0.0 && predict[1] == 0.0 {
        return;
    }
    let mut slot = live.current_stroke.borrow_mut();
    let Some(stroke) = slot.as_mut() else {
        return;
    };
    let Some(last) = stroke.points.last() else {
        return;
    };
    let (lx, ly) = (last.to_x, last.to_y);
    let (nx, ny) = (lx + predict[0], ly + predict[1]);

    let scale = live.transform.scale.get().max(1e-4);
    let screen_len = ((nx - lx).powi(2) + (ny - ly).powi(2)).sqrt() * scale;
    if screen_len < MIN_SAMPLE_DIST_SCREEN {
        // 复用采集抽稀的阈值：这一段在屏幕上不可辨，固化它只会留下退化胶囊。
        return;
    }

    let width = stroke
        .variable_widths
        .last()
        .copied()
        .unwrap_or(stroke.line_width);
    let period = live.frame_period_ms.get().max(1.0);
    stroke.add_point(lx, ly, nx, ny);
    stroke.variable_widths.push(width);
    if let Some(ref mut times) = stroke.segment_times {
        // 预测代表"再走一帧"，所以时间戳也推进一个刷新周期——这样
        // `apply_speed_taper` 算出的该段速度恰好等于原始瞬时速度。
        let t = times.last().copied().unwrap_or(period);
        times.push(t + period);
    }
    live.tip_predict.set([0.0, 0.0]);
}

/// 向当前橡皮擦追加一个画布坐标段（16ms 节流，零消息热路径）。
///
/// `eraser_size_screen`：屏幕坐标橡皮擦尺寸（配置原值），
/// 内部按当前缩放换算为画布宽度。`throttle`：节流时钟（CanvasGfx 持有）。
pub fn append_eraser_point(
    live: &LiveCanvasState,
    eraser_size_screen: f32,
    cx: f32,
    cy: f32,
    throttle: &Cell<Instant>,
) {
    let now = Instant::now();
    if now.duration_since(throttle.get()) < Duration::from_millis(ERASER_THROTTLE_MS) {
        return;
    }
    throttle.set(now);
    let mut es_ref = live.current_eraser_stroke.borrow_mut();
    if let Some(ref mut es) = es_ref.as_mut() {
        let base_width = eraser_size_screen / live.transform.scale.get();
        let (lx, ly) = es
            .points
            .last()
            .map(|p| (p.to_x, p.to_y))
            .unwrap_or((cx, cy));
        es.add_point(lx, ly, cx, cy);
        es.variable_widths.push(base_width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use viewink::gpu::StrokeGeometry;

    fn test_stroke(n_pts: usize) -> DrawStroke {
        let mut s = DrawStroke::new("#ff0000".to_string(), 5.0, 1, 0.4, 120.0);
        for i in 0..n_pts {
            let x = i as f32 * 10.0;
            s.add_point(x, 0.0, x, 0.0);
            s.variable_widths.push(5.0);
        }
        s
    }

    #[test]
    fn static_snapshot_sorted_and_gated() {
        let live = LiveCanvasState::new(1.0);
        let mut cache: HashMap<u64, CachedMesh> = HashMap::new();
        for id in [3u64, 1, 2] {
            cache.insert(id, CachedMesh::new(StrokeGeometry::default(), 0));
        }
        let mut scratch = Vec::new();
        refresh_static_snapshot(&live, &cache, 7, &mut scratch);
        assert_eq!(live.static_generation.get(), 7);
        let snap = live.static_snapshot.borrow().clone();
        assert_eq!(snap.len(), 3);
        assert!(
            snap.windows(2).all(|w| w[0].id < w[1].id),
            "快照必须 id 升序（渲染叠放次序）"
        );

        // 代数未变：早退，scratch 不被触碰
        scratch.clear();
        scratch.push(StaticStrokeRef::new(99, 0, Arc::new(StrokeGeometry::default())));
        refresh_static_snapshot(&live, &cache, 7, &mut scratch);
        assert_eq!(scratch.len(), 1, "代数未变应早退");
    }

    #[test]
    fn snapshot_carries_bounds_from_cache_not_rescanned() {
        // 快照里的 AABB 必须与缓存条目中预存的完全一致（渲染剔除的唯一依据）
        let live = LiveCanvasState::new(1.0);
        let mut cache: HashMap<u64, CachedMesh> = HashMap::new();
        let mut geom = StrokeGeometry::default();
        geom.push(StrokeSeg::new(10.0, 20.0, 30.0, 40.0, 2.0, [1.0, 1.0, 1.0, 1.0]));
        let expect = geom.bounds();
        cache.insert(1, CachedMesh::new(geom, 0));

        let mut scratch = Vec::new();
        refresh_static_snapshot(&live, &cache, 1, &mut scratch);
        let snap = live.static_snapshot.borrow().clone();
        assert_eq!(snap[0].bounds, expect, "快照 AABB 必须沿用缓存预存值");
        assert_eq!(snap[0].bounds, cache[&1].bounds);
    }

    #[test]
    fn live_geometry_incremental() {
        let live = LiveCanvasState::new(1.0);
        let mut scratch = Vec::new();
        *live.current_stroke.borrow_mut() = Some(test_stroke(2));
        refresh_live_geometry(&live, &mut scratch);
        assert_eq!(live.point_count.get(), 2);
        assert!(live.head.borrow().is_some(), "尾部几何（含端帽）必须生成");
        assert!(live.chunks.borrow().is_empty(), "2 点不足一批");

        // 追加新点 → 差量更新，无 panic
        {
            let mut s = live.current_stroke.borrow_mut();
            let st = s.as_mut().unwrap();
            st.add_point(20.0, 0.0, 30.0, 0.0);
            st.variable_widths.push(5.0);
        }
        refresh_live_geometry(&live, &mut scratch);
        assert_eq!(live.point_count.get(), 3);
        assert_eq!(live.path.borrow().len(), 4, "path[k]=points[k-1].to");
    }

    #[test]
    fn head_buffer_is_recycled_across_points() {
        // 尾部块每次重建都应**复用上一帧的底层缓冲**（`Arc::try_unwrap`
        // 收回 `Vec<StrokeSeg>` 继续用），而不是每帧 `Vec::new()`。
        //
        // 判据是"不同地址的数量"而非"地址恒定"：尾部段数在 1..16 之间
        // 涨落（满 16 段即升为常驻 chunk），缓冲扩容时地址**必然**变，
        // 所以恒定性不可断言；而容量按倍数增长 → 地址数是 O(log n)。
        // 每帧新建 Vec 的实现会给出 n 个不同地址。
        const FRAMES: usize = 24;
        let live = LiveCanvasState::new(1.0);
        *live.current_stroke.borrow_mut() = Some(test_stroke(2));
        let mut scratch = Vec::new();

        // 只取地址、不留引用：多留一个 `Arc` 克隆会让 refcount = 2，
        // 回收按设计失败（下一个测试专门覆盖那条路径），本测试就测不到复用了。
        let mut addresses: Vec<usize> = Vec::new();
        for frame in 0..FRAMES {
            {
                let mut s = live.current_stroke.borrow_mut();
                let st = s.as_mut().unwrap();
                let i = frame as f32;
                st.add_point((i + 3.0) * 10.0, 5.0, (i + 3.5) * 10.0, 5.0);
                st.variable_widths.push(5.0);
            }
            refresh_live_geometry(&live, &mut scratch);
            let head = live.head.borrow();
            let head = head.as_ref().expect("尾部块应始终存在");
            assert!(!head.segs.is_empty());
            let ptr = head.segs.as_ptr() as usize;
            if addresses.last() != Some(&ptr) {
                addresses.push(ptr);
            }
        }
        assert!(
            addresses.len() <= 8,
            "{} 帧只应出现少量扩容地址（容量倍数增长），实得 {} 个不同地址：\
             每帧新建 Vec 会得到 {FRAMES} 个",
            FRAMES,
            addresses.len()
        );
    }

    #[test]
    fn head_recycle_preserves_geometry_when_renderer_holds_reference() {
        // 渲染侧仍持有旧 head（refcount > 1）时回收必须失败：改用新缓冲，
        // 绝不能就地改写渲染侧正在读的那份。
        let live = LiveCanvasState::new(1.0);
        *live.current_stroke.borrow_mut() = Some(test_stroke(2));
        let mut scratch = Vec::new();
        refresh_live_geometry(&live, &mut scratch);

        let held: Arc<viewink::gpu::StrokeGeometry> = live.head.borrow().clone().expect("head");
        let held_segs = held.segs.clone();

        {
            let mut s = live.current_stroke.borrow_mut();
            let st = s.as_mut().unwrap();
            st.add_point(30.0, 0.0, 40.0, 0.0);
            st.variable_widths.push(5.0);
        }
        refresh_live_geometry(&live, &mut scratch);

        assert_eq!(held.segs, held_segs, "被渲染侧持有的旧快照不得被改写");
        // head 槽位换成了新几何（这正是要的），旧 Arc 只剩测试自己这一份引用
        assert_eq!(
            Arc::strong_count(&held),
            1,
            "回收失败时不应把同一 Arc 留在 head 槽里又交一份出去"
        );
        let new_head = live.head.borrow().clone().expect("新 head");
        assert!(
            !Arc::ptr_eq(&new_head, &held),
            "refcount>1 时必须退回新建，不能把同一 Arc 交出去两次"
        );
    }

    /// 回归：外推**绝不允许**让直线变歪。
    ///
    /// 曾经的缺陷：把预测量加到 `path[n]` 上，而它正是向心 Catmull-Rom 末段的
    /// p2 控制点 → 末段**切线方向**被改 → 指针速度噪声逐帧让末段微转 →
    /// 快速直线画出来是"歪歪扭扭"的。外推必须只延长末节胶囊的终点。
    ///
    /// 这里造一条**带亚像素噪声**的直线（真实输入总抖动），给一个大预测向量，
    /// 然后断言输出胶囊的所有端点仍然共线。
    #[test]
    fn tip_prediction_keeps_a_noisy_straight_line_straight() {
        /// 直线的单位方向
        const DIR: (f32, f32) = (0.6, 0.8);
        /// 造一条**完全沿 `DIR`** 的直线，带亚像素噪声。
        fn build(live: &LiveCanvasState, jitter: f32, n: usize) {
            let mut s = DrawStroke::new("#ff0000".to_string(), 5.0, 1, 0.4, 120.0);
            for i in 0..n {
                let d = i as f32 * 3.0;
                // 垂直于直线方向的亚像素抖动：模拟真实指针噪声
                let nx = -DIR.1 * jitter;
                let ny = DIR.0 * jitter;
                s.add_point(
                    DIR.0 * d + nx,
                    DIR.1 * d + ny,
                    DIR.0 * d + nx,
                    DIR.1 * d + ny,
                );
                s.variable_widths.push(5.0);
            }
            *live.current_stroke.borrow_mut() = Some(s);
            let mut scratch = Vec::new();
            refresh_live_geometry(live, &mut scratch);
        }

        // 无外推：基线必须是直的（先验证测试本身有效）
        let plain = LiveCanvasState::new(1.0);
        build(&plain, 0.0, 80);
        assert!(
            max_off_axis(&plain) < 1e-2,
            "基线（无噪声无外推）就不直，测试本身失效：{}",
            max_off_axis(&plain)
        );

        // 有噪声 + 有外推
        let live = LiveCanvasState::new(1.0);
        live.frame_period_ms.set(16.7);
        live.tip_predict.set([40.0, 0.0]);
        build(&live, 0.6, 80);

        // 只考察真实笔尖之前的胶囊：笔尖那一节**本来**就该被外推推出直线
        //（外推是沿指针方向的位移），它不属于"歪"。
        let off = max_off_axis_excluding_tip(&live);
        assert!(
            off < 0.35,
            "外推把直线画歪了：笔尖前各节最大离轴 {off}px（噪声仅 ±0.6px）"
        );
    }

    /// 所有胶囊端点到首尾连线的最大偏离（px）。
    fn max_off_axis(live: &LiveCanvasState) -> f32 {
        max_off_axis_excluding_tip(live)
    }

    /// 同上，但跳过最后一节（笔尖节，允许被外推推出直线）。
    fn max_off_axis_excluding_tip(live: &LiveCanvasState) -> f32 {
        let mut pts: Vec<(f32, f32)> = Vec::new();
        {
            let chunks = live.chunks.borrow();
            for chunk in chunks.iter() {
                for s in chunk.segs.iter() {
                    pts.push((s.a[0], s.a[1]));
                    pts.push((s.b[0], s.b[1]));
                }
            }
            if let Some(head) = live.head.borrow().as_ref() {
                let n = head.segs.len();
                for (i, s) in head.segs.iter().enumerate() {
                    if i + 1 == n {
                        // 笔尖节：只看起点，跳过被外推的终点
                        pts.push((s.a[0], s.a[1]));
                        continue;
                    }
                    pts.push((s.a[0], s.a[1]));
                    pts.push((s.b[0], s.b[1]));
                }
            }
        }
        if pts.len() < 2 {
            return 0.0;
        }
        let (x0, y0) = pts[0];
        let (x1, y1) = pts[pts.len() - 1];
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-6 {
            return 0.0;
        }
        // 噪声会让首尾连线本身略有偏，这里用**理论方向**做参考轴更严格
        let (ux, uy) = (0.6f32, 0.8f32);
        pts.iter()
            .map(|&(x, y)| ((x - x0) * -uy + (y - y0) * ux).abs())
            .fold(0.0f32, f32::max)
    }

    /// 笔尖外推：末节胶囊终点必须按预测向量前移，而 `points` 一个字节都不能变。
    ///
    /// 这两条性质合起来保证「外推只影响显示」：笔尖追上了指针，完成后的
    /// 笔迹却与未外推时逐段一致。
    #[test]
    fn tip_prediction_extends_the_head_but_never_the_stroke() {
        let live = LiveCanvasState::new(1.0);
        // 一个刷新周期（16.7ms）；scale=1 → 屏幕限幅不生效
        live.frame_period_ms.set(16.7);
        *live.current_stroke.borrow_mut() = Some(test_stroke(2));
        let mut scratch = Vec::new();
        refresh_live_geometry(&live, &mut scratch);

        // 无预测：尾段终点 == 最后一个真实点
        let baseline = head_tip(&live).expect("尾段");
        let real_tip = {
            let s = live.current_stroke.borrow();
            let p = s.as_ref().unwrap().points.last().unwrap();
            (p.to_x, p.to_y)
        };
        assert!(
            (baseline.0 - real_tip.0).abs() < 1e-3 && (baseline.1 - real_tip.1).abs() < 1e-3,
            "未开启外推时笔尖应落在真实点上：{baseline:?} vs {real_tip:?}"
        );

        // 追加一个点并给出预测向量
        {
            let mut s = live.current_stroke.borrow_mut();
            let st = s.as_mut().unwrap();
            st.add_point(20.0, 0.0, 30.0, 0.0);
            st.variable_widths.push(5.0);
        }
        live.tip_predict.set([12.0, 0.0]);
        refresh_live_geometry(&live, &mut scratch);

        let tip = head_tip(&live).expect("尾段");
        assert!(
            (tip.0 - (30.0 + 12.0)).abs() < 1e-3,
            "笔尖应按预测前移：实得 x={}，期望 {}",
            tip.0,
            30.0 + 12.0
        );
        // 关键：`points` 未被外推污染
        let s = live.current_stroke.borrow();
        let last = s.as_ref().unwrap().points.last().unwrap();
        assert_eq!(
            (last.to_x, last.to_y),
            (30.0, 0.0),
            "外推绝不能写进 stroke.points（否则完成后的笔迹会带回弹凸起）"
        );
    }

    /// 未测得刷新周期（`frame_period_ms == 0`）时不外推：保守退化成原行为。
    #[test]
    fn tip_prediction_is_off_until_the_frame_period_is_known() {
        let live = LiveCanvasState::new(1.0);
        assert_eq!(live.frame_period_ms.get(), 0.0, "默认不得外推");
        *live.current_stroke.borrow_mut() = Some(test_stroke(2));
        let mut scratch = Vec::new();
        refresh_live_geometry(&live, &mut scratch);
        // 手工塞一个预测向量：没有帧周期就不该被采纳
        live.tip_predict.set([9.0, 9.0]);
        {
            let mut s = live.current_stroke.borrow_mut();
            let st = s.as_mut().unwrap();
            st.add_point(20.0, 0.0, 30.0, 0.0);
            st.variable_widths.push(5.0);
        }
        refresh_live_geometry(&live, &mut scratch);
        let tip = head_tip(&live).expect("尾段");
        assert!(
            (tip.0 - 30.0).abs() < 1e-3,
            "未测得周期时不应外推，实得 x={}",
            tip.0
        );
    }

    /// 收笔固化外推点：最终笔尾必须等于抬笔前**屏幕上那一帧**的笔尾位置。
    ///
    /// 回归用例（用户报的"有时候笔画会闪一下"）：书写时笔头段终点被外推了
    /// `tip_predict`，而 `reset_current_stroke` 把 `tip_predict` 清零、按**真实
    /// 点**镶嵌最终几何——笔尾于是在抬笔那一帧朝真实笔尖回缩（上限 24 屏幕
    /// 像素）。只在快速收笔时可见，因为外推量正比于速度。
    #[test]
    fn commit_tip_prediction_keeps_the_final_tip_where_it_was_drawn() {
        let live = LiveCanvasState::new(1.0);
        live.frame_period_ms.set(16.7);
        // 必须用**非退化**笔画：`test_stroke` 的 `add_point(x,0,x,0)` 段长为 0，
        // 镶嵌出的末节没有意义，测不到外推。
        let mut stroke = DrawStroke::new("#ff0000".to_string(), 5.0, 1, 0.4, 120.0);
        let mut x = 0.0f32;
        for _ in 0..6 {
            stroke.add_point(x, 0.0, x + 10.0, 0.0);
            stroke.variable_widths.push(5.0);
            x += 10.0;
        }
        let true_tip = (60.0, 0.0);
        *live.current_stroke.borrow_mut() = Some(stroke);

        // 书写态：外推把笔尾从真实笔尖推出去
        live.tip_predict.set([7.0, -3.0]);
        let mut scratch = Vec::new();
        refresh_live_geometry(&live, &mut scratch);
        let drawn_tip = head_tip(&live).expect("书写态应有笔头段");

        let points_before = live.current_stroke.borrow().as_ref().unwrap().points.len();
        let widths_before = live.current_stroke.borrow().as_ref().unwrap().variable_widths.len();

        // 收笔：先固化，再清状态
        commit_tip_prediction(&live);
        let finalized = live.current_stroke.borrow_mut().take().unwrap();
        live.reset_current_stroke();

        let last = finalized.points.last().expect("固化后应有笔尾段");
        // 先确认测试前提成立：外推确实把书写态的笔尾推离了真实笔尖。
        assert!(
            (drawn_tip.0 - true_tip.0).abs() > 1.0,
            "前提不成立：书写态笔尾 ({}, {}) 应已被外推离开真实笔尖 {:?}",
            drawn_tip.0,
            drawn_tip.1,
            true_tip
        );
        assert!(
            (last.to_x - drawn_tip.0).abs() < 1e-3 && (last.to_y - drawn_tip.1).abs() < 1e-3,
            "最终笔尾 ({}, {}) 必须等于屏幕上那一帧的笔尾 ({}, {})",
            last.to_x,
            last.to_y,
            drawn_tip.0,
            drawn_tip.1
        );

        // points / variable_widths / segment_times 必须同步 +1：apply_speed_taper
        // 要求 widths.len() == points.len()，错位会静默跳过笔锋整段处理。
        assert_eq!(finalized.points.len(), points_before + 1);
        assert_eq!(
            finalized.variable_widths.len(),
            widths_before + 1,
            "宽度数组必须与点一一对应"
        );
        assert_eq!(
            finalized.variable_widths.len(),
            finalized.points.len(),
            "apply_speed_taper 要求宽度数 == 点数"
        );
        // 宽度沿用上一段：实时的做法是"延长末节胶囊终点、宽度不变"。
        let prev = finalized.variable_widths[finalized.variable_widths.len() - 2];
        let last_w = *finalized.variable_widths.last().unwrap();
        assert!(
            (last_w - prev).abs() < 1e-6,
            "固化点应沿用上一段宽度（{prev} → {last_w}）"
        );
        // 预测已消耗，不能残留给下一笔
        assert_eq!(live.tip_predict.get(), [0.0, 0.0]);
    }

    /// 亚像素预测不固化：那会留下一段零长胶囊（退化几何）。
    #[test]
    fn commit_skips_subpixel_prediction() {
        let live = LiveCanvasState::new(1.0);
        live.frame_period_ms.set(16.7);
        *live.current_stroke.borrow_mut() = Some(test_stroke(4));
        // 缩放 1.0 下小于 MIN_SAMPLE_DIST_SCREEN 的位移
        live.tip_predict.set([0.0001, 0.0]);
        let before = live.current_stroke.borrow().as_ref().unwrap().points.len();
        commit_tip_prediction(&live);
        let after = live.current_stroke.borrow().as_ref().unwrap().points.len();
        assert_eq!(after, before, "亚像素预测不应留下退化段");
    }

    /// 收笔必须清掉预测：否则下一笔第一帧会带上上一笔末端的位移。
    #[test]
    fn tip_prediction_is_cleared_with_the_stroke() {
        let live = LiveCanvasState::new(1.0);
        live.frame_period_ms.set(16.7);
        *live.current_stroke.borrow_mut() = Some(test_stroke(2));
        live.tip_predict.set([5.0, -3.0]);
        live.reset_current_stroke();
        assert_eq!(
            live.tip_predict.get(),
            [0.0, 0.0],
            "收笔后残留的预测会让下一笔笔尖凭空窜出去"
        );
    }

    /// 尾段几何的最后一个胶囊终点（外推作用点）。
    fn head_tip(live: &LiveCanvasState) -> Option<(f32, f32)> {
        let head = live.head.borrow();
        let head = head.as_ref()?;
        let last = head.segs.last()?;
        Some((last.b[0], last.b[1]))
    }

    #[test]
    fn eraser_trail_chunks_match_full_rebuild() {
        // 分块增量（eraser_chunks 拼接）必须与一次性整条镶嵌逐段一致。
        let live = LiveCanvasState::new(1.0);
        let mut es = DrawStroke::new_eraser(30.0, 2);
        es.add_point(0.0, 0.0, 10.0, 0.0);
        es.variable_widths.push(30.0);
        *live.current_eraser_stroke.borrow_mut() = Some(es);
        let mut scratch = Vec::new();

        // 逐点追加并重建（模拟节流后的追加）
        let mut reference = full_eraser_tessellation(&live);
        refresh_live_geometry(&live, &mut scratch);
        assert_eq!(collect_chunks(&live), reference, "首块应与整条一致");

        for i in 1..24 {
            {
                let mut s = live.current_eraser_stroke.borrow_mut();
                let st = s.as_mut().unwrap();
                st.add_point((i as f32 - 1.0) * 10.0, 0.0, i as f32 * 10.0, (i as f32) * 2.0);
                st.variable_widths.push(30.0);
            }
            refresh_live_geometry(&live, &mut scratch);
            reference = full_eraser_tessellation(&live);
        }

        let got = collect_chunks(&live);
        assert_eq!(
            got, reference,
            "分块拼接结果必须与一次性整条镶嵌逐段一致"
        );
    }

    #[test]
    fn eraser_trail_is_incremental_not_full_rebuild() {
        // 每次重建只应**新增一个块**，而非重建整条轨迹。
        //
        // 块数 = 已处理点数（段 i = 点 i 自身的胶囊，单点也成段——按下即见）：
        // 1 点 → 1 块；此后每追加一点 +1 块。注意"块数递增"正是增量语义的
        // 判据——若退化为整条重建，块数不会增长而总段数会暴涨。
        let live = LiveCanvasState::new(1.0);
        let mut es = DrawStroke::new_eraser(30.0, 2);
        es.add_point(0.0, 0.0, 10.0, 0.0);
        es.variable_widths.push(30.0);
        *live.current_eraser_stroke.borrow_mut() = Some(es);
        let mut scratch = Vec::new();
        refresh_live_geometry(&live, &mut scratch);
        assert_eq!(
            live.eraser_chunks.borrow().len(),
            1,
            "单点也应成段（笔尖胶囊），否则按下瞬间预览是空的"
        );

        for i in 1..20 {
            {
                let mut s = live.current_eraser_stroke.borrow_mut();
                let st = s.as_mut().unwrap();
                st.add_point((i as f32 - 1.0) * 10.0, 0.0, i as f32 * 10.0, 0.0);
                st.variable_widths.push(30.0);
            }
            refresh_live_geometry(&live, &mut scratch);
            assert_eq!(
                live.eraser_chunks.borrow().len(),
                i + 1,
                "追加第 {i} 点后应恰好有 {} 个块",
                i + 1
            );
        }
    }

    /// 预览笔尖必须覆盖**最新点**——落后一个点是"橡皮擦轨迹跟不上指针"。
    #[test]
    fn eraser_trail_tip_reaches_the_newest_point() {
        let live = LiveCanvasState::new(1.0);
        let mut es = DrawStroke::new_eraser(30.0, 2);
        es.add_point(0.0, 0.0, 10.0, 0.0);
        es.variable_widths.push(30.0);
        *live.current_eraser_stroke.borrow_mut() = Some(es);
        let mut scratch = Vec::new();

        for i in 1..8 {
            {
                let mut s = live.current_eraser_stroke.borrow_mut();
                let st = s.as_mut().unwrap();
                st.add_point((i as f32 - 1.0) * 10.0, 0.0, i as f32 * 10.0, 0.0);
                st.variable_widths.push(30.0);
            }
            refresh_live_geometry(&live, &mut scratch);
            let tip_x = i as f32 * 10.0;
            let reaches = collect_chunks(&live)
                .iter()
                .any(|seg| seg.b[0] >= tip_x && seg.a[0] >= (i as f32 - 1.0) * 10.0);
            assert!(
                reaches,
                "第 {i} 次追加后，预览必须画到最新位置 x={tip_x}（否则笔尖落后指针）"
            );
        }
    }

    #[test]
    fn eraser_trail_shrinking_resets_chunks() {
        // 轨迹被截短（撤销/重入）：块必须清空重建，不能留残段
        let live = LiveCanvasState::new(1.0);
        let mut es = DrawStroke::new_eraser(30.0, 2);
        for i in 0..10 {
            es.add_point(i as f32 * 10.0, 0.0, (i + 1) as f32 * 10.0, 0.0);
            es.variable_widths.push(30.0);
        }
        *live.current_eraser_stroke.borrow_mut() = Some(es);
        let mut scratch = Vec::new();
        refresh_live_geometry(&live, &mut scratch);
        assert!(!live.eraser_chunks.borrow().is_empty());

        {
            let mut s = live.current_eraser_stroke.borrow_mut();
            let st = s.as_mut().unwrap();
            st.points.truncate(3);
            st.variable_widths.truncate(3);
        }
        refresh_live_geometry(&live, &mut scratch);
        let got = collect_chunks(&live);
        let reference = full_eraser_tessellation(&live);
        assert_eq!(got, reference, "截短后应与截短轨迹的整条镶嵌一致");
    }

    #[test]
    fn eraser_trail_skips_zero_width_segments() {
        // 宽度为 0 的点必须跳过（与整条镶嵌同规则）
        let live = LiveCanvasState::new(1.0);
        let mut es = DrawStroke::new_eraser(30.0, 2);
        es.add_point(0.0, 0.0, 10.0, 0.0);
        es.variable_widths.push(0.0);
        es.add_point(10.0, 0.0, 20.0, 0.0);
        es.variable_widths.push(30.0);
        *live.current_eraser_stroke.borrow_mut() = Some(es);
        let mut scratch = Vec::new();
        refresh_live_geometry(&live, &mut scratch);

        assert_eq!(
            collect_chunks(&live).len(),
            1,
            "零宽段应被跳过，只剩 1 段"
        );
    }

    /// 拼接全部橡皮擦块（模拟渲染侧 pack_stable_segs 的读取方式）。
    fn collect_chunks(live: &LiveCanvasState) -> Vec<StrokeSeg> {
        let mut out = Vec::new();
        for chunk in live.eraser_chunks.borrow().iter() {
            out.extend_from_slice(&chunk.segs);
        }
        out
    }

    /// 整条一次性镶嵌的段表（分块增量的等效性基准）。
    fn full_eraser_tessellation(live: &LiveCanvasState) -> Vec<StrokeSeg> {
        let borrowed = live.current_eraser_stroke.borrow();
        let stroke = borrowed.as_ref().expect("进行中的橡皮擦笔画");
        viewink::ink::mesh::tessellate_eraser_trail(stroke)
            .expect("整条镶嵌")
            .segs
    }

    #[test]
    fn pen_point_appends_width() {
        let live = LiveCanvasState::new(1.0);
        *live.current_stroke.borrow_mut() = Some(test_stroke(1));
        live.pen_last_move_time.set(Some(Instant::now()));
        append_pen_point(&live, 5.0, 0.0);
        let s = live.current_stroke.borrow();
        let s = s.as_ref().unwrap();
        assert_eq!(s.points.len(), 2);
        assert_eq!(s.variable_widths.len(), 2, "每点追加一个宽度");
        assert_eq!(live.pen_speed_count.get(), 1);
    }

    #[test]
    fn pen_point_decimates_subpixel_jitter() {
        let live = LiveCanvasState::new(1.0);
        *live.current_stroke.borrow_mut() = Some(test_stroke(1));
        live.pen_last_move_time.set(Some(Instant::now()));

        // 亚像素抖动（0.1 屏幕像素）：不改变外观，必须被丢弃
        append_pen_point(&live, 0.1, 0.0);
        {
            let s = live.current_stroke.borrow();
            assert_eq!(
                s.as_ref().unwrap().points.len(),
                1,
                "亚像素抖动点必须被抽稀"
            );
        }

        // 超过阈值的位移正常追加（抽稀不得吃掉真实笔画）
        append_pen_point(&live, 5.0, 0.0);
        let s = live.current_stroke.borrow();
        assert_eq!(s.as_ref().unwrap().points.len(), 2);
    }

    #[test]
    fn pen_point_ignores_missing_stroke() {
        let live = LiveCanvasState::new(1.0);
        append_pen_point(&live, 5.0, 5.0); // 无进行中笔画 → 静默返回
        assert!(live.current_stroke.borrow().is_none());
    }

    #[test]
    fn eraser_point_throttled() {
        let live = LiveCanvasState::new(1.0);
        let throttle = Cell::new(Instant::now() - Duration::from_millis(100));
        *live.current_eraser_stroke.borrow_mut() = Some(DrawStroke::new_eraser(40.0, 2));

        append_eraser_point(&live, 40.0, 10.0, 10.0, &throttle);
        append_eraser_point(&live, 40.0, 11.0, 11.0, &throttle); // <16ms，被节流
        {
            let es = live.current_eraser_stroke.borrow();
            assert_eq!(
                es.as_ref().unwrap().points.len(),
                1,
                "new_eraser 起始 0 点，首次追加 +1；第二次追加应被节流"
            );
        }
        throttle.set(Instant::now() - Duration::from_millis(20));
        append_eraser_point(&live, 40.0, 12.0, 12.0, &throttle);
        let es = live.current_eraser_stroke.borrow();
        assert_eq!(es.as_ref().unwrap().points.len(), 2);
    }
}
