use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 画布/批注设置。
///
/// 只保留**确实被读取并改变行为**的字段。曾经存在的 `language` / `theme` /
/// `pen_effect_mode` / `blur_enabled` / `auto_clear_days` / `max_scale_image` /
/// `pinch_zoom_v2` 均已删除——它们只被写入并落盘，全仓没有任何读取点，
/// 属于"假设置"（详见 docs/ui-truthfulness-audit.md）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CanvasSettings {
    /// 摄像头预览推帧间隔档位：low=100ms / adaptive=33ms / high=16ms。
    /// 实际消费点在 `App::start_camera`。
    pub frame_rate_mode: String,
    /// 批注交互（绘画/拖拽/悬停）渲染帧率上限，默认 60。
    /// 高刷新率面板（120/144/165Hz）上限制帧数可线性降低 GPU 占用与功耗。
    /// 消费点在 `App::paced_redraw`。
    pub annotation_fps_cap: u32,
    pub momentum_enabled: bool,
    pub pen_colors: Vec<String>,
    pub pen_size_presets: Vec<f32>,
    /// 笔锋收尾最窄处宽度 / 基础宽度。消费点在 `begin_pen_stroke`。
    pub pen_min_width_ratio: f32,
    /// 笔锋收尾时间窗（毫秒）：笔速衰减的回溯窗口，越长笔尾越尖。
    pub pen_tail_duration: f32,
    pub eraser_size: f32,
    pub eraser_size_presets: Vec<f32>,
}

impl Default for CanvasSettings {
    fn default() -> Self {
        Self {
            frame_rate_mode: "adaptive".to_string(),
            annotation_fps_cap: 60,
            momentum_enabled: false,
            pen_colors: vec![
                "#ef4444".into(),
                "#f97316".into(),
                "#eab308".into(),
                "#22c55e".into(),
                "#06b6d4".into(),
                "#3b82f6".into(),
                "#6366f1".into(),
                "#a855f7".into(),
                "#ec4899".into(),
                "#f43f5e".into(),
                "#14b8a6".into(),
                "#64748b".into(),
                "#1e293b".into(),
                "#000000".into(),
                "#ffffff".into(),
            ],
            pen_size_presets: vec![2.0, 5.0, 10.0, 15.0, 21.0],
            pen_min_width_ratio: 0.2,
            pen_tail_duration: 30.0,
            eraser_size: 15.0,
            eraser_size_presets: vec![5.0, 15.0, 25.0, 38.0, 50.0],
        }
    }
}

/// 渲染器设置：呈现模式与 GPU 适配器偏好（启动时读取，改动需重启生效）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RendererSettings {
    /// 呈现模式（对应 `ICED_PRESENT_MODE`，见 `iced_wgpu::settings::present_mode_from_env`）：
    ///
    /// - `"immediate"` = `wgpu::PresentMode::Immediate`，呈现不等 vsync，
    ///   但**允许在扫描中途上屏 → 会撕裂**。
    /// - `"mailbox"`（默认）= `Mailbox`，队列深度 2，同样不等 vblank（保留
    ///   低延迟），但驱动留住最近 2 帧，屏幕不会显示半张图 → **不撕裂**。
    /// - `"no_vsync"` = `AutoNoVsync`，等 vsync 但允许提前换帧。
    /// - `"vsync"` = `AutoVsync`，严格锁刷新率，GPU 占用/功耗最低。
    ///
    /// **为什么默认是 mailbox 而不是 immediate。** `Immediate` 的语义是"可以在一行
    /// 扫描到一半时上屏"，即**允许撕裂**；本应用主画面是摄像头预览（持续变化的
    /// 真实图像），横向错位最容易被看见。`Mailbox` 驱动留住最近 2 帧，不会撕裂。
    ///
    /// **实测代价为零。** 本机（Vulkan 后端）A/B 三档，`fps` 与 `worst` 无可测
    /// 差异：`AutoVsync` 74/71/63/61、`Immediate` 73/72/60/60、`Mailbox` 同量级。
    /// 所以这是一个"零成本的安全默认值"，**不是**性能修复。
    ///
    /// **更正一条此前的错误结论。** 旧注释说"默认不该是 vsync，因为等 vsync 会
    /// 多出一拍延迟"——那是在 DXGI 语义下的推断，在**当前 Vulkan 后端不成立**：
    /// `AutoVsync` 并未把帧率钉到 60Hz（实测 60Hz 面板上跑到 74fps），所以它没有
    /// 引入那一拍延迟。保留这一行说明是为了不让下一个人再拿它当依据。
    pub present_mode: String,
    /// 优先核显：设置 WGPU_POWER_PREF=low，双显卡机器避免唤醒独显
    pub prefer_integrated_gpu: bool,
}

