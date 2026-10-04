#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StrokeType {
    Draw,
    Erase,
}

#[derive(Debug, Clone)]
pub struct StrokePoint {
    pub from_x: f32,
    pub from_y: f32,
    pub to_x: f32,
    pub to_y: f32,
}

/// 笔画包围盒（画布坐标系，含半线宽 padding）
#[derive(Debug, Clone, Copy, Default)]
pub struct StrokeBounds {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

impl StrokeBounds {
    pub fn is_valid(&self) -> bool {
        self.min_x <= self.max_x && self.min_y <= self.max_y
    }

    pub fn intersects(&self, vx: f32, vy: f32, vw: f32, vh: f32) -> bool {
        self.max_x >= vx && self.min_x <= vx + vw && self.max_y >= vy && self.min_y <= vy + vh
    }
}

/// 十六进制颜色（`#rrggbb` / `rrggbb`）→ 直通 RGBA（非预乘；预乘由着色器
/// 在 AA 之后执行）。非 6 位十六进制回退为不透明白。
///
/// 只在笔画构造时调用一次（结果存进 `DrawStroke::color_rgba`）；镶嵌热路径
/// 直接读预解析值。**不要**在逐帧路径里调本函数。
pub fn parse_hex_rgba(hex: &str) -> [f32; 4] {
    let body = hex.trim_start_matches('#');
    // 逐字节判定，避免 `&body[0..2]` 在非 ASCII 输入上 panic
    if body.len() == 6 && body.bytes().all(|b| b.is_ascii_hexdigit()) {
        let nibble = |i: usize| -> f32 {
            let b = body.as_bytes()[i];
            let v = if b.is_ascii_digit() {
                b - b'0'
            } else {
                (b | 0x20) - b'a' + 10
            };
            v as f32
        };
        [
            (nibble(0) * 16.0 + nibble(1)) / 255.0,
            (nibble(2) * 16.0 + nibble(3)) / 255.0,
            (nibble(4) * 16.0 + nibble(5)) / 255.0,
            1.0,
        ]
    } else {
        [1.0, 1.0, 1.0, 1.0]
    }
}

/// 用户绘制的笔画
#[derive(Debug, Clone)]
pub struct DrawStroke {
    pub id: u64,
    pub stroke_type: StrokeType,
    pub points: Vec<StrokePoint>,
    pub color: String,
    /// `color` 的预解析直通 RGBA（非预乘；预乘由着色器在 AA 后执行）。
    ///
    /// 直播镶嵌**每个输入点**都要取一次颜色，旧实现每次都重新
    /// `trim_start_matches('#')` + 3 次 `from_str_radix` 解字符串。颜色在
    /// 构造后不再变更（全仓无 `stroke.color =` 赋值），故解析一次即可。
    /// `color` 字段仍保留为 `pub` 以兼容既有构造代码，但**改写它不会**
    /// 同步本字段——`rgba()` 以本字段为准。
    pub color_rgba: [f32; 4],
    pub line_width: f32,
    pub variable_widths: Vec<f32>,
    pub stored_widths: Option<Vec<f32>>,
    pub segment_times: Option<Vec<f32>>,
    pub pen_min_width_ratio: f32,
    pub pen_tail_duration: f32,
    pub bounds: StrokeBounds,
}

impl DrawStroke {
    pub fn new(
        color: String,
        line_width: f32,
        id: u64,
        pen_min_width_ratio: f32,
        pen_tail_duration: f32,
    ) -> Self {
        Self {
            id,
            stroke_type: StrokeType::Draw,
            points: Vec::new(),
            color_rgba: parse_hex_rgba(&color),
            color,
            line_width,
            variable_widths: Vec::new(),
            stored_widths: None,
            segment_times: None,
            pen_min_width_ratio,
            pen_tail_duration,
            bounds: StrokeBounds::default(),
        }
    }

    pub fn new_eraser(line_width: f32, id: u64) -> Self {
        Self {
            id,
            stroke_type: StrokeType::Erase,
            points: Vec::new(),
            color_rgba: [0.0, 0.0, 0.0, 1.0],
            color: String::from("#000000"),
            line_width,
            variable_widths: Vec::new(),
            stored_widths: None,
            segment_times: None,
            pen_min_width_ratio: 0.4,
            pen_tail_duration: 30.0,
            bounds: StrokeBounds::default(),
        }
    }

    /// 笔画的直通 RGBA（预解析，见 `color_rgba` 字段说明）。
    #[inline]
    pub fn rgba(&self) -> [f32; 4] {
        self.color_rgba
    }

    pub fn add_point(&mut self, from_x: f32, from_y: f32, to_x: f32, to_y: f32) {
        let hw = self.line_width / 2.0;
        // 更新包围盒
        if !self.bounds.is_valid() {
            self.bounds = StrokeBounds {
                min_x: from_x.min(to_x) - hw,
                min_y: from_y.min(to_y) - hw,
                max_x: from_x.max(to_x) + hw,
                max_y: from_y.max(to_y) + hw,
            };
        } else {
            self.bounds.min_x = self.bounds.min_x.min(from_x.min(to_x) - hw);
            self.bounds.min_y = self.bounds.min_y.min(from_y.min(to_y) - hw);
            self.bounds.max_x = self.bounds.max_x.max(from_x.max(to_x) + hw);
            self.bounds.max_y = self.bounds.max_y.max(from_y.max(to_y) + hw);
        }
        self.points.push(StrokePoint {
            from_x,
            from_y,
            to_x,
            to_y,
        });
    }
}
