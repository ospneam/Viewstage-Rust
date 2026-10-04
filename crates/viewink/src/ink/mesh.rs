use crate::ink::stroke::{DrawStroke, StrokeType};
use crate::gpu::{StrokeGeometry, StrokeSeg};

/// 每段最多细分为多少子段（越大擦除越精确，越慢）
const MAX_SUBDIVISIONS: u32 = 8;

/// 橡皮擦空间网格单元大小（画布坐标）。越小越精细，越大查询越快。
const ERASER_GRID_CELL: f32 = 48.0;

// ---- 自适应胶囊合并（静态几何发射）----
//
// 参照希沃 StrokeRenderer.FuzzyContains 的节点合并启发式（相邻节点包围盒
// 重叠率 ≥95% 即跳过轮廓绘制）：形状不变的采样点不产生独立胶囊实例。
// 采样折线被贪心合并为尽量长的胶囊段——直行段合并率可达 30:1，弯曲区
// 2~6:1。静态层每帧全量实例化重放是批注 GPU 占用的主导项，实例数与
// overdraw（胶囊圆头重复覆盖）随合并率同比例下降。视觉差异亚像素级。

/// 采样折线与合并弦的最大垂直偏差（画布坐标 px）。
const MERGE_DEV_TOL: f32 = 0.5;
/// 合并后宽度线性插值的最大误差（画布坐标 px）。
const MERGE_WIDTH_TOL: f32 = 0.3;
/// 单个合并 run 的最大采样点数（限制最坏 CPU 成本与超大实例 AABB）。
const MERGE_MAX_RUN: usize = 32;
/// 低于该全宽（px）不合并：极细笔尾的胶囊半径容不下合并偏差，保持密集。
const MERGE_MIN_WIDTH: f32 = 1.5;

/// 将单个笔画转换为画布坐标线段表（GPU 胶囊扩展输入）。
/// 变换与三角形拓扑由独立渲染管线的着色器完成（pan/zoom 零数据重传）。
/// eraser_strokes 用于扣除被覆盖的像素（橡皮擦 id > 笔画 id 时才生效）
pub fn tessellate_stroke_canvas(
    stroke: &DrawStroke,
    eraser_strokes: &[DrawStroke],
    current_eraser: Option<&DrawStroke>,
) -> Option<StrokeGeometry> {
    if stroke.points.is_empty() || stroke.stroke_type == StrokeType::Erase {
        return None;
    }

    let mut geometry = StrokeGeometry::default();

    if !eraser_strokes.is_empty() || current_eraser.is_some() {
        let index = EraserIndex::build(eraser_strokes, current_eraser);
        append_stroke(stroke, Some(&index), &mut geometry);
    } else {
        append_stroke(stroke, None, &mut geometry);
    }

    if geometry.is_empty() {
        return None;
    }

    Some(geometry)
}

/// 构建可复用的橡皮擦空间索引（供**批量**重算多条笔画）。
///
/// [`tessellate_stroke_canvas`] 每次调用都从零重建索引——单笔重算（书写
/// 晾干 / 单笔 targeted rebuild）没问题，但「重建全部笔画缓存」或「一次橡皮
/// 擦触发 N 条相交笔画重算」会退化成 O(笔画数 × 橡皮擦数 × 橡皮擦点数)
/// 的索引构建，索引构建成为主导开销。批量场景应：建一次索引 → 对每条笔画
/// 调 [`tessellate_stroke_indexed`]。橡皮擦集不变时索引结果完全确定，
/// 复用不改变任何输出。
pub fn build_eraser_index(
    eraser_strokes: &[DrawStroke],
    current_eraser: Option<&DrawStroke>,
) -> EraserIndex {
    EraserIndex::build(eraser_strokes, current_eraser)
}

/// [`tessellate_stroke_canvas`] 的「索引已备好」变体。
///
/// 空索引（[`build_eraser_index`] 传入空集的结果）等价于无橡皮擦，走
/// 不查表的快路径——与 `tessellate_stroke_canvas` 的空集分支一致。
pub fn tessellate_stroke_indexed(
    stroke: &DrawStroke,
    index: &EraserIndex,
) -> Option<StrokeGeometry> {
    if stroke.points.is_empty() || stroke.stroke_type == StrokeType::Erase {
        return None;
    }

    let mut geometry = StrokeGeometry::default();
    if index.is_empty() {
        append_stroke(stroke, None, &mut geometry);
    } else {
        append_stroke(stroke, Some(index), &mut geometry);
    }

    if geometry.is_empty() {
        return None;
    }

    Some(geometry)
}

/// 橡皮擦轨迹颜色（红色半透明）。宿主侧 `viewgfx::live_geo` 的**分块增量**
/// 路径必须引用同一常量——两条路径都画在动态层同一位置，颜色不一致会出现
/// 拼接处的色差。
pub const ERASER_TRAIL_COLOR: [f32; 4] = [1.0, 0.0, 0.0, 0.3];

/// 渲染橡皮擦轨迹（红色半透明胶囊段表，画布坐标），用于擦除实时预览
///
/// 一次性整条镶嵌。直播增量路径见 `viewgfx::live_geo`——它按新增点分块追加
/// （`eraser_chunks`），每点成本 O(新增点数) 而非 O(轨迹总长)。
pub fn tessellate_eraser_trail(es: &DrawStroke) -> Option<StrokeGeometry> {
    if es.points.is_empty() {
        return None;
    }

    let mut geometry = StrokeGeometry::default();
    append_eraser_trail(es, &mut geometry);

    if geometry.is_empty() {
        return None;
    }

    Some(geometry)
}

/// 笔画路径折点（画布坐标 + 逐点宽度）：P[0] = 首段起点，P[k] = 第 k-1 段终点。
/// 各段端点不会随后续追加的点改变，是增量镶嵌复用的基础。
/// 直播笔画用 App 侧增量维护（current_stroke_path），避免每帧整条重建。
pub fn build_path(stroke: &DrawStroke) -> Vec<(f32, f32, f32)> {
    let n = stroke.points.len();
    if n == 0 {
        return Vec::new();
    }

    let mut path = Vec::with_capacity(n + 1);
    path.push((
        stroke.points[0].from_x,
        stroke.points[0].from_y,
        path_width(stroke, 0),
    ));
    for k in 1..=n {
        path.push((
            stroke.points[k - 1].to_x,
            stroke.points[k - 1].to_y,
            path_width(stroke, k),
        ));
    }
    path
}

/// 第 k 个路径点的宽度（k=0 为首点起点宽度，k>=1 对应第 k-1 段终点宽度）。
pub fn path_width(stroke: &DrawStroke, i: usize) -> f32 {
    if let Some(ref sw) = stroke.stored_widths {
        if i < sw.len() {
            sw[i]
        } else {
            stroke.line_width
        }
    } else if !stroke.variable_widths.is_empty() {
        if i < stroke.variable_widths.len() {
            stroke.variable_widths[i]
        } else {
            stroke.line_width
        }
    } else {
        stroke.line_width
    }
}

/// 向心 Catmull-Rom 样条的**段级不变量**（alpha = 0.5，故节点间距取
/// `|ΔP|^0.5`——`hypot(...).sqrt()` 不是笔误，而是向心参数化的定义）。
///
/// 抽成独立结构的理由：`d0/d1/d2`、节点时间 `t1..t3`、五个 `crw` 跨度的
/// 退化判定**只由四个控制点决定，与段内参数 t 无关**。旧实现把它们放在
/// `catmull_rom` 内逐采样点重算——一段要采样 `k_sub`（最多 8）次，等于
/// 把同一组常量算 8 遍，其中 3 次是 `hypot`（溢出安全实现，比 `sqrt`
/// 慢一个数量级）。
///
/// 提出到段级后：一段 4 次 `hypot` + 4 次 `sqrt`（含调用方要的段长），
/// 采样循环内**不再有任何超越函数**。
///
/// 逐位等价：跨度仍是 `t1 - t0` 等同样两个 f32 相减，退化判据仍是
/// `span.abs() < 1e-6`，除法与插值顺序均未改动，只是求值次数减少。
struct CurveCtx {
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
    /// 弦长参数化的中间节点（`t0 = 0` 为隐式常量）。
    ///
    /// **只存 t1/t2**：Barry-Goldman 形式里 t3 仅以跨度形式进入
    /// （`span23 = t3 - t2`、`span13 = t3 - t1`，见 [`Self::new`]），而 t1/t2
    /// 还要作为 `crw_span` 的左锚 `ta` 直接参与求值，故需逐段保留。
    t1: f32,
    t2: f32,
    /// `|P1-P2|`（未开方），供调用方定细分密度
    seg_len: f32,
    span01: f32,
    span12: f32,
    span23: f32,
    span02: f32,
    span13: f32,
    /// 五个跨度的退化位掩码：bit0=01 bit1=12 bit2=23 bit3=02 bit4=13
    deg: u8,
}

impl CurveCtx {
    fn new(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)) -> Self {
        let d0 = (p0.0 - p1.0).hypot(p0.1 - p1.1).sqrt();
        let d1 = (p1.0 - p2.0).hypot(p1.1 - p2.1).sqrt();
        let d2 = (p2.0 - p3.0).hypot(p2.1 - p3.1).sqrt();
        let t0 = 0.0;
        let t1 = t0 + d0;
        let t2 = t1 + d1;
        let t3 = t2 + d2;
        // 与旧 `crw` 内部完全相同的减法与判据，只是提到段级求值一次
        let span01 = t1 - t0;
        let span12 = t2 - t1;
        let span23 = t3 - t2;
        let span02 = t2 - t0;
        let span13 = t3 - t1;
        let mut deg = 0u8;
        if span01.abs() < 1e-6 {
            deg |= 1;
        }
        if span12.abs() < 1e-6 {
            deg |= 2;
        }
        if span23.abs() < 1e-6 {
            deg |= 4;
        }
        if span02.abs() < 1e-6 {
            deg |= 8;
        }
        if span13.abs() < 1e-6 {
            deg |= 16;
        }
        Self {
            p0,
            p1,
            p2,
            p3,
            t1,
            t2,
            seg_len: (p1.0 - p2.0).hypot(p1.1 - p2.1),
            span01,
            span12,
            span23,
            span02,
            span13,
            deg,
        }
    }

    /// 段内参数 t∈[0,1]（P1→P2 之间）处的曲线点。插值穿过每个控制点
    /// （零位置延迟），相邻点重合时相应子段退化为单点。
    #[inline]
    fn eval(&self, t: f32) -> (f32, f32) {
        let tt = self.t1 + (self.t2 - self.t1) * t;
        let d = self.deg;

        let (w00, w01) = crw_span(self.span01, d & 1 != 0, 0.0, tt);
        let a1 = lerp2(self.p0, self.p1, w00, w01);
        let (w10, w11) = crw_span(self.span12, d & 2 != 0, self.t1, tt);
        let a2 = lerp2(self.p1, self.p2, w10, w11);
        let (w20, w21) = crw_span(self.span23, d & 4 != 0, self.t2, tt);
        let a3 = lerp2(self.p2, self.p3, w20, w21);
        let (b10, b11) = crw_span(self.span02, d & 8 != 0, 0.0, tt);
        let b1 = lerp2(a1, a2, b10, b11);
        let (b20, b21) = crw_span(self.span13, d & 16 != 0, self.t1, tt);
        let b2 = lerp2(a2, a3, b20, b21);
        let (c0, c1) = crw_span(self.span12, d & 2 != 0, self.t1, tt);
        lerp2(b1, b2, c0, c1)
    }

    /// 该段的细分密度：亚 6px 短段不再强制拆 2 个采样（静态侧随后有自适应
    /// 合并兜底，直播侧直接减实例）；上限 8 保拐角精度。
    #[inline]
    fn subdivisions(&self) -> usize {
        if self.seg_len < 0.001 {
            1
        } else {
            ((self.seg_len / 6.0).ceil() as usize).clamp(1, MAX_SUBDIVISIONS as usize)
        }
    }
}

