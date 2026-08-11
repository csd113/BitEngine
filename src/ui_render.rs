use iced::widget::scrollable::{Direction, Scrollbar};
use iced::{
    font::Font,
    widget::{
        button, column, container, mouse_area, pick_list, progress_bar, row, scrollable, text,
        text_input, toggler, Id, Space,
    },
    Alignment, Color, Element, Length, Padding,
};

use crate::{
    binaries::{BinaryKind, BuildStage, DependencyReport, DependencyState},
    config::{BuildPerformance, Config},
    platform::APP_NAME,
};

use super::{App, DependencyLoad, Message, OutputPane, Page};

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

const BUILD_PERFORMANCE_OPTIONS: [BuildPerformance; 3] = [
    BuildPerformance::Low,
    BuildPerformance::Balanced,
    BuildPerformance::Fastest,
];

pub(super) const fn bitcoin_scroll_id() -> Id {
    Id::new("bitcoin_terminal")
}

pub(super) const fn electrs_scroll_id() -> Id {
    Id::new("electrs_terminal")
}

pub(super) const fn build_scroll_id() -> Id {
    Id::new("binary_build_details")
}

pub(super) const fn output_scroll_id(pane: OutputPane) -> Id {
    match pane {
        OutputPane::Bitcoin => bitcoin_scroll_id(),
        OutputPane::Electrs => electrs_scroll_id(),
        OutputPane::Build => build_scroll_id(),
    }
}

fn output_viewport_changed(
    pane: OutputPane,
    viewport: iced::widget::scrollable::Viewport,
) -> Message {
    Message::OutputViewportChanged {
        pane,
        offset_y: viewport.absolute_offset().y,
        viewport_height: viewport.bounds().height,
        content_height: viewport.content_bounds().height,
    }
}

pub(super) fn view(app: &App) -> Element<'_, Message> {
    let content = match app.page {
        Page::Dashboard => view_dashboard(app),
        Page::Binaries => view_binaries_page(app),
    };

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
        |msg| view_overlay(msg),
    )
}

fn view_dashboard(app: &App) -> Element<'_, Message> {
    column![
        view_toolbar(app),
        horizontal_rule(),
        view_paths_panel(app),
        view_node_panels(app),
        horizontal_rule(),
        view_bottom_bar(app),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn view_binaries_page(app: &App) -> Element<'_, Message> {
    let title = column![
        text("Binaries & Updates")
            .size(22)
            .font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            })
            .color(Color::BLACK),
        text("Build and install Bitcoin Core and electrs without leaving BitEngine.")
            .size(11)
            .color(TEXT_SEC),
    ]
    .spacing(2)
    .width(Length::Fill);
    let inventory_loading =
        app.binary_page.installed_load.is_loading() || app.binary_page.available_load.is_loading();
    let refresh_label = if inventory_loading {
        "Checking…"
    } else {
        "Refresh"
    };
    let header = container(
        row![
            styled_button("Back", ButtonStyle::Secondary).on_press(Message::OpenDashboard),
            Space::new().width(14),
            title,
            styled_button(refresh_label, ButtonStyle::Secondary)
                .on_press_maybe((!inventory_loading).then_some(Message::RefreshBinaryInfo)),
        ]
        .align_y(Alignment::Center)
        .padding(Padding::from([0, 20])),
    )
    .width(Length::Fill)
    .height(64)
    .style(|_| container::Style {
        background: Some(BAR.into()),
        ..Default::default()
    });

    let binaries = container(column![
        binary_row(app, BinaryKind::BitcoinCore),
        horizontal_rule(),
        binary_row(app, BinaryKind::Electrs),
    ])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(PANEL.into()),
        border: iced::Border {
            color: BORDER,
            width: 1.0,
            radius: 12.0.into(),
        },
        ..Default::default()
    });

    let mut sections: Vec<Element<Message>> = vec![
        column![
            text("Installed binaries")
                .size(17)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                })
                .color(Color::BLACK),
            text("BitEngine checks the binaries in your configured Binaries folder and compares them with stable upstream releases.")
                .size(11)
                .color(TEXT_SEC)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(5)
        .into(),
        binaries.into(),
    ];

    if binary_build_status_is_visible(app) {
        sections.push(view_binary_build_status(app));
    }
    sections.push(view_binary_advanced(app));

    let page_content = container(column(sections).spacing(20))
        .width(Length::Fill)
        .max_width(1100)
        .padding(Padding::from([28, 32]));
    let body = scrollable(
        container(page_content)
            .width(Length::Fill)
            .align_x(Alignment::Center),
    )
    .height(Length::Fill)
    .width(Length::Fill);

    column![header, horizontal_rule(), body]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn binary_row(app: &App, kind: BinaryKind) -> Element<'_, Message> {
    let presentation = binary_row_presentation(app, kind);
    let BinaryRowPresentation {
        installed,
        latest,
        action_label,
        action,
        status_label,
        status_color,
        inventory_error,
    } = presentation;
    let heading = column![
        text(kind.label())
            .size(17)
            .font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            })
            .color(Color::BLACK),
        text(match kind {
            BinaryKind::BitcoinCore => "Full Bitcoin node",
            BinaryKind::Electrs => "Electrum server index",
        })
        .size(10)
        .color(TEXT_TER),
    ]
    .spacing(2)
    .width(Length::FillPortion(3));
    let versions = row![
        version_value("INSTALLED", installed),
        Space::new().width(28),
        version_value("LATEST", latest),
    ]
    .width(Length::FillPortion(3));
    let action_button = styled_button(action_label, ButtonStyle::Primary).on_press_maybe(action);

    let summary = row![
        container(Space::new().width(4).height(50)).style(move |_| container::Style {
            background: Some(
                match kind {
                    BinaryKind::BitcoinCore => BTC_ACC,
                    BinaryKind::Electrs => ELS_ACC,
                }
                .into()
            ),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        }),
        Space::new().width(12),
        heading,
        versions,
        status_pill(status_label, status_color),
        Space::new().width(14),
        action_button,
    ]
    .align_y(Alignment::Center);
    let mut content: Vec<Element<Message>> = vec![summary.into()];
    if let Some(error) = inventory_error {
        content.push(build_notice(error, MAC_RED));
    }

    container(column(content).spacing(10))
        .width(Length::Fill)
        .padding(Padding::from([18, 20]))
        .into()
}

