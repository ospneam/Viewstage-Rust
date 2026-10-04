//! SVG path data（`d` 属性）解析。
//!
//! 覆盖完整命令集 `M m L l H h V v C c S s Q q T t A a Z z`（含隐式命令重复、
//! 全部数字字面格式），并把椭圆弧从端点参数化转换为中心参数化后展开为
//! 圆心角 ≤ 90° 的三次贝塞尔段（SVG 规范附录 F.6）。

use crate::Segment;

/// path data 解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// 命令参数意外结束
    UnexpectedEof,
    /// 未知命令字符
    UnknownCommand(char),
    /// 数字字面量非法
    BadNumber,
    /// 弧命令的 large-arc / sweep 标志不是 0 或 1
    BadArcFlag,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ParseError::UnexpectedEof => write!(f, "命令参数意外结束"),
            ParseError::UnknownCommand(c) => write!(f, "未知命令字符 {c:?}"),
            ParseError::BadNumber => write!(f, "数字字面量非法"),
            ParseError::BadArcFlag => write!(f, "弧标志必须是 0 或 1"),
        }
    }
}

/// 解析 path data 字符串为展平路径段。
pub fn parse_path_data(input: &str) -> Result<Vec<Segment>, ParseError> {
    let mut c = Cursor::new(input);
    let mut segments = Vec::new();

    let mut cx = 0.0f32; // 当前点
    let mut cy = 0.0f32;
    let mut sx = 0.0f32; // 当前子路径起点
    let mut sy = 0.0f32;
    let mut prev_c2: Option<(f32, f32)> = None; // 上一 C/S 的第二控制点（S 反射用）
    let mut prev_ctrl: Option<(f32, f32)> = None; // 上一 Q/T 的控制点（T 反射用）
    let mut cmd = 0u8;

    loop {
        if let Some(letter) = c.command() {
            cmd = letter;
        } else if c.i >= c.s.len() {
            break; // 干净结束
        } else if cmd == 0 {
            return Err(ParseError::BadNumber); // 数据以数字开头，无前置命令
        } else if cmd == b'Z' || cmd == b'z' {
            return Err(ParseError::BadNumber); // Z/z 不接受隐式重复参数
        }
        // 其余情况：隐式重复上一命令（各分支内部按参数个数消费）

        match cmd {
            b'M' | b'm' => {
                let rel = cmd == b'm';
                let mut first = true;
                while c.at_number() {
                    let x = c.number()?;
                    let y = c.number()?;
                    let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                    if first {
                        segments.push(Segment::MoveTo(nx, ny));
                        sx = nx;
                        sy = ny;
                        first = false;
                    } else {
                        // M/m 的隐式后续是 L/l
                        segments.push(Segment::LineTo(nx, ny));
                    }
                    cx = nx;
                    cy = ny;
                }
                if first {
                    return Err(ParseError::UnexpectedEof);
                }
                prev_c2 = None;
                prev_ctrl = None;
            }
            b'L' | b'l' => {
                let rel = cmd == b'l';
                let mut any = false;
                while c.at_number() {
                    let x = c.number()?;
                    let y = c.number()?;
                    let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                    segments.push(Segment::LineTo(nx, ny));
                    cx = nx;
                    cy = ny;
                    any = true;
                }
                if !any {
                    return Err(ParseError::UnexpectedEof);
                }
                prev_c2 = None;
                prev_ctrl = None;
            }
            b'H' | b'h' => {
                let rel = cmd == b'h';
                let mut any = false;
                while c.at_number() {
                    let x = c.number()?;
                    let nx = if rel { cx + x } else { x };
                    segments.push(Segment::LineTo(nx, cy));
                    cx = nx;
                    any = true;
                }
                if !any {
                    return Err(ParseError::UnexpectedEof);
                }
                prev_c2 = None;
                prev_ctrl = None;
            }
            b'V' | b'v' => {
                let rel = cmd == b'v';
                let mut any = false;
                while c.at_number() {
                    let y = c.number()?;
                    let ny = if rel { cy + y } else { y };
                    segments.push(Segment::LineTo(cx, ny));
                    cy = ny;
                    any = true;
                }
                if !any {
                    return Err(ParseError::UnexpectedEof);
                }
                prev_c2 = None;
                prev_ctrl = None;
            }
            b'C' | b'c' => {
                let rel = cmd == b'c';
                let mut any = false;
                while c.at_number() {
                    let x1 = c.number()?;
                    let y1 = c.number()?;
                    let x2 = c.number()?;
                    let y2 = c.number()?;
                    let x = c.number()?;
                    let y = c.number()?;
                    let (a1x, a1y) = if rel { (cx + x1, cy + y1) } else { (x1, y1) };
                    let (a2x, a2y) = if rel { (cx + x2, cy + y2) } else { (x2, y2) };
                    let (ex, ey) = if rel { (cx + x, cy + y) } else { (x, y) };
                    segments.push(Segment::CubicTo(a1x, a1y, a2x, a2y, ex, ey));
                    cx = ex;
                    cy = ey;
                    prev_c2 = Some((a2x, a2y));
                    any = true;
                }
                if !any {
                    return Err(ParseError::UnexpectedEof);
                }
                prev_ctrl = None;
            }
            b'S' | b's' => {
                let rel = cmd == b's';
                let mut any = false;
                while c.at_number() {
                    let x2 = c.number()?;
                    let y2 = c.number()?;
                    let x = c.number()?;
                    let y = c.number()?;
                    let (c1x, c1y) = reflect(prev_c2, cx, cy);
                    let (a2x, a2y) = if rel { (cx + x2, cy + y2) } else { (x2, y2) };
                    let (ex, ey) = if rel { (cx + x, cy + y) } else { (x, y) };
                    segments.push(Segment::CubicTo(c1x, c1y, a2x, a2y, ex, ey));
                    cx = ex;
                    cy = ey;
                    prev_c2 = Some((a2x, a2y));
                    any = true;
                }
                if !any {
                    return Err(ParseError::UnexpectedEof);
                }
                prev_ctrl = None;
            }
            b'Q' | b'q' => {
                let rel = cmd == b'q';
                let mut any = false;
                while c.at_number() {
                    let x1 = c.number()?;
                    let y1 = c.number()?;
                    let x = c.number()?;
                    let y = c.number()?;
                    let (qx, qy) = if rel { (cx + x1, cy + y1) } else { (x1, y1) };
                    let (ex, ey) = if rel { (cx + x, cy + y) } else { (x, y) };
                    segments.push(Segment::QuadraticTo(qx, qy, ex, ey));
                    cx = ex;
                    cy = ey;
                    prev_ctrl = Some((qx, qy));
                    any = true;
                }
                if !any {
                    return Err(ParseError::UnexpectedEof);
                }
                prev_c2 = None;
            }
            b'T' | b't' => {
                let rel = cmd == b't';
                let mut any = false;
                while c.at_number() {
                    let x = c.number()?;
                    let y = c.number()?;
                    let (tx, ty) = reflect(prev_ctrl, cx, cy);
                    let (ex, ey) = if rel { (cx + x, cy + y) } else { (x, y) };
                    segments.push(Segment::QuadraticTo(tx, ty, ex, ey));
                    cx = ex;
                    cy = ey;
                    prev_ctrl = Some((tx, ty));
                    any = true;
                }
                if !any {
                    return Err(ParseError::UnexpectedEof);
                }
                prev_c2 = None;
            }
            b'A' | b'a' => {
                let rel = cmd == b'a';
                let mut any = false;
                while c.at_number() {
                    let rx = c.number()?;
                    let ry = c.number()?;
                    let rot = c.number()?; // x-axis-rotation，度
                    let large = c.flag()?;
                    let sweep = c.flag()?;
                    let x = c.number()?;
                    let y = c.number()?;
                    let (ex, ey) = if rel { (cx + x, cy + y) } else { (x, y) };
                    arc_to_cubics(cx, cy, rx, ry, rot, large, sweep, ex, ey, &mut |seg| {
                        segments.push(seg)
                    });
                    cx = ex;
                    cy = ey;
                    any = true;
                }
                if !any {
                    return Err(ParseError::UnexpectedEof);
                }
                prev_c2 = None;
                prev_ctrl = None;
            }
            b'Z' | b'z' => {
                segments.push(Segment::Close);
                cx = sx;
                cy = sy;
                prev_c2 = None;
                prev_ctrl = None;
            }
            _ => {
                let ch = char::from_u32(cmd as u32).unwrap_or('?');
                return Err(ParseError::UnknownCommand(ch));
            }
        }
    }

    Ok(segments)
}