/// 线性插值：a*w_a + b*w_b
fn lerp2(a: (f32, f32), b: (f32, f32), w_a: f32, w_b: f32) -> (f32, f32) {
    (a.0 * w_a + b.0 * w_b, a.1 * w_a + b.1 * w_b)
}

/// 跨度 `span`（= tb - ta）的线段在 tt 处的权重对 (左, 右)，权重和恒为 1。
/// 端点重合（span ≈ 0）时全权给右端点（端点相等，结果不变），保证退化输入稳定。
///
/// `span` 与 `degenerate` 由调用方给出——见 [`CurveCtx`]，两者都是段级常量。
#[inline]
fn crw_span(span: f32, degenerate: bool, ta: f32, tt: f32) -> (f32, f32) {
    if degenerate {
        return (0.0, 1.0);
    }
    let w_b = (tt - ta) / span;
    (1.0 - w_b, w_b)
}

/// 将笔画路径插值成平滑折线（画布坐标 + 逐点宽度）。整条重建用（完成笔画/擦除预览）。
fn build_smoothed_points(stroke: &DrawStroke) -> Vec<(f32, f32, f32)> {
    let path = build_path(stroke);
    let n = stroke.points.len();
    if n == 0 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(n * 4 + 1);
    out.push((path[0].0, path[0].1, path[0].2));
    for i in 0..n {
        let p0 = path[i.saturating_sub(1)];
        let p1 = path[i];
        let p2 = path[i + 1];
        let p3 = path[(i + 2).min(n)];
        let curve = CurveCtx::new((p0.0, p0.1), (p1.0, p1.1), (p2.0, p2.1), (p3.0, p3.1));
        let k_sub = curve.subdivisions();
        let w0 = p1.2;
        let w1 = p2.2;
        for s in 1..=k_sub {
            let t = s as f32 / k_sub as f32;
            let (x, y) = curve.eval(t);
            let w = w0 * (1.0 - t) + w1 * t;
            out.push((x, y, w));
        }
    }
    out
}

/// 为进行中笔画增量转换第 [first, last] 段为线段表（含端点采样）。
///
/// 关键性质：向心 Catmull-Rom 中，只有最后一个段的形状会随新点追加而改变
/// （其 p3 控制点 = 当前笔尖）。因此已完成的段可一次性转换并缓存，
/// 仅「尾部」段（≤ 一批）每次追加重建 → 每点成本 O(1) 而非 O(全长)。
/// 段表由 GPU 着色器扩展为胶囊带——圆头关节/端帽/抗锯齿在片元阶段解析生成。
/// `path` 由调用方增量维护（build_path），避免每帧整条重建。
pub fn tessellate_live_range(
    stroke: &DrawStroke,
    path: &[(f32, f32, f32)],
    first: usize,
    last: usize,
) -> Option<StrokeGeometry> {
    tessellate_live_range_into(stroke, path, first, last, &mut Vec::new())
}

/// 生成段区间 `[first, last]` 的平滑采样点（`(x, y, 全宽)`）到 `pts`。
///
/// 满批块与尾部块共用的采样阶段：段级不变量（节点时间、跨度、细分密度）
/// 在 [`CurveCtx::new`] 里算一次，段内 `k_sub` 次采样只做纯多项式求值。
///
/// 返回 `false` 表示区间非法（空笔画 / `first > last` / `last >= 点数` /
/// `path` 未同步），此时**不触碰** `pts`。
fn live_range_samples(
    stroke: &DrawStroke,
    path: &[(f32, f32, f32)],
    first: usize,
    last: usize,
    pts: &mut Vec<(f32, f32, f32)>,
) -> bool {
    let n = stroke.points.len();
    if n == 0 || first > last || last >= n || path.len() < n + 1 {
        return false;
    }

    pts.clear();
    pts.push((path[first].0, path[first].1, path[first].2));
    for i in first..=last {
        let p0 = path[i.saturating_sub(1)];
        let p1 = path[i];
        let p2 = path[i + 1];
        let p3 = path[(i + 2).min(n)];
        let curve = CurveCtx::new((p0.0, p0.1), (p1.0, p1.1), (p2.0, p2.1), (p3.0, p3.1));
        let k_sub = curve.subdivisions();
        let w0 = p1.2;
        let w1 = p2.2;
        for s in 1..=k_sub {
            let t = s as f32 / k_sub as f32;
            let (x, y) = curve.eval(t);
            let w = w0 * (1.0 - t) + w1 * t;
            pts.push((x, y, w));
        }
    }
    true
}

/// 直播**满批块**（`chunks`）镶嵌：自适应合并发射。
///
/// 与 [`tessellate_live_range_segs`]（尾部 `head`）的差别只在发射策略：
/// 满批块一经镶嵌即定形并长期驻留动态层，因此值得付出合并代价把实例数
/// 降一个量级；尾部块每追加一点就整体重建，保持逐采样发射（正在书写的
/// 笔尖保持精确样条几何，不引入新的抬笔重排——抬笔时"未合并 → 已合并"
/// 的重排在改动前就已存在）。
///
/// 合并与晾干路径共用 [`emit_merged`]，因此书写中的满批部分与晾干后的
/// 静态部分胶囊切分完全一致。
pub fn tessellate_live_range_into(
    stroke: &DrawStroke,
    path: &[(f32, f32, f32)],
    first: usize,
    last: usize,
    scratch: &mut Vec<(f32, f32, f32)>,
) -> Option<StrokeGeometry> {
    if !live_range_samples(stroke, path, first, last, scratch) {
        return None;
    }
    let mut segs = Vec::new();
    emit_merged(scratch, stroke.rgba(), &mut segs);
    if segs.is_empty() {
        None
    } else {
        Some(StrokeGeometry { segs })
    }
}

/// 直播**尾部块**（`head`）镶嵌：逐采样发射，段缓冲复用。
///
/// 用于**每追加一个输入点就要重建**的尾部块：`head` 每帧被整体替换一次，
/// 旧实现每次都新建 `Vec<StrokeSeg>`（外加一个 `Arc` 头）——即每个输入点
/// 2 次堆分配。240Hz 触控屏上 ~480 次/秒，而文档声称"稳态零分配"，实际并非如此。
/// 调用方回收上一帧 `head` 的底层缓冲后传入，稳态才真正零分配。
///
/// 注意：仅适用于会被**整体替换**的短命缓冲。追加式累积缓冲（`chunks` 里的
/// 满批块）必须每块独立分配——那是要随笔画长期持有的数据。
pub fn tessellate_live_range_segs(
    stroke: &DrawStroke,
    path: &[(f32, f32, f32)],
    first: usize,
    last: usize,
    scratch: &mut Vec<(f32, f32, f32)>,
    out: &mut Vec<StrokeSeg>,
) -> bool {
    if !live_range_samples(stroke, path, first, last, scratch) {
        return false;
    }
    let pts = scratch;
    let color = stroke.rgba();
    out.clear();
    // 仅在容量不足时扩容：recycle 路径下稳态一次都不发生
    out.reserve(pts.len().saturating_sub(1));

    if pts.len() == 1 {
        // 单点：退化段 → GPU 胶囊退化为圆（点画）
        let (x, y, w) = pts[0];
        out.push(StrokeSeg::new(x, y, x, y, w / 2.0, color));
        return true;
    }

    for i in 0..pts.len() - 1 {
        let (fx, fy, w0) = pts[i];
        let (sx, sy, w1) = pts[i + 1];
        let half_w0 = w0 / 2.0;
        let half_w1 = w1 / 2.0;
        if half_w0 < 0.001 && half_w1 < 0.001 {
            continue;
        }
        // 两端半宽渐变段；圆头关节/端帽由胶囊 SDF 天然覆盖
        out.push(StrokeSeg {
            a: [fx, fy, half_w0, 0.0],
            b: [sx, sy, half_w1, 0.0],
            ca: color,
            cb: color,
        });
    }

    !out.is_empty()
}

/// 将（平滑后的）笔画路径转换为线段表——**自适应胶囊合并发射**。
///
/// 先把路径按橡皮擦网格细分扣除（保留段之间可能存在切口），再沿连续
/// 采样链贪心合并：只要弦偏差 ≤[`MERGE_DEV_TOL`] 且宽度插值误差
/// ≤[`MERGE_WIDTH_TOL`]，就把尽量多的采样点并成单个长胶囊实例。
/// 橡皮擦切口、零宽断链点强制断链，绝不桥接被擦除的间隙。
fn append_stroke(
    stroke: &DrawStroke,
    index: Option<&EraserIndex>,
    geometry: &mut StrokeGeometry,
) {
    if stroke.points.is_empty() || stroke.stroke_type == StrokeType::Erase {
        return;
    }

    let color = stroke.rgba();
    let pts = build_smoothed_points(stroke);
    if pts.is_empty() {
        return;
    }
    let n = pts.len();

    if n == 1 {
        let (x, y, w) = pts[0];
        // 点画：退化段 → 胶囊退化为圆
        geometry.push(StrokeSeg::new(x, y, x, y, w / 2.0, color));
        return;
    }

    // 笔画离全部橡皮擦足够远时走无扣除快路径：此时可证明**任何**
    // `segment_touches` 都不可能为真（推导见 `may_touch_samples`），跳过
    // 逐采样段的网格查询。画布上多数笔画离橡皮擦很远，这是擦除重建的
    // 主路径。
    let index = index.filter(|ix| !ix.is_empty() && ix.may_touch_samples(&pts));

    if index.is_none() {
        // 无橡皮擦（或已证不相交）：纯逐采样链 + 自适应合并
        emit_merged(&pts, color, &mut geometry.segs);
        return;
    }
    let index = index.expect("上一步已过滤");

    // 合并状态：run 内保存链上采样点 (x, y, 全宽)，首尾即当前胶囊端点
    let mut run = MergeRun::new();
    let mut subs: Vec<(f32, f32, f32, f32)> = Vec::new();
    let mut prev_end: Option<(f32, f32)> = None;

    for i in 0..n - 1 {
        let (fx, fy, w0) = pts[i];
        let (sx, sy, w1) = pts[i + 1];
        let half_w0 = w0 / 2.0;
        let half_w1 = w1 / 2.0;
        if half_w0 < 0.001 && half_w1 < 0.001 {
            // 零宽断链点：后续点重新起链
            run.emit(&mut geometry.segs, color);
            run.clear();
            prev_end = None;
            continue;
        }

        // 橡皮擦细分扣除（保留切口导致的间隙）
        subs.clear();
        subdivide_segment_into(
            fx, fy, sx, sy, index, half_w0.max(half_w1), stroke.id, &mut subs,
        );
        if subs.is_empty() {
            // 整段被擦除：断链
            run.emit(&mut geometry.segs, color);
            run.clear();
            prev_end = None;
            continue;
        }

        let seg_count = subs.len() as f32;
        for (seg_i, seg) in subs.iter().enumerate() {
            let t0 = seg_i as f32 / seg_count;
            let t1 = (seg_i + 1) as f32 / seg_count;
            let w_s = w0 * (1.0 - t0) + w1 * t0;
            let w_e = w0 * (1.0 - t1) + w1 * t1;

            // 链连续性：段起点必须严格接在上段终点（切口 → 断链）
            if let Some((pe_x, pe_y)) = prev_end {
                if (pe_x - seg.0).abs() > 1e-3 || (pe_y - seg.1).abs() > 1e-3 {
                    run.emit(&mut geometry.segs, color);
                    run.clear();
                }
            }
            run.extend(seg.0, seg.1, w_s, &mut geometry.segs, color);
            run.extend(seg.2, seg.3, w_e, &mut geometry.segs, color);
            prev_end = Some((seg.2, seg.3));
        }
    }
    run.emit(&mut geometry.segs, color);
}

