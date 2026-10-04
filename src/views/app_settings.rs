use iced::widget::{
    button, column, container, mouse_area, pick_list, row, scrollable, slider, text, toggler,
    Space,
};
use iced::{Background, Border, Color, Element, Length, Padding, Shadow};

use super::icons;
use crate::{Message, SettingsSection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resolution(u32, u32);

impl std::fmt::Display for Resolution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} × {}", self.0, self.1)
    }
}

const CANVAS: Color = Color::from_rgb8(45, 45, 45);
const SURFACE: Color = Color::from_rgb8(42, 42, 42);
const SURFACE_SOFT: Color = Color::from_rgb8(51, 51, 51);
const SURFACE_STRONG: Color = Color::from_rgb8(58, 58, 58);
const INK: Color = Color::WHITE;
const MUTED: Color = Color::from_rgba8(255, 255, 255, 0.55);
const HAIRLINE: Color = Color::from_rgba8(255, 255, 255, 0.10);
const ACCENT: Color = Color::from_rgb8(59, 130, 246);
const ERROR: Color = Color::from_rgb8(239, 68, 68);

fn sidebar_style() -> container::Style {
    container::Style {
        background: Some(SURFACE.into()),
        border: Border {
            radius: 12.0.into(),
            width: 1.0,
            color: Color::from_rgba8(255, 255, 255, 0.15),
        },
        shadow: Shadow {
            color: Color::from_rgba8(0, 0, 0, 0.20),
            offset: iced::Vector::new(0.0, 4.0),
            blur_radius: 12.0,
        },
        ..Default::default()
    }
}

fn nav_button(
    icon: &'static str,
    label: &'static str,
    section: SettingsSection,
    active: SettingsSection,
) -> Element<'static, Message> {
    let selected = section == active;
    // 图标颜色跟随选中态（与 text_color 一致），选中时 ACCENT 高亮
    let icon_color = if selected { ACCENT } else { INK };
    button(
        row![
            icons::icon(icon, 16.0, icon_color),
            text(label).size(12)
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    )
    .on_press(Message::SettingsSectionSelected(section))
    .width(Length::Fill)
    .padding([8, 10])
    .style(move |_, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(
                if selected {
                    Color::from_rgba8(59, 130, 246, 0.10)
                } else if hovered {
                    Color::from_rgba8(255, 255, 255, 0.08)
                } else {
                    Color::from_rgba8(255, 255, 255, 0.05)
                }
                .into(),
            ),
            text_color: if selected { ACCENT } else { INK },
            border: Border {
                radius: 8.0.into(),
                width: if selected || hovered { 1.0 } else { 0.0 },
                color: if selected {
                    Color::from_rgba8(59, 130, 246, 0.20)
                } else {
                    HAIRLINE
                },
            },
            ..Default::default()
        }
    })
    .into()
}

fn action_button(label: &'static str, message: Message) -> Element<'static, Message> {
    button(text(label).size(13))
        .on_press(message)
        .padding([8, 16])
        .style(|_, status| button::Style {
            background: Some(
                if matches!(status, button::Status::Hovered) {
                    Color::from_rgba8(59, 130, 246, 0.15)
                } else {
                    Color::from_rgba8(59, 130, 246, 0.10)
                }
                .into(),
            ),
            text_color: ACCENT,
            border: Border {
                radius: 8.0.into(),
                width: 1.0,
                color: Color::from_rgba8(59, 130, 246, 0.25),
            },
            ..Default::default()
        })
        .into()
}

fn option_button(
    label: &'static str,
    selected: bool,
    message: Message,
) -> Element<'static, Message> {
    button(text(label).size(12))
        .on_press(message)
        .padding([5, 12])
        .style(move |_, status| button::Style {
            background: Some(
                if selected {
                    INK
                } else if matches!(status, button::Status::Hovered) {
                    Color::from_rgba8(255, 255, 255, 0.08)
                } else {
                    Color::TRANSPARENT
                }
                .into(),
            ),
            text_color: if selected { CANVAS } else { MUTED },
            border: Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

fn option_group<'a>(
    controls: impl IntoIterator<Item = Element<'a, Message>>,
) -> Element<'a, Message> {
    container(row(controls).spacing(0))
        .padding(3)
        .style(|_| container::Style {
            background: Some(SURFACE_STRONG.into()),
            border: Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

fn toggle<'a>(value: bool, on_toggle: impl Fn(bool) -> Message + 'a) -> Element<'a, Message> {
    toggler(value)
        .on_toggle(on_toggle)
        .size(24)
        .style(|_, status| {
            let enabled = match status {
                iced::widget::toggler::Status::Active { is_toggled }
                | iced::widget::toggler::Status::Hovered { is_toggled }
                | iced::widget::toggler::Status::Disabled { is_toggled } => is_toggled,
            };
            iced::widget::toggler::Style {
                background: Background::Color(if enabled { ACCENT } else { SURFACE_STRONG }),
                background_border_width: 0.0,
                background_border_color: Color::TRANSPARENT,
                foreground: Background::Color(INK),
                foreground_border_width: 0.0,
                foreground_border_color: Color::TRANSPARENT,
                text_color: None,
                border_radius: Some(12.0.into()),
                padding_ratio: 0.125,
            }
        })
        .into()
}

fn setting_item<'a>(
    label: &'static str,
    description: &'static str,
    control: Element<'a, Message>,
) -> Element<'a, Message> {
    let details = if description.is_empty() {
        column![text(label).size(13).color(INK)]
    } else {
        column![
            text(label).size(13).color(INK),
            text(description).size(12).color(MUTED)
        ]
        .spacing(4)
    };

    container(
        row![details.width(Length::Fill), control]
            .spacing(20)
            .align_y(iced::Alignment::Center),
    )
    .padding(Padding::from([12, 0]))
    .width(Length::Fill)
    .into()
}

