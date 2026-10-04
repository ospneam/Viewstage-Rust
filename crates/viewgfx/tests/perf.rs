//! 批注热路径性能基准（headless：不依赖 GPU、窗口与平台输入）。
//!
//! 运行：
//! ```text
//! cargo test --release -p viewgfx --test perf -- --nocapture
//! ```
//! 必须 `--release`——debug 下的绝对耗时无参考意义（本基准只比较相对值）。
//!
//! 每项优化都做 **A/B 同轮实测**：A = 优化前的实现路径，B = 优化后。
//! A/B 跑在同一进程、同一数据上，比值消除机器与负载差异的影响。

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use viewgfx::CanvasGfx;
use viewink::annotation::{cull_visible, view_to_canvas_rect, VisibleSet};
use viewink::gpu::{StrokeGeometry, StrokeSeg, ViewTransform};
use viewink::ink::mesh::{tessellate_live_range, tessellate_live_range_into};
use viewink::ink::stroke::DrawStroke;
use viewink::render::annotation::StaticStrokeRef;

// ---------------------------------------------------------------- 公共构造

/// 构造一条 `n` 点的笔画：沿 x 前进 `step` 画布像素，y 轻微摆动。
fn make_stroke(n: usize, step: f32, id: u64) -> DrawStroke {
    let mut s = DrawStroke::new("#ff0000".to_string(), 5.0, id, 0.4, 120.0);
    let (mut x, mut y) = (0.0f32, 0.0f32);
    for i in 0..n {
        let nx = x + step;
        let ny = y + (i as f32 * 0.7).sin() * 1.5;
        s.add_point(x, y, nx, ny);
        s.variable_widths.push(5.0);
        x = nx;
        y = ny;
    }
    s
}