impl Default for RendererSettings {
    fn default() -> Self {
        Self {
            present_mode: "mailbox".to_string(),
            prefer_integrated_gpu: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceSettings {
    pub camera_index: usize,
    pub camera_width: u32,
    pub camera_height: u32,
    pub default_rotation: i32,
}

impl Default for SourceSettings {
    fn default() -> Self {
        Self {
            camera_index: 0,
            camera_width: 1280,
            camera_height: 720,
            default_rotation: 0,
        }
    }
}

/// 存储设置。照片目录被拍照保存路径真实读取（`PictureDirectorySelected`）。
/// 原有的 `auto_clear_days` 已删除——没有任何清理执行器会读它。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageSettings {
    pub picture_directory: Option<PathBuf>,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self { picture_directory: None }
    }
}

/// 开发者开关。`developer_mode` 本身是**真实**的：它控制设置页里开发者
/// 条目的显隐（`dev_click_count >= 5` 解锁）。`perf_monitor_enabled` 也被
/// 性能监视器真实读取。
///
/// 已删除的伪字段（详见 docs/ui-truthfulness-audit.md）：
/// - `pen_min_width_ratio` / `pen_tail_duration`：与 `canvas` 下的同名字段
///   重复，且旧设置页写的是这一份副本而应用读的是 `canvas` 那份——滑块无效。
/// - `max_scale_image`：真正的缩放上限是 `zephyra::input::Viewport::MAX_SCALE`
///   常量（10.0），此字段无任何读取点。
/// - `pinch_zoom_v2`：只有一套捏合缩放实现，此开关无任何读取点。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DeveloperSettings {
    pub developer_mode: bool,
    pub perf_monitor_enabled: bool,
    pub first_run_completed: bool,
}

impl Default for DeveloperSettings {
    fn default() -> Self {
        Self {
            developer_mode: false,
            perf_monitor_enabled: false,
            first_run_completed: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub canvas: CanvasSettings,
    pub renderer: RendererSettings,
    pub source: SourceSettings,
    pub storage: StorageSettings,
    pub developer: DeveloperSettings,
}

impl Config {
    pub fn config_path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("SECTL").join("ViewStage").join("config.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    fn load_from(path: &Path) -> Self {
        let Ok(data) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            let backup = path.with_extension("json.corrupt");
            let _ = std::fs::copy(path, backup);
            return Self::default();
        };
        let mut config = serde_json::from_value::<Self>(value.clone()).unwrap_or_default();
        config.migrate_legacy(&value);
        config.sanitize();
        config
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::config_path())
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let temporary = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&temporary, json).map_err(|e| e.to_string())?;
        if path.exists() {
            std::fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        std::fs::rename(temporary, path).map_err(|e| e.to_string())
    }

    pub fn import(path: &Path) -> Result<Self, String> {
        let data = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        let mut config: Self = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        config.migrate_legacy(&value);
        config.sanitize();
        Ok(config)
    }

