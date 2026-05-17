use std::path::PathBuf;

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::{
    font::Font,
    widget::{button, column, container, row, scrollable, text, text_input, Id, Space},
    Alignment, Color, Element, Length, Padding,
};

use crate::{config::Config, platform::APP_NAME};

use super::{App, Message};

const BG: Color = Color {
    r: 0.949,
    g: 0.949,
    b: 0.969,
    a: 1.0,
}; // #f2f2f7
const PANEL: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
}; // white
const BAR: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
}; // white
const BORDER: Color = Color {
    r: 0.820,
    g: 0.820,
    b: 0.839,
    a: 1.0,
}; // #d1d1d6
const TERM_BG: Color = Color {
    r: 0.102,
    g: 0.102,
    b: 0.114,
    a: 1.0,
}; // #1a1a1d
const TERM_FRAME: Color = Color {
    r: 0.145,
    g: 0.145,
    b: 0.157,
    a: 1.0,
}; // #252528
const TERM_BORDER: Color = Color {
    r: 0.224,
    g: 0.224,
    b: 0.239,
    a: 1.0,
}; // #39393d
const TERM_FG: Color = Color {
    r: 0.851,
    g: 0.859,
    b: 0.875,
    a: 1.0,
}; // #d9dbe0
const TERM_DIM: Color = Color {
    r: 0.612,
    g: 0.623,
    b: 0.643,
    a: 1.0,
}; // #9c9fb4
const GREEN: Color = Color {
    r: 0.204,
    g: 0.780,
    b: 0.349,
    a: 1.0,
}; // #34c759
const OFF: Color = Color {
    r: 0.820,
    g: 0.820,
    b: 0.839,
    a: 1.0,
}; // #d1d1d6
const MAC_BLUE: Color = Color {
    r: 0.0,
    g: 0.478,
    b: 1.0,
    a: 1.0,
}; // #007aff
const MAC_RED: Color = Color {
    r: 1.0,
    g: 0.231,
    b: 0.188,
    a: 1.0,
}; // #ff3b30
const MAC_ORG: Color = Color {
    r: 1.0,
    g: 0.584,
    b: 0.0,
    a: 1.0,
}; // #ff9500
const BTC_ACC: Color = Color {
    r: 0.973,
    g: 0.580,
    b: 0.102,
    a: 1.0,
}; // #f7931a
const ELS_ACC: Color = Color {
    r: 0.345,
    g: 0.337,
    b: 0.839,
    a: 1.0,
}; // #5856d6
const TEXT_SEC: Color = Color {
    r: 0.282,
    g: 0.282,
    b: 0.290,
    a: 1.0,
}; // #48484a
const TEXT_TER: Color = Color {
    r: 0.557,
    g: 0.557,
    b: 0.576,
    a: 1.0,
}; // #8e8e93
const DISABLED_BG: Color = Color {
    r: 0.914,
    g: 0.914,
    b: 0.933,
    a: 1.0,
}; // #e9e9ee

pub(super) const fn bitcoin_scroll_id() -> Id {
    Id::new("bitcoin_terminal")
}

pub(super) const fn electrs_scroll_id() -> Id {
    Id::new("electrs_terminal")
}

pub(super) fn view(app: &App) -> Element<'_, Message> {
    let content = column![
        view_toolbar(app),
        horizontal_rule(),
        view_paths_panel(app),
        view_node_panels(app),
        horizontal_rule(),
        view_bottom_bar(app),
    ]
    .width(Length::Fill)
    .height(Length::Fill);

    app.overlay_message.as_ref().map_or_else(
        || {
            container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_| container::Style {
                    background: Some(BG.into()),
                    ..Default::default()
                })
                .into()
        },
        |msg| view_overlay(msg, app.bitforge_path.clone()),
    )
}