struct BinaryRowPresentation {
    installed: String,
    latest: String,
    action_label: &'static str,
    action: Option<Message>,
    status_label: &'static str,
    status_color: Color,
    inventory_error: Option<String>,
}

fn binary_row_presentation(app: &App, kind: BinaryKind) -> BinaryRowPresentation {
    let (installed_result, releases_result, selected) = match kind {
        BinaryKind::BitcoinCore => (
            app.binary_page
                .installed_versions
                .as_ref()
                .map(|versions| &versions.bitcoin),
            app.binary_page
                .available_versions
                .as_ref()
                .map(|versions| &versions.bitcoin),
            app.binary_page.selected_bitcoin.as_ref(),
        ),
        BinaryKind::Electrs => (
            app.binary_page
                .installed_versions
                .as_ref()
                .map(|versions| &versions.electrs),
            app.binary_page
                .available_versions
                .as_ref()
                .map(|versions| &versions.electrs),
            app.binary_page.selected_electrs.as_ref(),
        ),
    };
    let installed =
        installed_result.and_then(|result| result.as_ref().ok().and_then(Option::as_ref));
    let latest = releases_result.and_then(|result| result.as_ref().ok()?.first());
    let installed_text = match installed_result {
        None if app.binary_page.installed_load.is_loading() => "Checking…".to_owned(),
        None => "Not checked".to_owned(),
        Some(Ok(Some(version))) => version.to_string(),
        Some(Ok(None)) => "Not installed".to_owned(),
        Some(Err(_)) => "Unavailable".to_owned(),
    };
    let latest_text = match releases_result {
        None if app.binary_page.available_load.is_loading() => "Checking…".to_owned(),
        None => "Not checked".to_owned(),
        Some(Ok(versions)) => versions
            .first()
            .map_or_else(|| "Unavailable".to_owned(), ToString::to_string),
        Some(Err(_)) => "Unavailable".to_owned(),
    };
    let current = installed
        .zip(latest)
        .is_some_and(|(installed, latest)| installed >= latest);
    let active_kind = app.binary_page.active_kind;
    let build_active = app.binary_page.active_operation.is_some();
    let selected_is_installed = installed
        .zip(selected)
        .is_some_and(|(left, right)| left == right);
    let selected_is_latest = latest
        .zip(selected)
        .is_some_and(|(left, right)| left == right);
    let version_error =
        installed_result.is_some_and(Result::is_err) || releases_result.is_some_and(Result::is_err);
    let mut inventory_errors = Vec::with_capacity(2);
    if let Some(Err(error)) = installed_result {
        inventory_errors.push(format!("Installed version check failed: {error}"));
    }
    if let Some(Err(error)) = releases_result {
        inventory_errors.push(format!("Stable release lookup failed: {error}"));
    }
    let inventory_error = (!inventory_errors.is_empty()).then(|| inventory_errors.join("\n"));
    let (action_label, action) = if build_active && active_kind == Some(kind) {
        ("Building…", None)
    } else if build_active {
        ("Build / Update", None)
    } else if app.binary_page.dependency_load == DependencyLoad::Checking {
        ("Checking dependencies…", None)
    } else if app.binary_page.dependency_load == DependencyLoad::Installing {
        ("Installing dependencies…", None)
    } else if app.pending_path_save.is_some() {
        ("Saving paths…", None)
    } else if selected.is_none() {
        ("Unavailable", None)
    } else if selected_is_installed || (current && selected_is_latest) {
        (if current { "Up to date" } else { "Installed" }, None)
    } else {
        ("Build / Update", Some(Message::StartBuild(kind)))
    };
    let (status_label, status_color) = if version_error {
        ("Status unavailable", MAC_RED)
    } else if current {
        ("Up to date", GREEN)
    } else if installed.is_none() {
        ("Not installed", MAC_ORG)
    } else if latest.is_some() {
        ("Update available", MAC_BLUE)
    } else {
        ("Status unknown", TEXT_TER)
    };

    BinaryRowPresentation {
        installed: installed_text,
        latest: latest_text,
        action_label,
        action,
        status_label,
        status_color,
        inventory_error,
    }
}

