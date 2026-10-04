//! 惯性动量（自 JS `main_momentum_tick` 移植，f32 重写）。
//!
//! - **帧率无关**：摩擦系数按实际帧间隔折算（基准 60Hz/16ms），
//!   掉帧时惯性不忽快忽慢；
//! - **静止零开销**：未激活时 [`Momentum::tick`] 立即返回 `None`，
//!   不做任何数学运算；
//! - **立即收敛**：速度低于停止阈值即停，不留长尾空转（省电）。

/// 低速摩擦基准（60Hz 每帧速度保留比例）。
const FRICTION_BASE: f32 = 0.65;
/// 高速摩擦上限。
const FRICTION_MAX: f32 = 0.85;
/// 速度 → 摩擦的平滑尺度（px/帧）。
const SPEED_SMOOTH: f32 = 8.0;
/// 停止阈值（px/帧）：任一分量低于即收敛。
const STOP_THRESHOLD: f32 = 0.5;
/// 基准帧时长（秒）。
const BASE_FRAME_SECS: f32 = 16.0 / 1000.0;

/// 惯性动量状态（`Copy`，12 字节）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Momentum {
    vx: f32,
    vy: f32,
    active: bool,
}

impl Momentum {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否正在惯性滚动。
    #[inline]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// 以释放瞬间的速度（px/帧）启动惯性。
    #[inline]
    pub fn start(&mut self, vx: f32, vy: f32) {
        self.vx = vx;
        self.vy = vy;
        self.active = true;
    }

    /// 立即停止惯性。
    #[inline]
    pub fn stop(&mut self) {
        self.vx = 0.0;
        self.vy = 0.0;
        self.active = false;
    }

    /// 每帧积分：返回本帧位移 `(dx, dy)`；已静止或已收敛时返回 `None`
    /// （无任何数学运算，空闲零开销）。
    ///
    /// 速度单位为 px/16ms；位移按实际帧间隔积分（`dt_units` 折算），
    /// 摩擦同样按帧间隔折算——tick 节拍变化（高刷 / 掉帧）时滚动速度与
    /// 总位移保持一致，实现真正的帧率无关。
    pub fn tick(&mut self, dt_secs: f32) -> Option<(f32, f32)> {
        if !self.active {
            return None;
        }

        let dt_units = (dt_secs / BASE_FRAME_SECS).clamp(0.1, 8.0);
        let speed = (self.vx * self.vx + self.vy * self.vy).sqrt();
        // 摩擦随速度平滑：慢 → FRICTION_BASE，快 → FRICTION_MAX
        let friction =
            FRICTION_MAX - (FRICTION_MAX - FRICTION_BASE) * (-speed / SPEED_SMOOTH).exp();
        let frame_friction = friction.powf(dt_units);

        self.vx *= frame_friction;
        self.vy *= frame_friction;

        if self.vx.abs() > STOP_THRESHOLD || self.vy.abs() > STOP_THRESHOLD {
            // 位移按实际帧间隔积分（速度 × 时间），而非每 tick 固定一步
            Some((self.vx * dt_units, self.vy * dt_units))
        } else {
            self.active = false;
            self.vx = 0.0;
            self.vy = 0.0;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_tick_is_none() {
        let mut m = Momentum::new();
        assert!(m.tick(1.0 / 60.0).is_none());
        assert!(!m.is_active());
    }

    #[test]
    fn decays_and_settles() {
        let mut m = Momentum::new();
        m.start(10.0, 0.0);
        let mut total = 0.0f32;
        let mut frames = 0;
        for _ in 0..2000 {
            match m.tick(1.0 / 60.0) {
                Some((dx, _)) => {
                    total += dx;
                    frames += 1;
                }
                None => break,
            }
        }
        assert!(!m.is_active(), "惯性应收敛停止");
        assert!(frames > 5, "应有实际滚动帧");
        assert!(total > 10.0, "总位移应超过初速（摩擦渐进），实际 {total}");
        assert!(total < 300.0, "总位移应有界，实际 {total}");
    }

    #[test]
    fn frame_rate_independent_total_distance() {
        // 同一初速度：60Hz 与 120Hz 的累计位移应接近（离散积分误差 < 10%）
        let run = |steps: usize, dt: f32| {
            let mut m = Momentum::new();
            m.start(20.0, 0.0);
            let mut total = 0.0f32;
            for _ in 0..steps {
                match m.tick(dt) {
                    Some((dx, _)) => total += dx,
                    None => break,
                }
            }
            total
        };
        let t60 = run(600, 1.0 / 60.0);
        let t120 = run(1200, 1.0 / 120.0);
        let rel = (t60 - t120).abs() / t60;
        assert!(rel < 0.10, "帧率无关性破坏：60Hz {t60} vs 120Hz {t120}");
    }

    #[test]
    fn stop_is_immediate() {
        let mut m = Momentum::new();
        m.start(50.0, 50.0);
        assert!(m.tick(1.0 / 60.0).is_some());
        m.stop();
        assert!(!m.is_active());
        assert!(m.tick(1.0 / 60.0).is_none());
    }
}