/// S/T 命令的反射控制点：取上一控制点关于当前点的对称点；无上一控制点则为当前点。
fn reflect(prev: Option<(f32, f32)>, cx: f32, cy: f32) -> (f32, f32) {
    match prev {
        Some((px, py)) => (2.0 * cx - px, 2.0 * cy - py),
        None => (cx, cy),
    }
}

/// 椭圆弧端点参数化 → 三次贝塞尔段（SVG 规范附录 F.6）。
///
/// - 端点重合：弧被跳过（规范行为）；
/// - 半径为 0：退化为直线；
/// - 端点距离超出直径：半径按规范放大；
/// - 每段圆心角 ≤ 90°，kappa = 4/3·tan(δ/4)，相对误差 ~2.7e-4（对 24px 图标亚像素）。
fn arc_to_cubics(
    x1: f32,
    y1: f32,
    rx: f32,
    ry: f32,
    rot_deg: f32,
    large: bool,
    sweep: bool,
    x2: f32,
    y2: f32,
    out: &mut dyn FnMut(Segment),
) {
    // 端点重合 → 无弧
    if (x1 - x2).abs() < f32::EPSILON && (y1 - y2).abs() < f32::EPSILON {
        return;
    }
    let (rx, ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 {
        out(Segment::LineTo(x2, y2));
        return;
    }

    let phi = rot_deg.to_radians();
    let (sin_phi, cos_phi) = phi.sin_cos();

    // F.6.5.1 步骤 2：中点变换到未旋转坐标系
    let dx2 = (x1 - x2) / 2.0;
    let dy2 = (y1 - y2) / 2.0;
    let x1p = cos_phi * dx2 + sin_phi * dy2;
    let y1p = -sin_phi * dx2 + cos_phi * dy2;

    // F.6.5.1 步骤 3 前置：半径过小则放大
    let rx2 = rx * rx;
    let ry2 = ry * ry;
    let x1p2 = x1p * x1p;
    let y1p2 = y1p * y1p;
    let lambda = x1p2 / rx2 + y1p2 / ry2;
    let (rx, ry, rx2, ry2) = if lambda > 1.0 {
        let s = lambda.sqrt();
        let rx = rx * s;
        let ry = ry * s;
        (rx, ry, rx * rx, ry * ry)
    } else {
        (rx, ry, rx2, ry2)
    };

    // F.6.5.1 步骤 3：中心点
    let sign = if large != sweep { 1.0 } else { -1.0 };
    let num = rx2 * ry2 - rx2 * y1p2 - ry2 * x1p2;
    let den = rx2 * y1p2 + ry2 * x1p2;
    let co = if den == 0.0 {
        0.0
    } else {
        sign * (num / den).max(0.0).sqrt()
    };
    let cxp = co * rx * y1p / ry;
    let cyp = -co * ry * x1p / rx;
    let cx = cxp * cos_phi - cyp * sin_phi + (x1 + x2) / 2.0;
    let cy = cxp * sin_phi + cyp * cos_phi + (y1 + y2) / 2.0;

    // F.6.5.2 步骤 4：起止角与扫过角（atan2 形式避免 acos 数值域问题）
    let theta1 = ((y1p - cyp) / ry).atan2((x1p - cxp) / rx);
    let theta2 = ((-y1p - cyp) / ry).atan2((-x1p - cxp) / rx);
    let mut dtheta = theta2 - theta1;
    if !sweep && dtheta > 0.0 {
        dtheta -= core::f32::consts::TAU;
    } else if sweep && dtheta < 0.0 {
        dtheta += core::f32::consts::TAU;
    }

    // 展开：每段 ≤ 90°
    let n = ((dtheta.abs() / (core::f32::consts::FRAC_PI_2)).ceil() as usize).max(1);
    let delta = dtheta / n as f32;
    let k = (4.0 / 3.0) * (delta / 4.0).tan();

    let mut t = theta1;
    for _ in 0..n {
        let t_next = t + delta;
        let (p0, d0) = arc_eval(cx, cy, rx, ry, cos_phi, sin_phi, t);
        let (p1, d1) = arc_eval(cx, cy, rx, ry, cos_phi, sin_phi, t_next);
        out(Segment::CubicTo(
            p0.0 + k * d0.0,
            p0.1 + k * d0.1,
            p1.0 - k * d1.0,
            p1.1 - k * d1.1,
            p1.0,
            p1.1,
        ));
        t = t_next;
    }
}

/// 椭圆弧在角度 `t` 处的点与切向量（已叠加 x 轴旋转）。
fn arc_eval(
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    cos_phi: f32,
    sin_phi: f32,
    t: f32,
) -> ((f32, f32), (f32, f32)) {
    let (st, ct) = t.sin_cos();
    let ex = rx * ct;
    let ey = ry * st;
    let px = cx + cos_phi * ex - sin_phi * ey;
    let py = cy + sin_phi * ex + cos_phi * ey;
    // 切向 = 参数化导数
    let dx = -rx * st;
    let dy = ry * ct;
    let tx = cos_phi * dx - sin_phi * dy;
    let ty = sin_phi * dx + cos_phi * dy;
    ((px, py), (tx, ty))
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            s: s.as_bytes(),
            i: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.i < self.s.len() {
            match self.s[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c' | b',' => self.i += 1,
                _ => break,
            }
        }
    }

    /// 消耗一个命令字母（若下一 token 是字母）。
    fn command(&mut self) -> Option<u8> {
        self.skip_ws();
        let b = *self.s.get(self.i)?;
        if b.is_ascii_alphabetic() {
            self.i += 1;
            Some(b)
        } else {
            None
        }
    }

    /// 下一 token 是否为数字起始（digit / '.' / '+' / '-'；无命令字母以 +/- 开头，安全）。
    fn at_number(&self) -> bool {
        let mut probe = Cursor { s: self.s, i: self.i };
        probe.skip_ws();
        match probe.s.get(probe.i) {
            Some(&b) => b.is_ascii_digit() || b == b'.' || b == b'+' || b == b'-',
            None => false,
        }
    }

    /// 解析一个数字：`[+-]?(\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?`
    fn number(&mut self) -> Result<f32, ParseError> {
        self.skip_ws();
        let start = self.i;
        let mut j = self.i;
        if j < self.s.len() && (self.s[j] == b'+' || self.s[j] == b'-') {
            j += 1;
        }
        let mut digits = 0;
        while j < self.s.len() && self.s[j].is_ascii_digit() {
            j += 1;
            digits += 1;
        }
        if j < self.s.len() && self.s[j] == b'.' {
            j += 1;
            while j < self.s.len() && self.s[j].is_ascii_digit() {
                j += 1;
                digits += 1;
            }
        }
        if digits == 0 {
            return Err(ParseError::BadNumber);
        }
        if j < self.s.len() && (self.s[j] == b'e' || self.s[j] == b'E') {
            let mut k = j + 1;
            if k < self.s.len() && (self.s[k] == b'+' || self.s[k] == b'-') {
                k += 1;
            }
            let mut exp_digits = 0;
            while k < self.s.len() && self.s[k].is_ascii_digit() {
                k += 1;
                exp_digits += 1;
            }
            if exp_digits > 0 {
                j = k;
            }
        }
        let text = core::str::from_utf8(&self.s[start..j]).map_err(|_| ParseError::BadNumber)?;
        let value = text.parse::<f32>().map_err(|_| ParseError::BadNumber)?;
        self.i = j;
        Ok(value)
    }

    /// 弧命令标志：单个 '0' 或 '1'（可与相邻数字无分隔）。
    fn flag(&mut self) -> Result<bool, ParseError> {
        self.skip_ws();
        match self.s.get(self.i) {
            Some(b'0') => {
                self.i += 1;
                Ok(false)
            }
            Some(b'1') => {
                self.i += 1;
                Ok(true)
            }
            _ => Err(ParseError::BadArcFlag),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Segment::{Close, CubicTo, LineTo, MoveTo, QuadraticTo};

    fn segs(d: &str) -> Vec<Segment> {
        parse_path_data(d).unwrap_or_else(|e| panic!("parse {d:?} failed: {e:?}"))
    }

    fn assert_close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-4, "{a} vs {b}");
    }

    #[test]
    fn absolute_and_relative_lines() {
        assert_eq!(segs("M 10 10 L 20 10 L 20 20 Z"), vec![
            MoveTo(10.0, 10.0),
            LineTo(20.0, 10.0),
            LineTo(20.0, 20.0),
            Close,
        ]);
        assert_eq!(segs("m 10 10 l 10 0 l 0 10 z"), vec![
            MoveTo(10.0, 10.0),
            LineTo(20.0, 10.0),
            LineTo(20.0, 20.0),
            Close,
        ]);
    }

    #[test]
    fn implicit_repeats() {
        // M 的隐式后续是 L
        assert_eq!(segs("M 0 0 10 10 20 20"), vec![
            MoveTo(0.0, 0.0),
            LineTo(10.0, 10.0),
            LineTo(20.0, 20.0),
        ]);
        // m 的隐式后续是 l
        assert_eq!(segs("m 0 0 10 10"), vec![
            MoveTo(0.0, 0.0),
            LineTo(10.0, 10.0),
        ]);
        // L 的隐式重复仍是 L
        assert_eq!(segs("M 0 0 L 1 1 2 2 3 3"), vec![
            MoveTo(0.0, 0.0),
            LineTo(1.0, 1.0),
            LineTo(2.0, 2.0),
            LineTo(3.0, 3.0),
        ]);
    }

    #[test]
    fn horizontal_vertical() {
        assert_eq!(segs("M 5 5 H 15 V 25"), vec![
            MoveTo(5.0, 5.0),
            LineTo(15.0, 5.0),
            LineTo(15.0, 25.0),
        ]);
        assert_eq!(segs("M 5 5 h 10 v 20"), vec![
            MoveTo(5.0, 5.0),
            LineTo(15.0, 5.0),
            LineTo(15.0, 25.0),
        ]);
    }

    #[test]
    fn smooth_cubic_reflection() {
        // S 的第一控制点 = 2·cur − prev_c2 = 2·(3,0) − (2,1) = (4,−1)
        assert_eq!(segs("M 0 0 C 1 1 2 1 3 0 S 5 -1 6 0"), vec![
            MoveTo(0.0, 0.0),
            CubicTo(1.0, 1.0, 2.0, 1.0, 3.0, 0.0),
            CubicTo(4.0, -1.0, 5.0, -1.0, 6.0, 0.0),
        ]);
    }

    #[test]
    fn smooth_quadratic_reflection() {
        // T 的控制点 = 2·(4,0) − (2,2) = (6,−2)
        assert_eq!(segs("M 0 0 Q 2 2 4 0 T 8 0"), vec![
            MoveTo(0.0, 0.0),
            QuadraticTo(2.0, 2.0, 4.0, 0.0),
            QuadraticTo(6.0, -2.0, 8.0, 0.0),
        ]);
    }

    #[test]
    fn number_formats() {
        // .5 / 5. / 指数 / 负号分隔
        assert_eq!(segs("M.5.5L1e1-1e-1"), vec![
            MoveTo(0.5, 0.5),
            LineTo(10.0, -0.1),
        ]);
        assert_eq!(segs("M 5. 5."), vec![MoveTo(5.0, 5.0)]);
    }

    #[test]
    fn quarter_circle_arc() {
        // 90° 圆弧：M 0 0 A 10 10 0 0 1 10 10，中心 (0,10)，kappa 展开单段
        // （out[0] 是 MoveTo，out[1] 才是弧段）
        let out = segs("M 0 0 A 10 10 0 0 1 10 10");
        assert_eq!(out.len(), 2);
        match out[1] {
            CubicTo(x1, y1, x2, y2, x, y) => {
                assert_close(x1, 5.5228);
                assert_close(y1, 0.0);
                assert_close(x2, 10.0);
                assert_close(y2, 4.4772);
                assert_close(x, 10.0);
                assert_close(y, 10.0);
            }
            other => panic!("expected cubic, got {other:?}"),
        }
    }

    #[test]
    fn semicircle_arc_two_segments() {
        // 半圆：180° → 2 段 cubic（加 MoveTo 共 3）
        let out = segs("M 0 0 A 5 5 0 1 1 10 0");
        assert_eq!(out.len(), 3);
        match out[2] {
            CubicTo(_, _, _, _, x, y) => {
                assert_close(x, 10.0);
                assert_close(y, 0.0);
            }
            other => panic!("expected cubic, got {other:?}"),
        }
    }

    #[test]
    fn rotated_arc_reaches_endpoint() {
        let out = segs("M 0 0 A 10 5 45 0 1 10 10");
        assert!(!out.is_empty());
        match *out.last().unwrap() {
            CubicTo(_, _, _, _, x, y) => {
                assert_close(x, 10.0);
                assert_close(y, 10.0);
            }
            other => panic!("expected cubic, got {other:?}"),
        }
    }

    #[test]
    fn small_radii_scale_up() {
        // 端点距 14.14 > 2r=2 → 半径按规范放大，仍产生合法弧且到达端点
        let out = segs("M 0 0 A 1 1 0 0 1 10 10");
        match *out.last().unwrap() {
            CubicTo(_, _, _, _, x, y) => {
                assert_close(x, 10.0);
                assert_close(y, 10.0);
            }
            other => panic!("expected cubic, got {other:?}"),
        }
    }

    #[test]
    fn degenerate_arcs() {
        // 端点重合 → 弧零段（仅剩 MoveTo）
        assert_eq!(segs("M 0 0 A 5 5 0 0 1 0 0"), vec![MoveTo(0.0, 0.0)]);
        // rx=0 → 直线
        assert_eq!(segs("M 0 0 A 0 5 0 0 1 10 10"), vec![
            MoveTo(0.0, 0.0),
            LineTo(10.0, 10.0),
        ]);
    }

    #[test]
    fn arc_flags_without_separator() {
        // large/sweep 标志可与后续数字连写；
        // 端点距 14.14 > 2r=10 → 半径放大到 7.07（端点距=直径 → 180° → 2 段 cubic）
        let out = segs("M 0 0 A 5 5 0 0110 10");
        assert_eq!(out.len(), 3);
        match out[2] {
            CubicTo(_, _, _, _, x, y) => {
                assert_close(x, 10.0);
                assert_close(y, 10.0);
            }
            other => panic!("expected cubic, got {other:?}"),
        }
    }

    #[test]
    fn close_resets_current_point() {
        // Z 后当前点回到子路径起点；新 M 重新定义子路径
        let out = segs("M 0 0 L 5 5 Z L 10 0");
        assert_eq!(out, vec![
            MoveTo(0.0, 0.0),
            LineTo(5.0, 5.0),
            Close,
            LineTo(10.0, 0.0),
        ]);
    }

    #[test]
    fn evenodd_style_path_with_many_arcs() {
        // 取自一枚 Bootstrap 风格图标（evenodd 描边、大量小弧）的 `d`。
        // 该图标本身已随工具栏精简移除，但这段 path 数据仍是最严苛的解析
        // 样本：相对弧（a 命令）、隐式重复命令、无分隔符负数、fill-rule 闭合。
        // 数据内联在此，解析器不依赖任何资产文件。
        let d = "M12 0a.75.75 0 0 1 .71.506L13.569 3H21a1.5 1.5 0 0 1 1.5 1.5v10.5a1.5 1.5 0 0 1-1.5 1.5h-2.775l1.985 5.756a.75.75 0 1 1-1.419.489L16.638 16.5H12.75v4.5a.75.75 0 0 1-1.5 0v-4.5H7.362l-2.153 6.245a.75.75 0 1 1-1.419-.489L5.775 16.5H3a1.5 1.5 0 0 1-1.5-1.5V4.5a1.5 1.5 0 0 1 1.5-1.5h7.431l.86-2.495A.75.75 0 0 1 12 0M3 4.5v10.5h18V4.5z";
        let out = segs(d);
        // 首 MoveTo(12,0)，尾 Close；段数充足
        assert!(matches!(out[0], MoveTo(12.0, 0.0)));
        assert!(matches!(*out.last().unwrap(), Close));
        assert!(out.len() > 30);
    }

    #[test]
    fn errors() {
        assert_eq!(parse_path_data(""), Ok(Vec::new()));
        assert_eq!(parse_path_data("   "), Ok(Vec::new()));
        assert_eq!(parse_path_data("M"), Err(ParseError::UnexpectedEof));
        assert_eq!(parse_path_data("M 1 1 Q"), Err(ParseError::UnexpectedEof));
        assert_eq!(parse_path_data("X 1 1"), Err(ParseError::UnknownCommand('X')));
        assert_eq!(parse_path_data("M 1 1 L 1 e"), Err(ParseError::BadNumber));
        assert_eq!(parse_path_data("M 1 1 A 5 5 0 2 0 5 5"), Err(ParseError::BadArcFlag));
        assert_eq!(parse_path_data("123"), Err(ParseError::BadNumber));
        // Z 后跟数字：非法
        assert!(parse_path_data("M 0 0 Z 3 4").is_err());
    }
}