    pub fn export(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())
    }

    fn migrate_legacy(&mut self, value: &Value) {
        // 旧版（0.1.0 之前）把设置平铺在顶层，没有 `canvas` 分组。
        // 只迁移**仍然存在**的字段；已删除的伪字段（language/theme 等）
        // 无需迁移——它们从来就没有过行为。
        if value.get("canvas").is_some() {
            return;
        }
        if let Some(v) = value.get("camera_width").and_then(Value::as_u64) {
            self.source.camera_width = v as u32;
        }
        if let Some(v) = value.get("camera_height").and_then(Value::as_u64) {
            self.source.camera_height = v as u32;
        }
        if let Some(v) = value.get("pen_colors").and_then(Value::as_array) {
            self.canvas.pen_colors = v
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        if let Some(v) = value.get("pen_size_presets").and_then(Value::as_array) {
            self.canvas.pen_size_presets = v
                .iter()
                .filter_map(Value::as_f64)
                .map(|n| n as f32)
                .collect();
        }
        if let Some(v) = value.get("eraser_size_presets").and_then(Value::as_array) {
            self.canvas.eraser_size_presets = v
                .iter()
                .filter_map(Value::as_f64)
                .map(|n| n as f32)
                .collect();
        }
    }

    /// 把落盘值钳回合法区间。每一项都必须对应**真实生效**的行为——
    /// 删掉的伪字段不再需要校验。
    fn sanitize(&mut self) {
        let defaults = Self::default();
        if !matches!(
            self.canvas.frame_rate_mode.as_str(),
            "low" | "adaptive" | "high"
        ) {
            self.canvas.frame_rate_mode = defaults.canvas.frame_rate_mode;
        }
        self.canvas.annotation_fps_cap = self.canvas.annotation_fps_cap.clamp(30, 240);
        // 笔锋参数的下界来自算法本身：`calc_pen_line_width` 用
        // `base_width * min_ratio` 作为最窄处，ratio 必须落在 (0, 1]。
        self.canvas.pen_min_width_ratio = if self.canvas.pen_min_width_ratio.is_finite() {
            self.canvas.pen_min_width_ratio.clamp(0.05, 1.0)
        } else {
            defaults.canvas.pen_min_width_ratio
        };
        self.canvas.pen_tail_duration = if self.canvas.pen_tail_duration.is_finite() {
            self.canvas.pen_tail_duration.clamp(0.0, 400.0)
        } else {
            defaults.canvas.pen_tail_duration
        };
        // 取值必须与 `iced_wgpu::settings::present_mode_from_env` 认识的字符串
        // 一致，否则拼错的键会静默退回 AutoVsync（又变成"设置了没生效"）。
        if !matches!(
            self.renderer.present_mode.as_str(),
            "immediate" | "no_vsync" | "mailbox" | "vsync" | "fifo" | "fifo_relaxed"
        ) {
            self.renderer.present_mode = defaults.renderer.present_mode;
        }
        if self.canvas.pen_colors.len() != 15
            || self.canvas.pen_colors.iter().any(|v| !valid_color(v))
        {
            self.canvas.pen_colors = defaults.canvas.pen_colors;
        }
        if !valid_presets(&self.canvas.pen_size_presets) {
            self.canvas.pen_size_presets = defaults.canvas.pen_size_presets;
        }
        if !valid_presets(&self.canvas.eraser_size_presets) {
            self.canvas.eraser_size_presets = defaults.canvas.eraser_size_presets;
        }
        self.source.camera_width = self.source.camera_width.clamp(320, 7680);
        self.source.camera_height = self.source.camera_height.clamp(240, 4320);
        if !matches!(self.source.default_rotation, 0 | 90 | 180 | 270) {
            self.source.default_rotation = 0;
        }
    }
}

