use iced::advanced::{
    layout, mouse,
    renderer::{self, Renderer as _},
    widget::Tree,
    Layout, Widget,
};
use iced::widget::scrollable::{Direction, Scrollbar};
use iced::{
    font::Font,
    widget::{
        button, column, container, mouse_area, pick_list, progress_bar, responsive, row,
        scrollable, text, text_input, toggler, Id, Space,
    },
    Alignment, Color, Element, Length, Padding, Point, Rectangle, Size,
};

use crate::{
    binaries::{BinaryKind, BuildStage, DependencyReport, DependencyState, ReleaseVersion},
    config::{BuildPerformance, Config, ThemePreference},
    connection::{ConnectionReadiness, LocalEndpointState},
    electrs_status::ElectrsStatus,
    platform::APP_NAME,
    tor::TorStatus,
};

use super::{
    App, ConnectionMode, ConnectionQr, DependencyLoad, InstallationRecoveryState, Message,
    OutputPane, Page, PENDING_PATH_SAVE_SETTINGS_MESSAGE,
};

#[derive(Clone, Copy)]
struct UiPalette {
    background: Color,
    panel: Color,
    bar: Color,
    border: Color,
    secondary_text: Color,
    tertiary_text: Color,
    disabled: Color,
    secondary_button: Color,
    secondary_button_hover: Color,
    indicator: Color,
}

fn ui_palette(theme: &iced::Theme) -> UiPalette {
    if theme.extended_palette().is_dark {
        UiPalette {
            background: Color::from_rgb8(17, 18, 22),
            panel: Color::from_rgb8(28, 29, 34),
            bar: Color::from_rgb8(24, 25, 30),
            border: Color::from_rgb8(60, 62, 71),
            secondary_text: Color::from_rgb8(207, 208, 216),
            tertiary_text: Color::from_rgb8(166, 168, 178),
            disabled: Color::from_rgb8(43, 45, 52),
            secondary_button: Color::from_rgb8(48, 50, 58),
            secondary_button_hover: Color::from_rgb8(60, 63, 73),
            indicator: Color::from_rgb8(37, 39, 46),
        }
    } else {
        UiPalette {
            background: Color::from_rgb8(242, 242, 247),
            panel: Color::WHITE,
            bar: Color::WHITE,
            border: Color::from_rgb8(209, 209, 214),
            secondary_text: Color::from_rgb8(72, 72, 74),
            tertiary_text: Color::from_rgb8(99, 99, 105),
            disabled: Color::from_rgb8(233, 233, 238),
            secondary_button: Color::from_rgb8(229, 229, 234),
            secondary_button_hover: Color::from_rgb8(216, 216, 222),
            indicator: Color::from_rgb8(246, 246, 249),
        }
    }
}

fn secondary_text_style(theme: &iced::Theme) -> iced::widget::text::Style {
    iced::widget::text::Style {
        color: Some(ui_palette(theme).secondary_text),
    }
}

fn tertiary_text_style(theme: &iced::Theme) -> iced::widget::text::Style {
    iced::widget::text::Style {
        color: Some(ui_palette(theme).tertiary_text),
    }
}

fn semantic_color(theme: &iced::Theme, color: Color) -> Color {
    let palette = ui_palette(theme);
    if color == NEUTRAL_STATUS || color == OFF {
        return palette.tertiary_text;
    }
    if theme.extended_palette().is_dark {
        return if color == MAC_BLUE {
            Color::from_rgb8(88, 166, 255)
        } else {
            color
        };
    }
    if color == GREEN {
        Color::from_rgb8(19, 115, 51)
    } else if color == MAC_BLUE {
        Color::from_rgb8(0, 80, 174)
    } else if color == MAC_RED {
        Color::from_rgb8(179, 38, 30)
    } else if color == MAC_ORG {
        Color::from_rgb8(138, 75, 0)
    } else {
        color
    }
}

fn semantic_text_style(color: Color) -> impl Fn(&iced::Theme) -> iced::widget::text::Style + Copy {
    move |theme| iced::widget::text::Style {
        color: Some(semantic_color(theme, color)),
    }
}
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
    r: 0.682,
    g: 0.698,
    b: 0.729,
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
const BUTTON_BLUE: Color = Color {
    r: 0.0,
    g: 0.353,
    b: 0.718,
    a: 1.0,
}; // #005ab7
const BUTTON_RED: Color = Color {
    r: 0.710,
    g: 0.098,
    b: 0.078,
    a: 1.0,
}; // #b51914
const BUTTON_ORANGE: Color = Color {
    r: 0.557,
    g: 0.302,
    b: 0.0,
    a: 1.0,
}; // #8e4d00
const BUTTON_GREEN: Color = Color {
    r: 0.071,
    g: 0.412,
    b: 0.176,
    a: 1.0,
}; // #12692d
const BUTTON_BITCOIN: Color = Color {
    r: 0.620,
    g: 0.286,
    b: 0.0,
    a: 1.0,
}; // #9e4900
const BUTTON_ELECTRS: Color = Color {
    r: 0.286,
    g: 0.275,
    b: 0.718,
    a: 1.0,
}; // #4946b7
const NEUTRAL_STATUS: Color = Color {
    r: 0.557,
    g: 0.557,
    b: 0.576,
    a: 1.0,
}; // #8e8e93

const BUILD_PERFORMANCE_OPTIONS: [BuildPerformance; 3] = [
    BuildPerformance::Low,
    BuildPerformance::Balanced,
    BuildPerformance::Fastest,
];
const DASHBOARD_SECTION_SPACING: f32 = 12.0;
const DASHBOARD_PADDING: f32 = 12.0;
const DASHBOARD_OVERVIEW_BREAKPOINT: f32 = 840.0;

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
                .style(|theme| container::Style {
                    background: Some(ui_palette(theme).background.into()),
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
        responsive(move |size| view_dashboard_workspace(app, size)),
        horizontal_rule(),
        view_bottom_bar(app),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn view_dashboard_workspace(app: &App, size: Size) -> Element<'_, Message> {
    let connection = container(view_connect_to_node(app)).width(Length::FillPortion(3));
    let paths = container(view_paths_panel(app)).width(Length::FillPortion(2));

    let overview: Element<Message> = if size.width >= DASHBOARD_OVERVIEW_BREAKPOINT {
        row![connection, paths]
            .spacing(DASHBOARD_SECTION_SPACING)
            .width(Length::Fill)
            .into()
    } else {
        column![connection, paths]
            .spacing(DASHBOARD_SECTION_SPACING)
            .width(Length::Fill)
            .into()
    };

    let mut sections = vec![overview];
    if let Some(notice) = installation_recovery_notice(app) {
        sections.push(notice);
    }
    sections.push(view_node_panels(app));

    container(
        column(sections)
            .spacing(DASHBOARD_SECTION_SPACING)
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(DASHBOARD_PADDING)
    .into()
}

fn view_binaries_page(app: &App) -> Element<'_, Message> {
    let header = view_binaries_header(app);
    let binaries = container(column![
        binary_row(app, BinaryKind::BitcoinCore),
        horizontal_rule(),
        binary_row(app, BinaryKind::Electrs),
    ])
    .width(Length::Fill)
    .style(|theme| container::Style {
        background: Some(ui_palette(theme).panel.into()),
        border: iced::Border {
            color: ui_palette(theme).border,
            width: 1.0,
            radius: 12.0.into(),
        },
        ..Default::default()
    });

    let mut sections: Vec<Element<Message>> = Vec::new();
    if let Some(notice) = installation_recovery_notice(app) {
        sections.push(notice);
    }
    sections.extend([
        column![
            text("Installed binaries")
                .size(17)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                }),
            text("BitEngine checks the binaries in your configured Binaries folder and compares them with stable upstream releases.")
                .size(11)
                .style(secondary_text_style)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(5)
        .into(),
        binaries.into(),
    ]);

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

fn view_binaries_header(app: &App) -> Element<'_, Message> {
    let title = column![
        text("Binaries & Updates").size(22).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::default()
        }),
        text("Build and install Bitcoin Core and electrs without leaving BitEngine.")
            .size(11)
            .style(secondary_text_style),
    ]
    .spacing(2)
    .width(Length::Fill);
    let inventory_loading =
        app.binary_page.installed_load.is_loading() || app.binary_page.available_load.is_loading();
    let installation_ready = app.installation_recovery_ready();
    let refresh_label = if !installation_ready {
        match &app.installation_recovery {
            InstallationRecoveryState::Checking { .. } => "Checking safety…",
            InstallationRecoveryState::Failed { .. } => "Recovery required",
            InstallationRecoveryState::Ready { .. } => "Refresh",
        }
    } else if inventory_loading {
        "Checking…"
    } else {
        "Refresh"
    };
    container(
        row![
            styled_button("Back", ButtonStyle::Secondary).on_press(Message::OpenDashboard),
            Space::new().width(14),
            title,
            theme_selector(app),
            Space::new().width(12),
            styled_button(refresh_label, ButtonStyle::Secondary).on_press_maybe(
                (installation_ready && !inventory_loading).then_some(Message::RefreshBinaryInfo)
            ),
        ]
        .align_y(Alignment::Center)
        .padding(Padding::from([0, 20])),
    )
    .width(Length::Fill)
    .height(64)
    .style(|theme| container::Style {
        background: Some(ui_palette(theme).bar.into()),
        ..Default::default()
    })
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
        text(kind.label()).size(17).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::default()
        }),
        text(match kind {
            BinaryKind::BitcoinCore => "Full Bitcoin node",
            BinaryKind::Electrs => "Electrum server index",
        })
        .size(10)
        .style(tertiary_text_style),
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
    let (action_label, action) = binary_action(
        app,
        kind,
        selected,
        selected_is_installed,
        current,
        selected_is_latest,
    );
    let (status_label, status_color) = if version_error {
        ("Status unavailable", MAC_RED)
    } else if current {
        ("Up to date", GREEN)
    } else if installed.is_none() {
        ("Not installed", MAC_ORG)
    } else if latest.is_some() {
        ("Update available", MAC_BLUE)
    } else {
        ("Status unknown", NEUTRAL_STATUS)
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

fn binary_action(
    app: &App,
    kind: BinaryKind,
    selected: Option<&ReleaseVersion>,
    selected_is_installed: bool,
    current: bool,
    selected_is_latest: bool,
) -> (&'static str, Option<Message>) {
    let build_active = app.binary_page.active_operation.is_some();
    if build_active && app.binary_page.active_kind == Some(kind) {
        ("Building…", None)
    } else if build_active {
        ("Build / Update", None)
    } else if !app.installation_recovery_ready() {
        match &app.installation_recovery {
            InstallationRecoveryState::Checking { .. } => ("Checking safety…", None),
            InstallationRecoveryState::Failed { .. } => ("Recovery required", None),
            InstallationRecoveryState::Ready { .. } => ("Build / Update", None),
        }
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
    }
}

fn version_value(label: &'static str, value: String) -> Element<'static, Message> {
    column![
        text(label).size(9).style(tertiary_text_style),
        text(value).size(14).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::default()
        }),
    ]
    .spacing(3)
    .into()
}