/// 复刻 `build_path`：path[0] = points[0].from，path[k] = points[k-1].to。
fn build_path(stroke: &DrawStroke) -> Vec<(f32, f32, f32)> {
    let n = stroke.points.len();
    let mut path = Vec::with_capacity(n + 1);
    path.push((stroke.points[0].from_x, stroke.points[0].from_y, 5.0));
    for k in 1..=n {
        path.push((stroke.points[k - 1].to_x, stroke.points[k - 1].to_y, 5.0));
    }
    path
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

// ------------------------------------------------- 基准 1：镶嵌缓冲复用 A/B

/// 段区间重建的 A/B：每次堆分配 vs 复用调用方缓冲。
///
/// 注：生产环境的**尾部块**走 `tessellate_live_range_segs`（逐采样发射 +
/// 复用 `out`）；本基准 A/B 的是同族的 `tessellate_live_range{,_into}`
/// （第五轮起为满批块的合并发射路径）。两者共用 `live_range_samples` 采样
/// 阶段，缓冲复用带来的加速比结论对尾部块同样成立。
#[test]
fn bench_tessellate_buffer_reuse() {
    const N: usize = 256; // 笔画点数
    const TAIL: usize = 16; // 尾部段数（LIVE_STROKE_BATCH）
    const ITERS: u32 = 20_000;

    let stroke = make_stroke(N, 6.0, 1);
    let path = build_path(&stroke);
    let first = N - 1 - TAIL;

    // A：每次分配（优化前）
    let t0 = Instant::now();
    let mut a_segs = 0usize;
    for _ in 0..ITERS {
        if let Some(g) = tessellate_live_range(&stroke, &path, first, N - 1) {
            a_segs += black_box(g).segs.len();
        }
    }
    let a_ms = ms(t0.elapsed());

    // B：复用缓冲（优化后）
    let mut scratch = Vec::new();
    let t1 = Instant::now();
    let mut b_segs = 0usize;
    for _ in 0..ITERS {
        if let Some(g) = tessellate_live_range_into(&stroke, &path, first, N - 1, &mut scratch) {
            b_segs += black_box(g).segs.len();
        }
    }
    let b_ms = ms(t1.elapsed());

    assert_eq!(a_segs, b_segs, "两条路径必须产出完全相同的段数");
    println!("\n[镶嵌缓冲复用] 尾部 {TAIL} 段 × {ITERS} 次重建");
    println!("  A 每次分配      : {a_ms:8.2} ms");
    println!("  B 复用缓冲      : {b_ms:8.2} ms");
    println!("  加速比          : {:.2}x", a_ms / b_ms.max(1e-9));
    println!("  单次重建        : A {:.3} us / B {:.3} us",
        a_ms * 1000.0 / ITERS as f64, b_ms * 1000.0 / ITERS as f64);
}

// ------------------------------------------- 基准 2：镶嵌时机（每点 vs 每帧）

/// 模拟一段书写，返回 `(耗时, 最终总段数)`。
///
/// `per_event_rebuild` = 旧策略（每个输入事件后镶嵌一次），
/// `per_frame_rebuild` = 现策略（事件只追加点，镶嵌每帧一次）。
///
/// 两者产出的几何必须**完全一致**——每帧一次并没有丢任何东西，只是把
/// 同一帧内注定被丢弃的重建合并了。段数相等即证明这一点。
fn stroke_build_cost(
    input_hz: u32,
    frame_hz: u32,
    seconds: f32,
    speed_px_per_s: f32,
    per_event_rebuild: bool,
) -> (f64, usize) {
    let gfx = CanvasGfx::new(1.0);
    let mut s = DrawStroke::new("#ff0000".to_string(), 5.0, 1, 0.4, 120.0);
    s.add_point(0.0, 0.0, 0.0, 0.0);
    s.variable_widths.push(5.0);
    *gfx.live.current_stroke.borrow_mut() = Some(s);

    let events = (input_hz as f32 * seconds) as usize;
    let events_per_frame = ((input_hz as f32 / frame_hz as f32).ceil() as usize).max(1);
    let step = speed_px_per_s / input_hz as f32;

    let started = Instant::now();
    let mut x = 0.0f32;
    for i in 0..events {
        x += step;
        gfx.append_pen_point(x, (i as f32 * 0.05).sin() * 0.3);
        // A：每个事件后都镶嵌；B：只在帧边界镶嵌
        if per_event_rebuild || (i + 1) % events_per_frame == 0 {
            gfx.refresh_live_geometry();
        }
    }
    if !per_event_rebuild {
        gfx.refresh_live_geometry(); // 末帧不满：同样要镶嵌，否则尾部缺点
    }
    let elapsed = ms(started.elapsed());

    // 总段数 = 常驻满批块 + 尾层
    let chunks: usize = gfx.live.chunks.borrow().iter().map(|c| c.segs.len()).sum();
    let head = gfx.live.head.borrow().as_ref().map(|h| h.segs.len()).unwrap_or(0);
    (elapsed, chunks + head)
}

/// 镶嵌时机的 A/B：把"每输入点镶嵌"降为"每帧镶嵌"。
///
/// 收益不在绝对耗时，而在**它落在帧起点的关键路径上**：AutoVsync 下帧起点
/// 由 vsync 钉死，这段 CPU 必须在下一个死线前跑完，超了整帧多等一个周期。
/// 1000Hz 输入 + 144Hz 屏幕 = 每帧 ~7 个事件，旧策略白做 6 次尾部重建。
#[test]
fn bench_tessellation_timing() {
    println!("\n[镶嵌时机] 尾部重建：每输入事件 vs 每渲染帧（2s 书写）");
    println!("  {:<34} {:>10} {:>10} {:>8}", "场景", "A 每点(ms)", "B 每帧(ms)", "加速比");
    for (input_hz, frame_hz, speed, label) in [
        (1000u32, 144u32, 200.0f32, "1000Hz 鼠标 → 144Hz 屏"),
        (1000u32, 60u32, 200.0f32, "1000Hz 鼠标 → 60Hz 屏"),
        (240u32, 144u32, 200.0f32, "240Hz 触屏 → 144Hz 屏"),
        (240u32, 60u32, 400.0f32, "240Hz 触屏 → 60Hz 屏"),
    ] {
        let (a_ms, a_segs) = stroke_build_cost(input_hz, frame_hz, 2.0, speed, true);
        let (b_ms, b_segs) = stroke_build_cost(input_hz, frame_hz, 2.0, speed, false);
        assert_eq!(
            a_segs, b_segs,
            "{label}: 每帧一次镶嵌不得改变产出的几何（段数 {a_segs} vs {b_segs}）"
        );
        println!(
            "  {:<34} {:>10.2} {:>10.2} {:>7.2}x",
            label,
            a_ms,
            b_ms,
            a_ms / b_ms.max(1e-9)
        );
    }
}

// ------------------------------------------------------- 基准 3：采集抽稀

/// 模拟高回报率输入的采集抽稀：(输入事件数, 实际入笔画点数)。
///
/// 抽稀判据是"与**已保留的最后一点**的距离"，因此不会累积漂移：
/// 慢速书写时连续多个亚像素事件只保留跨过阈值的那些。
fn decimation_stats(hz: u32, speed_px_per_s: f32, seconds: f32) -> (usize, usize) {
    let gfx = CanvasGfx::new(1.0);
    let mut s = DrawStroke::new("#ff0000".to_string(), 5.0, 1, 0.4, 120.0);
    s.add_point(0.0, 0.0, 0.0, 0.0);
    s.variable_widths.push(5.0);
    *gfx.live.current_stroke.borrow_mut() = Some(s);

    let events = (hz as f32 * seconds) as usize;
    let step = speed_px_per_s / hz as f32;
    let mut x = 0.0f32;
    for i in 0..events {
        x += step;
        gfx.append_pen_point(x, (i as f32 * 0.05).sin() * 0.3);
    }
    let kept = gfx
        .live
        .current_stroke
        .borrow()
        .as_ref()
        .unwrap()
        .points
        .len();
    (events, kept)
}

#[test]
fn bench_pen_decimation() {
    println!("\n[采集抽稀] 阈值 0.35 屏幕像素（任意缩放不可辨）");
    println!("  {:<28} {:>10} {:>10} {:>10}", "场景", "输入事件", "入笔画点", "削减");
    let mut total_in = 0usize;
    let mut total_kept = 0usize;
    // 场景：设备回报率 × 手写速度
    for (hz, speed, label) in [
        (1000u32, 50.0f32, "1000Hz 慢写 50px/s"),
        (1000u32, 200.0f32, "1000Hz 常速 200px/s"),
        (240u32, 200.0f32, "240Hz 触屏 200px/s"),
        (125u32, 400.0f32, "125Hz 鼠标 400px/s"),
    ] {
        let (input, kept) = decimation_stats(hz, speed, 2.0);
        let cut = 100.0 * (1.0 - kept as f64 / input.max(1) as f64);
        println!("  {:<28} {:>10} {:>10} {:>9.1}%", label, input, kept, cut);
        total_in += input;
        total_kept += kept;
    }
    println!("  合计削减 {:.1}%（点数直接决定镶嵌/上传/绘制成本）",
        100.0 * (1.0 - total_kept as f64 / total_in.max(1) as f64));
}

// ------------------------------------------------------- 基准 3：视口剔除

/// 在 `extent × extent` 的画布上均匀铺 `count` 笔，统计"全量"与"可见"段数。
///
/// 驻留式静态层的剔除收益：全驻留 vs 本帧可见。
///
/// 静态层现在把**全部**笔画段常驻 GPU 缓冲（平移不再触发重传），每帧只按
/// 笔画 AABB 挑出该画的段区间去绘制。因此剔除的收益体现在**顶点处理量与
/// draw 批次数**上，而不是上传字节上——上传只随笔画集合变化。
///
/// 返回 `(总段数, 可见段数, 绘制区间数)`。
fn culling_stats(
    count: usize,
    extent: f32,
    viewport: (f32, f32),
    scale: f32,
) -> (usize, usize, usize) {
    let per_side = (count as f32).sqrt().ceil() as usize;
    let mut strokes: Vec<StaticStrokeRef> = Vec::with_capacity(count);
    for i in 0..count {
        let gx = i % per_side;
        let gy = i / per_side;
        let ox = gx as f32 * (extent / per_side as f32);
        let oy = gy as f32 * (extent / per_side as f32);
        let mut geom = StrokeGeometry::default();
        // 每笔 40 段（约 240px 长的短笔画）
        for k in 0..40 {
            geom.push(StrokeSeg::new(
                ox + k as f32 * 6.0,
                oy,
                ox + (k + 1) as f32 * 6.0,
                oy + 3.0,
                2.5,
                [1.0, 0.0, 0.0, 1.0],
            ));
        }
        strokes.push(StaticStrokeRef::new(i as u64, 0, Arc::new(geom)));
    }

    let total: usize = strokes.iter().map(|s| s.geometry.segs.len()).sum();

    let transform = ViewTransform {
        scale,
        offset_x: 0.0,
        offset_y: 0.0,
    };
    let view = view_to_canvas_rect(viewport, &transform).expect("视口有效");

    // 与运行时同一条路径：先按逐笔顺序累出驻留区间，再剔除
    let mut ranges = Vec::with_capacity(count);
    let mut off = 0u32;
    for s in &strokes {
        let n = s.geometry.segs.len() as u32;
        ranges.push((off, n));
        off += n;
    }
    let mut visible = VisibleSet::default();
    cull_visible(&ranges, &strokes, Some(&view), off, &mut visible);

    println!(
        "    可见笔画 {}/{}，可见段 {}/{}，绘制区间 {} 批",
        visible.strokes,
        count,
        visible.segs as usize,
        total,
        visible.ranges.len()
    );
    (total, visible.segs as usize, visible.ranges.len())
}

#[test]
fn bench_viewport_culling() {
    println!("\n[视口剔除] 视口 1920x1080（驻留式：全量常驻，仅剔除绘制）");
    let mut sum_total = 0usize;
    let mut sum_visible = 0usize;
    let mut sum_draws = 0usize;

    for (count, extent, scale, label) in [
        (1000usize, 20_000.0f32, 1.0f32, "1000 笔铺满 20000px 画布"),
        (5000usize, 40_000.0f32, 1.0f32, "5000 笔铺满 40000px 画布"),
        (5000usize, 40_000.0f32, 0.25f32, "5000 笔 + 缩放到 25%"),
        (10_000usize, 60_000.0f32, 1.0f32, "10000 笔超大画布"),
    ] {
        println!("  {label}：");
        let (total, visible, draws) = culling_stats(count, extent, (1920.0, 1080.0), scale);
        let cut = 100.0 * (1.0 - visible as f64 / total.max(1) as f64);
        println!("    静态层顶点处理量削减 {cut:.1}%");
        sum_total += total;
        sum_visible += visible;
        sum_draws += draws;
    }
    println!(
        "  合计：静态层每帧段数 {} → {}（削减 {:.1}%），绘制批次合计 {sum_draws}",
        sum_total,
        sum_visible,
        100.0 * (1.0 - sum_visible as f64 / sum_total.max(1) as f64)
    );
}

// --------------------------------------------- 基准 4：绘制批次与上传字节

/// 静态层的 draw 调用次数与 bind group 切换次数。
///
/// 历史脉络：最初按 128 笔分批（批数随笔数线性增长，每批一次 bind group），
/// 合并成单缓冲后恒为 1；改为驻留式 + 绘制期剔除后，批数等于**可见连续游程
/// 数**——相邻可见笔画仍会并成一批，只有交错可见（零散标记）才会拆开，且
/// 超过 `MAX_STATIC_DRAWS` 会退回单批。
#[test]
fn bench_static_draw_batches() {
    println!("\n[绘制批次] 静态层 draw / bind group 调用次数");
    println!("  {:<14} {:>14} {:>16} {:>12}", "笔画数", "旧分批(128)", "连续可见", "交错可见");
    for count in [128usize, 1000, 5000, 10_000] {
        let before = count.div_ceil(128); // 旧：每 128 笔一批，每批一次 draw + 一次 bind group
        println!("  {:<14} {:>14} {:>16} {:>12}", count, before, 1, "≤64");
    }
    println!("  注：bind group 切换恒为 1（单缓冲），draw 次数为剔除后的连续游程数。");
}

// ------------------------------------------------- 基准 5：增量镶嵌增量性

/// 连续追加 N 点的总耗时：验证"每点 O(1)"而非"每点 O(全长)"
/// （后者会让长笔画的书写成本随长度平方增长）。
#[test]
fn bench_incremental_scaling() {
    println!("\n[增量镶嵌] 连续追加点的累计耗时（验证非 O(N^2)）");
    let gfx = CanvasGfx::new(1.0);
    let mut s = DrawStroke::new("#ff0000".to_string(), 5.0, 1, 0.4, 120.0);
    s.add_point(0.0, 0.0, 0.0, 0.0);
    s.variable_widths.push(5.0);
    *gfx.live.current_stroke.borrow_mut() = Some(s);

    let mut x = 0.0f32;
    let mut last_report = Instant::now();
    let start = Instant::now();
    const TOTAL: usize = 3000;
    for i in 0..TOTAL {
        x += 4.0;
        gfx.append_pen_point(x, (i as f32 * 0.03).sin() * 40.0);
        gfx.refresh_live_geometry();
        if i % 1000 == 999 {
            println!("  前 {} 点累计 {:7.2} ms", i + 1, ms(last_report.elapsed()));
            last_report = Instant::now();
        }
    }
    let per_point_us = ms(start.elapsed()) * 1000.0 / TOTAL as f64;
    println!("  总计 {} 点，{:.2} ms，单点 {:.3} us", TOTAL, ms(start.elapsed()), per_point_us);
    println!("  每段千点耗时若近似持平 → 每点 O(1)，无 O(N^2) 退化。");
}