fn valid_color(value: &str) -> bool {
    value.len() == 7 && value.starts_with('#') && value[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn valid_presets(values: &[f32]) -> bool {
    values.len() == 5
        && values
            .iter()
            .all(|v| v.is_finite() && *v >= 0.5 && *v <= 500.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_values_are_recovered() {
        let mut config = Config::default();
        config.canvas.frame_rate_mode = "broken".to_string();
        config.source.camera_width = 1;
        config.source.default_rotation = 45;
        config.sanitize();
        assert_eq!(config.canvas.frame_rate_mode, "adaptive");
        assert_eq!(config.source.camera_width, 320);
        assert_eq!(config.source.default_rotation, 0);
    }

    #[test]
    fn renderer_settings_are_sanitized() {
        let mut config = Config::default();
        // 默认 mailbox：防撕裂，且不等 vblank
        assert_eq!(config.renderer.present_mode, "mailbox");
        assert!(config.renderer.prefer_integrated_gpu);
        // 非法值回退默认；白名单内的值原样保留
        config.renderer.present_mode = "g-sync".to_string();
        config.sanitize();
        assert_eq!(config.renderer.present_mode, "mailbox");
        config.renderer.present_mode = "immediate".to_string();
        config.sanitize();
        assert_eq!(config.renderer.present_mode, "immediate");
    }

    /// 白名单必须与 `iced_wgpu::settings::present_mode_from_env` 认识的字符串
    /// 一致，否则会"配置通过校验但运行时被静默忽略"。
    #[test]
    fn present_mode_whitelist_matches_the_env_parser() {
        let mut config = Config::default();
        for mode in [
            "immediate",
            "no_vsync",
            "mailbox",
            "vsync",
            "fifo",
            "fifo_relaxed",
        ] {
            config.renderer.present_mode = mode.to_string();
            config.sanitize();
            assert_eq!(
                config.renderer.present_mode, mode,
                "{mode} 应被保留（须与 present_mode_from_env 的键一致）"
            );
        }
    }

    #[test]
    fn legacy_values_are_migrated() {
        let value = serde_json::json!({
            "camera_width": 1920,
            "camera_height": 1080,
            "pen_size_presets": [1, 2, 3, 4, 5]
        });
        let mut config = Config::default();
        config.migrate_legacy(&value);
        assert_eq!(
            (config.source.camera_width, config.source.camera_height),
            (1920, 1080)
        );
        assert_eq!(
            config.canvas.pen_size_presets,
            vec![1.0, 2.0, 3.0, 4.0, 5.0]
        );
    }

    /// 已删除的伪字段必须能被旧配置文件**静默忽略**——serde 默认忽略
    /// 未知字段，因此老 config.json 仍可正常加载（不会因缺字段而崩）。
    #[test]
    fn removed_fake_fields_are_ignored_on_load() {
        let legacy = serde_json::json!({
            "application": { "language": "en-US", "blackboard_enabled": true, "theme": "dark" },
            "theme": { "theme": "dark", "blur_enabled": true },
            "storage": { "picture_directory": null, "auto_clear_days": 15 },
            "canvas": { "frame_rate_mode": "adaptive", "annotation_fps_cap": 60, "pen_effect_mode": "full" },
            "developer": { "developer_mode": true, "max_scale_image": 8.0, "pinch_zoom_v2": true }
        });
        let config: Config = serde_json::from_value(legacy).expect("旧配置必须仍可解析");
        // 开发者模式（真实字段）应保留
        assert!(config.developer.developer_mode);
        // 真实字段取默认值（旧配置没有 renderer 段）
        assert_eq!(config.canvas.annotation_fps_cap, 60);
        assert_eq!(config.renderer.present_mode, "mailbox");
    }

    /// 笔锋参数越界/NaN 必须被夹回，而不是把 NaN 灌进镶嵌算法。
    #[test]
    fn pen_taper_params_are_clamped() {
        let mut config = Config::default();
        config.canvas.pen_min_width_ratio = 99.0;
        config.canvas.pen_tail_duration = -5.0;
        config.sanitize();
        assert_eq!(config.canvas.pen_min_width_ratio, 1.0);
        assert_eq!(config.canvas.pen_tail_duration, 0.0);

        let mut config = Config::default();
        config.canvas.pen_min_width_ratio = f32::NAN;
        config.canvas.pen_tail_duration = f32::NAN;
        config.sanitize();
        assert!(config.canvas.pen_min_width_ratio.is_finite());
        assert!(config.canvas.pen_tail_duration.is_finite());
    }

    /// 批注帧率上限的钳位区间与 UI 滑块范围一致。
    #[test]
    fn annotation_fps_cap_is_clamped_to_supported_range() {
        let mut config = Config::default();
        config.canvas.annotation_fps_cap = 5;
        config.sanitize();
        assert_eq!(config.canvas.annotation_fps_cap, 30);
        config.canvas.annotation_fps_cap = 1000;
        config.sanitize();
        assert_eq!(config.canvas.annotation_fps_cap, 240);
    }
}