fn view_toolbar(app: &App) -> Element<'_, Message> {
    let height_text: String = if app.block_height > 0 {
        let s = app.block_height.to_string();
        let mut out = String::with_capacity(s.len() + s.len() / 3);
        for (i, ch) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        out.chars().rev().collect::<String>()
    } else {
        "Waiting for node".to_owned()
    };

    let title_block = column![
        text(APP_NAME)
            .size(22)
            .font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            })
            .color(Color::BLACK),
        text("Bitcoin Core and Electrs control panel")
            .size(11)
            .color(TEXT_SEC)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    ]
    .spacing(2)
    .width(Length::FillPortion(2));

    let block_stat = column![
        text("BLOCK HEIGHT").size(9).color(TEXT_TER),
        text(height_text)
            .size(18)
            .font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            })
            .color(Color::BLACK),
    ]
    .spacing(2)
    .width(Length::Shrink);

    let update_btn =
        styled_button("Update Binaries", ButtonStyle::Secondary).on_press(Message::UpdateBinaries);

    let toolbar_row = row![
        title_block,
        Space::new().width(Length::Fill),
        block_stat,
        Space::new().width(16),
        update_btn,
    ]
    .spacing(0)
    .align_y(Alignment::Center)
    .padding(Padding::from([0, 20]));

    container(toolbar_row)
        .width(Length::Fill)
        .height(64)
        .style(|_| container::Style {
            background: Some(BAR.into()),
            ..Default::default()
        })
        .into()
}

fn view_paths_panel(app: &App) -> Element<'_, Message> {
    let toggle_label = if app.paths_visible {
        "Hide Paths"
    } else {
        "Show Paths"
    };

    let heading = column![
        text("DIRECTORY PATHS").size(10).color(TEXT_TER),
        text(format!("Config: {}", Config::config_file_path().display()))
            .size(9)
            .color(TEXT_TER)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    ]
    .spacing(3)
    .width(Length::Fill);

    let header = row![
        heading,
        Space::new().width(Length::Fill),
        styled_button(toggle_label, ButtonStyle::Secondary).on_press(Message::TogglePathsPanel),
    ]
    .align_y(Alignment::Center)
    .padding(Padding::from([12, 20]));

    if !app.paths_visible {
        return container(header)
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(BAR.into()),
                ..Default::default()
            })
            .into();
    }

    let rows = column![
        path_row(
            "Binaries Folder",
            "Folder containing bitcoind, bitcoin-cli, and electrs",
            &app.binaries_path_edit,
            Message::BinariesPathChanged,
            Message::BrowseBinaries,
            std::path::Path::new(&app.binaries_path_edit).exists(),
        ),
        path_row(
            "Bitcoin Data Directory",
            "Bitcoin Core data directory",
            &app.bitcoin_data_path_edit,
            Message::BitcoinDataPathChanged,
            Message::BrowseBitcoinData,
            std::path::Path::new(&app.bitcoin_data_path_edit).exists(),
        ),
        path_row(
            "Electrs DB Directory",
            "Electrs index database directory",
            &app.electrs_data_path_edit,
            Message::ElectrsDataPathChanged,
            Message::BrowseElectrsData,
            std::path::Path::new(&app.electrs_data_path_edit).exists(),
        ),
        row![
            text("Changes take effect on the next node launch.")
                .size(10)
                .color(TEXT_TER),
            Space::new().width(Length::Fill),
            styled_button("Save Paths", ButtonStyle::Confirm).on_press(Message::SavePaths),
        ]
        .align_y(Alignment::Center)
        .padding(Padding::from([8, 0])),
    ]
    .spacing(6)
    .padding(Padding::from([0, 20]));

    let body = column![header, rows].padding(Padding {
        top: 0.0,
        right: 0.0,
        bottom: 4.0,
        left: 0.0,
    });

    container(body)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(BAR.into()),
            ..Default::default()
        })
        .into()
}