fn status_pill(label: &str, color: Color) -> Element<'_, Message> {
    container(text(label).size(10).style(semantic_text_style(color)))
        .padding(Padding::from([5, 10]))
        .style(move |theme| {
            let color = semantic_color(theme, color);
            container::Style {
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
            }
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
            text(target).size(16).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            }),
            text(stage_label).size(11).style(secondary_text_style),
        ]
        .spacing(3)
        .width(Length::Fill),
        if app.binary_page.active_operation.is_some() {
            styled_button("Cancel", ButtonStyle::Destructive)
                .on_press_maybe(app.binary_page.can_cancel().then_some(Message::CancelBuild))
        } else {
            styled_button("Refresh", ButtonStyle::Secondary).on_press_maybe(
                app.installation_recovery_ready()
                    .then_some(Message::RefreshBinaryInfo),
            )
        },
    ]
    .align_y(Alignment::Center);

    let progress = progress_bar(0.0..=1.0, app.binary_page.progress)
        .girth(8)
        .style(|theme| iced::widget::progress_bar::Style {
            background: ui_palette(theme).disabled.into(),
            bar: semantic_color(theme, MAC_BLUE).into(),
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
                    .style(tertiary_text_style),
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
        .style(|theme| container::Style {
            background: Some(ui_palette(theme).panel.into()),
            border: iced::Border {
                color: ui_palette(theme).border,
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
            text("INSTALL DESTINATION")
                .size(9)
                .style(tertiary_text_style),
            text(request.binaries_dir.display().to_string())
                .size(10)
                .font(Font::MONOSPACE)
                .style(secondary_text_style)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
            text("BUILD WORKSPACE").size(9).style(tertiary_text_style),
            text(request.workspace.display().to_string())
                .size(10)
                .font(Font::MONOSPACE)
                .style(secondary_text_style)
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
            items.push(text("→").size(10).style(tertiary_text_style).into());
        }
        let complete = current_rank >= build_stage_rank(item);
        items.push(
            text(item.label())
                .size(9)
                .style(semantic_text_style(if complete {
                    MAC_BLUE
                } else {
                    NEUTRAL_STATUS
                }))
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
            .style(secondary_text_style)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    )
    .width(Length::Fill)
    .padding(Padding::from([10, 12]))
    .style(move |theme| {
        let color = semantic_color(theme, color);
        container::Style {
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
        }
    })
    .into()
}

fn installation_recovery_notice(app: &App) -> Option<Element<'_, Message>> {
    match &app.installation_recovery {
        InstallationRecoveryState::Ready { destination }
            if destination == &app.config.binaries_path =>
        {
            None
        }
        InstallationRecoveryState::Checking { destination, .. }
            if destination == &app.config.binaries_path =>
        {
            Some(build_notice(
                "Checking for an interrupted binary installation. Node launch, inventory, and updates remain locked.",
                MAC_BLUE,
            ))
        }
        InstallationRecoveryState::Failed {
            destination,
            error,
        } if destination == &app.config.binaries_path => Some(
            container(
                row![
                    text(format!(
                        "Binary installation recovery failed. Node launch, inventory, and updates remain blocked: {error}"
                    ))
                    .size(11)
                    .style(secondary_text_style)
                    .width(Length::Fill)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                    styled_button("Retry", ButtonStyle::Warning)
                        .on_press(Message::RetryInstallationRecovery),
                ]
                .spacing(14)
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding(Padding::from([10, 20]))
            .style(|theme| {
                let color = semantic_color(theme, MAC_RED);
                container::Style {
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
                }
            })
            .into(),
        ),
        InstallationRecoveryState::Checking { .. }
        | InstallationRecoveryState::Ready { .. }
        | InstallationRecoveryState::Failed { .. } => Some(build_notice(
            "The configured binaries destination has not completed installation recovery. Node launch, inventory, and updates remain locked.",
            MAC_RED,
        )),
    }
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
            text("Advanced").size(13).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            }),
            text("Release selection, build performance, source retention, and output detail.")
                .size(10)
                .style(tertiary_text_style),
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
                text("BUILD WORKSPACE").size(9).style(tertiary_text_style),
                text(binaries_workspace_text(app))
                    .size(10)
                    .font(Font::MONOSPACE)
                    .style(secondary_text_style)
                    .width(Length::Fill)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                text("Node-only Bitcoin flags, source authentication, and transactional installation remain enabled for every build.")
                .size(10)
                .style(tertiary_text_style)
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
        .style(|theme| container::Style {
            background: Some(ui_palette(theme).panel.into()),
            border: iced::Border {
                color: ui_palette(theme).border,
                width: 1.0,
                radius: 12.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn advanced_build_settings(app: &App) -> Element<'_, Message> {
    let settings = app.config.build_settings;
    let settings_editable =
        app.config_updates_are_editable() && app.binary_page.active_operation.is_none();
    let performance_options: &[BuildPerformance] = if settings_editable {
        &BUILD_PERFORMANCE_OPTIONS
    } else {
        &[]
    };
    column![
        row![
            column![
                text("BUILD PERFORMANCE").size(9).style(tertiary_text_style),
                text("Controls parallel work without exposing compiler flags.")
                    .size(10)
                    .style(secondary_text_style),
            ]
            .spacing(3)
            .width(Length::Fill),
            pick_list(
                performance_options,
                Some(settings.performance),
                Message::BuildPerformanceChanged,
            )
            .width(150)
            .text_size(11)
            .style(move |theme, status| config_pick_list_style(theme, status, settings_editable)),
        ]
        .align_y(Alignment::Center),
        build_setting_toggle(
            "Keep Source Code",
            "Retain authenticated source after a successful installation for faster repeat builds.",
            settings.keep_source,
            Message::KeepSourceChanged,
            settings_editable,
        ),
        build_setting_toggle(
            "Clean Build",
            "Discard reusable compilation artifacts before building; authenticated source stays verified.",
            settings.clean_build,
            Message::CleanBuildChanged,
            settings_editable,
        ),
        build_setting_toggle(
            "Verbose Build Output",
            "Show complete compiler and build-system output while preserving the durable build log.",
            settings.verbose_output,
            Message::VerboseBuildOutputChanged,
            settings_editable,
        ),
        row![
            text(format!(
                "Bitcoin Core: {} workers  •  electrs: {} workers",
                super::build_worker_count(BinaryKind::BitcoinCore, settings.performance),
                super::build_worker_count(BinaryKind::Electrs, settings.performance),
            ))
            .size(10)
            .style(tertiary_text_style)
            .width(Length::Fill),
            styled_button("Restore Defaults", ButtonStyle::Secondary)
                .on_press_maybe(settings_editable.then_some(Message::RestoreBuildDefaults)),
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
            || ("Not checked".to_owned(), NEUTRAL_STATUS),
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
            text("Build Dependencies").size(12).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            }),
            text(status).size(10).style(semantic_text_style(color)),
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
        .style(|theme| container::Style {
            background: Some(ui_palette(theme).background.into()),
            border: iced::Border {
                color: ui_palette(theme).border,
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
                .style(tertiary_text_style),
            text(&report.guidance)
                .size(9)
                .style(tertiary_text_style)
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
                    text(item.name).size(10),
                    text(detected)
                        .size(9)
                        .font(Font::MONOSPACE)
                        .style(tertiary_text_style)
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
                .style(tertiary_text_style)
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
    column![text(label).size(9).style(tertiary_text_style), picker]
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
        text(APP_NAME).size(22).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::default()
        }),
        text("Bitcoin Core and Electrs control panel")
            .size(11)
            .style(secondary_text_style)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    ]
    .spacing(2)
    .width(Length::FillPortion(2));

    let block_stat = column![
        text("BLOCK HEIGHT").size(9).style(tertiary_text_style),
        text(height_text).size(18).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::default()
        }),
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
        theme_selector(app),
        Space::new().width(12),
        update_btn,
    ]
    .spacing(0)
    .align_y(Alignment::Center)
    .padding(Padding::from([0, 20]));

    container(toolbar_row)
        .width(Length::Fill)
        .height(64)
        .style(|theme| container::Style {
            background: Some(ui_palette(theme).bar.into()),
            ..Default::default()
        })
        .into()
}

fn theme_selector(app: &App) -> Element<'_, Message> {
    let editable = app.config_updates_are_editable();
    let options: &[ThemePreference] = if editable { &ThemePreference::ALL } else { &[] };
    column![
        text("THEME").size(9).style(tertiary_text_style),
        pick_list(
            options,
            Some(app.config.theme_preference),
            Message::ThemePreferenceChanged,
        )
        .width(104)
        .text_size(10)
        .style(move |theme, status| config_pick_list_style(theme, status, editable)),
    ]
    .spacing(3)
    .width(Length::Shrink)
    .into()
}

fn config_pick_list_style(
    theme: &iced::Theme,
    status: pick_list::Status,
    enabled: bool,
) -> pick_list::Style {
    let mut style = pick_list::default(theme, status);
    if !enabled {
        let palette = ui_palette(theme);
        style.text_color = palette.tertiary_text;
        style.placeholder_color = palette.tertiary_text;
        style.handle_color = palette.tertiary_text;
        style.background = palette.disabled.into();
        style.border.color = palette.border;
    }
    style
}

fn view_connect_to_node(app: &App) -> Element<'_, Message> {
    let readiness = app.connection_readiness();
    let (status_label, status_color) = connection_status(app, &readiness);
    let header = row![
        column![
            text("Wallet connection").size(17).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::default()
            }),
            text("Electrum access appears only when both services are ready.")
                .size(10)
                .style(secondary_text_style),
        ]
        .spacing(3)
        .width(Length::Fill),
        status_pill(status_label, status_color),
        Space::new().width(14),
        connection_mode_selector(app, &readiness),
    ]
    .align_y(Alignment::Center)
    .padding(Padding::from([12, 16]));

    let body = match app.selected_connection_mode {
        ConnectionMode::Local => view_local_connection(app, &readiness),
        ConnectionMode::Tor => view_tor_connection(app, &readiness),
    };
    container(column![header, horizontal_rule(), body])
        .width(Length::Fill)
        .style(|theme| container::Style {
            background: Some(ui_palette(theme).panel.into()),
            border: iced::Border {
                color: ui_palette(theme).border,
                width: 1.0,
                radius: 12.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn connection_mode_selector<'a>(
    app: &'a App,
    readiness: &ConnectionReadiness,
) -> Element<'a, Message> {
    row(ConnectionMode::ALL.into_iter().map(|mode| {
        connection_mode_button(
            mode,
            app.selected_connection_mode,
            connection_mode_label(app, mode, readiness),
        )
        .into()
    }))
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

fn connection_mode_button(
    mode: ConnectionMode,
    selected: ConnectionMode,
    label: &'static str,
) -> button::Button<'static, Message> {
    let is_selected = mode == selected;
    button(text(label).size(11).font(Font {
        weight: iced::font::Weight::Bold,
        ..Font::default()
    }))
    .padding(Padding::from([6, 16]))
    .on_press_maybe((!is_selected).then_some(Message::ConnectionModeChanged(mode)))
    .style(move |theme, status| {
        let palette = ui_palette(theme);
        let active = if status == button::Status::Hovered {
            darken(BUTTON_BLUE)
        } else {
            BUTTON_BLUE
        };
        button::Style {
            background: Some(
                if is_selected {
                    active
                } else if status == button::Status::Hovered {
                    palette.secondary_button_hover
                } else {
                    palette.secondary_button
                }
                .into(),
            ),
            text_color: if is_selected {
                Color::WHITE
            } else {
                theme.palette().text
            },
            border: iced::Border {
                color: if is_selected {
                    BUTTON_BLUE
                } else {
                    palette.border
                },
                width: 1.0,
                radius: 7.0.into(),
            },
            ..Default::default()
        }
    })
}

fn connection_mode_label(
    app: &App,
    mode: ConnectionMode,
    readiness: &ConnectionReadiness,
) -> &'static str {
    if mode == ConnectionMode::Local {
        return "Local";
    }
    if app.closing {
        return "Tor stopping";
    }
    if app.tor_runtime_stop_pending() {
        return "Tor stopping";
    }
    if app.tor_control_error.is_some() {
        return "Tor error";
    }
    if !app.config.tor_enabled {
        return "Tor off";
    }
    if app.tor_electrs_sync_error.is_some() {
        return "Tor error";
    }
    if matches!(&app.tor_status, TorStatus::Available { .. }) && readiness.is_ready() {
        return "Tor ready";
    }
    match &app.tor_status {
        TorStatus::Disabled => "Tor idle",
        TorStatus::Starting | TorStatus::Bootstrapping { .. } | TorStatus::Publishing { .. } => {
            "Tor starting"
        }
        TorStatus::WaitingForElectrs { .. } | TorStatus::Available { .. } => "Tor waiting",
        TorStatus::TemporarilyUnavailable { .. } => "Tor retrying",
        TorStatus::Error { .. } => "Tor error",
    }
}

