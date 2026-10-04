use iced::widget::{button, container, row, Space};
use iced::{Alignment, Border, Color, Element, Length, Shadow};

use super::icons;
use crate::Message;
use crate::ToolButton;

const GROUP_BG: Color = Color::from_rgb8(26, 26, 26);
const GROUP_BORDER: Color = Color::from_rgba8(255, 255, 255, 25.0 / 255.0);
const GROUP_RADIUS: f32 = 12.0;
const GROUP_SHADOW: Shadow = Shadow {
    color: Color::from_rgba8(0, 0, 0, 0.08),
    offset: iced::Vector::new(0.0, 4.0),
    blur_radius: 12.0,
};

const BTN_TEXT_COLOR: Color = Color::from_rgb8(229, 229, 229);
const BTN_ACTIVE_BG: Color = Color::from_rgb8(42, 42, 42);
const BTN_HOVER_BG: Color = Color::from_rgb8(42, 42, 42);
const BTN_ACTIVE_BORDER: Color = Color::from_rgba8(255, 255, 255, 25.0 / 255.0);

const SEP_COLOR: Color = Color::from_rgba8(255, 255, 255, 25.0 / 255.0);

fn group_style() -> container::Style {
    container::Style {
        background: Some(GROUP_BG.into()),
        border: Border {
            radius: GROUP_RADIUS.into(),
            width: 1.0,
            color: GROUP_BORDER,
        },
        shadow: GROUP_SHADOW,
        text_color: None,
        snap: false,
    }
}

fn btn_style(is_active: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_, status| {
        let bg = if is_active {
            BTN_ACTIVE_BG
        } else {
            match status {
                button::Status::Hovered => BTN_HOVER_BG,
                _ => Color::TRANSPARENT,
            }
        };
        let border_color = if is_active {
            BTN_ACTIVE_BORDER
        } else {
            match status {
                button::Status::Hovered => Color::from_rgba8(255, 255, 255, 15.0 / 255.0),
                _ => Color::TRANSPARENT,
            }
        };
        button::Style {
            background: Some(bg.into()),
            text_color: BTN_TEXT_COLOR,
            border: Border {
                radius: 6.0.into(),
                width: 1.0,
                color: border_color,
            },
            shadow: Shadow::default(),
            snap: false,
        }
    }
}

fn make_btn(
    name: &str,
    tool: ToolButton,
    active_tool: ToolButton,
    is_stateful: bool,
    is_on: bool,
    enabled: bool,
) -> Element<'_, Message> {
    let icon_name = if is_on && name == "light-off" {
        "light"
    } else {
        name
    };
    let icon = icons::icon(icon_name, 18.0, BTN_TEXT_COLOR);
    let is_active = is_stateful && tool == active_tool;

    let mut btn = button(icon)
        .padding(8)
        .width(40)
        .height(40)
        .style(btn_style(is_active || is_on));
    if enabled {
        btn = btn.on_press(Message::ToolClicked(tool));
    }
    btn.into()
}

fn make_menu_btn(tool: ToolButton, active_tool: ToolButton) -> Element<'static, Message> {
    let icon = icons::icon("menu", 18.0, BTN_TEXT_COLOR);

    button(icon)
        .on_press(Message::ToolClicked(tool))
        .padding(8)
        .width(40)
        .height(40)
        .style(btn_style(tool == active_tool))
        .into()
}

pub fn view(app: &crate::App) -> Element<'_, Message> {
    let active = app.active_tool;
    let is_image_mode = app.showing_image;

    let separator: Element<Message> = row![
        Space::new().width(6),
        container(Space::new().width(1).height(24)).style(|_| container::Style {
            background: Some(SEP_COLOR.into()),
            ..container::Style::default()
        }),
        Space::new().width(6),
    ]
    .align_y(Alignment::Center)
    .into();

    // Left group
    let left_group = row![
        make_menu_btn(ToolButton::Menu, active),
        make_btn("minimize", ToolButton::Minimize, active, false, false, true),
    ]
    .spacing(2)
    .align_y(Alignment::Center);

    // Center group
    //
    // 「小黑板」按钮已移除：黑板模式会清空全部笔迹与撤销栈并冻结摄像头，
    // 属于不可逆的状态切换，藏在主工具栏一键可达过于危险。相机画面本身
    // 就是一块可写的板子，批注能力完全覆盖原黑板场景。
    let photo_icon = if is_image_mode {
        "camera-fill"
    } else {
        "camera"
    };
    let mut center_children: Vec<Element<Message>> = vec![
        make_btn("move", ToolButton::Move, active, true, false, true),
        make_btn("pen", ToolButton::Comment, active, true, false, true),
        make_btn("eraser", ToolButton::Eraser, active, true, false, true),
        make_btn(
            "undo",
            ToolButton::Undo,
            active,
            false,
            false,
            app.history.can_undo(),
        ),
        separator,
        make_btn(photo_icon, ToolButton::Photo, active, false, false, true),
    ];
    if !is_image_mode {
        center_children.push(make_btn(
            "light-off",
            ToolButton::Light,
            active,
            false,
            app.light_on,
            true,
        ));
    }
    center_children.push(make_btn(
        "settings",
        ToolButton::Settings,
        active,
        false,
        false,
        true,
    ));

    let center_group = row(center_children).spacing(2).align_y(Alignment::Center);

    // Right group
    //
    // 原先此处还有「文档」按钮（`ToolButton::File`）。它是**完全无效**的：
    // handler 只翻转 `file_sidebar_visible`，而该标志全仓只写不读——没有任何
    // 代码据它渲染东西。按下它唯一的实际效果是把图片侧栏关掉。
    // 图片库改由「图片」按钮（`ToolButton::Image`）开关。
    let right_group = row![make_btn("image", ToolButton::Image, active, false, false, true)]
        .spacing(2)
        .align_y(Alignment::Center);

    let toolbar = row![
        container(left_group)
            .padding([6, 10])
            .style(|_| group_style()),
        Space::new().width(Length::Fill),
        container(center_group)
            .padding([6, 10])
            .style(|_| group_style()),
        Space::new().width(Length::Fill),
        container(right_group)
            .padding([6, 10])
            .style(|_| group_style()),
    ]
    .width(Length::Fill)
    .height(Length::Shrink)
    .padding([10.0, 10.0])
    .align_y(Alignment::End);

    toolbar.into()
}