fn view_node_panels(app: &App) -> Element<'_, Message> {
    let bitcoin_panel = view_node_panel(NodePanelSpec {
        title: "Bitcoin",
        subtitle: "Bitcoin Core node",
        accent: BTC_ACC,
        launch_action: (!app.bitcoin_running).then_some(Message::LaunchBitcoin),
        launch_hint: if app.bitcoin_running {
            "Bitcoin is already running."
        } else {
            "Starts bitcoind with the configured data directory."
        },
        running: app.bitcoin_running,
        synced: app.bitcoin_synced,
        ready: app.bitcoin_running && app.bitcoin_synced,
        lines: &app.bitcoin_lines,
        scroll_id: bitcoin_scroll_id(),
    });
    let electrs_panel = view_node_panel(NodePanelSpec {
        title: "Electrs",
        subtitle: "Electrum server index",
        accent: ELS_ACC,
        launch_action: (app.bitcoin_running && !app.electrs_status.running)
            .then_some(Message::LaunchElectrs),
        launch_hint: if app.electrs_status.running {
            "Electrs is already running."
        } else if !app.bitcoin_running {
            "Start Bitcoin before launching Electrs."
        } else {
            "Starts electrs against the configured Bitcoin data directory."
        },
        running: app.electrs_status.running,
        synced: app.electrs_status.synced,
        ready: app.electrs_status.ready,
        lines: &app.electrs_lines,
        scroll_id: electrs_scroll_id(),
    });

    row![bitcoin_panel, electrs_panel]
        .spacing(0)
        .height(Length::Fill)
        .into()
}

struct NodePanelSpec<'a> {
    title: &'a str,
    subtitle: &'a str,
    accent: Color,
    launch_action: Option<Message>,
    launch_hint: &'a str,
    running: bool,
    synced: bool,
    ready: bool,
    lines: &'a [String],
    scroll_id: Id,
}

fn view_node_panel(spec: NodePanelSpec<'_>) -> Element<'_, Message> {
    let panel = column![
        accent_bar(spec.accent),
        panel_header(
            spec.title,
            spec.subtitle,
            spec.accent,
            spec.launch_action,
            spec.launch_hint,
        ),
        horizontal_rule(),
        panel_indicators(spec.running, spec.synced, spec.ready),
        horizontal_rule(),
        terminal_container(spec.running, spec.lines, spec.scroll_id),
    ]
    .width(Length::Fill)
    .height(Length::Fill);

    container(panel)
        .width(Length::FillPortion(1))
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(PANEL.into()),
            border: iced::Border {
                color: BORDER,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn accent_bar(accent: Color) -> Element<'static, Message> {
    container(Space::new().height(3))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(accent.into()),
            ..Default::default()
        })
        .into()
}

fn panel_header<'a>(
    title: &'a str,
    subtitle: &'a str,
    accent: Color,
    launch_action: Option<Message>,
    launch_hint: &'a str,
) -> Element<'a, Message> {
    let launch_btn = button(text("Launch").size(13).font(Font {
        weight: iced::font::Weight::Bold,
        ..Font::default()
    }))
    .padding(Padding::from([5, 18]))
    .style(move |_, status| {
        let disabled = status == button::Status::Disabled;
        button::Style {
            background: Some(match status {
                button::Status::Disabled => DISABLED_BG.into(),
                button::Status::Hovered | button::Status::Pressed => darken(accent).into(),
                button::Status::Active => accent.into(),
            }),
            text_color: if disabled { TEXT_TER } else { Color::WHITE },
            border: iced::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 6.0.into(),
            },
            shadow: iced::Shadow::default(),
            snap: false,
        }
    })
    .on_press_maybe(launch_action);

    let heading = column![
        text(title)
            .size(20)
            .font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            })
            .color(Color::BLACK),
        text(subtitle).size(11).color(TEXT_SEC),
        text(launch_hint)
            .size(10)
            .color(TEXT_TER)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    ]
    .spacing(3)
    .width(Length::Fill);

    row![heading, Space::new().width(Length::Fill), launch_btn,]
        .align_y(Alignment::Center)
        .padding(Padding {
            top: 14.0,
            right: 20.0,
            bottom: 10.0,
            left: 20.0,
        })
        .into()
}