fn connection_status(app: &App, readiness: &ConnectionReadiness) -> (&'static str, Color) {
    if app.selected_connection_mode == ConnectionMode::Tor {
        if app.closing {
            return ("Tor stopping", NEUTRAL_STATUS);
        }
        if app.tor_runtime_stop_pending() {
            return ("Tor stopping", NEUTRAL_STATUS);
        }
        if app.tor_control_error.is_some() {
            return ("Tor error", MAC_RED);
        }
        if !app.config.tor_enabled {
            return ("Tor disabled", NEUTRAL_STATUS);
        }
        if app.tor_electrs_sync_error.is_some() {
            return ("Tor error", MAC_RED);
        }
        return match &app.tor_status {
            TorStatus::Disabled => ("Tor not started", NEUTRAL_STATUS),
            TorStatus::Starting
            | TorStatus::Bootstrapping { .. }
            | TorStatus::Publishing { .. }
            | TorStatus::WaitingForElectrs { .. } => ("Tor starting", MAC_BLUE),
            TorStatus::Available { .. } if readiness.is_ready() => ("Tor ready", GREEN),
            TorStatus::Available { .. } => ("Waiting for node", MAC_BLUE),
            TorStatus::TemporarilyUnavailable { .. } => ("Tor unavailable", MAC_ORG),
            TorStatus::Error { .. } => ("Tor error", MAC_RED),
        };
    }
    match readiness {
        ConnectionReadiness::Ready => ("Ready", GREEN),
        ConnectionReadiness::BitcoinFailed { .. }
        | ConnectionReadiness::ElectrsUnavailable { .. } => ("Needs attention", MAC_RED),
        ConnectionReadiness::BitcoinSyncing { .. }
        | ConnectionReadiness::ElectrsIndexing { .. } => ("Syncing", MAC_BLUE),
        ConnectionReadiness::BitcoinStarting | ConnectionReadiness::ElectrsStarting => {
            ("Starting", MAC_BLUE)
        }
        ConnectionReadiness::ServicesStopped | ConnectionReadiness::ElectrsStopped => {
            ("Not ready", NEUTRAL_STATUS)
        }
    }
}

fn view_local_connection<'a>(
    app: &'a App,
    readiness: &ConnectionReadiness,
) -> Element<'a, Message> {
    let local = app.local_endpoint_state();
    let visual = if local_qr_available(readiness, &local) {
        view_connection_qr(app)
    } else {
        let (title, detail) = if readiness.is_ready()
            && matches!(local, LocalEndpointState::SameMachineOnly { .. })
        {
            ("This device only", "LAN QR hidden")
        } else {
            ("Connection pending", "QR appears when reachable")
        };
        connection_placeholder(title, detail)
    };

    let mut details: Vec<Element<Message>> = vec![connection_readiness_detail(readiness)];
    if readiness.is_ready() {
        if app.connection_endpoint.is_some() {
            details.push(endpoint_details(
                app,
                if local.is_lan_reachable() {
                    "Reachable on your local network"
                } else {
                    "This computer only"
                },
                local.is_lan_reachable(),
            ));
        }
        if let Some(message) = local.message() {
            details.push(
                text(message)
                    .size(10)
                    .style(tertiary_text_style)
                    .width(Length::Fill)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            );
        }
    }
    details.push(local_network_control(app));

    row![visual, column(details).spacing(7).width(Length::Fill)]
        .spacing(12)
        .align_y(Alignment::Center)
        .padding(Padding::from([10, 12]))
        .into()
}

const fn local_qr_available(readiness: &ConnectionReadiness, local: &LocalEndpointState) -> bool {
    readiness.is_ready() && local.is_lan_reachable()
}

fn view_tor_connection<'a>(app: &'a App, readiness: &ConnectionReadiness) -> Element<'a, Message> {
    let presentation = tor_presentation(app, readiness);
    let endpoint_ready = app.available_tor_endpoint().is_some()
        && readiness.is_ready()
        && app.connection_endpoint.is_some();
    let visual = if endpoint_ready {
        view_connection_qr(app)
    } else {
        connection_placeholder(
            presentation.title.clone(),
            presentation.visual_detail.clone(),
        )
    };
    let mut details: Vec<Element<Message>> = vec![connection_readiness_detail(readiness)];
    if endpoint_ready {
        details.push(endpoint_details(app, "Available over Tor", true));
    } else {
        details.push(
            text(presentation.detail)
                .size(10)
                .style(secondary_text_style)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
        );
        if let Some(progress) = presentation.progress {
            details.push(
                progress_bar(0.0..=1.0, progress)
                    .girth(7)
                    .style(|theme| iced::widget::progress_bar::Style {
                        background: ui_palette(theme).disabled.into(),
                        bar: semantic_color(theme, MAC_BLUE).into(),
                        border: iced::Border {
                            radius: 4.0.into(),
                            ..Default::default()
                        },
                    })
                    .into(),
            );
        }
    }
    details.push(tor_controls(app, presentation.action));

    row![visual, column(details).spacing(7).width(Length::Fill)]
        .spacing(12)
        .align_y(Alignment::Center)
        .padding(Padding::from([10, 12]))
        .into()
}