/// 固定容量的合并 run（`x`, `y`, 全宽），栈上分配、零堆分配。
///
/// 旧实现是 `Vec::with_capacity(MERGE_MAX_RUN.min(16))`——每次镶嵌调用一次
/// 堆分配。长度上界由 [`MergeRun::extend`] 的 `len >= MERGE_MAX_RUN` 判定
/// 保证（到界即断链发射，故 `len` 恒 ≤ `MERGE_MAX_RUN`），因此定长数组
/// 不会溢出：唯一写入路径 `push` 前必先判界。
struct MergeRun {
    pts: [(f32, f32, f32); MERGE_MAX_RUN],
    len: usize,
}

impl MergeRun {
    #[inline]
    fn new() -> Self {
        Self {
            pts: [(0.0, 0.0, 0.0); MERGE_MAX_RUN],
            len: 0,
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.len = 0;
    }

    #[inline]
    fn as_slice(&self) -> &[(f32, f32, f32)] {
        &self.pts[..self.len]
    }

    /// 向当前合并 run 追加一个链上采样点；无法继续合并时发射当前胶囊，
    /// 并以 run 末点为新胶囊起点（保证胶囊链无缝衔接）。
    fn extend(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        out: &mut Vec<StrokeSeg>,
        color: [f32; 4],
    ) {
        match self.pts[..self.len].last() {
            Some(&(lx, ly, _)) if (lx - x).abs() < 1e-6 && (ly - y).abs() < 1e-6 => {
                // 同点（细分子段共享端点），宽度由相同插值公式得出亦相同
            }
            Some(_) => {
                if self.len >= MERGE_MAX_RUN || !can_merge(self.as_slice(), x, y, w) {
                    // 单点 run 不发圆点（避免细笔尾多出冗余实例），
                    // 仅重置链；≥2 点才发射胶囊
                    if self.len >= 2 {
                        self.emit(out, color);
                    }
                    let last = self.pts[self.len - 1];
                    self.len = 0;
                    self.pts[0] = last;
                    self.len = 1;
                }
                self.pts[self.len] = (x, y, w);
                self.len += 1;
            }
            None => {
                self.pts[0] = (x, y, w);
                self.len = 1;
            }
        }
    }

    /// 发射当前合并 run：首尾两端即胶囊端点（宽度取端点全宽的一半）。
    /// 单点 run = 退化段（胶囊渲染为圆点）。
    fn emit(&self, out: &mut Vec<StrokeSeg>, color: [f32; 4]) {
        let run = self.as_slice();
        match run.len() {
            0 => {}
            1 => {
                let (x, y, w) = run[0];
                out.push(StrokeSeg::new(x, y, x, y, w / 2.0, color));
            }
            _ => {
                let (ax, ay, aw) = run[0];
                let &(bx, by, bw) = run.last().unwrap();
                out.push(StrokeSeg {
                    a: [ax, ay, aw / 2.0, 0.0],
                    b: [bx, by, bw / 2.0, 0.0],
                    ca: color,
                    cb: color,
                });
            }
        }
    }
}

/// 采样折线 → 贪心合并长胶囊（**无橡皮擦扣除**路径）。
///
/// 共享给静态镶嵌与直播满批块：两处必须走同一份合并逻辑，否则同一笔画在
/// "书写中"与"晾干后"的胶囊切分方式不同，抬笔瞬间会看到一次几何重排。
///
/// 零宽的处理分两种形态，勿混：
/// - **单个**零宽采样点是笔锋收尖——`MERGE_MIN_WIDTH` 会拦下合并，发出一段
///   末端半宽渐缩到 0 的胶囊（视觉上的笔锋），不额外断链；
/// - **连续 ≥2 个**零宽采样点才强制断链（`w0 < 0.002 && w1 < 0.002`），
///   绝不跨越断链点桥接。
fn emit_merged(pts: &[(f32, f32, f32)], color: [f32; 4], out: &mut Vec<StrokeSeg>) {
    // 显式 match 而非 `if len == 1`：`pts.len() - 1` 在空切片上会下溢
    match pts.len() {
        0 => {}
        1 => {
            let (x, y, w) = pts[0];
            // 点画：退化段 → 胶囊退化为圆
            out.push(StrokeSeg::new(x, y, x, y, w / 2.0, color));
        }
        _ => {
            let mut run = MergeRun::new();
            for i in 0..pts.len() - 1 {
                let (fx, fy, w0) = pts[i];
                let (sx, sy, w1) = pts[i + 1];
                if w0 < 0.002 && w1 < 0.002 {
                    // 零宽断点：断链（后续点重新起链）
                    run.emit(out, color);
                    run.clear();
                    continue;
                }
                run.extend(fx, fy, w0, out, color);
                run.extend(sx, sy, w1, out, color);
            }
            run.emit(out, color);
        }
    }
}

/// 合并判定：run 内全部中间采样点对新弦 (run[0] → 新点) 的垂直偏差与
/// 宽度插值误差均在容差内，且链上最小全宽足够容纳合并偏差。
fn can_merge(run: &[(f32, f32, f32)], x: f32, y: f32, w: f32) -> bool {
    let (ax, ay, aw) = run[0];
    let dx = x - ax;
    let dy = y - ay;
    let len_sq = dx * dx + dy * dy;
    if len_sq < 1e-12 {
        return false;
    }
    let mut min_w = w.min(aw);
    if min_w < MERGE_MIN_WIDTH {
        return false;
    }
    for &(px, py, pw) in &run[1..] {
        min_w = min_w.min(pw);
        let rx = px - ax;
        let ry = py - ay;
        // 垂直偏差（到**弦线段**的距离——必须钳制投影：回折笔画的中途点
        // 可能投影到弦外，无限直线距离会漏判导致缺段）
        let t = ((rx * dx + ry * dy) / len_sq).clamp(0.0, 1.0);
        let ex = rx - dx * t;
        let ey = ry - dy * t;
        if ex * ex + ey * ey > MERGE_DEV_TOL * MERGE_DEV_TOL {
            return false;
        }
        // 宽度线性插值误差
        let wl = aw + (w - aw) * t;
        if (pw - wl).abs() > MERGE_WIDTH_TOL {
            return false;
        }
    }
    min_w >= MERGE_MIN_WIDTH
}

/// 一组线段的**中心线**包围盒 + 最大半宽，用于橡皮擦扣除的笔画级早退。
///
/// 刻意**不**把半宽 padding 折进盒子里：命中判据是段间距离 `< draw_hw +
/// eraser_hw`，而两段之间的距离下界是中心线包围盒之间的距离。padding 一旦
/// 并入盒子，"盒子分离"只能推出距离 > 0，推不出 > `draw_hw + eraser_hw`
/// ——那会把仍需扣除的笔画误判为可跳过。半宽必须单独参与比较。
#[derive(Clone, Copy)]
struct SampleBounds {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    /// 组内各段的最大半宽
    max_half_w: f32,
}

impl SampleBounds {
    const EMPTY: Self = Self {
        min_x: f32::INFINITY,
        min_y: f32::INFINITY,
        max_x: f32::NEG_INFINITY,
        max_y: f32::NEG_INFINITY,
        max_half_w: 0.0,
    };

    /// 采样折线 `(x, y, 全宽)` 的中心线包围盒 + 最大半宽。
    fn from_samples(pts: &[(f32, f32, f32)]) -> Self {
        let mut b = Self::EMPTY;
        for &(x, y, w) in pts {
            b.add_point(x, y, x, y, w / 2.0);
        }
        b
    }

    #[inline]
    fn add_point(&mut self, min_x: f32, min_y: f32, max_x: f32, max_y: f32, half_w: f32) {
        if min_x < self.min_x {
            self.min_x = min_x;
        }
        if min_y < self.min_y {
            self.min_y = min_y;
        }
        if max_x > self.max_x {
            self.max_x = max_x;
        }
        if max_y > self.max_y {
            self.max_y = max_y;
        }
        if half_w > self.max_half_w {
            self.max_half_w = half_w;
        }
    }