fn page_title<'a>(title: &'static str) -> Element<'a, Message> {
    column![
        text(title).size(18).color(INK),
        container(Space::new().height(1)).style(|_| container::Style {
            background: Some(HAIRLINE.into()),
            ..Default::default()
        })
    ]
    .spacing(10)
    .into()
}

fn select_style(_: &iced::Theme, status: pick_list::Status) -> pick_list::Style {
    pick_list::Style {
        text_color: INK,
        placeholder_color: MUTED,
        handle_color: MUTED,
        background: Background::Color(if matches!(status, pick_list::Status::Hovered) {
            SURFACE_STRONG
        } else {
            SURFACE_SOFT
        }),
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: HAIRLINE,
        },
    }
}

/// 滑块右侧的数值徽标。
fn value_badge_style() -> container::Style {
    container::Style {
        background: Some(Color::from_rgba8(59, 130, 246, 0.12).into()),
        border: Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 「需重启生效」提示条。用于启动期才读取的设置（呈现模式 / 核显偏好），
/// 避免用户改完以为没反应。
fn restart_notice() -> Element<'static, Message> {
    container(
        text("以下两项在程序启动时读取，修改后需重启 ViewStage 生效")
            .size(12)
            .color(MUTED),
    )
    .padding([10, 12])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Color::from_rgba8(239, 168, 68, 0.10).into()),
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: Color::from_rgba8(239, 168, 68, 0.25),
        },
        ..Default::default()
    })
    .into()
}

fn application_page(_app: &crate::App) -> Element<'_, Message> {
    // 参数保留是为了与其余分页（canvas/source/…）在同一 match 分派表里形状
    // 一致；本页当前无动态内容（三个按钮都只发 Message，不读状态）。
    // 原先此页有「语言」下拉与「小黑板」开关，两者均已删除：
    // - language：全仓无读取点，`src/locales/*.json` 从未被加载，切换语言
    //   不会有任何可见变化；
    // - 小黑板：工具栏按钮已移除，开关失去意义。
    // 详见 docs/ui-truthfulness-audit.md。
    let mut page = column![page_title("应用"), setting_item(
        "设置管理",
        "导出、导入或恢复全部设置",
        row![
            action_button("导出设置", Message::ExportSettings),
            action_button("导入设置", Message::ImportSettings),
            action_button("重置全部", Message::ResetAllSettings),
        ]
        .spacing(8)
        .into()
    )]
    .spacing(8);

    page = page.push(setting_item(
        "配置文件位置",
        "设置以临时文件替换方式写入，损坏时自动备份为 .json.corrupt",
        text(
            crate::app::config::Config::config_path()
                .display()
                .to_string()
        )
        .size(11)
        .color(MUTED)
        .into(),
    ));

    page.into()
}

/// 数值滑块行（带右侧数值标签）。滑块拖动期间只改内存，
/// `on_release` 统一落盘，避免每个 tick 写一次配置文件。
fn slider_row(
    label: &'static str,
    description: &'static str,
    range: std::ops::RangeInclusive<u32>,
    value: u32,
    to_message: fn(u32) -> Message,
) -> Element<'static, Message> {
    let current = value.clamp(*range.start(), *range.end());
    // 必须显式标注 Element：`row!` 内的 `.into()` 目标在推断完成前未定，
    // Slider 的 Theme 无法落实 → `From` 实现选不出来（E0283）。
    let slider: Element<'static, Message> = slider(range, current, to_message)
        .on_release(Message::PersistDeveloperConfig)
        .width(220)
        .into();
    setting_item(
        label,
        description,
        row![
            slider,
            container(text(format!("{current}")).size(12).color(INK))
                .padding([2, 8])
                .width(52)
                .style(|_| value_badge_style()),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center)
        .into(),
    )
}