struct TorPresentation {
    title: String,
    visual_detail: String,
    detail: String,
    progress: Option<f32>,
    action: Option<(&'static str, Message)>,
}

fn tor_presentation(app: &App, readiness: &ConnectionReadiness) -> TorPresentation {
    if app.closing {
        return TorPresentation {
            title: "Stopping Tor".to_owned(),
            visual_detail: "Closing safely".to_owned(),
            detail: "Removing the onion publication and stopping the embedded Tor client before BitEngine closes…"
                .to_owned(),
            progress: None,
            action: None,
        };
    }
    if app.tor_runtime_stop_pending() {
        return TorPresentation {
            title: "Stopping Tor".to_owned(),
            visual_detail: "Remote endpoint closing".to_owned(),
            detail: "Removing the onion publication and disabling remote access. The persistent onion identity is retained…"
                .to_owned(),
            progress: None,
            action: None,
        };
    }
    if let Some(error) = app.tor_control_error.as_deref() {
        return TorPresentation {
            title: "Tor control error".to_owned(),
            visual_detail: if app.config.tor_enabled {
                "Retry available".to_owned()
            } else {
                "Remote state uncertain".to_owned()
            },
            detail: error.to_owned(),
            progress: None,
            action: (app.config.tor_enabled && app.tor_manager.is_some())
                .then_some(("Retry", Message::RetryTor)),
        };
    }
    if !app.config.tor_enabled {
        return TorPresentation {
            title: "Tor disabled".to_owned(),
            visual_detail: "Remote endpoint off".to_owned(),
            detail: "Enable Tor to prepare a persistent v3 onion endpoint. Existing identity material is retained while disabled."
                .to_owned(),
            progress: None,
            action: None,
        };
    }
    if app.tor_manager_starting {
        return TorPresentation {
            title: "Preparing Tor".to_owned(),
            visual_detail: "Manager starting".to_owned(),
            detail: "Preparing BitEngine's private Tor storage and lifecycle manager…".to_owned(),
            progress: None,
            action: None,
        };
    }
    if let Some(error) = app.tor_electrs_sync_error.as_deref() {
        return TorPresentation {
            title: "Tor control error".to_owned(),
            visual_detail: "Retry available".to_owned(),
            detail: error.to_owned(),
            progress: None,
            action: app
                .tor_manager
                .is_some()
                .then_some(("Retry", Message::RetryTor)),
        };
    }
    tor_status_presentation(&app.tor_status, readiness, app.tor_manager.is_some())
}

fn tor_status_presentation(
    status: &TorStatus,
    readiness: &ConnectionReadiness,
    control_available: bool,
) -> TorPresentation {
    match status {
        TorStatus::Disabled => TorPresentation {
            title: "Tor not started".to_owned(),
            visual_detail: "Manual start".to_owned(),
            detail: "Tor access is configured but not running. Start it when remote access is needed."
                .to_owned(),
            progress: None,
            action: control_available.then_some(("Start Tor", Message::StartTor)),
        },
        TorStatus::Starting => TorPresentation {
            title: "Starting Tor".to_owned(),
            visual_detail: "Initializing Arti".to_owned(),
            detail: "Initializing the embedded Tor client and persistent onion service…".to_owned(),
            progress: None,
            action: None,
        },
        TorStatus::Bootstrapping { progress, summary } => TorPresentation {
            title: "Bootstrapping Tor".to_owned(),
            visual_detail: format!("{progress}% complete"),
            detail: summary
                .clone()
                .unwrap_or_else(|| "Downloading and validating Tor network state…".to_owned()),
            progress: Some(f32::from(*progress) / 100.0),
            action: None,
        },
        TorStatus::Publishing { onion_host } => TorPresentation {
            title: "Publishing onion service".to_owned(),
            visual_detail: "Descriptor pending".to_owned(),
            detail: format!("Publishing {onion_host} to the Tor network…"),
            progress: None,
            action: None,
        },
        TorStatus::WaitingForElectrs { onion_host } => TorPresentation {
            title: "Waiting for electrs".to_owned(),
            visual_detail: "Onion identity ready".to_owned(),
            detail: format!(
                "The persistent onion hostname is {onion_host}, but it will not be advertised as usable until {}",
                readiness.message().to_lowercase()
            ),
            progress: None,
            action: None,
        },
        TorStatus::Available { onion_host } if readiness.is_ready() => TorPresentation {
            title: "Tor available".to_owned(),
            visual_detail: "Onion endpoint ready".to_owned(),
            detail: format!("{onion_host} is published and ready for Electrum TCP wallets."),
            progress: None,
            action: None,
        },
        TorStatus::Available { onion_host } => TorPresentation {
            title: "Waiting for node".to_owned(),
            visual_detail: "Endpoint gated".to_owned(),
            detail: format!(
                "Tor reports {onion_host} as published, but BitEngine is withholding connection details until node readiness is reconfirmed."
            ),
            progress: None,
            action: None,
        },
        TorStatus::TemporarilyUnavailable {
            message, retry_in, ..
        } => TorPresentation {
            title: "Tor temporarily unavailable".to_owned(),
            visual_detail: retry_in.map_or_else(
                || "Recovery pending".to_owned(),
                |delay| format!("Retry in {}s", delay.as_secs()),
            ),
            detail: message.clone(),
            progress: None,
            action: Some(("Retry now", Message::RetryTor)),
        },
        TorStatus::Error { message, retryable } => TorPresentation {
            title: "Tor error".to_owned(),
            visual_detail: if *retryable {
                "Retry available".to_owned()
            } else {
                "Correction required".to_owned()
            },
            detail: message.clone(),
            progress: None,
            action: (*retryable).then_some(("Retry", Message::RetryTor)),
        },
    }
}

fn tor_controls<'a>(app: &'a App, action: Option<(&'static str, Message)>) -> Element<'a, Message> {
    let enable_control_available = tor_enable_control_available(app);
    let config_editable = app.config_updates_are_editable();
    let action: Element<Message> = action.map_or_else(
        || Space::new().width(0).into(),
        |(label, message)| {
            styled_button(label, ButtonStyle::Primary)
                .on_press(message)
                .into()
        },
    );
    row![
        column![
            text("Tor remote access").size(10),
            text("Only electrs port 50001 is published; disabling retains the onion identity.")
                .size(9)
                .style(tertiary_text_style)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(2)
        .width(Length::Fill),
        action,
        Space::new().width(10),
        column![
            text("Enabled").size(8).style(tertiary_text_style),
            toggler(app.config.tor_enabled)
                .on_toggle_maybe(enable_control_available.then_some(Message::TorEnabledChanged),)
                .size(18),
        ]
        .spacing(2)
        .align_x(Alignment::Center),
        Space::new().width(10),
        column![
            text("Auto-start").size(8).style(tertiary_text_style),
            toggler(app.config.tor_auto_start)
                .on_toggle_maybe(
                    (app.config.tor_enabled && config_editable)
                        .then_some(Message::TorAutoStartChanged),
                )
                .size(18),
        ]
        .spacing(2)
        .align_x(Alignment::Center),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

const fn tor_enable_control_available(app: &App) -> bool {
    !app.closing
        && app.config_updates_are_editable()
        && (app.config.tor_enabled || app.tor_manager_starting || app.tor_manager.is_some())
}

fn connection_readiness_detail(readiness: &ConnectionReadiness) -> Element<'static, Message> {
    let message = readiness.message();
    let detail = match readiness {
        ConnectionReadiness::BitcoinSyncing {
            blocks, headers, ..
        } => Some(format!(
            "{blocks} blocks verified • {headers} headers known"
        )),
        ConnectionReadiness::ElectrsIndexing {
            indexed_height,
            bitcoin_height,
            ..
        } => Some(format!(
            "Indexed {} • Bitcoin height {}",
            indexed_height.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
            bitcoin_height.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
        )),
        ConnectionReadiness::ServicesStopped
        | ConnectionReadiness::BitcoinStarting
        | ConnectionReadiness::BitcoinFailed { .. }
        | ConnectionReadiness::ElectrsStopped
        | ConnectionReadiness::ElectrsStarting
        | ConnectionReadiness::ElectrsUnavailable { .. }
        | ConnectionReadiness::Ready => None,
    };
    let mut content: Vec<Element<Message>> = vec![text(message)
        .size(12)
        .font(Font {
            weight: iced::font::Weight::Semibold,
            ..Font::default()
        })
        .width(Length::Fill)
        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
        .into()];
    if let Some(progress) = connection_progress(readiness) {
        content.push(
            progress_bar(0.0..=1.0, progress)
                .girth(7)
                .style(|theme| iced::widget::progress_bar::Style {
                    background: ui_palette(theme).disabled.into(),
                    bar: semantic_color(theme, MAC_BLUE).into(),
                    border: iced::Border {
                        radius: 4.0.into(),
                        ..Default::default()
                    },
                })
                .into(),
        );
    }
    if let Some(detail) = detail {
        content.push(text(detail).size(9).style(tertiary_text_style).into());
    }
    column(content).spacing(5).width(Length::Fill).into()
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "Iced progress bars use f32; readiness percentages are bounded display-only f64 values"
)]
fn connection_progress(readiness: &ConnectionReadiness) -> Option<f32> {
    match readiness {
        ConnectionReadiness::BitcoinSyncing { percent, .. }
        | ConnectionReadiness::ElectrsIndexing {
            percent: Some(percent),
            ..
        } => Some((*percent as f32) / 100.0),
        ConnectionReadiness::ServicesStopped
        | ConnectionReadiness::BitcoinStarting
        | ConnectionReadiness::BitcoinFailed { .. }
        | ConnectionReadiness::ElectrsStopped
        | ConnectionReadiness::ElectrsStarting
        | ConnectionReadiness::ElectrsIndexing { percent: None, .. }
        | ConnectionReadiness::ElectrsUnavailable { .. }
        | ConnectionReadiness::Ready => None,
    }
}

fn endpoint_details<'a>(
    app: &'a App,
    reachability: &'static str,
    remotely_reachable: bool,
) -> Element<'a, Message> {
    let Some(endpoint) = app.connection_endpoint.as_ref() else {
        return connection_unavailable("Preparing connection details…");
    };
    let Some(payload) = app.connection_endpoint_payload.as_deref() else {
        return connection_unavailable("Preparing connection details…");
    };
    let copied = app.copied_endpoint_at.is_some();
    column![
        row![
            text(endpoint.protocol_label())
                .size(10)
                .style(secondary_text_style),
            Space::new().width(8),
            status_pill(
                reachability,
                if remotely_reachable {
                    GREEN
                } else {
                    NEUTRAL_STATUS
                },
            ),
            Space::new().width(Length::Fill),
            text(format!("Port {}", endpoint.port()))
                .size(10)
                .style(tertiary_text_style),
        ]
        .align_y(Alignment::Center),
        text(endpoint.host())
            .size(10)
            .font(Font::MONOSPACE)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        row![
            text_input("Electrum endpoint", payload)
                .font(Font::MONOSPACE)
                .size(11)
                .padding(Padding::from([7, 9]))
                .style(read_only_input_style),
            Space::new().width(8),
            styled_button(
                if copied { "Copied ✓" } else { "Copy" },
                if copied {
                    ButtonStyle::Confirm
                } else {
                    ButtonStyle::Secondary
                },
            )
            .on_press(Message::CopySelectedEndpoint),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(5)
    .width(Length::Fill)
    .into()
}

fn read_only_input_style(
    theme: &iced::Theme,
    _: iced::widget::text_input::Status,
) -> iced::widget::text_input::Style {
    let mut style =
        iced::widget::text_input::default(theme, iced::widget::text_input::Status::Active);
    style.border.color = ui_palette(theme).border;
    style.border.radius = 6.0.into();
    style.value = theme.palette().text;
    style
}

fn local_network_control(app: &App) -> Element<'_, Message> {
    let locked = app.electrs_handle.is_some()
        || app.electrs_shutdown.is_some()
        || app.electrs_launch_pending_for_lan
        || !app.config_updates_are_editable();
    let description = if !app.config_updates_are_editable() {
        PENDING_PATH_SAVE_SETTINGS_MESSAGE
    } else if app.electrs_launch_pending_for_lan {
        "Determining a private local address before electrs starts…"
    } else if locked {
        "Stop electrs before changing this setting; changes apply on its next launch."
    } else if app.config.local_network_access {
        "Enabled for the next electrs launch on one validated private interface address."
    } else {
        "Off — wallets on this computer can still use the loopback endpoint."
    };
    row![
        column![
            text("Local network access").size(10),
            text(description)
                .size(9)
                .style(tertiary_text_style)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(2)
        .width(Length::Fill),
        toggler(app.config.local_network_access)
            .on_toggle_maybe((!locked).then_some(Message::LocalNetworkAccessChanged))
            .size(18),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .into()
}

fn view_connection_qr(app: &App) -> Element<'_, Message> {
    let Some(qr) = app.connection_qr.as_ref() else {
        return connection_placeholder(
            "Preparing QR",
            app.connection_qr_error
                .as_deref()
                .unwrap_or("Encoding endpoint")
                .to_owned(),
        );
    };
    container(ConnectionQrGrid::new(qr))
        .width(142)
        .height(142)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .padding(10)
        .style(|_| container::Style {
            background: Some(Color::WHITE.into()),
            border: iced::Border {
                color: Color::from_rgb8(195, 195, 200),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}

const CONNECTION_QR_SIZE: f32 = 122.0;
const CONNECTION_QR_QUIET_ZONE: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
struct ConnectionQrGeometry {
    origin: Point,
    cell_size: f32,
    side_length: f32,
}

fn connection_qr_geometry(width: usize, bounds: Rectangle) -> Option<ConnectionQrGeometry> {
    let modules =
        u16::try_from(width.checked_add(CONNECTION_QR_QUIET_ZONE.checked_mul(2)?)?).ok()?;
    if modules == 0 {
        return None;
    }

    let cell_size = (bounds.width.min(bounds.height) / f32::from(modules)).floor();
    if cell_size < 1.0 {
        return None;
    }

    let side_length = f32::from(modules) * cell_size;
    Some(ConnectionQrGeometry {
        origin: Point::new(
            (bounds.center_x() - side_length / 2.0).floor(),
            (bounds.center_y() - side_length / 2.0).floor(),
        ),
        cell_size,
        side_length,
    })
}

fn for_each_connection_qr_dark_run(qr: &ConnectionQr, mut visit: impl FnMut(usize, usize, usize)) {
    if qr.width == 0 || qr.cells.len() != qr.width.saturating_mul(qr.width) {
        return;
    }

    for (row, cells) in qr.cells.chunks_exact(qr.width).enumerate() {
        let mut column = 0;
        while column < cells.len() {
            if !cells[column] {
                column += 1;
                continue;
            }

            let start = column;
            while column < cells.len() && cells[column] {
                column += 1;
            }
            visit(row, start, column - start);
        }
    }
}

struct ConnectionQrGrid<'a> {
    qr: &'a ConnectionQr,
}

impl<'a> ConnectionQrGrid<'a> {
    const fn new(qr: &'a ConnectionQr) -> Self {
        Self { qr }
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for ConnectionQrGrid<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(CONNECTION_QR_SIZE),
            Length::Fixed(CONNECTION_QR_SIZE),
        )
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(
            Length::Fixed(CONNECTION_QR_SIZE),
            Length::Fixed(CONNECTION_QR_SIZE),
            Size::ZERO,
        ))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &iced::Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                ..renderer::Quad::default()
            },
            Color::WHITE,
        );

        let Some(geometry) = connection_qr_geometry(self.qr.width, bounds) else {
            return;
        };
        for_each_connection_qr_dark_run(self.qr, |row, start, length| {
            let (Ok(module_column), Ok(module_row), Ok(run_length)) = (
                u16::try_from(start + CONNECTION_QR_QUIET_ZONE),
                u16::try_from(row + CONNECTION_QR_QUIET_ZONE),
                u16::try_from(length),
            ) else {
                return;
            };
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(
                            f32::from(module_column).mul_add(geometry.cell_size, geometry.origin.x),
                            f32::from(module_row).mul_add(geometry.cell_size, geometry.origin.y),
                        ),
                        Size::new(
                            f32::from(run_length) * geometry.cell_size,
                            geometry.cell_size,
                        ),
                    ),
                    ..renderer::Quad::default()
                },
                Color::BLACK,
            );
        });
    }
}