    /// 两中心线包围盒之间的最小距离（相交时为 0）。
    #[inline]
    fn separation(&self, other: &Self) -> f32 {
        let dx = (self.min_x - other.max_x)
            .max(other.min_x - self.max_x)
            .max(0.0);
        let dy = (self.min_y - other.max_y)
            .max(other.min_y - self.max_y)
            .max(0.0);
        (dx * dx + dy * dy).sqrt()
    }
}

/// 橡皮擦空间网格：把橡皮擦段按其包围盒(+半宽)散列到网格单元，
/// 查询时只测与绘制段包围盒(+半宽)重叠的单元，避免逐点全量遍历。
struct EraserEntry {
    id: u64,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    half_w: f32,
}

pub struct EraserIndex {
    cells: std::collections::HashMap<(i32, i32), Vec<EraserEntry>>,
    /// 全部橡皮擦段的并集**中心线**包围盒（画布坐标，不含半宽 padding）。
    /// 用于笔画级早退，见 [`EraserIndex::may_touch_samples`]。
    bounds: SampleBounds,
}

impl EraserIndex {
    /// 索引是否为空（无任何橡皮擦段）——等价于不做扣除。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// 笔画级早退：笔画采样折线与全部橡皮擦**相距足够远**时可证明无需扣除，
    /// 直接跳过整条笔画的逐采样段网格查询。
    ///
    /// **等价性推导**（`>` 严格大于，等号时走慢路径，浮点上更保险）：
    /// 记笔画采样中心线包围盒 `S`、橡皮擦并集中心线包围盒 `E`，两侧最大半宽
    /// `S_hw` / `E_hw`。对任意被查询的采样段 `a` 与橡皮擦段 `b`：
    ///
    /// 1. `a ⊆ S`、`b ⊆ E`，故 `box_sep(a, b) ≥ box_sep(S, E)`
    ///    （子集的点对更少，最小距离只会更大）；
    /// 2. 两线段都落在各自中心线盒内，故 `dist(a, b) ≥ box_sep(a, b)`；
    /// 3. 合并 1+2：`dist(a, b) ≥ box_sep(S, E)`；
    /// 4. 查询判据是 `dist(a, b) < draw_hw + e_half_w ≤ S_hw + E_hw`。
    ///
    /// 于是 `box_sep(S, E) > S_hw + E_hw` 时判据恒假 ⇒ 与"无橡皮擦"逐段
    /// 一致。`S_hw < 0.001` 时所有段都被零宽断链跳过，本就无查询。
    ///
    /// 收益：画布上绝大多数笔画离橡皮擦很远。擦除重建 / 全量重算时这条早退
    /// 把每笔 O(采样数 × 单元查询) 降到 O(采样数)。
    #[inline]
    fn may_touch_samples(&self, pts: &[(f32, f32, f32)]) -> bool {
        let s = SampleBounds::from_samples(pts);
        // 全部采样点都是零宽 → 零宽断链，无任何查询（append_stroke 已跳段）
        if s.max_half_w < 0.001 {
            return false;
        }
        s.separation(&self.bounds) <= s.max_half_w + self.bounds.max_half_w
    }

    fn build(eraser_strokes: &[DrawStroke], current_eraser: Option<&DrawStroke>) -> Self {
        let mut cells: std::collections::HashMap<(i32, i32), Vec<EraserEntry>> =
            std::collections::HashMap::new();
        let mut bounds = SampleBounds::EMPTY;

        let mut insert_eraser = |es: &DrawStroke| {
            let has_variable = !es.variable_widths.is_empty();
            for (i, pt) in es.points.iter().enumerate() {
                let w = if has_variable && i < es.variable_widths.len() {
                    es.variable_widths[i]
                } else {
                    es.line_width
                };
                if w < 0.001 {
                    continue;
                }
                let half_w = w / 2.0;
                let min_x = pt.from_x.min(pt.to_x);
                let min_y = pt.from_y.min(pt.to_y);
                let max_x = pt.from_x.max(pt.to_x);
                let max_y = pt.from_y.max(pt.to_y);
                bounds.add_point(min_x, min_y, max_x, max_y, half_w);
                let c0 = ((min_x - half_w) / ERASER_GRID_CELL).floor() as i32;
                let c1 = ((min_y - half_w) / ERASER_GRID_CELL).floor() as i32;
                let c2 = ((max_x + half_w) / ERASER_GRID_CELL).floor() as i32;
                let c3 = ((max_y + half_w) / ERASER_GRID_CELL).floor() as i32;
                for cx in c0..=c2 {
                    for cy in c1..=c3 {
                        cells.entry((cx, cy)).or_default().push(EraserEntry {
                            id: es.id,
                            x1: pt.from_x,
                            y1: pt.from_y,
                            x2: pt.to_x,
                            y2: pt.to_y,
                            half_w,
                        });
                    }
                }
            }
        };

        for es in eraser_strokes {
            insert_eraser(es);
        }
        if let Some(es) = current_eraser {
            insert_eraser(es);
        }

        Self { cells, bounds }
    }

