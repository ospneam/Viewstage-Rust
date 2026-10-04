use iced::widget::{button, column, container, pick_list, row, slider, text, Space};
use iced::{Border, Color, Element, Length, Theme};

use crate::{Message, OobeAction};

const SURFACE: Color = Color::from_rgb8(30, 30, 30);
const INK: Color = Color::WHITE;
const MUTED: Color = Color::from_rgba8(255, 255, 255, 153.0 / 255.0);
const ACCENT: Color = Color::from_rgb8(59, 130, 246);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resolution(u32, u32);

impl std::fmt::Display for Resolution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} x {}", self.0, self.1)
    }
}

fn step_indicator(current: usize, total: usize) -> Element<'static, Message> {
    let mut dots = Vec::new();
    for i in 0..total {
        let dot = container(
            Space::new()
                .width(Length::Fixed(8.0))
                .height(Length::Fixed(8.0)),
        )
        .style(move |_: &Theme| container::Style {
            background: Some(
                if i == current {
                    ACCENT
                } else {
                    Color::from_rgb8(58, 58, 58)
                }
                .into(),
            ),
            border: Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });
        dots.push(dot.into());
    }
    row(dots).spacing(6).into()
}

fn nav_button(label: &'static str, message: Message, primary: bool) -> Element<'static, Message> {
    button(text(label).size(13))
        .on_press(message)
        .padding([8, 20])
        .style(move |_, _| button::Style {
            background: Some(
                if primary {
                    ACCENT
                } else {
                    Color::from_rgb8(58, 58, 58)
                }
                .into(),
            ),
            text_color: INK,
            border: Border {
                radius: 8.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

fn step_performance(app: &crate::App) -> Element<'_, Message> {
    let frame_mode = app.config.canvas.frame_rate_mode.as_str();

    let low_btn: Element<'_, Message> = button(text("低帧率").size(12))
        .on_press(Message::OobeAction(OobeAction::FrameRateModeSelected(
            "low".into(),
        )))
        .padding([6, 12])
        .style(move |_, _| button::Style {
            background: Some(
                if frame_mode == "low" {
                    ACCENT
                } else {
                    Color::from_rgb8(58, 58, 58)
                }
                .into(),
            ),
            text_color: INK,
            border: Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into();
    let adaptive_btn: Element<'_, Message> = button(text("自适应").size(12))
        .on_press(Message::OobeAction(OobeAction::FrameRateModeSelected(
            "adaptive".into(),
        )))
        .padding([6, 12])
        .style(move |_, _| button::Style {
            background: Some(
                if frame_mode == "adaptive" {
                    ACCENT
                } else {
                    Color::from_rgb8(58, 58, 58)
                }
                .into(),
            ),
            text_color: INK,
            border: Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into();
    let high_btn: Element<'_, Message> = button(text("高帧率").size(12))
        .on_press(Message::OobeAction(OobeAction::FrameRateModeSelected(
            "high".into(),
        )))
        .padding([6, 12])
        .style(move |_, _| button::Style {
            background: Some(
                if frame_mode == "high" {
                    ACCENT
                } else {
                    Color::from_rgb8(58, 58, 58)
                }
                .into(),
            ),
            text_color: INK,
            border: Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into();


    let frame_row: Element<'_, Message> = row![low_btn, adaptive_btn, high_btn].spacing(6).into();
    // 批注帧率上限：全应用最有效的性能旋钮（高刷屏上限帧可线性降低 GPU
    // 占用与功耗），旧版引导里被「钢笔效果」占位——那是个从不生效的假设置。
    // 滑块先绑定成 Element 再入 `row!`：宏内 `.into()` 的目标类型在推断完成
    // 前未定，Slider 的 Theme 落不下来（E0283）。
    let fps_slider: Element<'_, Message> = slider(
        30..=240,
        app.config.canvas.annotation_fps_cap,
        |v| Message::OobeAction(OobeAction::AnnotationFpsCapChanged(v)),
    )
    .width(220)
    .into();
    let annotation_fps: Element<'_, Message> = row![
        fps_slider,
        text(format!("{} fps", app.config.canvas.annotation_fps_cap))
            .size(12)
            .color(INK),
    ]
    .spacing(10)
    .align_y(iced::Alignment::Center)
    .into();
    column![
        text("性能设置").size(22).color(INK),
        Space::new().height(8),
        text("调整摄像头预览与批注渲染的帧率。两项互相独立：")
            .size(13)
            .color(MUTED),
        text("· 预览帧率决定相机画面每秒采集几帧")
            .size(12)
            .color(MUTED),
        text("· 批注帧率决定书写时界面每秒重绘几次（GPU 占用的主来源）")
            .size(12)
            .color(MUTED),
        Space::new().height(16),
        text("摄像头预览帧率").size(13).color(INK),
        frame_row,
        Space::new().height(12),
        text("批注帧率上限").size(13).color(INK),
        annotation_fps,
    ]
    .spacing(6)
    .into()
}

fn step_camera(app: &crate::App) -> Element<'_, Message> {
    let resolutions = vec![
        Resolution(640, 480),
        Resolution(1280, 720),
        Resolution(1920, 1080),
        Resolution(3840, 2160),
    ];
    let idx_slider: Element<'_, Message> =
        slider(0..=8, app.config.source.camera_index as u32, move |v| {
            Message::OobeAction(OobeAction::CameraIndexChanged(v))
        })
        .width(220)
        .into();
    let res_pick: Element<'_, Message> = pick_list(
        resolutions,
        Some(Resolution(
            app.config.source.camera_width,
            app.config.source.camera_height,
        )),
        move |value| Message::OobeAction(OobeAction::CameraResolutionSelected((value.0, value.1))),
    )
    .into();
    column![
        text("摄像头设置").size(22).color(INK),
        Space::new().height(8),
        text("配置摄像头索引和分辨率").size(13).color(MUTED),
        Space::new().height(16),
        text("摄像头索引").size(13).color(INK),
        idx_slider,
        Space::new().height(12),
        text("摄像头分辨率").size(13).color(INK),
        res_pick,
    ]
    .spacing(6)
    .into()
}

fn step_complete() -> Element<'static, Message> {
    column![
        text("设置完成").size(22).color(INK),
        Space::new().height(8),
        text("感谢您完成首次设置！").size(13).color(MUTED),
        text("您可以随时在设置页面中调整这些选项。")
            .size(13)
            .color(MUTED),
        Space::new().height(8),
        text("点击完成开始使用 ViewStage").size(13).color(MUTED),
    ]
    .spacing(6)
    .align_x(iced::Alignment::Center)
    .into()
}

pub fn view(app: &crate::App) -> Element<'_, Message> {
    let step = app.oobe_step;
    // 引导只问**真正生效**的三件事：性能（预览帧率 + 批注帧率）、摄像头、完成。
    // 原先的「语言」「主题」两步已删除——`language` 从未被任何代码读取，
    // `theme` 只决定哪个按钮高亮而所有 UI 颜色都是编译期内置的深色常量。
    let content: Element<'_, Message> = match step {
        0 => step_performance(app),
        1 => step_camera(app),
        _ => step_complete(),
    };

    let step_dots = step_indicator(step, crate::OOBE_LAST_STEP + 1);

    let next_btn = if step < crate::OOBE_LAST_STEP {
        nav_button("下一步", Message::OobeAction(OobeAction::NextStep), true)
    } else {
        nav_button("完成", Message::OobeAction(OobeAction::Complete), true)
    };

    let skip_btn = nav_button("跳过", Message::OobeAction(OobeAction::Skip), false);

    let bottom_row = row![
        step_dots,
        Space::new().width(Length::Fill),
        skip_btn,
        next_btn,
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let content = column![content, Space::new().height(Length::Fill), bottom_row,]
        .spacing(12)
        .padding(24)
        .height(Length::Fill);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(SURFACE.into()),
            ..Default::default()
        })
        .into()
}