impl<'a> From<ConnectionQrGrid<'a>> for Element<'a, Message> {
    fn from(qr: ConnectionQrGrid<'a>) -> Self {
        Self::new(qr)
    }
}

fn connection_placeholder(
    title: impl Into<String>,
    detail: impl Into<String>,
) -> Element<'static, Message> {
    container(
        column![
            text("◇").size(23).style(tertiary_text_style),
            text(title.into()).size(11).font(Font {
                weight: iced::font::Weight::Semibold,
                ..Font::default()
            }),
            text(detail.into())
                .size(9)
                .style(tertiary_text_style)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(4)
        .align_x(Alignment::Center),
    )
    .width(100)
    .height(100)
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .padding(10)
    .style(|theme| container::Style {
        background: Some(ui_palette(theme).indicator.into()),
        border: iced::Border {
            color: ui_palette(theme).border,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn connection_unavailable(message: &str) -> Element<'_, Message> {
    text(message)
        .size(10)
        .style(secondary_text_style)
        .width(Length::Fill)
        .into()
}

fn view_paths_panel(app: &App) -> Element<'_, Message> {
    let paths_editable = app.paths_are_editable();
    let toggle_label = if app.paths_visible {
        "Collapse"
    } else {
        "Edit Paths"
    };

    let heading = column![
        text("Storage & directories").size(15).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::default()
        }),
        text("Binary, blockchain, index, and configuration locations")
            .size(10)
            .style(secondary_text_style)
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
            .style(|theme| container::Style {
                background: Some(ui_palette(theme).panel.into()),
                border: iced::Border {
                    color: ui_palette(theme).border,
                    width: 1.0,
                    radius: 12.0.into(),
                },
                ..container::Style::default()
            })
            .into();
    }

    let body = column![header, view_path_rows(app, paths_editable)].padding(Padding {
        top: 0.0,
        right: 0.0,
        bottom: 4.0,
        left: 0.0,
    });

    container(body)
        .width(Length::Fill)
        .style(|theme| container::Style {
            background: Some(ui_palette(theme).panel.into()),
            border: iced::Border {
                color: ui_palette(theme).border,
                width: 1.0,
                radius: 12.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

fn view_path_rows(app: &App, paths_editable: bool) -> Element<'_, Message> {
    column![
        path_row(
            "Binaries Folder",
            "Folder containing bitcoind, bitcoin-cli, and electrs",
            &app.binaries_path_edit,
            Message::BinariesPathChanged,
            Message::BrowseBinaries,
            binaries_path_status(app),
            paths_editable,
        ),
        path_row(
            "Bitcoin Data Directory",
            "Bitcoin Core data directory",
            &app.bitcoin_data_path_edit,
            Message::BitcoinDataPathChanged,
            Message::BrowseBitcoinData,
            configured_path_status(
                app,
                &app.bitcoin_data_path_edit,
                &app.config.bitcoin_data_path,
            ),
            paths_editable,
        ),
        path_row(
            "Electrs DB Directory",
            "Electrs index database directory",
            &app.electrs_data_path_edit,
            Message::ElectrsDataPathChanged,
            Message::BrowseElectrsData,
            configured_path_status(
                app,
                &app.electrs_data_path_edit,
                &app.config.electrs_data_path,
            ),
            paths_editable,
        ),
        text(format!(
            "Config file: {}",
            Config::config_file_path().display()
        ))
        .size(9)
        .style(tertiary_text_style)
        .width(Length::Fill)
        .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        row![
            text(paths_status_text(app))
                .size(10)
                .style(tertiary_text_style),
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
    .padding(Padding::from([0, 20]))
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

fn binaries_path_status(app: &App) -> (&'static str, bool) {
    if app.binaries_path_edit != app.config.binaries_path.to_string_lossy() {
        return ("Edited", false);
    }
    if app.installation_recovery_ready() {
        return ("Validated", true);
    }
    match app.installation_recovery {
        InstallationRecoveryState::Checking { .. } => ("Checking", false),
        InstallationRecoveryState::Ready { .. } | InstallationRecoveryState::Failed { .. } => {
            ("Blocked", false)
        }
    }
}

fn configured_path_status(
    app: &App,
    edited: &str,
    saved: &std::path::Path,
) -> (&'static str, bool) {
    if edited != saved.to_string_lossy() {
        return ("Edited", false);
    }
    if app.installation_recovery_ready() {
        return ("Validated", true);
    }
    match app.installation_recovery {
        InstallationRecoveryState::Checking { .. } => ("Checking", false),
        InstallationRecoveryState::Ready { .. } | InstallationRecoveryState::Failed { .. } => {
            ("Blocked", false)
        }
    }
}

const fn installation_recovery_short_hint(app: &App) -> &'static str {
    match app.installation_recovery {
        InstallationRecoveryState::Checking { .. } => {
            "Checking binary installation safety before launch."
        }
        InstallationRecoveryState::Failed { .. } => {
            "Binary installation recovery must succeed before launch."
        }
        InstallationRecoveryState::Ready { .. } => {
            "The configured binary destination has not completed safety validation."
        }
    }
}

fn bitcoin_launch_action(app: &App) -> Option<Message> {
    (app.bitcoin_handle.is_none()
        && app.bitcoin_shutdown.is_none()
        && app.electrs_handle.is_none()
        && app.electrs_shutdown.is_none()
        && app.installation_recovery_ready())
    .then_some(Message::LaunchBitcoin)
}

fn electrs_launch_action(app: &App) -> Option<Message> {
    (app.bitcoin_handle.is_some()
        && app.bitcoin_shutdown.is_none()
        && app.bitcoin_ready()
        && app.electrs_handle.is_none()
        && app.electrs_shutdown.is_none()
        && app.installation_recovery_ready())
    .then_some(Message::LaunchElectrs)
}

#[derive(Debug, Clone, PartialEq)]
struct NodeStatePresentation {
    label: &'static str,
    color: Color,
    detail: String,
}

#[derive(Clone, Copy)]
struct NodeIndicator {
    label: &'static str,
    value: &'static str,
    color: Color,
}

fn bitcoin_node_presentation(app: &App) -> NodeStatePresentation {
    if app.bitcoin_shutdown.is_some() {
        return NodeStatePresentation {
            label: "Stopping",
            color: NEUTRAL_STATUS,
            detail: "Graceful Bitcoin Core shutdown is in progress.".to_owned(),
        };
    }
    if let Some(error) = app.bitcoin_process_error.as_ref() {
        return NodeStatePresentation {
            label: "Failed",
            color: MAC_RED,
            detail: error.clone(),
        };
    }
    if !app.bitcoin_running {
        return NodeStatePresentation {
            label: "Stopped",
            color: NEUTRAL_STATUS,
            detail: "Launch Bitcoin Core to begin node synchronization.".to_owned(),
        };
    }
    if let Some(error) = app.bitcoin_compatibility_error.as_ref() {
        return NodeStatePresentation {
            label: "Unavailable",
            color: MAC_RED,
            detail: error.clone(),
        };
    }
    if let Some(error) = app
        .bitcoin_rpc_error
        .as_ref()
        .or(app.bitcoin_p2p_error.as_ref())
    {
        return NodeStatePresentation {
            label: "Retrying",
            color: MAC_ORG,
            detail: error.clone(),
        };
    }

    match app.connection_readiness() {
        ConnectionReadiness::BitcoinStarting => NodeStatePresentation {
            label: "Starting",
            color: MAC_BLUE,
            detail: app
                .bitcoin_rpc_startup_status
                .clone()
                .unwrap_or_else(|| "Waiting for authenticated RPC and chain status.".to_owned()),
        },
        ConnectionReadiness::BitcoinSyncing {
            percent,
            blocks,
            headers,
        } => NodeStatePresentation {
            label: "Synchronizing",
            color: MAC_BLUE,
            detail: format!("{percent:.1}% complete · {blocks} blocks · {headers} headers"),
        },
        ConnectionReadiness::BitcoinFailed { reason } => NodeStatePresentation {
            label: "Unavailable",
            color: MAC_RED,
            detail: reason,
        },
        ConnectionReadiness::ServicesStopped => NodeStatePresentation {
            label: "Stopped",
            color: NEUTRAL_STATUS,
            detail: "Bitcoin Core is not running.".to_owned(),
        },
        ConnectionReadiness::ElectrsStopped
        | ConnectionReadiness::ElectrsStarting
        | ConnectionReadiness::ElectrsIndexing { .. }
        | ConnectionReadiness::ElectrsUnavailable { .. }
        | ConnectionReadiness::Ready => {
            if app.bitcoin_synced && app.bitcoin_ready() {
                NodeStatePresentation {
                    label: "Ready",
                    color: GREEN,
                    detail: "Chain synchronized; authenticated RPC and P2P checks passed."
                        .to_owned(),
                }
            } else {
                NodeStatePresentation {
                    label: "Running",
                    color: MAC_BLUE,
                    detail: "Bitcoin Core is running while readiness checks complete.".to_owned(),
                }
            }
        }
    }
}

fn electrs_node_presentation(app: &App) -> NodeStatePresentation {
    if app.electrs_shutdown.is_some() {
        return NodeStatePresentation {
            label: "Stopping",
            color: NEUTRAL_STATUS,
            detail: "Graceful Electrs shutdown is in progress.".to_owned(),
        };
    }
    if let Some(error) = app.electrs_process_error.as_ref() {
        return NodeStatePresentation {
            label: "Failed",
            color: MAC_RED,
            detail: error.clone(),
        };
    }
    if let Some(error) = app.electrs_listener_invalidation.as_ref() {
        return NodeStatePresentation {
            label: "Unavailable",
            color: MAC_RED,
            detail: error.clone(),
        };
    }
    if app.electrs_launch_pending_for_lan {
        return NodeStatePresentation {
            label: "Starting",
            color: MAC_BLUE,
            detail: "Resolving the validated local-network listener before launch.".to_owned(),
        };
    }
    if !app.electrs_status.running {
        return NodeStatePresentation {
            label: "Stopped",
            color: NEUTRAL_STATUS,
            detail: "Start Electrs after Bitcoin Core is ready.".to_owned(),
        };
    }
    if let Some(error) = electrs_retry_error(&app.electrs_status) {
        return NodeStatePresentation {
            label: "Retrying",
            color: MAC_ORG,
            detail: error.to_owned(),
        };
    }
    if app.electrs_status.is_connection_ready() {
        return NodeStatePresentation {
            label: "Ready",
            color: GREEN,
            detail: "Index synchronized; Electrum protocol checks passed for wallet access."
                .to_owned(),
        };
    }
    if app.electrs_status.sync_percent.is_some()
        || app.electrs_status.electrs_height.is_some()
        || app.electrs_status.connected
    {
        return NodeStatePresentation {
            label: "Synchronizing",
            color: MAC_BLUE,
            detail: electrs_sync_detail(&app.electrs_status),
        };
    }

    NodeStatePresentation {
        label: "Starting",
        color: MAC_BLUE,
        detail: "Waiting for Bitcoin connectivity, metrics, and Electrum protocol checks."
            .to_owned(),
    }
}

fn electrs_retry_error(status: &ElectrsStatus) -> Option<&str> {
    status
        .bitcoin_error
        .as_deref()
        .or(status.metrics_error.as_deref())
        .or(status.connect_error.as_deref())
}

fn electrs_sync_detail(status: &ElectrsStatus) -> String {
    match (
        status.sync_percent,
        status.electrs_height,
        status.bitcoin_blocks,
    ) {
        (Some(percent), Some(indexed), Some(bitcoin)) => {
            format!("{percent:.1}% indexed · Electrs {indexed} · Bitcoin {bitcoin}")
        }
        (Some(percent), _, _) => format!("{percent:.1}% of the Bitcoin chain indexed"),
        (_, Some(indexed), Some(bitcoin)) => {
            format!("Index height {indexed} · Bitcoin height {bitcoin}")
        }
        _ => "Electrs is connected to Bitcoin Core and building its index.".to_owned(),
    }
}

const fn bitcoin_node_indicators(app: &App) -> [NodeIndicator; 3] {
    if app.bitcoin_shutdown.is_some() {
        return [
            NodeIndicator {
                label: "Process",
                value: "Stopping",
                color: NEUTRAL_STATUS,
            },
            NodeIndicator {
                label: "RPC + P2P",
                value: "Closing",
                color: NEUTRAL_STATUS,
            },
            NodeIndicator {
                label: "Chain",
                value: "Paused",
                color: NEUTRAL_STATUS,
            },
        ];
    }

    let process = if app.bitcoin_process_error.is_some() {
        NodeIndicator {
            label: "Process",
            value: "Failed",
            color: MAC_RED,
        }
    } else if app.bitcoin_running {
        NodeIndicator {
            label: "Process",
            value: "Running",
            color: GREEN,
        }
    } else {
        NodeIndicator {
            label: "Process",
            value: "Stopped",
            color: NEUTRAL_STATUS,
        }
    };
    let network = if app.bitcoin_ready() {
        NodeIndicator {
            label: "RPC + P2P",
            value: "Ready",
            color: GREEN,
        }
    } else if app.bitcoin_compatibility_error.is_some()
        || app.bitcoin_rpc_error.is_some()
        || app.bitcoin_p2p_error.is_some()
    {
        NodeIndicator {
            label: "RPC + P2P",
            value: "Retrying",
            color: MAC_ORG,
        }
    } else if app.bitcoin_running {
        NodeIndicator {
            label: "RPC + P2P",
            value: "Checking",
            color: MAC_BLUE,
        }
    } else {
        NodeIndicator {
            label: "RPC + P2P",
            value: "Offline",
            color: NEUTRAL_STATUS,
        }
    };
    let chain = if app.bitcoin_synced {
        NodeIndicator {
            label: "Chain",
            value: "Synced",
            color: GREEN,
        }
    } else if app.bitcoin_running {
        NodeIndicator {
            label: "Chain",
            value: "Syncing",
            color: MAC_BLUE,
        }
    } else {
        NodeIndicator {
            label: "Chain",
            value: "Waiting",
            color: NEUTRAL_STATUS,
        }
    };

    [process, network, chain]
}

fn electrs_node_indicators(app: &App) -> [NodeIndicator; 3] {
    if app.electrs_shutdown.is_some() {
        return [
            NodeIndicator {
                label: "Process",
                value: "Stopping",
                color: NEUTRAL_STATUS,
            },
            NodeIndicator {
                label: "Index",
                value: "Paused",
                color: NEUTRAL_STATUS,
            },
            NodeIndicator {
                label: "Electrum",
                value: "Closing",
                color: NEUTRAL_STATUS,
            },
        ];
    }

    let process =
        if app.electrs_process_error.is_some() || app.electrs_listener_invalidation.is_some() {
            NodeIndicator {
                label: "Process",
                value: "Failed",
                color: MAC_RED,
            }
        } else if app.electrs_status.running {
            NodeIndicator {
                label: "Process",
                value: "Running",
                color: GREEN,
            }
        } else {
            NodeIndicator {
                label: "Process",
                value: "Stopped",
                color: NEUTRAL_STATUS,
            }
        };
    let index = if app.electrs_status.synced {
        NodeIndicator {
            label: "Index",
            value: "Synced",
            color: GREEN,
        }
    } else if app.electrs_status.running {
        NodeIndicator {
            label: "Index",
            value: "Indexing",
            color: MAC_BLUE,
        }
    } else {
        NodeIndicator {
            label: "Index",
            value: "Waiting",
            color: NEUTRAL_STATUS,
        }
    };
    let service = if app.electrs_status.is_connection_ready() {
        NodeIndicator {
            label: "Electrum",
            value: "Ready",
            color: GREEN,
        }
    } else if electrs_retry_error(&app.electrs_status).is_some() {
        NodeIndicator {
            label: "Electrum",
            value: "Retrying",
            color: MAC_ORG,
        }
    } else if app.electrs_status.running {
        NodeIndicator {
            label: "Electrum",
            value: "Checking",
            color: MAC_BLUE,
        }
    } else {
        NodeIndicator {
            label: "Electrum",
            value: "Offline",
            color: NEUTRAL_STATUS,
        }
    };

    [process, index, service]
}

fn view_node_panels(app: &App) -> Element<'_, Message> {
    let installation_ready = app.installation_recovery_ready();
    let bitcoin_panel = view_node_panel(NodePanelSpec {
        title: "Bitcoin Core",
        subtitle: "Full node · consensus and peer network",
        terminal_label: "BITCOIN CORE LOG",
        accent: BTC_ACC,
        state: bitcoin_node_presentation(app),
        indicators: bitcoin_node_indicators(app),
        launch_action: bitcoin_launch_action(app),
        launch_hint: if !installation_ready {
            installation_recovery_short_hint(app)
        } else if app.bitcoin_shutdown.is_some() {
            "Bitcoin is shutting down."
        } else if app.bitcoin_handle.is_some() {
            "Bitcoin is already running."
        } else if app.electrs_handle.is_some() || app.electrs_shutdown.is_some() {
            "Stop Electrs from the previous Bitcoin generation before relaunching Bitcoin."
        } else {
            "Starts bitcoind with the configured data directory."
        },
        running: app.bitcoin_running,
        lines: &app.bitcoin_lines,
        output_pane: OutputPane::Bitcoin,
        scroll_id: bitcoin_scroll_id(),
        follow_output: app.output_viewports.bitcoin.follow_output,
    });
    let electrs_panel = view_node_panel(NodePanelSpec {
        title: "Electrs",
        subtitle: "Electrum server · wallet index",
        terminal_label: "ELECTRS LOG",
        accent: ELS_ACC,
        state: electrs_node_presentation(app),
        indicators: electrs_node_indicators(app),
        launch_action: electrs_launch_action(app),
        launch_hint: if !installation_ready {
            installation_recovery_short_hint(app)
        } else if app.electrs_shutdown.is_some() {
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
        lines: &app.electrs_lines,
        output_pane: OutputPane::Electrs,
        scroll_id: electrs_scroll_id(),
        follow_output: app.output_viewports.electrs.follow_output,
    });

    row![bitcoin_panel, electrs_panel]
        .spacing(DASHBOARD_SECTION_SPACING)
        .height(Length::Fill)
        .into()
}

struct NodePanelSpec<'a> {
    title: &'a str,
    subtitle: &'a str,
    terminal_label: &'a str,
    accent: Color,
    state: NodeStatePresentation,
    indicators: [NodeIndicator; 3],
    launch_action: Option<Message>,
    launch_hint: &'a str,
    running: bool,
    lines: &'a [String],
    output_pane: OutputPane,
    scroll_id: Id,
    follow_output: bool,
}

fn view_node_panel(spec: NodePanelSpec<'_>) -> Element<'_, Message> {
    let panel = column![
        accent_bar(spec.accent),
        panel_header(
            spec.title,
            spec.subtitle,
            spec.accent,
            spec.state,
            spec.launch_action,
            spec.launch_hint,
        ),
        panel_indicators(spec.indicators),
        terminal_container(
            spec.terminal_label,
            spec.running,
            spec.lines,
            spec.output_pane,
            spec.scroll_id,
            spec.follow_output,
        ),
    ]
    .width(Length::Fill)
    .height(Length::Fill);

    container(panel)
        .width(Length::FillPortion(1))
        .height(Length::Fill)
        .style(|theme| container::Style {
            background: Some(ui_palette(theme).panel.into()),
            border: iced::Border {
                color: ui_palette(theme).border,
                width: 1.0,
                radius: 12.0.into(),
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
    state: NodeStatePresentation,
    launch_action: Option<Message>,
    launch_hint: &'a str,
) -> Element<'a, Message> {
    let launch_color = if accent == BTC_ACC {
        BUTTON_BITCOIN
    } else {
        BUTTON_ELECTRS
    };
    let launch_disabled = launch_action.is_none();
    let launch_btn = button(text("Launch").size(13).font(Font {
        weight: iced::font::Weight::Bold,
        ..Font::default()
    }))
    .padding(Padding::from([5, 18]))
    .style(move |theme, status| {
        let disabled = status == button::Status::Disabled;
        button::Style {
            background: Some(match status {
                button::Status::Disabled => ui_palette(theme).disabled.into(),
                button::Status::Hovered | button::Status::Pressed => darken(launch_color).into(),
                button::Status::Active => launch_color.into(),
            }),
            text_color: if disabled {
                ui_palette(theme).tertiary_text
            } else {
                Color::WHITE
            },
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
        text(title).size(18).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::default()
        }),
        text(subtitle).size(10).style(secondary_text_style),
    ]
    .spacing(2)
    .width(Length::Fill);

    let detail = if launch_disabled && state.label == "Stopped" {
        launch_hint.to_owned()
    } else {
        state.detail
    };

    column![
        row![
            heading,
            status_pill(state.label, state.color),
            Space::new().width(8),
            launch_btn,
        ]
        .align_y(Alignment::Center),
        text(detail)
            .size(10)
            .style(secondary_text_style)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    ]
    .spacing(7)
    .padding(Padding::from([10, 12]))
    .into()
}

fn panel_indicators(indicators: [NodeIndicator; 3]) -> Element<'static, Message> {
    row(indicators
        .into_iter()
        .map(|indicator| indicator_badge(indicator.label, indicator.value, indicator.color)))
    .spacing(6)
    .align_y(Alignment::Center)
    .padding(Padding {
        top: 0.0,
        right: 12.0,
        bottom: 9.0,
        left: 12.0,
    })
    .into()
}

fn terminal_scroll_style(
    theme: &iced::Theme,
    status: iced::widget::scrollable::Status,
) -> iced::widget::scrollable::Style {
    let mut style = scrollable::default(theme, status);
    style.vertical_rail.background = Some(TERM_FRAME.into());
    style.vertical_rail.scroller.background = TERM_DIM.into();
    style.vertical_rail.border.color = TERM_BORDER;
    style
}

fn terminal_action_button(pane: OutputPane) -> button::Button<'static, Message> {
    button(text("Jump to latest").size(9).color(TERM_FG))
        .padding(Padding::from([3, 7]))
        .on_press(Message::OutputFollowLatest(pane))
        .style(|_, status| button::Style {
            background: Some(
                match status {
                    button::Status::Hovered | button::Status::Pressed => TERM_BORDER,
                    button::Status::Active | button::Status::Disabled => TERM_FRAME,
                }
                .into(),
            ),
            text_color: TERM_FG,
            border: iced::Border {
                color: TERM_BORDER,
                width: 1.0,
                radius: 5.0.into(),
            },
            ..button::Style::default()
        })
}

fn terminal_container<'a>(
    label: &'a str,
    running: bool,
    lines: &'a [String],
    output_pane: OutputPane,
    scroll_id: Id,
    follow_output: bool,
) -> Element<'a, Message> {
    let activity = if !running {
        "Idle"
    } else if follow_output {
        "Live · Following"
    } else {
        "Live · Viewing history"
    };
    let activity_color = if !running {
        TERM_DIM
    } else if follow_output {
        GREEN
    } else {
        MAC_ORG
    };
    let mut header_actions: Vec<Element<Message>> = vec![
        text("●").size(11).color(activity_color).into(),
        text(activity).size(9).color(TERM_DIM).into(),
    ];
    if !follow_output {
        header_actions.push(Space::new().width(4).into());
        header_actions.push(terminal_action_button(output_pane).into());
    }

    let terminal_header = container(
        row![
            row![
                text(label).size(9).color(TERM_DIM).font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::MONOSPACE
                }),
                Space::new().width(7),
                text(format!("{} lines", lines.len()))
                    .size(9)
                    .color(TERM_DIM),
            ]
            .align_y(Alignment::Center),
            Space::new().width(Length::Fill),
            row(header_actions).spacing(4).align_y(Alignment::Center),
        ]
        .align_y(Alignment::Center),
    )
    .padding(Padding::from([7, 9]))
    .style(|_| container::Style {
        background: Some(TERM_FRAME.into()),
        border: iced::Border {
            color: TERM_BORDER,
            width: 1.0,
            radius: 7.0.into(),
        },
        ..Default::default()
    });

    let terminal_lines: Vec<Element<Message>> = if lines.is_empty() {
        vec![empty_terminal_state(running)]
    } else {
        lines
            .iter()
            .map(|line| terminal_line_element(line))
            .collect()
    };

    let terminal_content = column(terminal_lines)
        .spacing(1)
        .width(Length::Fill)
        .padding(Padding::from([12, 14]));

    let terminal = scrollable(terminal_content)
        .id(scroll_id)
        .on_scroll(move |viewport| output_viewport_changed(output_pane, viewport))
        .direction(Direction::Vertical(
            Scrollbar::default().width(9).scroller_width(6).spacing(2),
        ))
        .style(terminal_scroll_style)
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

    container(column![terminal_header, terminal].spacing(6))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding {
            top: 0.0,
            right: 8.0,
            bottom: 8.0,
            left: 8.0,
        })
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

fn build_setting_toggle<'a>(
    label: &'a str,
    description: &'a str,
    value: bool,
    on_toggle: fn(bool) -> Message,
    enabled: bool,
) -> Element<'a, Message> {
    row![
        column![
            text(label).size(11),
            text(description)
                .size(9)
                .style(tertiary_text_style)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(2)
        .width(Length::Fill),
        toggler(value)
            .on_toggle_maybe(enabled.then_some(on_toggle))
            .size(18),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
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
            .style(tertiary_text_style)
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
        .style(|theme| container::Style {
            background: Some(ui_palette(theme).bar.into()),
            ..Default::default()
        })
        .into()
}

fn view_overlay(message: &str) -> Element<'_, Message> {
    let dialog = container(
        column![
            text(message)
                .size(14)
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
    .style(|theme| container::Style {
        background: Some(ui_palette(theme).panel.into()),
        border: iced::Border {
            color: ui_palette(theme).border,
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
        .style(|theme| container::Style {
            background: Some(ui_palette(theme).border.into()),
            ..Default::default()
        })
        .into()
}

fn indicator_badge(
    label: &'static str,
    value: &'static str,
    color: Color,
) -> Element<'static, Message> {
    container(
        row![
            text("●").size(12).style(semantic_text_style(color)),
            column![
                text(label).size(8).style(tertiary_text_style),
                text(value).size(10).style(semantic_text_style(color)),
            ]
            .spacing(1),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding(Padding::from([4, 7]))
    .style(|theme| container::Style {
        background: Some(ui_palette(theme).indicator.into()),
        border: iced::Border {
            color: ui_palette(theme).border,
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
            .size(12)
            .font(Font::MONOSPACE)
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
        .size(12)
        .font(font)
        .color(style.color)
        .width(Length::Fill)
        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
        .line_height(iced::widget::text::LineHeight::Relative(1.35))
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
    status: (&'static str, bool),
    enabled: bool,
) -> Element<'a, Message> {
    let (status_text, available) = status;
    let status_dot =
        text("●")
            .size(13)
            .style(semantic_text_style(if available { GREEN } else { OFF }));
    let status = row![
        status_dot,
        Space::new().width(4),
        text(status_text).size(10).style(secondary_text_style),
    ]
    .align_y(Alignment::Center);

    column![
        row![
            text(label).size(11).style(secondary_text_style),
            Space::new().width(Length::Fill),
            status,
        ]
        .align_y(Alignment::Center),
        row![
            text_input(placeholder, value)
                .on_input_maybe(enabled.then_some(on_change))
                .padding(Padding::from([6, 8]))
                .font(Font::MONOSPACE)
                .size(11),
            styled_button("Browse", ButtonStyle::Secondary)
                .on_press_maybe(enabled.then_some(browse_msg)),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    ]
    .spacing(4)
    .padding(Padding::from([3, 0]))
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
    button(text(label).size(11))
        .padding(Padding::from([5, 14]))
        .style(move |theme, status| {
            let palette = ui_palette(theme);
            let (bg, hover_bg, fg) = match style {
                ButtonStyle::Primary => (BUTTON_BLUE, darken(BUTTON_BLUE), Color::WHITE),
                ButtonStyle::Secondary => (
                    palette.secondary_button,
                    palette.secondary_button_hover,
                    theme.palette().text,
                ),
                ButtonStyle::Destructive => (BUTTON_RED, darken(BUTTON_RED), Color::WHITE),
                ButtonStyle::Warning => (BUTTON_ORANGE, darken(BUTTON_ORANGE), Color::WHITE),
                ButtonStyle::Confirm => (BUTTON_GREEN, darken(BUTTON_GREEN), Color::WHITE),
            };
            let disabled = status == button::Status::Disabled;
            button::Style {
                background: Some(match status {
                    button::Status::Disabled => palette.disabled.into(),
                    button::Status::Hovered | button::Status::Pressed => hover_bg.into(),
                    button::Status::Active => bg.into(),
                }),
                text_color: if disabled { palette.tertiary_text } else { fg },
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
    fn dashboard_node_presentations_cover_stopped_retrying_failed_and_ready() -> anyhow::Result<()>
    {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );

        assert!(!app.paths_visible);
        assert_eq!(bitcoin_node_presentation(&app).label, "Stopped");
        assert_eq!(electrs_node_presentation(&app).label, "Stopped");

        app.bitcoin_running = true;
        app.bitcoin_rpc_error = Some("RPC warmup is still in progress".to_owned());
        let bitcoin = bitcoin_node_presentation(&app);
        assert_eq!(bitcoin.label, "Retrying");
        assert!(bitcoin.detail.contains("RPC warmup"));

        app.bitcoin_process_error = Some("bitcoind exited unexpectedly".to_owned());
        assert_eq!(bitcoin_node_presentation(&app).label, "Failed");

        app.electrs_status = ElectrsStatus {
            running: true,
            connected: true,
            synced: true,
            ready: true,
            ..ElectrsStatus::default()
        };
        assert_eq!(electrs_node_presentation(&app).label, "Ready");

        app.electrs_status.metrics_error = Some("metrics probe timed out".to_owned());
        let electrs = electrs_node_presentation(&app);
        assert_eq!(electrs.label, "Retrying");
        assert!(electrs.detail.contains("metrics probe"));
        Ok(())
    }

    #[test]
    fn tor_presentation_never_claims_availability_when_disabled() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        app.tor_manager_starting = false;
        app.selected_connection_mode = ConnectionMode::Tor;
        app.tor_status = TorStatus::Available {
            onion_host: "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion".to_owned(),
        };

        let readiness = ConnectionReadiness::Ready;
        assert_eq!(connection_status(&app, &readiness).0, "Tor disabled");
        let presentation = tor_presentation(&app, &readiness);
        assert_eq!(presentation.title, "Tor disabled");
        assert!(presentation.action.is_none());
        Ok(())
    }

    #[test]
    fn tor_disable_presentation_waits_for_runtime_acknowledgement() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        app.tor_manager_starting = false;
        app.selected_connection_mode = ConnectionMode::Tor;
        app.tor_status = TorStatus::Available {
            onion_host: "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion".to_owned(),
        };
        app.tor_runtime_requested = false;
        app.tor_runtime_command_in_flight = Some(false);

        let readiness = ConnectionReadiness::Ready;
        assert_eq!(
            connection_mode_label(&app, ConnectionMode::Tor, &readiness),
            "Tor stopping"
        );
        assert_eq!(connection_status(&app, &readiness).0, "Tor stopping");
        let presentation = tor_presentation(&app, &readiness);
        assert_eq!(presentation.title, "Stopping Tor");
        assert_eq!(presentation.visual_detail, "Remote endpoint closing");
        assert!(presentation.action.is_none());

        app.tor_runtime_command_in_flight = None;
        app.tor_control_error = Some("embedded Tor teardown did not acknowledge".to_owned());
        assert_eq!(
            connection_mode_label(&app, ConnectionMode::Tor, &readiness),
            "Tor error"
        );
        assert_eq!(connection_status(&app, &readiness).0, "Tor error");
        let presentation = tor_presentation(&app, &readiness);
        assert_eq!(presentation.title, "Tor control error");
        assert_eq!(presentation.visual_detail, "Remote state uncertain");
        assert!(presentation.action.is_none());

        app.tor_control_error = None;
        assert_eq!(connection_status(&app, &readiness).0, "Tor disabled");
        assert_eq!(tor_presentation(&app, &readiness).title, "Tor disabled");
        Ok(())
    }

    #[test]
    fn persisted_tor_can_be_disabled_without_a_manager() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        app.config.tor_enabled = true;
        app.tor_manager_starting = false;
        app.tor_manager = None;
        assert!(tor_enable_control_available(&app));

        app.config.tor_enabled = false;
        assert!(!tor_enable_control_available(&app));
        app.tor_manager_starting = true;
        assert!(tor_enable_control_available(&app));
        Ok(())
    }

    #[test]
    fn connection_presentation_matrix_covers_readiness_and_local_reachability() -> anyhow::Result<()>
    {
        let temporary = tempfile::tempdir()?;
        let app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        let cases = [
            (ConnectionReadiness::ServicesStopped, "Not ready", None),
            (
                ConnectionReadiness::BitcoinSyncing {
                    percent: 42.0,
                    blocks: 420,
                    headers: 1_000,
                },
                "Syncing",
                Some(0.42),
            ),
            (
                ConnectionReadiness::ElectrsIndexing {
                    percent: Some(65.0),
                    indexed_height: Some(650),
                    bitcoin_height: Some(1_000),
                },
                "Syncing",
                Some(0.65),
            ),
            (ConnectionReadiness::Ready, "Ready", None),
        ];
        for (readiness, expected_status, expected_progress) in cases {
            assert_eq!(connection_status(&app, &readiness).0, expected_status);
            assert_eq!(connection_progress(&readiness), expected_progress);
        }

        let loopback_listener = crate::connection::ElectrsListenAddr::for_policy(
            crate::connection::ElectrsBindPolicy::LoopbackOnly,
            None,
            crate::connection::DEFAULT_ELECTRUM_PORT,
        )?;
        let loopback = LocalEndpointState::resolve(
            crate::connection::ElectrsBindPolicy::LoopbackOnly,
            Some(loopback_listener),
        );
        assert!(loopback.endpoint().is_some());
        assert!(!loopback.is_lan_reachable());
        assert!(!local_qr_available(&ConnectionReadiness::Ready, &loopback));

        let lan_listener = crate::connection::ElectrsListenAddr::for_policy(
            crate::connection::ElectrsBindPolicy::LocalNetwork,
            Some("192.168.10.12".parse()?),
            crate::connection::DEFAULT_ELECTRUM_PORT,
        )?;
        let lan = LocalEndpointState::resolve(
            crate::connection::ElectrsBindPolicy::LocalNetwork,
            Some(lan_listener),
        );
        assert!(lan.is_lan_reachable());
        assert!(local_qr_available(&ConnectionReadiness::Ready, &lan));
        assert!(!local_qr_available(
            &ConnectionReadiness::ElectrsStarting,
            &lan
        ));
        assert_eq!(
            lan.endpoint()
                .map(crate::connection::ElectrumEndpoint::payload),
            Some("tcp://192.168.10.12:50001".to_owned())
        );
        Ok(())
    }

    #[test]
    fn tor_presentation_matrix_covers_lifecycle_states() {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";

        let cases = [
            (
                TorStatus::Disabled,
                ConnectionReadiness::ServicesStopped,
                "Tor not started",
                None,
                true,
            ),
            (
                TorStatus::Bootstrapping {
                    progress: 37,
                    summary: Some("Loading Tor directory".to_owned()),
                },
                ConnectionReadiness::ServicesStopped,
                "Bootstrapping Tor",
                Some(0.37),
                false,
            ),
            (
                TorStatus::Publishing {
                    onion_host: ONION_HOST.to_owned(),
                },
                ConnectionReadiness::Ready,
                "Publishing onion service",
                None,
                false,
            ),
            (
                TorStatus::WaitingForElectrs {
                    onion_host: ONION_HOST.to_owned(),
                },
                ConnectionReadiness::ElectrsStarting,
                "Waiting for electrs",
                None,
                false,
            ),
            (
                TorStatus::Available {
                    onion_host: ONION_HOST.to_owned(),
                },
                ConnectionReadiness::Ready,
                "Tor available",
                None,
                false,
            ),
            (
                TorStatus::TemporarilyUnavailable {
                    message: "Tor circuit failed".to_owned(),
                    onion_host: Some(ONION_HOST.to_owned()),
                    retry_in: Some(std::time::Duration::from_secs(4)),
                },
                ConnectionReadiness::Ready,
                "Tor temporarily unavailable",
                None,
                true,
            ),
            (
                TorStatus::Error {
                    message: "Tor configuration failed".to_owned(),
                    retryable: true,
                },
                ConnectionReadiness::Ready,
                "Tor error",
                None,
                true,
            ),
        ];

        for (status, readiness, title, progress, has_action) in cases {
            let presentation = tor_status_presentation(&status, &readiness, true);
            assert_eq!(presentation.title, title);
            assert_eq!(presentation.progress, progress);
            assert_eq!(presentation.action.is_some(), has_action);
        }
    }

    #[test]
    fn tor_selector_always_exposes_a_textual_state_cue() -> anyhow::Result<()> {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";

        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        app.selected_connection_mode = ConnectionMode::Local;
        app.tor_manager_starting = false;
        assert_eq!(
            connection_mode_label(&app, ConnectionMode::Tor, &ConnectionReadiness::Ready),
            "Tor off"
        );

        app.config.tor_enabled = true;
        app.tor_status = TorStatus::Disabled;
        assert_eq!(
            connection_mode_label(&app, ConnectionMode::Tor, &ConnectionReadiness::Ready),
            "Tor idle"
        );
        app.tor_status = TorStatus::Bootstrapping {
            progress: 8,
            summary: None,
        };
        assert_eq!(
            connection_mode_label(&app, ConnectionMode::Tor, &ConnectionReadiness::Ready),
            "Tor starting"
        );
        app.tor_status = TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        };
        assert_eq!(
            connection_mode_label(&app, ConnectionMode::Tor, &ConnectionReadiness::Ready),
            "Tor ready"
        );
        app.tor_status = TorStatus::Error {
            message: "failed".to_owned(),
            retryable: false,
        };
        assert_eq!(
            connection_mode_label(&app, ConnectionMode::Tor, &ConnectionReadiness::Ready),
            "Tor error"
        );
        Ok(())
    }

    #[test]
    fn theme_palettes_keep_dark_status_blue_readable() {
        let light_palette = ui_palette(&iced::Theme::Light);
        let dark_palette = ui_palette(&iced::Theme::Dark);
        assert_ne!(light_palette.background, dark_palette.background);

        assert_eq!(
            semantic_color(&iced::Theme::Dark, MAC_BLUE),
            Color::from_rgb8(88, 166, 255)
        );
        assert_eq!(
            semantic_color(&iced::Theme::Light, MAC_BLUE),
            Color::from_rgb8(0, 80, 174)
        );
    }

    #[test]
    fn connection_qr_grid_has_integer_cells_quiet_zone_and_dark_runs() -> anyhow::Result<()> {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";
        let payloads = [
            "tcp://10.0.0.181:50001".to_owned(),
            format!("tcp://{ONION_HOST}:50001"),
        ];

        for payload in payloads {
            let qr = ConnectionQr::encode(payload)?;
            assert_eq!(qr.cells.len(), qr.width * qr.width);
            assert!(qr.cells.iter().any(|cell| *cell));

            let bounds = Rectangle::new(Point::ORIGIN, Size::new(122.0, 122.0));
            let geometry = connection_qr_geometry(qr.width, bounds)
                .ok_or_else(|| anyhow::anyhow!("connection QR geometry"))?;
            assert!(geometry.cell_size.fract().abs() <= f32::EPSILON);
            assert!(geometry.origin.x.fract().abs() <= f32::EPSILON);
            assert!(geometry.origin.y.fract().abs() <= f32::EPSILON);
            assert!(geometry.side_length <= bounds.width);
            assert!(geometry.cell_size >= 2.0);
            let quiet_zone =
                f32::from(u16::try_from(CONNECTION_QR_QUIET_ZONE)?) * geometry.cell_size;
            let first_data_module = f32::from(u16::try_from(CONNECTION_QR_QUIET_ZONE)?)
                .mul_add(geometry.cell_size, geometry.origin.x);
            let trailing_quiet_zone = geometry.origin.x + geometry.side_length
                - f32::from(u16::try_from(CONNECTION_QR_QUIET_ZONE + qr.width)?)
                    .mul_add(geometry.cell_size, geometry.origin.x);
            assert!((first_data_module - geometry.origin.x - quiet_zone).abs() <= f32::EPSILON);
            assert!((trailing_quiet_zone - quiet_zone).abs() <= f32::EPSILON);
            assert!(geometry.origin.x >= bounds.x);
            assert!(geometry.origin.y >= bounds.y);
            assert!(geometry.origin.x + geometry.side_length <= bounds.x + bounds.width);
            assert!(geometry.origin.y + geometry.side_length <= bounds.y + bounds.height);

            let mut covered_dark_cells = 0;
            let mut run_count = 0;
            for_each_connection_qr_dark_run(&qr, |row, start, length| {
                assert!(row < qr.width);
                assert!(start < qr.width);
                assert!(start + length <= qr.width);
                covered_dark_cells += length;
                run_count += 1;
            });
            assert!(run_count > 0);
            assert_eq!(
                covered_dark_cells,
                qr.cells.iter().filter(|cell| **cell).count()
            );
        }
        Ok(())
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
        app.installation_recovery = InstallationRecoveryState::Ready {
            destination: app.config.binaries_path.clone(),
        };
        app.binary_page.selected_bitcoin = Some("v30.0".parse().map_err(anyhow::Error::msg)?);
        app.pending_path_save = Some(1);

        let presentation = binary_row_presentation(&app, BinaryKind::BitcoinCore);
        assert_eq!(presentation.action_label, "Saving paths…");
        assert!(presentation.action.is_none());
        Ok(())
    }

    #[test]
    fn installation_recovery_state_gates_launch_build_and_retry_presentation() -> anyhow::Result<()>
    {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        app.binary_page.selected_bitcoin = Some("v30.0".parse().map_err(anyhow::Error::msg)?);

        let checking = binary_row_presentation(&app, BinaryKind::BitcoinCore);
        assert_eq!(checking.action_label, "Checking safety…");
        assert!(checking.action.is_none());
        assert!(bitcoin_launch_action(&app).is_none());
        assert!(installation_recovery_notice(&app).is_some());

        app.installation_recovery = InstallationRecoveryState::Failed {
            destination: app.config.binaries_path.clone(),
            error: "external volume denied access".to_owned(),
        };
        let failed = binary_row_presentation(&app, BinaryKind::BitcoinCore);
        assert_eq!(failed.action_label, "Recovery required");
        assert!(failed.action.is_none());
        assert!(bitcoin_launch_action(&app).is_none());
        assert!(installation_recovery_notice(&app).is_some());

        app.installation_recovery = InstallationRecoveryState::Ready {
            destination: app.config.binaries_path.clone(),
        };
        let ready = binary_row_presentation(&app, BinaryKind::BitcoinCore);
        assert_eq!(ready.action_label, "Build / Update");
        assert!(ready.action.is_some());
        assert!(bitcoin_launch_action(&app).is_some());
        assert!(installation_recovery_notice(&app).is_none());
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
        app.installation_recovery = InstallationRecoveryState::Ready {
            destination: app.config.binaries_path.clone(),
        };
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