fn canvas_page(app: &crate::App) -> Element<'_, Message> {
    let frame_mode = app.config.canvas.frame_rate_mode.as_str();
    // 见 `slider_row`：`row!` 内的滑块必须先定类型再入列，否则 Theme 推不出来
    let fps_slider: Element<'_, Message> = slider(
        30..=240,
        app.config.canvas.annotation_fps_cap,
        Message::AnnotationFpsCapChanged,
    )
    .on_release(Message::PersistDeveloperConfig)
    .width(200)
    .into();
    column![
        page_title("批注与画布"),
        setting_item(
            "批注帧率上限",
            "书写/拖拽时的渲染帧率上限。高刷屏上限帧可线性降低 GPU 占用与功耗",
            row![
                fps_slider,
                container(
                    text(format!("{} fps", app.config.canvas.annotation_fps_cap))
                        .size(12)
                        .color(INK)
                )
                .padding([2, 8])
                .width(66)
                .style(|_| value_badge_style()),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
            .into()
        ),
        setting_item(
            "摄像头预览帧率",
            "相机画面的采集间隔（10 / 30 / 60 fps），改动后立即重启预览",
            option_group([
                option_button(
                    "低 · 10fps",
                    frame_mode == "low",
                    Message::FrameRateModeSelected("low".into())
                ),
                option_button(
                    "中 · 30fps",
                    frame_mode == "adaptive",
                    Message::FrameRateModeSelected("adaptive".into())
                ),
                option_button(
                    "高 · 60fps",
                    frame_mode == "high",
                    Message::FrameRateModeSelected("high".into())
                ),
            ])
        ),
        setting_item(
            "惯性移动",
            "拖拽或缩放释放后画布继续滑行",
            toggle(
                app.config.canvas.momentum_enabled,
                Message::MomentumSettingToggled
            )
        ),
        slider_row(
            "笔锋最窄比例",
            "笔尾收细到的宽度 ÷ 基础宽度。0.2 = 收到两成，1.0 = 不收细",
            5..=100,
            (app.config.canvas.pen_min_width_ratio * 100.0).round() as u32,
            |pct| Message::PenMinWidthRatioChanged(pct as f32 / 100.0),
        ),
        slider_row(
            "笔锋收尾时长",
            "按笔速回溯重算宽度的时间窗（毫秒）。越长笔尾越尖",
            0..=400,
            app.config.canvas.pen_tail_duration.round() as u32,
            |ms| Message::PenTailDurationChanged(ms as f32),
        ),
        preset_editor(
            "画笔预设粗细",
            "画笔面板的 5 档粗细，直接决定落笔线宽",
            &app.config.canvas.pen_size_presets,
            0.5,
            80.0,
            Message::PenSizePresetChanged,
        ),
        preset_editor(
            "橡皮擦预设粗细",
            "橡皮擦面板的 5 档直径",
            &app.config.canvas.eraser_size_presets,
            0.5,
            200.0,
            Message::EraserSizePresetChanged,
        ),
        setting_item(
            "画布缩放范围",
            "滚轮与捏合共用的固定上下限（编译期常量，不可配置）",
            text(format!(
                "{}% – {}%",
                (viewcore_touch::ScreenTransform::MIN_SCALE * 100.0) as i32,
                (viewcore_touch::ScreenTransform::MAX_SCALE * 100.0) as i32
            ))
            .size(12)
            .color(MUTED)
            .into()
        ),
    ]
    .spacing(8)
    .into()
}

/// 5 档预设粗细编辑器：每档一个滑块。
///
/// 旧实现把预设以 `format!("{:?}", presets)` 的调试文本**只读**展示
/// （`[2.0, 5.0, 10.0, 15.0, 21.0]`），用户无法修改却看着像一项设置。
fn preset_editor(
    label: &'static str,
    description: &'static str,
    presets: &[f32],
    min: f32,
    max: f32,
    to_message: fn(usize, f32) -> Message,
) -> Element<'static, Message> {
    let min = min as u32;
    let max = max as u32;
    let mut rows = column![].spacing(4);
    for (index, value) in presets.iter().enumerate() {
        let current = ((*value).max(min as f32) as u32).min(max);
        // 见 `slider_row`：`row!` 内的滑块必须先定类型再入列
        let preset_slider: Element<'static, Message> =
            slider(min..=max, current, move |v| to_message(index, v as f32))
                .on_release(Message::PersistDeveloperConfig)
                .width(180)
                .into();
        rows = rows.push(
            row![
                text(format!("档 {}", index + 1))
                    .size(12)
                    .color(MUTED)
                    .width(44),
                preset_slider,
                container(text(format!("{current}")).size(12).color(INK))
                    .padding([2, 8])
                    .width(52)
                    .style(|_| value_badge_style()),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center),
        );
    }

    setting_item(
        label,
        description,
        rows.width(Length::Shrink).into(),
    )
}