fn panel_indicators(running: bool, synced: bool, ready: bool) -> Element<'static, Message> {
    row![
        indicator_badge(
            "Process",
            if running { "Running" } else { "Stopped" },
            running
        ),
        Space::new().width(8),
        indicator_badge("Chain", if synced { "Synced" } else { "Waiting" }, synced),
        Space::new().width(8),
        indicator_badge("Service", if ready { "Ready" } else { "Not ready" }, ready),
    ]
    .align_y(Alignment::Center)
    .padding(Padding::from([10, 20]))
    .into()
}

fn terminal_container(running: bool, lines: &[String], scroll_id: Id) -> Element<'_, Message> {
    let terminal_header = container(
        row![
            row![
                text("LOG OUTPUT").size(9).color(TEXT_TER),
                Space::new().width(6),
                text(format!("{} lines", lines.len()))
                    .size(9)
                    .color(TERM_DIM),
            ]
            .align_y(Alignment::Center),
            Space::new().width(Length::Fill),
            row![
                text("●").size(11).color(if running { GREEN } else { OFF }),
                Space::new().width(4),
                text(if running { "Live" } else { "Idle" })
                    .size(9)
                    .color(TERM_DIM),
            ]
            .align_y(Alignment::Center),
        ]
        .align_y(Alignment::Center)
        .padding(Padding {
            top: 0.0,
            right: 2.0,
            bottom: 0.0,
            left: 2.0,
        }),
    )
    .padding(Padding::from([8, 10]))
    .style(|_| container::Style {
        background: Some(TERM_FRAME.into()),
        border: iced::Border {
            color: TERM_BORDER,
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    });

    let terminal_lines: Vec<Element<Message>> = if lines.is_empty() {
        vec![empty_terminal_state(running)]
    } else {
        lines.iter().map(|l| terminal_line_element(l)).collect()
    };

    let terminal_content = column(terminal_lines)
        .spacing(0)
        .width(Length::Fill)
        .padding(Padding::from([10, 12]));

    let terminal = scrollable(terminal_content)
        .id(scroll_id)
        .direction(Direction::Vertical(Scrollbar::default()))
        .height(Length::Fill)
        .width(Length::Fill);

    container(column![terminal_header, terminal].spacing(8))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(12)
        .style(|_| container::Style {
            background: Some(TERM_BG.into()),
            border: iced::Border {
                color: TERM_BORDER,
                width: 1.0,
                radius: 14.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn view_bottom_bar(app: &App) -> Element<'_, Message> {
    let any_running = app.bitcoin_running || app.electrs_status.running;
    let electrs_running = app.electrs_status.running;

    let shutdown_both = styled_button("Shutdown Bitcoin & Electrs", ButtonStyle::Destructive)
        .on_press_maybe(any_running.then_some(Message::ShutdownBoth));
    let shutdown_els = styled_button("Shutdown Electrs Only", ButtonStyle::Warning)
        .on_press_maybe(electrs_running.then_some(Message::ShutdownElectrsOnly));
    let help_text = if any_running {
        "Shutdown requests use graceful stop first, then terminate if needed."
    } else {
        "Start a service before shutdown controls become available."
    };

    let btn_row = row![
        text(help_text)
            .size(10)
            .color(TEXT_TER)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        shutdown_both,
        Space::new().width(8),
        shutdown_els,
    ]
    .align_y(Alignment::Center)
    .padding(Padding::from([12, 16]));

    container(btn_row)
        .width(Length::Fill)
        .height(56)
        .style(|_| container::Style {
            background: Some(BAR.into()),
            ..Default::default()
        })
        .into()
}

fn view_overlay(message: &str, bitforge_path: Option<PathBuf>) -> Element<'_, Message> {
    let mut buttons: Vec<Element<Message>> = vec![styled_button("OK", ButtonStyle::Primary)
        .on_press(Message::DismissOverlay)
        .into()];

    if let Some(path) = bitforge_path {
        buttons.insert(
            0,
            styled_button("Open BitForge", ButtonStyle::Confirm)
                .on_press(Message::OpenBitForge(path))
                .into(),
        );
    }

    let dialog = container(
        column![
            text(message)
                .size(14)
                .color(Color::BLACK)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .line_height(iced::widget::text::LineHeight::Relative(1.35)),
            Space::new().height(20),
            row![
                Space::new().width(Length::Fill),
                row(buttons).spacing(8).align_y(Alignment::Center),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(0)
        .padding(24)
        .width(Length::Fill),
    )
    .width(520)
    .style(|_| container::Style {
        background: Some(Color::WHITE.into()),
        border: iced::Border {
            color: BORDER,
            width: 1.0,
            radius: 12.0.into(),
        },
        shadow: iced::Shadow {
            color: Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.25,
            },
            offset: iced::Vector { x: 0.0, y: 4.0 },
            blur_radius: 20.0,
        },
        ..Default::default()
    });

    container(dialog)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(|_| container::Style {
            background: Some(
                Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.4,
                }
                .into(),
            ),
            ..Default::default()
        })
        .into()
}

fn horizontal_rule<'a>() -> Element<'a, Message> {
    container(Space::new().height(1))
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(BORDER.into()),
            ..Default::default()
        })
        .into()
}