fn version_value(label: &'static str, value: String) -> Element<'static, Message> {
    column![
        text(label).size(9).color(TEXT_TER),
        text(value)
            .size(14)
            .font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            })
            .color(Color::BLACK),
    ]
    .spacing(3)
    .into()
}

fn status_pill(label: &str, color: Color) -> Element<'_, Message> {
    container(text(label).size(10).color(color))
        .padding(Padding::from([5, 10]))
        .style(move |_| container::Style {
            background: Some(
                Color {
                    r: color.r,
                    g: color.g,
                    b: color.b,
                    a: 0.10,
                }
                .into(),
            ),
            border: iced::Border {
                color,
                width: 1.0,
                radius: 12.0.into(),
            },
            ..Default::default()
        })
        .into()
}

const fn binary_build_status_is_visible(app: &App) -> bool {
    app.binary_page.active_operation.is_some()
        || app.binary_page.error.is_some()
        || app.binary_page.success.is_some()
        || !app.binary_page.log_lines.is_empty()
}

fn view_binary_build_status(app: &App) -> Element<'_, Message> {
    let stage_label = if app.binary_page.cancellation_requested {
        "Cancelling safely…"
    } else {
        app.binary_page.stage.map_or("Waiting", BuildStage::label)
    };
    let target = build_target_text(app);
    let heading = row![
        column![
            text(target)
                .size(16)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                })
                .color(Color::BLACK),
            text(stage_label).size(11).color(TEXT_SEC),
        ]
        .spacing(3)
        .width(Length::Fill),
        if app.binary_page.active_operation.is_some() {
            styled_button("Cancel", ButtonStyle::Destructive)
                .on_press_maybe(app.binary_page.can_cancel().then_some(Message::CancelBuild))
        } else {
            styled_button("Refresh", ButtonStyle::Secondary).on_press(Message::RefreshBinaryInfo)
        },
    ]
    .align_y(Alignment::Center);

    let progress = progress_bar(0.0..=1.0, app.binary_page.progress)
        .girth(8)
        .style(|_| iced::widget::progress_bar::Style {
            background: DISABLED_BG.into(),
            bar: MAC_BLUE.into(),
            border: iced::Border {
                radius: 4.0.into(),
                ..Default::default()
            },
        });

    let mut content: Vec<Element<Message>> = vec![
        heading.into(),
        progress.into(),
        build_stage_track(app.binary_page.stage),
    ];
    if let Some(request_details) = build_request_details(app) {
        content.push(request_details);
    }
    if let Some(success) = app.binary_page.success.as_deref() {
        content.push(build_notice(success, GREEN));
    }
    if let Some(error) = app.binary_page.error.as_deref() {
        content.push(build_notice(error, MAC_RED));
    }

    let details_label = if app.binary_page.disclosures.build_details {
        "Hide Build Details"
    } else {
        "Build Details"
    };
    if !app.binary_page.log_lines.is_empty() || app.binary_page.last_log_path.is_some() {
        content.push(
            row![
                styled_button(details_label, ButtonStyle::Secondary)
                    .on_press(Message::ToggleBuildDetails),
                Space::new().width(Length::Fill),
                text(format!("{} log lines", app.binary_page.log_lines.len()))
                    .size(9)
                    .color(TEXT_TER),
            ]
            .align_y(Alignment::Center)
            .into(),
        );
    }
    if app.binary_page.disclosures.build_details {
        content.push(build_details(app));
    }

    container(column(content).spacing(14))
        .width(Length::Fill)
        .padding(20)
        .style(|_| container::Style {
            background: Some(PANEL.into()),
            border: iced::Border {
                color: BORDER,
                width: 1.0,
                radius: 12.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn build_target_text(app: &App) -> String {
    app.binary_page
        .displayed_request
        .as_ref()
        .filter(|request| app.binary_page.displayed_operation == Some(request.operation_id))
        .map_or_else(
            || "Recent build".to_owned(),
            |request| format!("{} {}", request.kind.label(), request.version),
        )
}

fn build_request_details(app: &App) -> Option<Element<'_, Message>> {
    let request = app
        .binary_page
        .displayed_request
        .as_ref()
        .filter(|request| app.binary_page.displayed_operation == Some(request.operation_id))?;
    Some(
        column![
            text("INSTALL DESTINATION").size(9).color(TEXT_TER),
            text(request.binaries_dir.display().to_string())
                .size(10)
                .font(Font::MONOSPACE)
                .color(TEXT_SEC)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
            text("BUILD WORKSPACE").size(9).color(TEXT_TER),
            text(request.workspace.display().to_string())
                .size(10)
                .font(Font::MONOSPACE)
                .color(TEXT_SEC)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(4)
        .into(),
    )
}

fn build_stage_track(stage: Option<BuildStage>) -> Element<'static, Message> {
    let stages = [
        BuildStage::DownloadingSource,
        BuildStage::VerifyingSource,
        BuildStage::PreparingBuild,
        BuildStage::Compiling,
        BuildStage::VerifyingBinary,
        BuildStage::Installing,
        BuildStage::Complete,
    ];
    let current_rank = stage.map_or(0, build_stage_rank);
    let mut items = Vec::with_capacity(stages.len() * 2 - 1);
    for (index, item) in stages.into_iter().enumerate() {
        if index > 0 {
            items.push(text("→").size(10).color(TEXT_TER).into());
        }
        let complete = current_rank >= build_stage_rank(item);
        items.push(
            text(item.label())
                .size(9)
                .color(if complete { MAC_BLUE } else { TEXT_TER })
                .into(),
        );
    }
    row(items)
        .spacing(8)
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
}

const fn build_stage_rank(stage: BuildStage) -> usize {
    match stage {
        BuildStage::CheckingRequirements => 0,
        BuildStage::DownloadingSource => 1,
        BuildStage::VerifyingSource => 2,
        BuildStage::PreparingBuild => 3,
        BuildStage::Compiling => 4,
        BuildStage::VerifyingBinary => 5,
        BuildStage::Installing => 6,
        BuildStage::Complete => 7,
    }
}

fn build_notice<'a>(
    message: impl iced::widget::text::IntoFragment<'a>,
    color: Color,
) -> Element<'a, Message> {
    container(
        text(message)
            .size(11)
            .color(TEXT_SEC)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    )
    .width(Length::Fill)
    .padding(Padding::from([10, 12]))
    .style(move |_| container::Style {
        background: Some(
            Color {
                r: color.r,
                g: color.g,
                b: color.b,
                a: 0.08,
            }
            .into(),
        ),
        border: iced::Border {
            color,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn build_details(app: &App) -> Element<'_, Message> {
    let terminal_content: Element<Message> = if app.binary_page.log_lines.is_empty() {
        text(app.binary_page.last_log_path.as_ref().map_or_else(
            || "No build output yet.".to_owned(),
            |path| format!("Detailed log: {}", path.display()),
        ))
        .size(10)
        .font(Font::MONOSPACE)
        .color(TERM_DIM)
        .into()
    } else {
        column(
            app.binary_page
                .log_lines
                .iter()
                .map(|line| terminal_line_element(line)),
        )
        .spacing(0)
        .into()
    };
    let details = scrollable(container(terminal_content).padding(12).width(Length::Fill))
        .id(build_scroll_id())
        .on_scroll(|viewport| output_viewport_changed(OutputPane::Build, viewport))
        .direction(Direction::Vertical(Scrollbar::default()))
        .height(260)
        .width(Length::Fill);
    let details = mouse_area(details)
        .on_enter(Message::OutputPaneHoverChanged {
            pane: OutputPane::Build,
            hovered: true,
        })
        .on_exit(Message::OutputPaneHoverChanged {
            pane: OutputPane::Build,
            hovered: false,
        });
    container(details)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(TERM_BG.into()),
            border: iced::Border {
                color: TERM_BORDER,
                width: 1.0,
                radius: 10.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn view_binary_advanced(app: &App) -> Element<'_, Message> {
    let label = if app.binary_page.disclosures.advanced {
        "Hide Advanced"
    } else {
        "Advanced"
    };
    let header = row![
        column![
            text("Advanced")
                .size(13)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                })
                .color(Color::BLACK),
            text("Release selection, build performance, source retention, and output detail.")
                .size(10)
                .color(TEXT_TER),
        ]
        .spacing(2)
        .width(Length::Fill),
        styled_button(label, ButtonStyle::Secondary).on_press(Message::ToggleBuildAdvanced),
    ]
    .align_y(Alignment::Center);

    let mut content: Vec<Element<Message>> = vec![header.into()];
    if app.binary_page.disclosures.advanced {
        content.push(horizontal_rule());
        content.push(
            row![
                release_picker(app, BinaryKind::BitcoinCore),
                Space::new().width(24),
                release_picker(app, BinaryKind::Electrs),
            ]
            .into(),
        );
        content.push(advanced_build_settings(app));
        content.push(build_dependencies_card(app));
        content.push(
            column![
                text("BUILD WORKSPACE").size(9).color(TEXT_TER),
                text(binaries_workspace_text(app))
                    .size(10)
                    .font(Font::MONOSPACE)
                    .color(TEXT_SEC)
                    .width(Length::Fill)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                text("Node-only Bitcoin flags, source authentication, and transactional installation remain enabled for every build.")
                .size(10)
                .color(TEXT_TER)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
            ]
            .spacing(5)
            .into(),
        );
    }

    container(column(content).spacing(14))
        .width(Length::Fill)
        .padding(18)
        .style(|_| container::Style {
            background: Some(PANEL.into()),
            border: iced::Border {
                color: BORDER,
                width: 1.0,
                radius: 12.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn advanced_build_settings(app: &App) -> Element<'_, Message> {
    let settings = app.config.build_settings;
    column![
        row![
            column![
                text("BUILD PERFORMANCE").size(9).color(TEXT_TER),
                text("Controls parallel work without exposing compiler flags.")
                    .size(10)
                    .color(TEXT_SEC),
            ]
            .spacing(3)
            .width(Length::Fill),
            pick_list(
                BUILD_PERFORMANCE_OPTIONS,
                Some(settings.performance),
                Message::BuildPerformanceChanged,
            )
            .width(150)
            .text_size(11),
        ]
        .align_y(Alignment::Center),
        build_setting_toggle(
            "Keep Source Code",
            "Retain authenticated source after a successful installation for faster repeat builds.",
            settings.keep_source,
            Message::KeepSourceChanged,
        ),
        build_setting_toggle(
            "Clean Build",
            "Discard reusable compilation artifacts before building; authenticated source stays verified.",
            settings.clean_build,
            Message::CleanBuildChanged,
        ),
        build_setting_toggle(
            "Verbose Build Output",
            "Show complete compiler and build-system output while preserving the durable build log.",
            settings.verbose_output,
            Message::VerboseBuildOutputChanged,
        ),
        row![
            text(format!(
                "Bitcoin Core: {} workers  •  electrs: {} workers",
                super::build_worker_count(BinaryKind::BitcoinCore, settings.performance),
                super::build_worker_count(BinaryKind::Electrs, settings.performance),
            ))
            .size(10)
            .color(TEXT_TER)
            .width(Length::Fill),
            styled_button("Restore Defaults", ButtonStyle::Secondary)
                .on_press(Message::RestoreBuildDefaults),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(13)
    .into()
}

fn build_dependencies_card(app: &App) -> Element<'_, Message> {
    let report = app.binary_page.dependency_report.as_ref();
    let (status, color) = match app.binary_page.dependency_load {
        DependencyLoad::Checking => ("Checking…".to_owned(), MAC_BLUE),
        DependencyLoad::Installing => ("Installing…".to_owned(), MAC_BLUE),
        DependencyLoad::Idle => report.map_or_else(
            || ("Not checked".to_owned(), TEXT_TER),
            |report| {
                if report.is_ready() {
                    ("✓ Ready".to_owned(), GREEN)
                } else {
                    let count = report.issue_count();
                    (
                        format!(
                            "⚠ {count} {} missing or outdated",
                            if count == 1 {
                                "dependency"
                            } else {
                                "dependencies"
                            }
                        ),
                        MAC_ORG,
                    )
                }
            },
        ),
    };
    let can_install = report.is_some_and(|report| report.platform.supports_installation());
    let action = if app.binary_page.active_operation.is_some()
        || app.binary_page.dependency_load != DependencyLoad::Idle
    {
        None
    } else if report.is_some_and(|report| !report.is_ready()) && can_install {
        Some(Message::InstallDependencies)
    } else {
        Some(Message::CheckDependencies)
    };
    let action_label = if report.is_some_and(|report| !report.is_ready()) && can_install {
        "Install Build Dependencies"
    } else {
        "Check Dependencies"
    };
    let mut content: Vec<Element<Message>> = vec![row![
        column![
            text("Build Dependencies")
                .size(12)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                })
                .color(Color::BLACK),
            text(status).size(10).color(color),
        ]
        .spacing(3)
        .width(Length::Fill),
        styled_button(action_label, ButtonStyle::Secondary).on_press_maybe(action),
    ]
    .align_y(Alignment::Center)
    .into()];
    if let Some(message) = app.binary_page.dependency_message.as_deref() {
        content.push(build_notice(
            message,
            if report.is_some_and(DependencyReport::is_ready) {
                GREEN
            } else {
                MAC_ORG
            },
        ));
    }
    if let Some(report) = report {
        content.extend(dependency_report_details(
            report,
            app.binary_page.disclosures.dependency_details,
        ));
    }

    container(column(content).spacing(10))
        .width(Length::Fill)
        .padding(14)
        .style(|_| container::Style {
            background: Some(BG.into()),
            border: iced::Border {
                color: BORDER,
                width: 1.0,
                radius: 9.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn dependency_report_details(
    report: &DependencyReport,
    expanded: bool,
) -> Vec<Element<'_, Message>> {
    let mut content = vec![row![
        column![
            text(format!("PLATFORM: {}", report.platform.label()))
                .size(8)
                .color(TEXT_TER),
            text(&report.guidance)
                .size(9)
                .color(TEXT_TER)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(2)
        .width(Length::Fill),
        styled_button(
            if expanded { "Hide Details" } else { "Details" },
            ButtonStyle::Secondary,
        )
        .on_press(Message::ToggleDependencyDetails),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()];
    if expanded {
        let rows = report.items.iter().map(|item| {
            let detected = item.detected_version.as_deref().map_or_else(
                || item.detail.as_deref().unwrap_or("Not detected").to_owned(),
                |version| {
                    item.path.as_ref().map_or_else(
                        || version.to_owned(),
                        |path| format!("{version}  •  {}", path.display()),
                    )
                },
            );
            row![
                column![
                    text(item.name).size(10).color(Color::BLACK),
                    text(detected)
                        .size(9)
                        .font(Font::MONOSPACE)
                        .color(TEXT_TER)
                        .width(Length::Fill)
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                ]
                .spacing(2)
                .width(Length::Fill),
                status_pill(
                    item.state.label(),
                    match item.state {
                        DependencyState::Ready => GREEN,
                        DependencyState::Missing => MAC_ORG,
                        DependencyState::Outdated => MAC_RED,
                    },
                ),
            ]
            .align_y(Alignment::Center)
            .into()
        });
        content.push(column(rows).spacing(9).into());
    }
    content
}

fn release_picker(app: &App, kind: BinaryKind) -> Element<'_, Message> {
    let (label, options, selected) = match kind {
        BinaryKind::BitcoinCore => (
            "BITCOIN CORE RELEASE",
            app.binary_page
                .available_versions
                .as_ref()
                .and_then(|versions| versions.bitcoin.as_ref().ok()),
            app.binary_page.selected_bitcoin.as_ref(),
        ),
        BinaryKind::Electrs => (
            "ELECTRS RELEASE",
            app.binary_page
                .available_versions
                .as_ref()
                .and_then(|versions| versions.electrs.as_ref().ok()),
            app.binary_page.selected_electrs.as_ref(),
        ),
    };
    let picker: Element<Message> = options.map_or_else(
        || {
            text("Release list unavailable")
                .size(11)
                .color(TEXT_TER)
                .into()
        },
        |versions| match kind {
            BinaryKind::BitcoinCore => {
                pick_list(versions.as_slice(), selected, Message::SelectBitcoinVersion)
                    .width(180)
                    .text_size(11)
                    .into()
            }
            BinaryKind::Electrs => {
                pick_list(versions.as_slice(), selected, Message::SelectElectrsVersion)
                    .width(180)
                    .text_size(11)
                    .into()
            }
        },
    );
    column![text(label).size(9).color(TEXT_TER), picker]
        .spacing(6)
        .width(Length::FillPortion(1))
        .into()
}

fn binaries_workspace_text(app: &App) -> String {
    app.binary_page
        .displayed_request
        .as_ref()
        .filter(|request| app.binary_page.active_operation == Some(request.operation_id))
        .map_or_else(
            || {
                crate::binaries::workspace_for(&app.config.binaries_path)
                    .display()
                    .to_string()
            },
            |request| request.workspace.display().to_string(),
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
        styled_button("Update Binaries", ButtonStyle::Secondary).on_press(Message::OpenBinaries);

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
    let paths_editable = app.paths_are_editable();
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
            paths_editable,
        ),
        path_row(
            "Bitcoin Data Directory",
            "Bitcoin Core data directory",
            &app.bitcoin_data_path_edit,
            Message::BitcoinDataPathChanged,
            Message::BrowseBitcoinData,
            std::path::Path::new(&app.bitcoin_data_path_edit).exists(),
            paths_editable,
        ),
        path_row(
            "Electrs DB Directory",
            "Electrs index database directory",
            &app.electrs_data_path_edit,
            Message::ElectrsDataPathChanged,
            Message::BrowseElectrsData,
            std::path::Path::new(&app.electrs_data_path_edit).exists(),
            paths_editable,
        ),
        row![
            text(paths_status_text(app)).size(10).color(TEXT_TER),
            Space::new().width(Length::Fill),
            styled_button(
                if app.pending_path_save.is_some() {
                    "Saving…"
                } else {
                    "Save Paths"
                },
                ButtonStyle::Confirm,
            )
            .on_press_maybe(paths_editable.then_some(Message::SavePaths)),
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

const fn paths_status_text(app: &App) -> &'static str {
    if app.pending_path_save.is_some() {
        "Saving and validating paths…"
    } else if app.binary_page.active_operation.is_some() {
        "Paths are locked for the active binary build."
    } else if app.node_lifecycle_active() {
        "Paths are locked while a node is running or shutting down."
    } else {
        "Changes take effect on the next node launch."
    }
}

fn view_node_panels(app: &App) -> Element<'_, Message> {
    let bitcoin_panel = view_node_panel(NodePanelSpec {
        title: "Bitcoin",
        subtitle: "Bitcoin Core node",
        accent: BTC_ACC,
        launch_action: (app.bitcoin_handle.is_none()
            && app.bitcoin_shutdown.is_none()
            && app.electrs_handle.is_none()
            && app.electrs_shutdown.is_none())
        .then_some(Message::LaunchBitcoin),
        launch_hint: if app.bitcoin_shutdown.is_some() {
            "Bitcoin is shutting down."
        } else if app.bitcoin_handle.is_some() {
            "Bitcoin is already running."
        } else if app.electrs_handle.is_some() || app.electrs_shutdown.is_some() {
            "Stop Electrs from the previous Bitcoin generation before relaunching Bitcoin."
        } else {
            "Starts bitcoind with the configured data directory."
        },
        running: app.bitcoin_running,
        synced: app.bitcoin_synced,
        ready: app.bitcoin_ready(),
        lines: &app.bitcoin_lines,
        output_pane: OutputPane::Bitcoin,
        scroll_id: bitcoin_scroll_id(),
    });
    let electrs_panel = view_node_panel(NodePanelSpec {
        title: "Electrs",
        subtitle: "Electrum server index",
        accent: ELS_ACC,
        launch_action: (app.bitcoin_handle.is_some()
            && app.bitcoin_shutdown.is_none()
            && app.bitcoin_ready()
            && app.electrs_handle.is_none()
            && app.electrs_shutdown.is_none())
        .then_some(Message::LaunchElectrs),
        launch_hint: if app.electrs_shutdown.is_some() {
            "Electrs is shutting down."
        } else if app.electrs_handle.is_some() {
            "Electrs is already running."
        } else if app.bitcoin_handle.is_none() || app.bitcoin_shutdown.is_some() {
            "Start Bitcoin before launching Electrs."
        } else if !app.bitcoin_ready() {
            app.bitcoin_dependency_error()
                .unwrap_or("Wait for Bitcoin RPC and P2P readiness.")
        } else {
            "Starts Electrs with this Bitcoin generation's verified RPC and P2P endpoints."
        },
        running: app.electrs_status.running,
        synced: app.electrs_status.synced,
        ready: app.electrs_status.ready,
        lines: &app.electrs_lines,
        output_pane: OutputPane::Electrs,
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
    output_pane: OutputPane,
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
        terminal_container(spec.running, spec.lines, spec.output_pane, spec.scroll_id,),
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

fn build_setting_toggle<'a>(
    label: &'a str,
    description: &'a str,
    value: bool,
    on_toggle: fn(bool) -> Message,
) -> Element<'a, Message> {
    row![
        column![
            text(label).size(11).color(Color::BLACK),
            text(description)
                .size(9)
                .color(TEXT_TER)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(2)
        .width(Length::Fill),
        toggler(value).on_toggle(on_toggle).size(18),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

fn terminal_container(
    running: bool,
    lines: &[String],
    output_pane: OutputPane,
    scroll_id: Id,
) -> Element<'_, Message> {
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
        .on_scroll(move |viewport| output_viewport_changed(output_pane, viewport))
        .direction(Direction::Vertical(Scrollbar::default()))
        .height(Length::Fill)
        .width(Length::Fill);
    let terminal = mouse_area(terminal)
        .on_enter(Message::OutputPaneHoverChanged {
            pane: output_pane,
            hovered: true,
        })
        .on_exit(Message::OutputPaneHoverChanged {
            pane: output_pane,
            hovered: false,
        });

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
    let shutdown_in_progress = app.bitcoin_shutdown.is_some() || app.electrs_shutdown.is_some();

    let shutdown_both = styled_button("Shutdown Bitcoin & Electrs", ButtonStyle::Destructive)
        .on_press_maybe((any_running && !shutdown_in_progress).then_some(Message::ShutdownBoth));
    let shutdown_els = styled_button("Shutdown Electrs Only", ButtonStyle::Warning).on_press_maybe(
        (electrs_running && app.electrs_shutdown.is_none()).then_some(Message::ShutdownElectrsOnly),
    );
    let help_text = if shutdown_in_progress {
        "Shutdown is in progress; relaunch remains locked until cleanup completes."
    } else if any_running {
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

fn view_overlay(message: &str) -> Element<'_, Message> {
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
                styled_button("OK", ButtonStyle::Primary).on_press(Message::DismissOverlay),
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
    enabled: bool,
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
            .on_input_maybe(enabled.then_some(on_change))
            .padding(Padding::from([6, 8]))
            .font(Font::MONOSPACE)
            .size(11),
        Space::new().width(6),
        styled_button("Browse", ButtonStyle::Secondary)
            .on_press_maybe(enabled.then_some(browse_msg)),
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
    use crate::binaries::{AvailableVersions, InstalledVersions};

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

    #[test]
    fn advanced_build_settings_are_collapsed_and_render_with_defaults() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );

        assert!(!app.binary_page.disclosures.advanced);
        assert_eq!(
            app.config.build_settings.performance,
            BuildPerformance::Balanced
        );
        assert!(!app.config.build_settings.keep_source);
        assert!(!app.config.build_settings.clean_build);
        assert!(!app.config.build_settings.verbose_output);
        drop(view_binary_advanced(&app));
        Ok(())
    }

    #[test]
    fn binary_inventory_presentation_surfaces_exact_errors() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        app.binary_page.installed_versions = Some(InstalledVersions {
            bitcoin: Err("bitcoind probe timed out after 5s".to_owned()),
            electrs: Ok(None),
        });
        app.binary_page.available_versions = Some(AvailableVersions {
            bitcoin: Err("upstream response omitted the release tag".to_owned()),
            electrs: Ok(Vec::new()),
        });

        let presentation = binary_row_presentation(&app, BinaryKind::BitcoinCore);
        assert_eq!(presentation.status_label, "Status unavailable");
        let error = presentation
            .inventory_error
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("exact inventory error should be visible"))?;
        assert!(error.contains("Installed version check failed: bitcoind probe timed out after 5s"));
        assert!(error
            .contains("Stable release lookup failed: upstream response omitted the release tag"));
        Ok(())
    }

    #[test]
    fn pending_path_save_disables_binary_build_action() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        app.binary_page.selected_bitcoin = Some("v30.0".parse().map_err(anyhow::Error::msg)?);
        app.pending_path_save = Some(1);

        let presentation = binary_row_presentation(&app, BinaryKind::BitcoinCore);
        assert_eq!(presentation.action_label, "Saving paths…");
        assert!(presentation.action.is_none());
        Ok(())
    }

    #[test]
    fn advanced_panel_retains_active_workspace_snapshot() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        app.binary_page.selected_bitcoin = Some("v30.0".parse().map_err(anyhow::Error::msg)?);
        drop(app.start_build(BinaryKind::BitcoinCore));
        let expected_workspace = crate::binaries::workspace_for(&app.config.binaries_path)
            .display()
            .to_string();
        app.config = Config::defaults(&temporary.path().join("changed"));
        app.binary_page.selected_bitcoin = Some("v29.2".parse().map_err(anyhow::Error::msg)?);

        assert_eq!(binaries_workspace_text(&app), expected_workspace);
        assert_eq!(build_target_text(&app), "Bitcoin Core 30.0");
        Ok(())
    }

    #[test]
    fn installing_state_locks_paths_and_cancel_action() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        app.binary_page.active_operation = Some(crate::binaries::BuildOperationId(4));
        app.binary_page.active_kind = Some(BinaryKind::BitcoinCore);
        app.binary_page.stage = Some(BuildStage::Installing);

        assert!(!app.paths_are_editable());
        assert!(!app.binary_page.can_cancel());
        Ok(())
    }
}