fn source_page(app: &crate::App) -> Element<'_, Message> {
    let resolutions = vec![
        Resolution(640, 480),
        Resolution(1280, 720),
        Resolution(1920, 1080),
        Resolution(3840, 2160),
    ];
    let rotations = vec![0, 90, 180, 270];
    column![
        page_title("信号源"),
        setting_item(
            "摄像头索引",
            "按系统枚举顺序选择摄像头，修改后立即重启预览",
            slider(
                0..=8,
                app.config.source.camera_index as u32,
                Message::CameraIndexChanged
            )
            .width(220)
            .into()
        ),
        setting_item(
            "摄像头分辨率",
            "修改后使用所选分辨率重新打开摄像头",
            pick_list(
                resolutions,
                Some(Resolution(
                    app.config.source.camera_width,
                    app.config.source.camera_height
                )),
                |value| Message::CameraResolutionSelected((value.0, value.1))
            )
            .style(select_style)
            .into()
        ),
        setting_item(
            "默认旋转角度",
            "启动和修改后应用到当前信号源状态",
            pick_list(
                rotations,
                Some(app.config.source.default_rotation),
                Message::DefaultRotationSelected
            )
            .style(select_style)
            .into()
        ),
    ]
    .spacing(8)
    .into()
}

fn storage_page(app: &crate::App) -> Element<'_, Message> {
    let path = app
        .config
        .storage
        .picture_directory
        .as_ref()
        .map(|value| value.display().to_string())
        .unwrap_or_else(|| "系统图片目录 / ViewStage（默认）".to_string());
    column![
        page_title("存储"),
        setting_item(
            "照片目录",
            "拍照结果按日期写入该目录；导入的图片不受影响",
            row![
                text(path).size(11).color(MUTED),
                action_button("选择目录", Message::PictureDirectorySelected),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
            .into()
        ),
        setting_item(
            "配置文件",
            "采用临时文件替换写入；解析失败时自动备份为 .json.corrupt 并回退默认",
            text(
                crate::app::config::Config::config_path()
                    .display()
                    .to_string()
            )
            .size(11)
            .color(MUTED)
            .into()
        ),
    ]
    .spacing(8)
    .into()
}

/// 渲染器 / 硬件页。
///
/// 取代原先的「主题」页。原页两项都是假设置：`theme.theme` 只决定哪个
/// 按钮高亮（所有 UI 颜色都是各视图里硬编码的深色常量，`Application::theme`
/// 的入参被 `_theme` 丢弃）；`blur_enabled` 无任何读取点，且 iced 的
/// `Shadow::blur_radius` 是阴影模糊而非背景毛玻璃，描述的能力本就不存在。
///
/// 本页三项**确实生效**——但都在 `main()` 启动期读取（写环境变量后创建
/// wgpu 实例），所以运行期改动需要重启，页面顶部有明确提示。
fn renderer_page(app: &crate::App) -> Element<'_, Message> {
    let present = app.config.renderer.present_mode.as_str();
    column![
        page_title("渲染与硬件"),
        restart_notice(),
        setting_item(
            "呈现模式",
            "立即呈现 = 完全不等垂直同步，延迟最低（本应用自己按笔迹热路径\
             60fps / 批注上限节流，无需 vsync 兜底）；\
             无背压 = 交换链深度 2，撕裂风险最低的不等待模式；\
             垂直同步 = 严格锁屏幕刷新率，GPU 占用与功耗最低但多一个 vsync 延迟",
            option_group([
                option_button(
                    "立即呈现",
                    present == "immediate",
                    Message::PresentModeSelected("immediate".into())
                ),
                option_button(
                    "无背压",
                    present == "mailbox",
                    Message::PresentModeSelected("mailbox".into())
                ),
                option_button(
                    "垂直同步",
                    present == "vsync",
                    Message::PresentModeSelected("vsync".into())
                ),
            ])
        ),
        setting_item(
            "优先使用核显",
            "双显卡笔记本上锁定核显，避免唤醒独显（设置 WGPU_POWER_PREF=low）",
            toggle(
                app.config.renderer.prefer_integrated_gpu,
                Message::IntegratedGpuToggled
            )
        ),
        setting_item(
            "界面主题",
            "当前版本所有界面颜色为编译期内置的深色常量，暂不支持切换",
            text("深色（固定）").size(12).color(MUTED).into()
        ),
    ]
    .spacing(8)
    .into()
}