    fn cell_range(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> (i32, i32, i32, i32) {
        (
            (min_x / ERASER_GRID_CELL).floor() as i32,
            (min_y / ERASER_GRID_CELL).floor() as i32,
            (max_x / ERASER_GRID_CELL).floor() as i32,
            (max_y / ERASER_GRID_CELL).floor() as i32,
        )
    }

    /// 判断绘制段是否与任何橡皮擦段相交（距离 < draw_hw + eraser_hw，只统计 id > draw_id 的橡皮擦）
    fn segment_touches(
        &self,
        fx: f32,
        fy: f32,
        sx: f32,
        sy: f32,
        draw_hw: f32,
        draw_id: u64,
    ) -> bool {
        let (c0, c1, c2, c3) = Self::cell_range(
            fx.min(sx) - draw_hw,
            fy.min(sy) - draw_hw,
            fx.max(sx) + draw_hw,
            fy.max(sy) + draw_hw,
        );
        for cx in c0..=c2 {
            for cy in c1..=c3 {
                if let Some(entries) = self.cells.get(&(cx, cy)) {
                    for e in entries {
                        if e.id <= draw_id {
                            continue;
                        }
                        if segment_segment_distance(fx, fy, sx, sy, e.x1, e.y1, e.x2, e.y2)
                            < draw_hw + e.half_w
                        {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    fn point_touches(&self, x: f32, y: f32, draw_hw: f32, draw_id: u64) -> bool {
        self.segment_touches(x, y, x, y, draw_hw, draw_id)
    }
}

fn subdivide_segment_into(
    fx: f32,
    fy: f32,
    sx: f32,
    sy: f32,
    index: &EraserIndex,
    draw_hw: f32,
    draw_id: u64,
    out: &mut Vec<(f32, f32, f32, f32)>,
) {
    if !index.segment_touches(fx, fy, sx, sy, draw_hw, draw_id) {
        out.push((fx, fy, sx, sy));
        return;
    }
    subdivide_recursive_into(fx, fy, sx, sy, index, draw_hw, draw_id, 0, out);
}

fn subdivide_recursive_into(
    fx: f32,
    fy: f32,
    sx: f32,
    sy: f32,
    index: &EraserIndex,
    draw_hw: f32,
    draw_id: u64,
    depth: u32,
    result: &mut Vec<(f32, f32, f32, f32)>,
) {
    if !index.segment_touches(fx, fy, sx, sy, draw_hw, draw_id) {
        result.push((fx, fy, sx, sy));
        return;
    }
    if depth >= MAX_SUBDIVISIONS {
        let fully_inside = index.point_touches(fx, fy, draw_hw, draw_id)
            && index.point_touches(sx, sy, draw_hw, draw_id);
        if !fully_inside {
            result.push((fx, fy, sx, sy));
        }
        return;
    }
    let mx = (fx + sx) / 2.0;
    let my = (fy + sy) / 2.0;
    subdivide_recursive_into(fx, fy, mx, my, index, draw_hw, draw_id, depth + 1, result);
    subdivide_recursive_into(mx, my, sx, sy, index, draw_hw, draw_id, depth + 1, result);
}

fn segment_segment_distance(
    ax: f32,
    ay: f32,
    bx: f32,
    by: f32,
    cx: f32,
    cy: f32,
    dx: f32,
    dy: f32,
) -> f32 {
    let ux = bx - ax;
    let uy = by - ay;
    let vx = dx - cx;
    let vy = dy - cy;
    let wx = ax - cx;
    let wy = ay - cy;
    let a = ux * ux + uy * uy;
    let b = ux * vx + uy * vy;
    let c = vx * vx + vy * vy;
    let d = ux * wx + uy * wy;
    let e = vx * wx + vy * wy;
    let det = a * c - b * b;
    if det.abs() > 1e-10 {
        let s = (b * e - c * d) / det;
        let t = (a * e - b * d) / det;
        if (0.0..=1.0).contains(&s) && (0.0..=1.0).contains(&t) {
            let px = wx + ux * s - vx * t;
            let py = wy + uy * s - vy * t;
            return (px * px + py * py).sqrt();
        }
    }
    let d1 = point_segment_distance_sq(ax, ay, cx, cy, dx, dy);
    let d2 = point_segment_distance_sq(bx, by, cx, cy, dx, dy);
    let d3 = point_segment_distance_sq(cx, cy, ax, ay, bx, by);
    let d4 = point_segment_distance_sq(dx, dy, ax, ay, bx, by);
    d1.min(d2).min(d3).min(d4).sqrt()
}

fn point_segment_distance_sq(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let ex = bx - ax;
    let ey = by - ay;
    let len_sq = ex * ex + ey * ey;
    if len_sq < 1e-10 {
        return (px - ax) * (px - ax) + (py - ay) * (py - ay);
    }
    let t = (((px - ax) * ex + (py - ay) * ey) / len_sq).clamp(0.0, 1.0);
    let closest_x = ax + t * ex;
    let closest_y = ay + t * ey;
    (px - closest_x) * (px - closest_x) + (py - closest_y) * (py - closest_y)
}

/// 渲染擦除预览路径（红色半透明胶囊段表，画布坐标）
fn append_eraser_trail(es: &DrawStroke, geometry: &mut StrokeGeometry) {
    let color: [f32; 4] = ERASER_TRAIL_COLOR;
    let has_variable = !es.variable_widths.is_empty();

    for (i, pt) in es.points.iter().enumerate() {
        let w = if has_variable && i < es.variable_widths.len() {
            es.variable_widths[i]
        } else {
            es.line_width
        };
        if w < 0.001 {
            continue;
        }
        // 画布坐标；退化段由胶囊 SDF 渲染为圆点
        geometry.push(StrokeSeg::new(
            pt.from_x,
            pt.from_y,
            pt.to_x,
            pt.to_y,
            w / 2.0,
            color,
        ));
    }
}

#[cfg(test)]
mod eraser_reject_tests {
    use super::*;

    fn eraser(id: u64, width: f32, pts: &[(f32, f32)]) -> DrawStroke {
        let mut es = DrawStroke::new_eraser(width, id);
        for i in 0..pts.len().saturating_sub(1) {
            es.add_point(pts[i].0, pts[i].1, pts[i + 1].0, pts[i + 1].1);
            es.variable_widths.push(width);
        }
        if pts.len() == 1 {
            es.add_point(pts[0].0, pts[0].1, pts[0].0, pts[0].1);
            es.variable_widths.push(width);
        }
        es
    }

    /// 绕过早退的参考实现：强制走扣除路径（`index` 非空且包围盒重叠判定
    /// 被短路为 true）。
    fn tessellate_forced(stroke: &DrawStroke, erasers: &[DrawStroke]) -> Option<StrokeGeometry> {
        let index = EraserIndex::build(erasers, None);
        let mut geometry = StrokeGeometry::default();
        // 复制 append_stroke 的扣除分支，但不做 may_touch_samples 早退
        let pts = build_smoothed_points(stroke);
        if pts.is_empty() {
            return None;
        }
        let color = stroke.rgba();
        if pts.len() == 1 {
            let (x, y, w) = pts[0];
            geometry.push(StrokeSeg::new(x, y, x, y, w / 2.0, color));
            return Some(geometry);
        }
        let mut run = MergeRun::new();
        let mut subs: Vec<(f32, f32, f32, f32)> = Vec::new();
        let mut prev_end: Option<(f32, f32)> = None;
        for i in 0..pts.len() - 1 {
            let (fx, fy, w0) = pts[i];
            let (sx, sy, w1) = pts[i + 1];
            let half_w0 = w0 / 2.0;
            let half_w1 = w1 / 2.0;
            if half_w0 < 0.001 && half_w1 < 0.001 {
                run.emit(&mut geometry.segs, color);
                run.clear();
                prev_end = None;
                continue;
            }
            subs.clear();
            subdivide_segment_into(
                fx,
                fy,
                sx,
                sy,
                &index,
                half_w0.max(half_w1),
                stroke.id,
                &mut subs,
            );
            if subs.is_empty() {
                run.emit(&mut geometry.segs, color);
                run.clear();
                prev_end = None;
                continue;
            }
            let seg_count = subs.len() as f32;
            for (seg_i, seg) in subs.iter().enumerate() {
                let t0 = seg_i as f32 / seg_count;
                let t1 = (seg_i + 1) as f32 / seg_count;
                let w_s = w0 * (1.0 - t0) + w1 * t0;
                let w_e = w0 * (1.0 - t1) + w1 * t1;
                if let Some((pe_x, pe_y)) = prev_end {
                    if (pe_x - seg.0).abs() > 1e-3 || (pe_y - seg.1).abs() > 1e-3 {
                        run.emit(&mut geometry.segs, color);
                        run.clear();
                    }
                }
                run.extend(seg.0, seg.1, w_s, &mut geometry.segs, color);
                run.extend(seg.2, seg.3, w_e, &mut geometry.segs, color);
                prev_end = Some((seg.2, seg.3));
            }
        }
        run.emit(&mut geometry.segs, color);
        if geometry.is_empty() {
            None
        } else {
            Some(geometry)
        }
    }

    /// 早退**绝不能**改变结果：把"早退版"与"强制扣除版"逐段比对。
    ///
    /// 扫过一段距离/粗细的网格（含远、贴边、擦穿、擦一半、同心等多种
    /// 相对位置），任何一处早退漏判都会让本测试失败。
    #[test]
    fn early_reject_never_changes_geometry() {
        let mut ink = DrawStroke::new("#112233".into(), 10.0, 1, 0.4, 30.0);
        let mut x = 0.0f32;
        for i in 0..60 {
            let nx = x + 7.0;
            let ny = (i as f32 * 0.6).sin() * 22.0;
            ink.add_point(x, 0.0, nx, ny);
            ink.variable_widths.push(10.0);
            x = nx;
        }

        for &ew in &[4.0f32, 10.0, 30.0, 60.0] {
            for &dx in &[-200.0f32, -40.0, -18.0, -8.0, -3.0, 0.0, 3.0, 8.0, 18.0, 40.0, 200.0] {
                for &dy in &[-200.0f32, -30.0, -6.0, 0.0, 6.0, 30.0, 200.0] {
                    // 橡皮擦横穿笔画中部
                    let cx = 200.0 + dx;
                    let cy = dy;
                    let es = eraser(9, ew, &[(cx - 40.0, cy), (cx + 40.0, cy)]);
                    let got = tessellate_stroke_indexed(&ink, &build_eraser_index(&[es.clone()], None));
                    let want = tessellate_forced(&ink, &[es]);
                    match (got, want) {
                        (None, None) => {}
                        (Some(a), Some(b)) => assert_eq!(
                            a.segs, b.segs,
                            "ew={ew} dx={dx} dy={dy}: 早退改变了几何（段数 {} vs {}）",
                            a.segs.len(),
                            b.segs.len()
                        ),
                        (a, b) => panic!(
                            "ew={ew} dx={dx} dy={dy}: 早退改变了存在性 got={} want={}",
                            a.is_some(),
                            b.is_some()
                        ),
                    }
                }
            }
        }
    }

    /// 早退在"擦得掉"与"擦不掉"两种情形下都必须放行到慢路径，
    /// 否则上面的等价性就是靠"全都跳过"蒙对的。
    #[test]
    fn early_reject_keeps_real_erasures() {
        let mut ink = DrawStroke::new("#112233".into(), 10.0, 1, 0.4, 30.0);
        for i in 0..40 {
            ink.add_point(i as f32 * 10.0, 0.0, (i + 1) as f32 * 10.0, 0.0);
            ink.variable_widths.push(10.0);
        }
        // 正压在笔画上
        let on = eraser(9, 20.0, &[(150.0, 0.0), (250.0, 0.0)]);
        let idx = build_eraser_index(&[on], None);
        assert!(
            idx.may_touch_samples(&build_smoothed_points(&ink)),
            "压在笔画上的橡皮擦必须放行到扣除路径"
        );
        let g = tessellate_stroke_indexed(&ink, &idx).expect("仍有未擦部分");
        let clean = tessellate_stroke_canvas(&ink, &[], None).expect("无橡皮擦基准");
        assert!(
            g.segs.len() < clean.segs.len(),
            "擦除后段数应少于未擦（{} vs {}）——扣除没生效？",
            g.segs.len(),
            clean.segs.len()
        );
        // 远离笔画
        let far = eraser(9, 20.0, &[(150.0, 900.0), (250.0, 900.0)]);
        let idx = build_eraser_index(&[far], None);
        assert!(
            !idx.may_touch_samples(&build_smoothed_points(&ink)),
            "远离笔画的橡皮擦应触发早退"
        );
        assert_eq!(
            tessellate_stroke_indexed(&ink, &idx).unwrap().segs,
            clean.segs,
            "早退结果必须与无橡皮擦逐段一致"
        );
    }

    /// 零宽笔画（笔锋全收尖）不产生任何查询 → 早退必须放行为"无需扣除"，
    /// 且不得因为包围盒退化（EMPTY）而出错。
    #[test]
    fn early_reject_handles_degenerate_bounds() {
        let mut ink = DrawStroke::new("#112233".into(), 10.0, 1, 0.4, 30.0);
        ink.add_point(50.0, 50.0, 50.0, 50.0);
        ink.variable_widths.push(0.0);
        let idx = build_eraser_index(&[eraser(9, 20.0, &[(50.0, 50.0), (90.0, 50.0)])], None);
        // 单采样点 → 走点画分支，压根不到 may_touch_samples
        assert!(tessellate_stroke_indexed(&ink, &idx).is_some());
        // 显式验证零宽折线的判定
        let zero_samples = vec![(0.0f32, 0.0f32, 0.0f32)];
        assert!(
            !idx.may_touch_samples(&zero_samples),
            "全零宽采样折线应判定为无需扣除"
        );
    }
}

#[cfg(test)]
mod curve_ctx_tests {
    use super::*;

    /// 旧 `catmull_rom` 的**逐字复制**（含每采样点重算 d0/d1/d2 的冗余）。
    /// `CurveCtx` 必须是它的逐位等价重构——这是纯性能改写，不许动任何输出。
    fn catmull_rom_reference(
        p0: (f32, f32),
        p1: (f32, f32),
        p2: (f32, f32),
        p3: (f32, f32),
        t: f32,
    ) -> (f32, f32) {
        let d0 = (p0.0 - p1.0).hypot(p0.1 - p1.1).sqrt();
        let d1 = (p1.0 - p2.0).hypot(p1.1 - p2.1).sqrt();
        let d2 = (p2.0 - p3.0).hypot(p2.1 - p3.1).sqrt();
        let t0 = 0.0;
        let t1 = t0 + d0;
        let t2 = t1 + d1;
        let t3 = t2 + d2;
        let tt = t1 + (t2 - t1) * t;

        // 下面两个嵌套 fn **故意**与父模块的 `crw` / `lerp2` 同名：它们是旧
        // 实现的逐字副本，遮蔽父模块版本，保证参考实现不会"借用"到新代码。
        fn crw(ta: f32, tb: f32, tt: f32) -> (f32, f32) {
            let span = tb - ta;
            if span.abs() < 1e-6 {
                return (0.0, 1.0);
            }
            let w_b = (tt - ta) / span;
            (1.0 - w_b, w_b)
        }
        fn lerp2(a: (f32, f32), b: (f32, f32), w_a: f32, w_b: f32) -> (f32, f32) {
            (a.0 * w_a + b.0 * w_b, a.1 * w_a + b.1 * w_b)
        }

        let (w00, w01) = crw(t0, t1, tt);
        let a1 = lerp2(p0, p1, w00, w01);
        let (w10, w11) = crw(t1, t2, tt);
        let a2 = lerp2(p1, p2, w10, w11);
        let (w20, w21) = crw(t2, t3, tt);
        let a3 = lerp2(p2, p3, w20, w21);
        let (b10, b11) = crw(t0, t2, tt);
        let b1 = lerp2(a1, a2, b10, b11);
        let (b20, b21) = crw(t1, t3, tt);
        let b2 = lerp2(a2, a3, b20, b21);
        let (c0, c1) = crw(t1, t2, tt);
        lerp2(b1, b2, c0, c1)
    }

    fn control_sets() -> Vec<((f32, f32), (f32, f32), (f32, f32), (f32, f32))> {
        vec![
            // 常规
            ((0.0, 0.0), (10.0, 4.0), (22.0, -3.0), (31.0, 6.0)),
            // 间距极不均匀（向心参数化存在的意义）
            ((0.0, 0.0), (0.5, 0.2), (180.0, 40.0), (182.0, 41.0)),
            ((0.0, 0.0), (90.0, 0.0), (95.0, 0.0), (400.0, 0.0)),
            // p1 == p2（d1 ≈ 0 → 退化跨度）
            ((5.0, 5.0), (10.0, 10.0), (10.0, 10.0), (20.0, 3.0)),
            // p0 == p1（d0 ≈ 0）
            ((7.0, 7.0), (7.0, 7.0), (15.0, -2.0), (25.0, 4.0)),
            // p2 == p3（d2 ≈ 0）
            ((0.0, 0.0), (4.0, 9.0), (30.0, 1.0), (30.0, 1.0)),
            // 三点重合（多个跨度同时退化）
            ((3.0, 3.0), (3.0, 3.0), (3.0, 3.0), (3.0, 3.0)),
            // 负坐标 / 大坐标
            ((-2000.0, -1500.0), (-1990.0, -1490.0), (-1800.0, -1400.0), (0.0, 0.0)),
            // 负跨度方向的尖角回折
            ((20.0, 0.0), (10.0, 1.0), (9.0, -1.0), (30.0, 0.0)),
        ]
    }

    #[test]
    fn curve_ctx_is_bit_identical_to_reference() {
        for cs in control_sets() {
            let ctx = CurveCtx::new(cs.0, cs.1, cs.2, cs.3);
            for step in 1..=8 {
                let t = step as f32 / 8.0;
                assert_eq!(
                    ctx.eval(t),
                    catmull_rom_reference(cs.0, cs.1, cs.2, cs.3, t),
                    "控制点 {cs:?} t={t} 必须逐位一致"
                );
            }
        }
    }

    #[test]
    fn curve_ctx_degenerate_flags_match_reference_crw() {
        // 退化跨度集合必须与旧 crw 内部的 `span.abs() < 1e-6` 判据一致
        for cs in control_sets() {
            let ctx = CurveCtx::new(cs.0, cs.1, cs.2, cs.3);
            let expect = [
                ctx.span01.abs() < 1e-6,
                ctx.span12.abs() < 1e-6,
                ctx.span23.abs() < 1e-6,
                ctx.span02.abs() < 1e-6,
                ctx.span13.abs() < 1e-6,
            ];
            for (bit, want) in expect.iter().enumerate() {
                assert_eq!(
                    ctx.deg & (1u8 << bit) != 0,
                    *want,
                    "控制点 {cs:?} 跨度#{bit} 退化标记不符"
                );
            }
        }
    }

    #[test]
    fn subdivisions_match_legacy_rule() {
        // 细分密度规则被原样搬进 CurveCtx：<0.001 → 1，否则 ceil(len/6) 钳 [1,8]
        let legacy = |seg_len: f32| -> usize {
            if seg_len < 0.001 {
                1
            } else {
                ((seg_len / 6.0).ceil() as usize).clamp(1, 8)
            }
        };
        for &len in &[
            0.0f32, 0.0005, 0.001, 0.002, 1.0, 5.9, 6.0, 6.1, 12.0, 47.9, 48.0, 1000.0, 1e9,
        ] {
            // 造一对间距恰为 len 的控制点，读回 subdivisions()
            let ctx = CurveCtx::new((0.0, 0.0), (0.0, 0.0), (len, 0.0), (len, 0.0));
            assert_eq!(
                ctx.subdivisions(),
                legacy(len),
                "seg_len={len} 细分密度必须与旧规则一致"
            );
        }
    }
}

#[cfg(test)]
mod chunk_merge_tests {
    use super::*;

    fn dense_stroke(n: usize) -> DrawStroke {
        let mut s = DrawStroke::new("#3b82f6".to_string(), 12.0, 1, 0.4, 30.0);
        let mut x = 0.0f32;
        let mut y = 0.0f32;
        for i in 0..n {
            let nx = x + 6.0 + (i as f32 * 0.7).sin() * 3.0;
            let ny = y + 4.0 + (i as f32 * 0.5).cos() * 2.0;
            s.add_point(x, y, nx, ny);
            s.variable_widths.push(12.0 - (i as f32) * 0.3);
            x = nx;
            y = ny;
        }
        s
    }

    /// 满批块的核心收益：实例数必须显著少于逐采样发射，否则合并白做。
    #[test]
    fn chunk_merging_reduces_instance_count() {
        let s = dense_stroke(80);
        let path = build_path(&s);
        let mut scratch = Vec::new();

        let mut merged = Vec::new();
        assert!(live_range_samples(&s, &path, 0, 79, &mut scratch));
        let sample_count = scratch.len();
        emit_merged(&scratch, s.rgba(), &mut merged);
        let merged_count = merged.len();

        let mut plain = Vec::new();
        assert!(tessellate_live_range_segs(&s, &path, 0, 79, &mut scratch, &mut plain));

        assert_eq!(sample_count - 1, plain.len(), "基准：逐采样发射 = 采样数-1");
        assert!(
            merged_count * 2 < plain.len(),
            "合并后实例数应至少减半：merged={merged_count} plain={}",
            plain.len()
        );
    }

    /// 满批块走合并、尾部块走逐采样——两条直播路径的发射策略必须**不同**，
    /// 且各自稳定（否则说明拆分没生效）。
    #[test]
    fn into_merges_and_segs_does_not() {
        let s = dense_stroke(48);
        let path = build_path(&s);
        let mut scratch = Vec::new();

        let chunk = tessellate_live_range_into(&s, &path, 0, 47, &mut scratch).expect("满批块");
        let mut head = Vec::new();
        assert!(tessellate_live_range_segs(&s, &path, 0, 47, &mut scratch, &mut head));
        assert!(
            chunk.segs.len() < head.len(),
            "满批块({}) 应比尾部块({}) 合并得更狠",
            chunk.segs.len(),
            head.len()
        );
    }

    /// 合并后的满批块必须与"同样区间走晾干路径"逐段一致——
    /// 这锁住"书写中的满批部分 == 晾干后的静态部分"。
    #[test]
    fn chunk_merge_matches_dry_tessellation_of_same_range() {
        let s = dense_stroke(64);
        let path = build_path(&s);
        let mut scratch = Vec::new();

        // 满批块：直播合并路径
        let live = tessellate_live_range_into(&s, &path, 0, 63, &mut scratch).expect("满批块");

        // 参照：把同一段采样点交给晾干用的 emit_merged
        let mut samples = Vec::new();
        assert!(live_range_samples(&s, &path, 0, 63, &mut samples));
        let mut dry = Vec::new();
        emit_merged(&samples, s.rgba(), &mut dry);

        assert_eq!(live.segs, dry, "直播满批块必须与晾干合并逐段一致");
    }

    /// 合并的零宽处理分两种形态，行为不同，本测试逐一锁定：
    ///
    /// - **单个**零宽采样点 = 笔锋收尖。`MERGE_MIN_WIDTH` 拦下合并，于是主体
    ///   胶囊之后跟一段**末端半宽渐缩到 0** 的锥尖胶囊（视觉上的笔锋）。
    /// - **连续 ≥2 个**零宽采样点 = 断链点。两侧各自成胶囊，绝不桥接。
    ///
    /// 注意零宽点必须位于**不同**位置：位置与前一点相同时会被 `extend` 的
    /// 同点去重吃掉，宽度根本进不了 run，也就谈不上收尖。真实数据里笔锋
    /// 一边前进一边收窄，位置必然不同。
    #[test]
    fn merge_tapers_at_single_zero_and_breaks_at_run() {
        let w = 8.0f32;
        let body = |from: f32, n: usize| -> Vec<(f32, f32, f32)> {
            (0..n)
                .map(|i| (from + i as f32 * 20.0, 0.0, w))
                .collect::<Vec<_>>()
        };

        // ---- 形态一：单个零宽点 → 主体 + 锥尖 ----
        let mut taper = body(0.0, 6);
        taper.push((120.0, 0.0, 0.0));
        let mut out = Vec::new();
        emit_merged(&taper, [1.0; 4], &mut out);
        assert_eq!(out.len(), 2, "主体 + 锥尖应为 2 段，实际 {}", out.len());
        assert!(
            out[0].b[2] > 0.001,
            "主体胶囊末端半宽应保持 > 0，实际 {}",
            out[0].b[2]
        );
        assert!(
            out[1].b[2] < 0.001,
            "锥尖胶囊末端半宽应收缩到 0，实际 {}",
            out[1].b[2]
        );
        assert!(
            out[1].a[2] > 0.001 && out[1].b[2] < 0.001,
            "锥尖应从满宽渐缩到 0：{} -> {}",
            out[1].a[2],
            out[1].b[2]
        );

        // ---- 形态二：连续两个零宽点 → 断链，两侧不得桥接 ----
        let mut brk = body(0.0, 6);
        brk.push((100.0, 0.0, 0.0));
        brk.push((120.0, 0.0, 0.0)); // 连续第二点 → 断链
        brk.extend(body(140.0, 6));
        let mut out = Vec::new();
        emit_merged(&brk, [1.0; 4], &mut out);
        assert!(!out.is_empty(), "断链两侧各自成胶囊，不得空输出");
        for s in &out {
            let lo = s.a[0].min(s.b[0]);
            let hi = s.a[0].max(s.b[0]);
            assert!(
                !(lo < 100.0 && hi > 140.0),
                "有胶囊从 x={lo} 跨到 x={hi}，把零宽断链点两侧桥接起来了"
            );
        }
        assert!(
            out.iter().any(|s| s.a[0].max(s.b[0]) <= 120.0) && out.iter().any(|s| s.a[0].min(s.b[0]) >= 140.0),
            "断链点两侧都应有胶囊，实际 {:?}",
            out.iter().map(|s| (s.a[0], s.b[0])).collect::<Vec<_>>()
        );
    }

    /// 单点（点画）退化段在两条直播路径与晾干路径上都要产出一个圆点。
    #[test]
    fn single_point_emits_dot_on_all_paths() {
        let mut s = DrawStroke::new("#ff0000".to_string(), 9.0, 1, 0.4, 30.0);
        s.add_point(7.0, 8.0, 7.0, 8.0);
        s.variable_widths.push(9.0);
        let path = build_path(&s);
        let mut scratch = Vec::new();

        let mut head = Vec::new();
        assert!(tessellate_live_range_segs(&s, &path, 0, 0, &mut scratch, &mut head));
        assert_eq!(head.len(), 1, "尾部路径单点应为一个圆点");
        assert_eq!(head[0].a, head[0].b, "点画应为退化段");

        let mut dry = Vec::new();
        emit_merged(&scratch, s.rgba(), &mut dry);
        assert_eq!(dry, head, "点画在合并路径与逐采样路径上必须一致");
    }
}

#[cfg(test)]
mod live_reuse_tests {
    use super::*;

    fn dense_stroke(n: usize) -> DrawStroke {
        let mut s = DrawStroke::new("#3b82f6".to_string(), 12.0, 1, 0.4, 30.0);
        let mut x = 0.0f32;
        let mut y = 0.0f32;
        for i in 0..n {
            let nx = x + 6.0 + (i as f32 * 0.7).sin() * 3.0;
            let ny = y + 4.0 + (i as f32 * 0.5).cos() * 2.0;
            s.add_point(x, y, nx, ny);
            s.variable_widths.push(12.0 - (i as f32) * 0.3);
            x = nx;
            y = ny;
        }
        s
    }

    /// 复用变体（`out` 由调用方持有）与一次性分配变体必须产出**完全相同**的
    /// 段表——`out` 的历史容量不得影响内容。
    ///
    /// 注意基准取的是**同一条发射策略**（未合并的 `_segs`）。第五轮起
    /// `_into`（满批块）改走合并发射、`_segs`（尾部块）保持逐采样，两者段数
    /// 本就不同（见 `chunk_merge_tests`），拿它们互相当基准会误报。
    #[test]
    fn recycled_buffer_matches_fresh_allocation() {
        let s = dense_stroke(64);
        let path = build_path(&s);
        let mut scratch = Vec::new();
        let mut recycled: Vec<StrokeSeg> = Vec::new();
        let mut fresh: Vec<StrokeSeg> = Vec::new();

        // 先把 recycled 撑大（模拟它此前承载过更长的尾部）
        assert!(tessellate_live_range_segs(&s, &path, 0, 63, &mut scratch, &mut fresh));
        assert!(tessellate_live_range_segs(&s, &path, 0, 63, &mut scratch, &mut recycled));
        assert!(recycled.capacity() > 0);

        assert!(tessellate_live_range_segs(&s, &path, 0, 63, &mut scratch, &mut recycled));
        assert_eq!(
            recycled, fresh,
            "复用缓冲（cap={}）与全新分配必须逐段一致",
            recycled.capacity()
        );
    }

    #[test]
    fn reuse_buffer_is_stable_across_calls() {
        let s = dense_stroke(64);
        let path = build_path(&s);
        let mut scratch = Vec::new();
        let mut out: Vec<StrokeSeg> = Vec::new();

        assert!(tessellate_live_range_segs(&s, &path, 0, 63, &mut scratch, &mut out));
        let first_capacity = out.capacity();
        let first_len = out.len();
        let snapshot = out.clone();

        // 反复重建（模拟每个输入点一次）：容量不得下降，内容不得变化
        for _ in 0..32 {
            assert!(tessellate_live_range_segs(&s, &path, 0, 63, &mut scratch, &mut out));
            assert_eq!(out.capacity(), first_capacity, "复用缓冲不得反复重新分配");
            assert_eq!(out.len(), first_len);
            assert_eq!(out, snapshot, "同一输入必须产出完全相同的段");
        }
    }

    #[test]
    fn reuse_buffer_handles_growing_ranges() {
        // 尾部段随笔画增长而变长：容量只增不减，且每轮结果与全新分配一致
        let s = dense_stroke(200);
        let path = build_path(&s);
        let mut scratch = Vec::new();
        let mut out: Vec<StrokeSeg> = Vec::new();
        let mut fresh: Vec<StrokeSeg> = Vec::new();
        let mut capacity_floor = 0usize;

        for last in (10..200).step_by(7) {
            assert!(tessellate_live_range_segs(&s, &path, 0, last, &mut scratch, &mut out));
            assert!(
                out.capacity() >= capacity_floor,
                "容量只增不减（floor={capacity_floor}, now={}）",
                out.capacity()
            );
            capacity_floor = out.capacity();

            assert!(tessellate_live_range_segs(&s, &path, 0, last, &mut scratch, &mut fresh));
            assert_eq!(out, fresh, "last={last} 段表应与全新分配逐段一致");
        }
    }

    #[test]
    fn reuse_returns_false_on_invalid_range_without_touching_buffer() {
        let s = dense_stroke(8);
        let path = build_path(&s);
        let mut scratch = Vec::new();
        let mut out = vec![StrokeSeg::new(0.0, 0.0, 1.0, 1.0, 1.0, [1.0; 4])];

        // first > last
        assert!(!tessellate_live_range_segs(&s, &path, 5, 2, &mut scratch, &mut out));
        // last >= n
        assert!(!tessellate_live_range_segs(&s, &path, 0, 99, &mut scratch, &mut out));
        // 空笔画
        let mut empty = dense_stroke(0);
        empty.points.clear();
        assert!(!tessellate_live_range_segs(&empty, &path, 0, 0, &mut scratch, &mut out));

        assert_eq!(out.len(), 1, "非法输入不得清空调用方缓冲");
    }

    /// 非法区间必须被两条直播路径**一致**地拒绝（发射策略不同，但合法性
    /// 判定同源于 `live_range_samples`）。
    #[test]
    fn both_live_variants_reject_the_same_invalid_ranges() {
        let s = dense_stroke(8);
        let path = build_path(&s);
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        for &(first, last) in &[(0usize, 7usize), (3, 2), (0, 99)] {
            let produced = tessellate_live_range_segs(&s, &path, first, last, &mut scratch, &mut out);
            let into = tessellate_live_range_into(&s, &path, first, last, &mut scratch);
            assert_eq!(
                produced,
                into.is_some(),
                "区间 ({first}, {last}) 的成败判定必须一致"
            );
        }
    }
}

#[cfg(test)]
mod index_reuse_tests {
    use super::*;

    fn draw_stroke(id: u64, n: usize) -> DrawStroke {
        let mut s = DrawStroke::new("#ff0000".to_string(), 10.0, id, 0.4, 30.0);
        let (mut x, mut y) = (0.0f32, 0.0f32);
        for i in 0..n {
            let nx = x + 6.0 + (i as f32 * 0.7).sin() * 3.0;
            let ny = y + 4.0 + (i as f32 * 0.5).cos() * 2.0;
            s.add_point(x, y, nx, ny);
            x = nx;
            y = ny;
        }
        s
    }

    fn assert_same(a: &Option<StrokeGeometry>, b: &Option<StrokeGeometry>) {
        match (a, b) {
            (None, None) => {}
            (Some(a), Some(b)) => {
                assert_eq!(a.segs.len(), b.segs.len(), "段数应一致");
                for (i, (x, y)) in a.segs.iter().zip(b.segs.iter()).enumerate() {
                    assert_eq!(x, y, "段 {i} 应逐字段一致");
                }
            }
            _ => panic!("索引复用路径与非索引路径结果不一致"),
        }
    }

    #[test]
    fn indexed_matches_per_stroke_rebuild() {
        // 批量路径（建一次索引）与逐笔路径（每次重建索引）必须完全等价
        let strokes: Vec<DrawStroke> = (0..6u64).map(|i| draw_stroke(i, 40)).collect();
        let erasers: Vec<DrawStroke> = (0..3u64)
            .map(|i| {
                let mut e = DrawStroke::new_eraser(50.0, 100 + i);
                e.add_point(10.0 + i as f32 * 30.0, -20.0, 10.0 + i as f32 * 30.0, 200.0);
                e
            })
            .collect();

        let index = build_eraser_index(&erasers, None);
        assert!(!index.is_empty());
        for s in &strokes {
            assert_same(
                &tessellate_stroke_indexed(s, &index),
                &tessellate_stroke_canvas(s, &erasers, None),
            );
        }
    }

    #[test]
    fn empty_index_takes_no_subtraction_path() {
        // 空索引等价于「无橡皮擦」
        let s = draw_stroke(1, 40);
        let index = build_eraser_index(&[], None);
        assert!(index.is_empty());
        assert_same(
            &tessellate_stroke_indexed(&s, &index),
            &tessellate_stroke_canvas(&s, &[], None),
        );
    }

    #[test]
    fn index_covers_current_eraser_too() {
        // current_eraser 也参与建索引（实时橡皮擦预览走这条路）
        let s = draw_stroke(1, 40);
        let mut live = DrawStroke::new_eraser(60.0, 999);
        live.add_point(30.0, -10.0, 30.0, 120.0);
        let index = build_eraser_index(&[], Some(&live));
        assert!(!index.is_empty());
        assert_same(
            &tessellate_stroke_indexed(&s, &index),
            &tessellate_stroke_canvas(&s, &[], Some(&live)),
        );
    }

    #[test]
    fn indexed_ignores_erase_strokes_and_empty() {
        let mut eraser = DrawStroke::new_eraser(20.0, 5);
        eraser.add_point(0.0, 0.0, 5.0, 5.0);
        let index = build_eraser_index(&[eraser.clone()], None);
        assert!(
            tessellate_stroke_indexed(&eraser, &index).is_none(),
            "橡皮擦笔画本身不产生几何"
        );
        let mut empty = draw_stroke(2, 0);
        empty.points.clear();
        assert!(tessellate_stroke_indexed(&empty, &index).is_none());
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::ink::stroke::DrawStroke;

    fn make_stroke(n: usize) -> DrawStroke {
        let mut stroke = DrawStroke::new("#ff0000".to_string(), 12.0, 1, 0.4, 30.0);
        let mut x = 0.0f32;
        let mut y = 0.0f32;
        for i in 0..n {
            let nx = x + 6.0 + (i as f32 * 0.7).sin() * 3.0;
            let ny = y + 4.0 + (i as f32 * 0.5).cos() * 2.0;
            stroke.add_point(x, y, nx, ny);
            x = nx;
            y = ny;
        }
        stroke
    }

    const LIVE_BENCH_BATCH: usize = 16;

    fn bench(stroke: &DrawStroke) -> f64 {
        for _ in 0..5 {
            let _ = tessellate_stroke_canvas(stroke, &[], None);
        }
        let mut total = std::time::Duration::ZERO;
        let n = 60;
        for _ in 0..n {
            let t = std::time::Instant::now();
            let g = tessellate_stroke_canvas(stroke, &[], None);
            total += t.elapsed();
            assert!(g.is_some());
        }
        total.as_secs_f64() * 1000.0 / n as f64
    }

    #[test]
    fn bench_tessellate() {
        for &pts in &[10usize, 30, 60, 100, 200, 300, 500] {
            let s = make_stroke(pts);
            println!("{pts}pts avg {:.3}ms", bench(&s));
        }
    }

    #[test]
    fn bench_live_tail() {
        // 增量尾部（≤一批+笔尖）成本应随笔画长度增长保持近似恒定
        for &pts in &[10usize, 30, 60, 100, 200, 300, 500] {
            let s = make_stroke(pts);
            let path = build_path(&s);
            let batch_start = pts.saturating_sub(LIVE_BENCH_BATCH);
            let mut total = std::time::Duration::ZERO;
            let n = 200;
            for _ in 0..n {
                let t = std::time::Instant::now();
                let g = tessellate_live_range(&s, &path, batch_start, pts - 1);
                total += t.elapsed();
                assert!(g.is_some());
            }
            println!(
                "{pts}pts tail[{batch_start}..{}] avg {:.4}ms",
                pts - 1,
                total.as_secs_f64() * 1000.0 / n as f64
            );
        }
    }

    /// 逐采样（未合并）几何的测试基准。
    ///
    /// 不能用 `tessellate_live_range` / `tessellate_live_range_into`——第五轮
    /// 起它们走**合并**发射（满批块），已不再是"密集"基准。未合并的密集
    /// 几何只有尾部路径 `tessellate_live_range_segs` 还在产出。
    fn dense_geometry(stroke: &DrawStroke, path: &[(f32, f32, f32)]) -> Vec<StrokeSeg> {
        let last = stroke.points.len() - 1;
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        assert!(
            tessellate_live_range_segs(stroke, path, 0, last, &mut scratch, &mut out),
            "密集基准镶嵌应产出几何"
        );
        out
    }

    #[test]
    fn bench_merge_ratio() {
        // 自适应合并的段数压缩比：合并后（静态/满批块）vs 逐采样密集（尾部块）
        for &pts in &[60usize, 200, 500] {
            let s = make_stroke(pts);
            let path = build_path(&s);
            let dense = dense_geometry(&s, &path);
            let merged = tessellate_stroke_canvas(&s, &[], None).unwrap();
            let chunk = tessellate_live_range(&s, &path, 0, pts - 1).unwrap();
            println!(
                "{pts}pts: dense {} → 静态合并 {} ({:.1}×) / 满批块合并 {} ({:.1}×)",
                dense.len(),
                merged.segs.len(),
                dense.len() as f64 / merged.segs.len() as f64,
                chunk.segs.len(),
                dense.len() as f64 / chunk.segs.len() as f64
            );
            assert_eq!(
                chunk.segs, merged.segs,
                "满批块与静态镶嵌应逐段一致（同一合并实现）"
            );
        }
    }

    #[test]
    fn static_merged_covers_live_polyline() {
        // 自适应合并后的静态几何与逐采样密集几何的等效性：
        // 1) 合并段数 ≤ 密集段数（性能目标）；
        // 2) 包围盒一致（±0.6px）；
        // 3) 全部平滑采样点的密集圆盘落在某合并胶囊的容差邻域内（效果保证）。
        // （直播路径自身的增量一致性由 live_range_batch_consistency 覆盖）
        for &pts in &[2usize, 5, 17, 40, 100, 300] {
            let s = make_stroke(pts);
            let path = build_path(&s);
            let merged = tessellate_stroke_canvas(&s, &[], None).expect("merged");
            let dense = dense_geometry(&s, &path);

            assert!(
                merged.segs.len() <= dense.len(),
                "{pts}pts: 合并段数 {} 应 ≤ 密集段数 {}",
                merged.segs.len(),
                dense.len()
            );

            let (mb, db) = (
                merged.bounds(),
                crate::gpu::StrokeGeometry { segs: dense.clone() }.bounds(),
            );
            for k in 0..4 {
                assert!(
                    (mb[k] - db[k]).abs() < 0.6,
                    "{pts}pts: bounds[{k}] {} vs {}",
                    mb[k],
                    db[k]
                );
            }

            for &(x, y, w) in &build_smoothed_points(&s) {
                assert!(
                    point_covered(x, y, w, &merged.segs),
                    "{pts}pts: 采样点 ({x},{y},w={w}) 未被合并胶囊覆盖"
                );
            }
        }
    }

    /// 采样点 (x,y) 处半径 w/2 的密集圆盘是否落在某合并胶囊的容差邻域内：
    /// dist(p, 胶囊轴) + w/2 − 胶囊半宽 ≤ 弦偏差容差 + 半宽误差容差。
    fn point_covered(x: f32, y: f32, w: f32, segs: &[StrokeSeg]) -> bool {
        segs.iter().any(|seg| {
            let (ax, ay, ahw) = (seg.a[0], seg.a[1], seg.a[2]);
            let (bx, by, bhw) = (seg.b[0], seg.b[1], seg.b[2]);
            let abx = bx - ax;
            let aby = by - ay;
            let len_sq = abx * abx + aby * aby;
            let t = if len_sq > 1e-12 {
                (((x - ax) * abx + (y - ay) * aby) / len_sq).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let cx = ax + abx * t;
            let cy = ay + aby * t;
            let dist = ((x - cx) * (x - cx) + (y - cy) * (y - cy)).sqrt();
            let hw = ahw * (1.0 - t) + bhw * t;
            dist + w / 2.0 - hw <= MERGE_DEV_TOL + MERGE_WIDTH_TOL / 2.0 + 1e-3
        })
    }

    #[test]
    fn merge_collapses_straight_runs() {
        // 直行等宽笔画：合并后段数远少于采样点数（直行合并率受
        // MERGE_MAX_RUN 钳制，约 采样数/31）
        let mut s = DrawStroke::new("#0000ff".to_string(), 10.0, 1, 0.4, 30.0);
        for i in 0..100usize {
            let x = i as f32 * 6.0;
            s.add_point(x, 0.0, x + 6.0, 0.0);
        }
        let g = tessellate_stroke_canvas(&s, &[], None).expect("straight");
        assert!(
            g.segs.len() <= 8,
            "100 点直行笔画应合并到 ≤8 段，实际 {}",
            g.segs.len()
        );
        // 两端位置精确（起收笔无位移）
        assert!((g.segs[0].a[0] - 0.0).abs() < 1e-4);
        let last = g.segs.last().unwrap();
        assert!((last.b[0] - 600.0).abs() < 1e-4);
    }

    #[test]
    fn merge_respects_corners() {
        // L 形拐角：拐点两侧不得并成一段（弦偏差超限强制断链）
        let mut s = DrawStroke::new("#00ff00".to_string(), 10.0, 1, 0.4, 30.0);
        for i in 0..40usize {
            let x = i as f32 * 6.0;
            s.add_point(x, 0.0, x + 6.0, 0.0);
        }
        for i in 0..40usize {
            let y = i as f32 * 6.0;
            s.add_point(240.0, y, 240.0, y + 6.0);
        }
        let g = tessellate_stroke_canvas(&s, &[], None).expect("L");
        assert!(g.segs.len() >= 2, "拐角必须断链，实际 {} 段", g.segs.len());
        // 全部采样点仍被覆盖（拐点不断笔）
        for &(x, y, w) in &build_smoothed_points(&s) {
            assert!(point_covered(x, y, w, &g.segs), "拐角点 ({x},{y}) 未覆盖");
        }
    }

    #[test]
    fn merge_respects_width_taper() {
        // 强笔锋渐变（宽度快速衰减）不得合并掉宽度细节：端点半宽与
        // 原始宽度链一致
        let mut s = DrawStroke::new("#ff0000".to_string(), 16.0, 1, 0.4, 30.0);
        for i in 0..30usize {
            let x = i as f32 * 8.0;
            s.add_point(x, 0.0, x + 8.0, 0.0);
            // 16 → 2 线性衰减
            s.variable_widths.push(16.0 - (i as f32) * 14.0 / 29.0);
        }
        let g = tessellate_stroke_canvas(&s, &[], None).expect("taper");
        assert!(!g.segs.is_empty());
        let first = &g.segs[0];
        assert!(
            (first.a[2] - 8.0).abs() < MERGE_WIDTH_TOL / 2.0 + 1e-3,
            "首端半宽 {} 应 ≈ 8.0",
            first.a[2]
        );
        for &(x, y, w) in &build_smoothed_points(&s) {
            assert!(point_covered(x, y, w, &g.segs), "笔锋点 ({x},{y}) 未覆盖");
        }
    }

    #[test]
    fn eraser_cut_breaks_merge_chain() {
        // 横线被竖向橡皮擦切出缺口：合并不得桥接擦除间隙
        let mut s = DrawStroke::new("#ff0000".to_string(), 10.0, 1, 0.4, 30.0);
        for i in 0..60usize {
            let x = i as f32 * 6.0;
            s.add_point(x, 0.0, x + 6.0, 0.0);
        }
        let mut es = DrawStroke::new_eraser(60.0, 2);
        es.add_point(210.0, -40.0, 210.0, 40.0);
        let g = tessellate_stroke_canvas(&s, &[es], None).expect("cut");
        assert!(g.segs.len() >= 2, "切口两侧应各有内容");
        // 缺口中心 (210, 0) 不应被任何胶囊轴覆盖（橡皮擦半宽 30，边界 ~35）
        for seg in &g.segs {
            let d = (point_axis_distance(210.0, 0.0, seg)).abs();
            assert!(
                d > 25.0,
                "合并桥接了擦除缺口：段轴距缺口中心仅 {d}px"
            );
        }
    }

    /// 点到胶囊轴段（中心线）的距离。
    fn point_axis_distance(x: f32, y: f32, seg: &StrokeSeg) -> f32 {
        let (ax, ay) = (seg.a[0], seg.a[1]);
        let (bx, by) = (seg.b[0], seg.b[1]);
        point_segment_distance_sq(x, y, ax, ay, bx, by).sqrt()
    }

    /// **未合并**（尾部块）路径必须区间可分解：`[0..15] ++ [16..36]` 与
    /// `[0..36]` 逐段一致。
    ///
    /// 这是尾部块每追加一点就整体重建时依赖的性质：段与段一一对应，批边界
    /// 上不重不漏。逐采样发射天然满足（每个采样点产一段，与区间无关）。
    #[test]
    fn unmerged_live_range_is_range_decomposable() {
        let s = make_stroke(37);
        let path = build_path(&s);
        let mut scratch = Vec::new();

        let mut tess = |first: usize, last: usize, out: &mut Vec<StrokeSeg>| {
            assert!(tessellate_live_range_segs(
                &s, &path, first, last, &mut scratch, out
            ));
        };

        let mut full = Vec::new();
        tess(0, 36, &mut full);
        let mut stable = Vec::new();
        tess(0, 15, &mut stable);
        let mut head = Vec::new();
        tess(16, 36, &mut head);

        let mut cat = stable.clone();
        cat.extend_from_slice(&head);
        assert_eq!(full.len(), cat.len(), "seg count");
        for (i, (a, b)) in full.iter().zip(cat.iter()).enumerate() {
            let d = (a.a[0] - b.a[0])
                .abs()
                .max((a.a[1] - b.a[1]).abs())
                .max((a.b[0] - b.b[0]).abs())
                .max((a.b[1] - b.b[1]).abs())
                .max((a.a[2] - b.a[2]).abs())
                .max((a.b[2] - b.b[2]).abs());
            assert!(d < 1e-3, "seg[{i}] mismatch {d}");
        }
    }

    /// **合并**（满批块）路径**刻意不是**区间可分解的：合并 run 从区间起点
    /// 起算，跨边界的 run 会被切成两条。因此这里断言的是生产真正依赖的性质
    /// ——**拼接处几何连续**（无缝隙、无重叠），而不是"拼接 == 整条"。
    ///
    /// 之所以仍然安全：满批块的区间在生产中是固定推进的（[0..15]、[16..31]…），
    /// 一经镶嵌即定形并长期驻留，从不与相邻区间重新合并；尾部块走未合并路径，
    /// 保持区间可分解。
    #[test]
    fn merged_chunks_join_continuously_at_batch_boundary() {
        let s = make_stroke(37);
        let path = build_path(&s);
        let mut scratch = Vec::new();

        let stable = tessellate_live_range_into(&s, &path, 0, 15, &mut scratch).expect("稳定批");
        let mut head = Vec::new();
        assert!(tessellate_live_range_segs(
            &s, &path, 16, 36, &mut scratch, &mut head
        ));

        // 批边界的几何连续性：稳定批末点 == 头部首点
        // （CR 在 t=1 精确落在 path[16]，头部起点也是 path[16]）
        let last = stable.segs.last().expect("稳定批非空");
        let first = head.first().expect("头部非空");
        let d = (last.b[0] - first.a[0]).abs().max((last.b[1] - first.a[1]).abs());
        assert!(
            d < 1e-3,
            "批边界应连续：稳定批末点 ({}, {}) vs 头部首点 ({}, {})，偏差 {d}",
            last.b[0],
            last.b[1],
            first.a[0],
            first.a[1]
        );
        // 宽度也要接得上（同一采样点的宽度，两侧应一致）
        assert!(
            (last.b[2] - first.a[2]).abs() < 1e-3,
            "批边界宽度应连续：{} vs {}",
            last.b[2],
            first.a[2]
        );
        // 位置单调推进，不得回退（否则会出现自交重叠的胶囊）
        assert!(
            first.b[0].max(first.a[0]) > last.b[0].min(last.a[0]),
            "头部应继续向前，不得回退到稳定批内部"
        );
    }
}