fn indicator_badge<'a>(label: &'a str, value: &'a str, active: bool) -> Element<'a, Message> {
    let dot_color = if active { GREEN } else { OFF };
    let value_color = if active { Color::BLACK } else { TEXT_SEC };
    container(
        row![
            text("●").size(13).color(dot_color),
            column![
                text(label).size(9).color(TEXT_TER),
                text(value).size(11).color(value_color),
            ]
            .spacing(1),
        ]
        .spacing(7)
        .align_y(Alignment::Center),
    )
    .padding(Padding::from([5, 9]))
    .style(|_| container::Style {
        background: Some(
            Color {
                r: 0.965,
                g: 0.965,
                b: 0.976,
                a: 1.0,
            }
            .into(),
        ),
        border: iced::Border {
            color: BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn empty_terminal_state(running: bool) -> Element<'static, Message> {
    let message = if running {
        "Waiting for new output..."
    } else {
        "No log output yet. Launch the service to start streaming logs."
    };

    container(
        text(message)
            .size(11)
            .color(TERM_DIM)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    )
    .width(Length::Fill)
    .padding(Padding::from([10, 0]))
    .into()
}

struct TerminalTextStyle {
    color: Color,
    bold: bool,
}

fn terminal_line_element(line: &str) -> Element<'_, Message> {
    let style = terminal_line_style(line);
    let font = if style.bold {
        Font {
            weight: iced::font::Weight::Bold,
            ..Font::MONOSPACE
        }
    } else {
        Font::MONOSPACE
    };

    text(line)
        .size(11)
        .font(font)
        .color(style.color)
        .width(Length::Fill)
        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
        .line_height(iced::widget::text::LineHeight::Relative(1.25))
        .into()
}

fn terminal_line_style(line: &str) -> TerminalTextStyle {
    let trimmed = line.trim_start();
    let lower = trimmed.to_ascii_lowercase();

    if trimmed.starts_with('$') {
        return TerminalTextStyle {
            color: GREEN,
            bold: true,
        };
    }

    if trimmed.starts_with("===") || trimmed.starts_with("---") {
        return TerminalTextStyle {
            color: MAC_BLUE,
            bold: true,
        };
    }

    if lower.contains("error")
        || lower.contains("failed")
        || lower.contains("fatal")
        || lower.contains("panic")
        || lower.contains("cannot")
    {
        return TerminalTextStyle {
            color: MAC_RED,
            bold: true,
        };
    }

    if lower.contains("warning") || lower.contains("warn") {
        return TerminalTextStyle {
            color: MAC_ORG,
            bold: false,
        };
    }

    if lower.contains("synced")
        || lower.contains("running")
        || lower.contains("listening")
        || lower.contains("ready")
        || lower.contains("done")
    {
        return TerminalTextStyle {
            color: GREEN,
            bold: false,
        };
    }

    if lower.contains("config")
        || lower.contains("binaries")
        || lower.contains("data dir")
        || lower.contains("db dir")
    {
        return TerminalTextStyle {
            color: TERM_DIM,
            bold: false,
        };
    }

    TerminalTextStyle {
        color: TERM_FG,
        bold: false,
    }
}

fn path_row<'a>(
    label: &'a str,
    placeholder: &'a str,
    value: &'a str,
    on_change: impl Fn(String) -> Message + 'a,
    browse_msg: Message,
    exists: bool,
) -> Element<'a, Message> {
    let exists_text = if exists { "Found" } else { "Missing" };
    let exists_dot = text("●").size(13).color(if exists { GREEN } else { OFF });
    let status = row![
        exists_dot,
        Space::new().width(4),
        text(exists_text).size(10).color(TEXT_SEC),
    ]
    .align_y(Alignment::Center)
    .width(76);

    row![
        text(label).size(11).color(TEXT_SEC).width(180),
        text_input(placeholder, value)
            .on_input(on_change)
            .padding(Padding::from([6, 8]))
            .font(Font::MONOSPACE)
            .size(11),
        Space::new().width(6),
        styled_button("Browse", ButtonStyle::Secondary).on_press(browse_msg),
        Space::new().width(6),
        status,
    ]
    .align_y(Alignment::Center)
    .spacing(6)
    .padding(Padding::from([4, 0]))
    .into()
}