fn about_page(app: &crate::App) -> Element<'_, Message> {
    let unlock_label = if app.config.developer.developer_mode {
        "已解锁".to_string()
    } else {
        format!("点击解锁 ({}/{})", app.dev_click_count, 5)
    };
    let mut page = column![
        page_title("关于"),
        setting_item(
            "ViewStage",
            "用于视频演示与图片批注的应用程序",
            text("0.1.0").size(13).color(ACCENT).into()
        ),
        setting_item(
            "开源许可",
            "",
            text("Apache-2.0").size(12).color(MUTED).into()
        ),
        setting_item(
            "开发者选项",
            if app.config.developer.developer_mode {
                "已解锁，可再次点击关闭"
            } else {
                "连续点击 5 次解锁"
            },
            button(text(unlock_label).size(12))
                .on_press(Message::DevClickIncremented)
                .padding([6, 12])
                .style(|_, _| button::Style {
                    background: Some(SURFACE_STRONG.into()),
                    text_color: INK,
                    border: Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .into()
        ),
    ]
    .spacing(8);

    if app.config.developer.developer_mode {
        page = page.push(
            setting_item(
                "性能监视器",
                "在主界面叠加显示 FPS、显存与丢弃帧统计",
                toggle(
                    app.config.developer.perf_monitor_enabled,
                    Message::PerfMonitorToggled,
                ),
            ),
        );
    }
    page.into()
}

pub fn view(app: &crate::App, window: iced::window::Id) -> Element<'_, Message> {
    let close_button = button(text("×").size(20))
        .on_press(Message::SettingsWindowCloseRequested(window))
        .width(36)
        .height(36)
        .padding(0)
        .style(|_, status| {
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: Some(if hovered { ERROR } else { CANVAS }.into()),
                text_color: if hovered { INK } else { MUTED },
                border: Border {
                    radius: 18.0.into(),
                    width: 1.0,
                    color: if hovered { ERROR } else { HAIRLINE },
                },
                ..Default::default()
            }
        });

    let drag_region = mouse_area(
        container(text("设置").size(14).color(INK))
            .center_x(Length::Fill)
            .center_y(36),
    )
    .on_press(Message::SettingsWindowDragRequested(window));

    let topbar = row![close_button, drag_region, Space::new().width(36)]
        .align_y(iced::Alignment::Center)
        .padding([4, 8])
        .height(44);

    let sidebar = container(
        column![
            nav_button(
                "canvas",
                "批注与画布",
                SettingsSection::Canvas,
                app.settings_section
            ),
            nav_button(
                "source",
                "信号源",
                SettingsSection::Source,
                app.settings_section
            ),
            nav_button(
                "gear",
                "渲染与硬件",
                SettingsSection::Renderer,
                app.settings_section
            ),
            nav_button(
                "storage",
                "存储",
                SettingsSection::Storage,
                app.settings_section
            ),
            nav_button(
                "application",
                "应用",
                SettingsSection::Application,
                app.settings_section
            ),
            nav_button(
                "about",
                "关于",
                SettingsSection::About,
                app.settings_section
            ),
        ]
        .spacing(6),
    )
    .width(160)
    .height(Length::Fill)
    .padding(10)
    .style(|_| sidebar_style());

    let page = match app.settings_section {
        SettingsSection::Application => application_page(app),
        SettingsSection::Canvas => canvas_page(app),
        SettingsSection::Source => source_page(app),
        SettingsSection::Storage => storage_page(app),
        SettingsSection::Renderer => renderer_page(app),
        SettingsSection::About => about_page(app),
    };
    let feedback: Element<'_, Message> = app
        .settings_feedback
        .as_ref()
        .map(|value| text(value).size(12).color(ACCENT).into())
        .unwrap_or_else(|| Space::new().height(0).into());
    let content = scrollable(
        container(column![page, feedback].spacing(12))
            .padding(Padding::from([14, 20]))
            .width(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill);

    container(column![
        topbar,
        row![sidebar, content].spacing(7).padding(Padding {
            top: 0.0,
            right: 0.0,
            bottom: 7.0,
            left: 7.0,
        })
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_| container::Style {
        background: Some(CANVAS.into()),
        text_color: Some(INK),
        ..Default::default()
    })
    .into()
}
