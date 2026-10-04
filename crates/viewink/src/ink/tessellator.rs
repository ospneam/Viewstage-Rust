use crate::ink::stroke::StrokePoint;

const TAPER_SEGMENTS: usize = 4;
const MIN_START_RATIO: f32 = 0.2;

/// 笔锋算法核心：速度→宽度映射
/// 根据速度、基础宽度、上一段宽度和距离计算当前段宽度
/// 使用 smoothstep 缓动 + 时序混合 + 最大增量钳制
pub fn calc_pen_line_width(
    speed: f32,
    base_width: f32,
    last_line_width: f32,
    dist: f32,
    min_ratio: f32,
) -> f32 {
    let speed_scale = (base_width / 4.0).max(0.4).min(2.5);
    let max_speed = 2.5 * speed_scale;
    let min_speed = 0.2 * speed_scale;

    let line_width = if speed >= max_speed {
        base_width * min_ratio
    } else if speed <= min_speed {
        base_width
    } else {
        let ratio = (speed - min_speed) / (max_speed - min_speed);
        let eased = ratio * ratio * (3.0 - 2.0 * ratio);
        base_width - eased * (base_width * min_ratio)
    };

    let blend = (1.0 - dist / (base_width * 3.0)).max(0.3).min(0.85);
    let line_width = line_width * (1.0 - blend) + last_line_width * blend;

    let max_delta = base_width * 0.12;
    let line_width = line_width
        .max(last_line_width - max_delta)
        .min(last_line_width + max_delta);

    line_width.max(0.5)
}

/// 起笔渐变：前 TAPER_SEGMENTS(=4) 段从 base*0.2 开始平滑过渡到计算宽度
pub fn apply_start_taper(line_width: f32, base_width: f32, global_index: usize) -> f32 {
    if global_index < TAPER_SEGMENTS {
        let taper_t = (global_index + 1) as f32 / TAPER_SEGMENTS as f32;
        let eased = taper_t * taper_t * (3.0 - 2.0 * taper_t);
        let min_start = base_width * MIN_START_RATIO;
        min_start + (line_width - min_start) * eased
    } else {
        line_width
    }
}

/// 笔尾速度递减：根据真实段时序重新计算最后 tail_duration 毫秒的宽度
pub fn apply_speed_taper(
    widths: &mut [f32],
    points: &[StrokePoint],
    base_width: f32,
    segment_times: &[f32],
    min_ratio: f32,
    tail_duration: f32,
) {
    if points.len() < 2 || widths.len() != points.len() || segment_times.len() < points.len() {
        return;
    }
    let total_duration = segment_times.last().copied().unwrap_or(0.0);
    if total_duration <= 0.0 {
        return;
    }
    let tail_start_time = (total_duration - tail_duration).max(0.0);
    let mut tail_start = None;
    for (i, t) in segment_times.iter().enumerate() {
        if *t >= tail_start_time {
            tail_start = Some(i);
            break;
        }
    }
    let tail_start = match tail_start {
        Some(s) => s,
        None => return,
    };
    let tail_start = tail_start.max(TAPER_SEGMENTS);
    if tail_start >= widths.len() {
        return;
    }
    // 逐段速度必须与**书写时**（`viewgfx::live_geo` 的 `apply_live_tail_taper`）
    // 完全一致——都是逐段瞬时速度、不做平滑。两者若不同，抬笔那一帧 `stored_widths`
    // 一被激活（`path_width` 优先读它）笔尾宽度就会突变，即用户看到的"闪一下"。
    // 见 `live_tail_matches_final_broadcast` / `live_tail_after_start_taper_matches_finalize`。
    let mut last_width = if tail_start > 0 {
        widths[tail_start - 1]
    } else {
        base_width
    };
    for i in tail_start..widths.len() {
        let pt = &points[i];
        let dx = pt.to_x - pt.from_x;
        let dy = pt.to_y - pt.from_y;
        let dist = (dx * dx + dy * dy).sqrt();
        let prev_time = if i > 0 { segment_times[i - 1] } else { 0.0 };
        let seg_time = (segment_times[i] - prev_time).max(1.0);
        let speed = dist / seg_time;
        widths[i] = calc_pen_line_width(speed, base_width, last_width, dist, min_ratio);
        last_width = widths[i];
    }
}

/// 笔尾窗口起点：第一个落在最后 tail_duration 毫秒内的段的 index。
/// 从尾段倒推（O(尾段数)）。生产代码内联同逻辑；此函数供测试比对。
#[cfg(test)]
pub fn tail_start_index(segment_times: &[f32], tail_duration: f32) -> usize {
    let total_duration = segment_times.last().copied().unwrap_or(0.0);
    if total_duration <= 0.0 || segment_times.is_empty() {
        return 0;
    }
    let tail_start_time = (total_duration - tail_duration).max(0.0);
    for (i, t) in segment_times.iter().enumerate().rev() {
        if *t < tail_start_time {
            return (i + 1).max(TAPER_SEGMENTS);
        }
    }
    TAPER_SEGMENTS
}