#[derive(Clone, Copy)]
enum ButtonStyle {
    Primary,
    Secondary,
    Destructive,
    Warning,
    Confirm,
}

fn styled_button(label: &str, style: ButtonStyle) -> button::Button<'_, Message> {
    let (bg, hover_bg, fg) = match style {
        ButtonStyle::Primary => (MAC_BLUE, darken(MAC_BLUE), Color::WHITE),
        ButtonStyle::Secondary => (
            Color {
                r: 0.898,
                g: 0.898,
                b: 0.918,
                a: 1.0,
            },
            Color {
                r: 0.847,
                g: 0.847,
                b: 0.871,
                a: 1.0,
            },
            Color::BLACK,
        ),
        ButtonStyle::Destructive => (MAC_RED, darken(MAC_RED), Color::WHITE),
        ButtonStyle::Warning => (MAC_ORG, darken(MAC_ORG), Color::WHITE),
        ButtonStyle::Confirm => (GREEN, darken(GREEN), Color::WHITE),
    };

    button(text(label).size(11))
        .padding(Padding::from([5, 14]))
        .style(move |_, status| {
            let disabled = status == button::Status::Disabled;
            button::Style {
                background: Some(match status {
                    button::Status::Disabled => DISABLED_BG.into(),
                    button::Status::Hovered | button::Status::Pressed => hover_bg.into(),
                    button::Status::Active => bg.into(),
                }),
                text_color: if disabled { TEXT_TER } else { fg },
                border: iced::Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: 6.0.into(),
                },
                shadow: iced::Shadow::default(),
                snap: false,
            }
        })
}

fn darken(c: Color) -> Color {
    Color {
        r: (c.r * 0.85).min(1.0),
        g: (c.g * 0.85).min(1.0),
        b: (c.b * 0.85).min(1.0),
        a: c.a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_line_style_highlights_common_states() {
        let error = terminal_line_style("error: failed to launch");
        assert_eq!(error.color, MAC_RED);
        assert!(error.bold);

        let warning = terminal_line_style("warning: retrying");
        assert_eq!(warning.color, MAC_ORG);
        assert!(!warning.bold);

        let success = terminal_line_style("electrs ready");
        assert_eq!(success.color, GREEN);
        assert!(!success.bold);
    }
}