/// 批注过程中实时笔锋：对最后 tail_duration 毫秒的笔尾做速度衰减。
/// 相比 apply_speed_taper（整条重扫），此处从尾端倒推仅重算笔尾若干段
/// （O(尾段数)），供每帧增量镶嵌复用，几何与最终笔锋一致。
pub fn apply_live_tail_taper(
    widths: &mut [f32],
    segments: &[StrokePoint],
    base_width: f32,
    segment_times: &[f32],
    min_ratio: f32,
    tail_duration: f32,
) {
    if segments.len() < 2 || widths.len() != segments.len() {
        return;
    }
    let total_duration = segment_times.last().copied().unwrap_or(0.0);
    if total_duration <= 0.0 {
        return;
    }
    let tail_start_time = (total_duration - tail_duration).max(0.0);
    // 倒推找到第一个落在笔尾窗口内的段（O(尾宽) 无需遍历整条）
    let tail_start = if let Some(s) = segment_times
        .iter()
        .enumerate()
        .rev()
        .find(|(_, t)| **t < tail_start_time)
    {
        s.0 + 1
    } else {
        0
    };
    let tail_start = tail_start.max(TAPER_SEGMENTS);
    if tail_start >= widths.len() {
        return;
    }
    let mut last_width = if tail_start > 0 {
        widths[tail_start - 1]
    } else {
        base_width
    };
    for i in tail_start..widths.len() {
        let pt = &segments[i];
        let dx = pt.to_x - pt.from_x;
        let dy = pt.to_y - pt.from_y;
        let dist = (dx * dx + dy * dy).sqrt();
        let prev_time = if i > 0 { segment_times[i - 1] } else { 0.0 };
        let seg_time = (segment_times[i] - prev_time).max(1.0);
        let speed = dist / seg_time;
        widths[i] = calc_pen_line_width(speed, base_width, last_width, dist, min_ratio);
        last_width = widths[i];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_segments(n: usize) -> Vec<StrokePoint> {
        let mut pts = Vec::with_capacity(n);
        let mut x = 0.0f32;
        let mut y = 0.0f32;
        for i in 0..n {
            let nx = x + 6.0 + (i as f32 * 0.7).sin() * 3.0;
            let ny = y + 4.0 + (i as f32 * 0.5).cos() * 2.0;
            pts.push(StrokePoint {
                from_x: x,
                from_y: y,
                to_x: nx,
                to_y: ny,
            });
            x = nx;
            y = ny;
        }
        pts
    }

    fn times(n: usize) -> Vec<f32> {
        // 每段 8ms，总时长逐渐累积
        let mut v = Vec::with_capacity(n);
        let mut t = 8.0f32;
        for _ in 0..n {
            v.push(t);
            t += 8.0;
        }
        v
    }

    #[test]
    fn live_tail_matches_final_broadcast() {
        // 实时笔锋（apply_live_tail_taper）应与整条笔锋（apply_speed_taper）在笔尾窗口内逐段一致
        let n = 60usize;
        let segments = make_segments(n);
        let times = times(n);
        let base_width = 12.0f32;
        let min_ratio = 0.4f32;
        let tail_duration = 50.0f32;

        // 复刻 live 路径：先填 base 宽度，再对最后的窗口实时衰减
        let mut live_w = vec![base_width; n];
        apply_live_tail_taper(
            &mut live_w,
            &segments,
            base_width,
            &times,
            min_ratio,
            tail_duration,
        );

        // 复刻 finalize 路径：整笔完整衰减
        let mut final_w = vec![base_width; n];
        apply_speed_taper(
            &mut final_w,
            &segments,
            base_width,
            &times,
            min_ratio,
            tail_duration,
        );

        // 尾部窗口之外保持不变
        let tail = tail_start_index(&times, tail_duration);
        for i in 0..tail {
            assert_eq!(live_w[i], base_width, "非尾部段应保持 base，i={i}");
        }
        // 尾部窗口内与整笔衰减逐点一致
        for i in tail..n {
            let d = (live_w[i] - final_w[i]).abs();
            assert!(
                d < 1e-4,
                "尾部段宽度不一致 i={i}: live={} final={}",
                live_w[i],
                final_w[i]
            );
        }
    }

    #[test]
    fn tail_window_agrees_with_broadcast() {
        // tail_start_index 与 apply_speed_taper 内部推出的窗口起点一致
        let n = 20usize;
        let segments = make_segments(n);
        let times = times(n);
        let mut w = vec![1.0f32; n];
        apply_speed_taper(&mut w, &segments, 12.0, &times, 0.4, 60.0);

        // 尾部窗口内宽度应偏离 base（被衰减过）；窗口外等于 base(1.0)
        let tail = tail_start_index(&times, 60.0);
        for i in 0..tail {
            assert_eq!(w[i], 1.0, "窗口外不应被触碰 i={i}");
        }
        assert!(tail < n, "窗口应落在尾段 tail={tail} n={n}");
    }

    #[test]
    fn live_tail_after_start_taper_matches_finalize() {
        // 真实批注场景：前4段有起笔渐变，再对笔尾做速度衰减 → 实时与最终一致
        let n = 30usize;
        let segs = make_segments(n);
        let times = times(n);
        let base = 12.0f32;
        let min_ratio = 0.4f32;
        let tail_dur = 50.0f32;

        // 模拟直播中逐点计算 variable_widths（含起笔渐变）
        let mut base_w = vec![base; n];
        for i in 0..n {
            let dx = segs[i].to_x - segs[i].from_x;
            let dy = segs[i].to_y - segs[i].from_y;
            let dist = (dx * dx + dy * dy).sqrt();
            let speed = dist / 8.0; // times 增量 8ms
            base_w[i] = calc_pen_line_width(
                speed,
                base,
                if i > 0 { base_w[i - 1] } else { base },
                dist,
                min_ratio,
            );
            base_w[i] = apply_start_taper(base_w[i], base, i);
        }

        let mut live = base_w.clone();
        apply_live_tail_taper(&mut live, &segs, base, &times, min_ratio, tail_dur);

        let mut fin = base_w.clone();
        apply_speed_taper(&mut fin, &segs, base, &times, min_ratio, tail_dur);

        for i in 0..n {
            assert_eq!(live[i], fin[i], "i={i}: live={} final={}", live[i], fin[i]);
        }
    }
}
