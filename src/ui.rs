//! Rendering. Every clickable region is recorded in `App::hits` as it is drawn.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::app::{
    Action, App, Auth, Click, EditTarget, InstallState, Loadable, LoginField, RunningJob,
    SettingsRow, Tab, display_version, human_bytes,
};
use crate::patcher::{Mode, Phase};
use crate::session;

const ACCENT: Color = Color::Yellow;

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.hits.clear();
    match app.auth {
        Auth::Checking => draw_checking(frame),
        Auth::SignedOut => draw_login(frame, app),
        Auth::SignedIn(_) => draw_main(frame, app),
    }
    if app.editor.is_some() {
        draw_editor(frame, app);
    }
    if app.confirm.is_some() {
        draw_confirm(frame, app);
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn draw_checking(frame: &mut Frame) {
    let area = centered(frame.area(), 40, 3);
    frame.render_widget(
        Paragraph::new("Checking for a saved session…")
            .alignment(Alignment::Center)
            .block(Block::bordered().border_type(BorderType::Rounded)),
        area,
    );
}

fn draw_login(frame: &mut Frame, app: &mut App) {
    let two_factor = app.login.challenge.is_some();
    let area = centered(frame.area(), 64, if two_factor { 13 } else { 15 });
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Sign in to Monsters & Memories ".bold())
        .border_style(Style::new().fg(ACCENT));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [fields_area, error_area, button_area, hint_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(inner.inner(ratatui::layout::Margin::new(1, 0)));

    let focused = app.login.focused();
    let fields: Vec<(LoginField, String, String)> = if let Some(challenge) = &app.login.challenge {
        vec![(
            LoginField::Code,
            format!("Code (sent by {})", challenge.method),
            app.login.code.value(),
        )]
    } else {
        vec![
            (LoginField::Email, "Email".into(), app.login.email.value()),
            (
                LoginField::Password,
                "Password".into(),
                "•".repeat(app.login.password.len()),
            ),
        ]
    };
    let rows = Layout::vertical(fields.iter().map(|_| Constraint::Length(3))).split(fields_area);
    for ((field, label, shown), rect) in fields.into_iter().zip(rows.iter().copied()) {
        let style = if field == focused {
            Style::new().fg(ACCENT)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        frame.render_widget(
            Paragraph::new(shown).block(
                Block::bordered()
                    .title(format!(" {label} "))
                    .border_style(style),
            ),
            rect,
        );
        app.hits.push((rect, Click::LoginField(field)));
        if field == focused && !app.login.busy && app.editor.is_none() {
            let input = match field {
                LoginField::Email => &app.login.email,
                LoginField::Password => &app.login.password,
                LoginField::Code => &app.login.code,
            };
            frame.set_cursor_position((rect.x + 1 + input.cursor() as u16, rect.y + 1));
        }
    }

    if let Some(error) = &app.login.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .fg(Color::LightRed)
                .wrap(Wrap { trim: true }),
            error_area,
        );
    }

    let label = if app.login.busy {
        "Signing in…"
    } else if two_factor {
        "Verify"
    } else {
        "Sign in"
    };
    let button = centered(button_area, 18, 3);
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .bold()
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(ACCENT)),
            ),
        button,
    );
    app.hits.push((button, Click::LoginSubmit));

    let hint = if two_factor {
        "Enter verify · Ctrl-R resend code · Esc back"
    } else {
        "Enter sign in · Tab next field · Esc quit"
    };
    frame.render_widget(
        Paragraph::new(hint)
            .alignment(Alignment::Center)
            .fg(Color::DarkGray),
        hint_area,
    );
}

fn draw_main(frame: &mut Frame, app: &mut App) {
    let job_height = if app.job.is_some() { 3 } else { 0 };
    let [header, tabs, body, job, status, buttons] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(job_height),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(frame.area());
    if let Some(running) = &app.job {
        draw_job(frame, running, job);
    }

    draw_header(frame, app, header);
    draw_tabs(frame, app, tabs);
    match app.tab {
        Tab::News => draw_news(frame, app, body),
        Tab::PatchNotes => draw_patch_notes(frame, app, body),
        Tab::Settings => draw_settings(frame, app, body),
    }
    let status_text = app
        .status
        .clone()
        .unwrap_or_else(|| tab_hint(app.tab).to_string());
    frame.render_widget(Paragraph::new(status_text).fg(Color::Gray), status);
    draw_buttons(frame, app, buttons);
}

fn tab_hint(tab: Tab) -> &'static str {
    match tab {
        Tab::News => "↑↓ select · Enter open in browser · Tab switch tab · F5 refresh · q quit",
        Tab::PatchNotes => {
            "↑↓ select · PgUp/PgDn scroll · Enter open in browser · F5 refresh · q quit"
        }
        Tab::Settings => {
            "Enter edit · Del reset/remove · a add variable · Space toggle · i import token · q quit"
        }
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let account = match &app.auth {
        Auth::SignedIn(s) => format!(
            "{} · token expires {} ",
            s.email,
            session::format_date(s.expires_at)
        ),
        _ => String::new(),
    };
    let [left, right] = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Length(account.chars().count() as u16),
    ])
    .areas(area);
    let (state, color) = match app.install_state() {
        InstallState::Unknown => ("checking for updates…".to_string(), Color::DarkGray),
        InstallState::NotInstalled => (
            "not installed · press U to install".to_string(),
            Color::LightRed,
        ),
        InstallState::Incomplete => (
            "last patch did not finish · press U".to_string(),
            Color::LightRed,
        ),
        InstallState::UpdateAvailable => {
            let target = match &app.game_version {
                Loadable::Ready(v) => display_version(&v.version).to_string(),
                _ => String::new(),
            };
            (format!("update available ({target}) · press U"), ACCENT)
        }
        InstallState::UpToDate => {
            let version = app
                .install
                .as_ref()
                .and_then(|i| i.version.as_deref())
                .unwrap_or_default();
            (
                format!("{} · up to date", display_version(version)),
                Color::Green,
            )
        }
    };
    frame.render_widget(
        Line::from(vec![
            " rmnm ".black().on_yellow().bold(),
            " Monsters & Memories  ".bold(),
            Span::styled(state, Style::new().fg(color)),
        ]),
        left,
    );
    frame.render_widget(Line::from(account).fg(Color::Gray).right_aligned(), right);
}

fn draw_tabs(frame: &mut Frame, app: &mut App, area: Rect) {
    let labels: Vec<String> = Tab::ALL
        .iter()
        .map(|t| format!(" {} ", t.title()))
        .collect();
    let mut constraints: Vec<Constraint> = labels
        .iter()
        .map(|l| Constraint::Length(l.chars().count() as u16 + 1))
        .collect();
    constraints.push(Constraint::Min(0));
    let rects = Layout::horizontal(constraints).split(area);
    for ((tab, label), rect) in Tab::ALL.into_iter().zip(labels).zip(rects.iter().copied()) {
        let style = if tab == app.tab {
            Style::new()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        frame.render_widget(Span::styled(label, style), rect);
        app.hits.push((rect, Click::Tab(tab)));
    }
}

fn draw_buttons(frame: &mut Frame, app: &mut App, area: Rect) {
    let [play, patch, repair, reinstall, _, logout] = Layout::horizontal([
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(17),
        Constraint::Min(0),
        Constraint::Length(14),
    ])
    .areas(area);
    let rects = [play, patch, repair, reinstall, logout];
    for (action, rect) in Action::ALL.into_iter().zip(rects) {
        let primary = action == Action::Play;
        let border = if primary {
            Style::new().fg(ACCENT)
        } else {
            Style::new().fg(Color::Gray)
        };
        let key = action.key().to_ascii_uppercase();
        let job_mode = app.job.as_ref().map(|j| j.mode);
        let starts_job = action.mode().is_some();
        let (text, dimmed) = match job_mode {
            Some(mode) if action.mode() == Some(mode) => ("Cancel", false),
            Some(_) if starts_job || action == Action::Play => (action.label(), true),
            None if action == Action::Play && app.game_running() => ("Running", true),
            None if starts_job && app.game_running() => (action.label(), true),
            _ => (action.label(), false),
        };
        let border = if dimmed {
            Style::new().fg(Color::DarkGray)
        } else {
            border
        };
        let text_style = if dimmed {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        };
        let label = Line::from(vec![
            Span::styled(text, text_style),
            Span::styled(format!(" ({key})"), Style::new().fg(Color::DarkGray)),
        ]);
        frame.render_widget(
            Paragraph::new(label).alignment(Alignment::Center).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(border),
            ),
            rect,
        );
        app.hits.push((rect, Click::Action(action)));
    }
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "))
}

/// Renders a stateful list and records a click target for each visible row.
fn render_list(
    frame: &mut Frame,
    hits: &mut Vec<(Rect, Click)>,
    list: List<'_>,
    len: usize,
    area: Rect,
    state: &mut ListState,
    click: impl Fn(usize) -> Click,
) {
    let inner = list_inner(area);
    frame.render_stateful_widget(list, area, state);
    for row in 0..inner.height as usize {
        let index = state.offset() + row;
        if index >= len {
            break;
        }
        hits.push((
            Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            click(index),
        ));
    }
}

fn list_inner(area: Rect) -> Rect {
    Block::new().borders(Borders::ALL).inner(area)
}

fn highlight() -> Style {
    Style::new().fg(Color::Black).bg(ACCENT)
}

fn draw_loading_or_error<T>(
    frame: &mut Frame,
    title: &str,
    state: &Loadable<T>,
    area: Rect,
) -> bool {
    let text = match state {
        Loadable::Ready(_) => return false,
        Loadable::Loading => Line::from("Loading…").fg(Color::Gray),
        Loadable::Failed(e) => Line::from(format!("Could not load: {e}")).fg(Color::LightRed),
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(panel(title)),
        area,
    );
    true
}

fn draw_news(frame: &mut Frame, app: &mut App, area: Rect) {
    if draw_loading_or_error(frame, "News", &app.news, area) {
        return;
    }
    let Loadable::Ready(items) = &app.news else {
        return;
    };
    let rows: Vec<ListItem> = items
        .iter()
        .map(|item| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<12}", item.date),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::raw(item.title.clone()),
            ]))
        })
        .collect();
    let len = rows.len();
    let list = List::new(rows)
        .block(panel("News"))
        .highlight_style(highlight());
    render_list(
        frame,
        &mut app.hits,
        list,
        len,
        area,
        &mut app.news_state,
        Click::News,
    );
}

fn draw_patch_notes(frame: &mut Frame, app: &mut App, area: Rect) {
    if draw_loading_or_error(frame, "Patch Notes", &app.patch_index, area) {
        return;
    }
    let Loadable::Ready(notes) = &app.patch_index else {
        return;
    };
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Length(48), Constraint::Min(20)]).areas(area);

    let rows: Vec<ListItem> = notes
        .iter()
        .map(|n| ListItem::new(n.list_label()))
        .collect();
    let len = rows.len();
    let list = List::new(rows)
        .block(panel("Released"))
        .highlight_style(highlight());
    render_list(
        frame,
        &mut app.hits,
        list,
        len,
        list_area,
        &mut app.patch_state,
        Click::PatchNote,
    );

    let text_width = list_inner(detail_area).width;
    let note = app
        .selected_patch_note()
        .and_then(|n| app.patch_notes.get(&n.id));
    let lines: Vec<Line> = match note {
        None | Some(Loadable::Loading) => vec![Line::from("Loading…").fg(Color::Gray)],
        Some(Loadable::Failed(e)) => {
            vec![Line::from(format!("Could not load: {e}")).fg(Color::LightRed)]
        }
        Some(Loadable::Ready(note)) => {
            let mut lines = vec![
                Line::from(note.title.clone()).bold().fg(ACCENT),
                Line::from(note.date.clone()).fg(Color::DarkGray),
            ];
            for section in &note.sections {
                lines.push(Line::default());
                lines.push(Line::from(section.heading.clone()).bold().underlined());
                for item in &section.items {
                    let indent = "  ".repeat(item.depth + 1);
                    let bullet = if item.depth == 0 { "• " } else { "◦ " };
                    let first = format!("{indent}{bullet}");
                    let hang = " ".repeat(first.chars().count());
                    let width = (text_width as usize).saturating_sub(hang.len()).max(10);
                    for (i, chunk) in wrap_words(&item.text, width).into_iter().enumerate() {
                        let lead = if i == 0 {
                            first.as_str()
                        } else {
                            hang.as_str()
                        };
                        lines.push(Line::from(format!("{lead}{chunk}")));
                    }
                }
            }
            lines
        }
    };
    let max_scroll = lines.len().saturating_sub(1) as u16;
    app.patch_scroll = app.patch_scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.patch_scroll, 0))
            .block(panel("Notes")),
        detail_area,
    );
    app.hits.push((detail_area, Click::PatchDetail));
}

fn draw_settings(frame: &mut Frame, app: &mut App, area: Rect) {
    let [list_area, help_area] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(4)]).areas(area);
    let label_width = 22;
    let rows_spec = app.settings_rows();
    let rows: Vec<ListItem> = rows_spec
        .iter()
        .map(|row| match *row {
            SettingsRow::Setting(s) => {
                let value = app.settings.get(s);
                let shown = if value.is_empty() {
                    "(not set)".to_string()
                } else {
                    value
                };
                let mut spans = vec![
                    Span::styled(format!("{:<label_width$}", s.label()), Style::new().bold()),
                    Span::raw(shown),
                ];
                if app.settings.is_default(s) {
                    spans.push(Span::styled("  default", Style::new().fg(Color::DarkGray)));
                }
                ListItem::new(Line::from(spans))
            }
            SettingsRow::EnvVar(i) => {
                let var = &app.env_vars[i];
                let state = if var.enabled {
                    Span::styled("on ", Style::new().fg(Color::Green))
                } else {
                    Span::styled("off", Style::new().fg(Color::DarkGray))
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:<label_width$}", "env"),
                        Style::new().fg(Color::DarkGray),
                    ),
                    state,
                    Span::raw(format!("  {}={}", var.name, var.value)),
                ]))
            }
            SettingsRow::AddEnvVar => {
                ListItem::new(Line::from("+ Add environment variable").fg(Color::Cyan))
            }
            SettingsRow::ImportToken => ListItem::new(
                Line::from("⇣ Import token from the official launcher").fg(Color::Cyan),
            ),
        })
        .collect();
    let len = rows.len();
    let list = List::new(rows)
        .block(panel("Settings"))
        .highlight_style(highlight());
    render_list(
        frame,
        &mut app.hits,
        list,
        len,
        list_area,
        &mut app.settings_state,
        Click::SettingsRow,
    );

    let help = match app.settings_state.selected().and_then(|i| rows_spec.get(i)) {
        Some(SettingsRow::Setting(s)) => s.help(),
        Some(SettingsRow::EnvVar(_)) => {
            "Extra environment variable passed to the game. Space toggles it, Del removes it."
        }
        Some(SettingsRow::AddEnvVar) => "Add a variable as NAME=VALUE, e.g. DXVK_HUD=fps.",
        Some(SettingsRow::ImportToken) => {
            "Reads launcher.db (read-only) and signs in with its token if the server accepts it."
        }
        None => "",
    };
    frame.render_widget(
        Paragraph::new(help)
            .wrap(Wrap { trim: true })
            .fg(Color::Gray)
            .block(panel("About")),
        help_area,
    );
}

fn draw_editor(frame: &mut Frame, app: &mut App) {
    let Some(editor) = &app.editor else { return };
    let title = match editor.target {
        EditTarget::Setting(s) => format!(" {} — empty restores the default ", s.label()),
        EditTarget::NewEnvVar => " New variable — NAME=VALUE ".to_string(),
        EditTarget::EnvVar(_) => " Edit variable — NAME=VALUE ".to_string(),
    };
    let width = frame.area().width.saturating_sub(8).clamp(20, 100);
    let area = centered(frame.area(), width, 6);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.bold())
        .title_bottom(Line::from(" Enter save · Esc cancel ").right_aligned())
        .border_style(Style::new().fg(ACCENT));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    // Keep the cursor visible in values longer than the box.
    let visible = inner.width.saturating_sub(1) as usize;
    let start = editor.input.cursor().saturating_sub(visible);
    let text: String = editor
        .input
        .value()
        .chars()
        .skip(start)
        .take(visible + 1)
        .collect();
    let [input_row, error_row] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    frame.render_widget(Paragraph::new(text), input_row);
    if let Some(error) = &editor.error {
        frame.render_widget(
            Paragraph::new(error.as_str()).fg(Color::LightRed),
            error_row,
        );
    }
    frame.set_cursor_position((inner.x + (editor.input.cursor() - start) as u16, inner.y));
}

fn draw_job(frame: &mut Frame, job: &RunningJob, area: Rect) {
    let title = match job.mode {
        Mode::Patch => "Patching",
        Mode::Repair => "Repairing",
        Mode::Reinstall => "Reinstalling",
    };
    let (ratio, label) = match &job.progress {
        None => (0.0, "Starting…".to_string()),
        Some(p) => {
            let ratio = if p.total == 0 {
                0.0
            } else {
                (p.done as f64 / p.total as f64).clamp(0.0, 1.0)
            };
            let rate = job.rate();
            let mut parts = vec![match p.phase {
                Phase::Manifest => "Downloading the manifest".to_string(),
                Phase::Clearing => "Deleting the installed files".to_string(),
                Phase::Checking => format!(
                    "Checking {} / {}",
                    human_bytes(p.done),
                    human_bytes(p.total)
                ),
                Phase::Downloading => format!(
                    "Downloading {} / {}",
                    human_bytes(p.done),
                    human_bytes(p.total)
                ),
            }];
            if p.files_total > 0 {
                parts.push(format!(
                    "file {}/{}",
                    (p.files_done + 1).min(p.files_total),
                    p.files_total
                ));
            }
            if let Some(rate) = rate {
                parts.push(format!("{}/s", human_bytes(rate as u64)));
                if rate > 0.0 && p.total > p.done {
                    parts.push(format!(
                        "{} left",
                        human_duration((p.total - p.done) as f64 / rate)
                    ));
                }
            }
            (ratio, parts.join(" · "))
        }
    };
    let current = job
        .progress
        .as_ref()
        .map(|p| p.current.clone())
        .unwrap_or_default();
    let block = panel(title).title_bottom(Line::from(format!(" {current} ")).fg(Color::DarkGray));
    frame.render_widget(
        Gauge::default()
            .block(block)
            .gauge_style(Style::new().fg(ACCENT).bg(Color::Black))
            .ratio(ratio)
            .label(label),
        area,
    );
}

fn human_duration(secs: f64) -> String {
    let secs = secs.round() as u64;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn draw_confirm(frame: &mut Frame, app: &App) {
    let Some(action) = app.confirm else { return };
    let target = match &app.game_version {
        Loadable::Ready(v) => display_version(&v.version).to_string(),
        _ => String::new(),
    };
    let dir = app.install_dir().display().to_string();
    let (title, body) = match action {
        Action::Repair => (
            format!(" Repair {target} "),
            "Re-hashes every file and re-downloads any that do not match. \
             Takes longer than Patch.",
        ),
        Action::Reinstall => (
            format!(" Reinstall {target} "),
            "Deletes the game files and downloaded bundles, then downloads the whole game again. \
             The server is asked for the current build first.",
        ),
        _ => (
            format!(" Patch to {target} "),
            "Checks the installed files and downloads only what is missing or changed. \
             A fresh install downloads the whole game.",
        ),
    };
    let width = frame.area().width.saturating_sub(8).clamp(30, 90);
    let area = centered(frame.area(), width, 9);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.bold())
        .title_bottom(Line::from(" Enter start · Esc cancel ").right_aligned())
        .border_style(Style::new().fg(ACCENT));
    let text = vec![
        Line::from(vec![
            Span::raw("Install folder: "),
            Span::styled(dir, Style::new().bold()),
        ]),
        Line::default(),
        Line::from(body),
        Line::default(),
        Line::from("Files not listed in the manifest are left alone.").fg(Color::DarkGray),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

/// Greedy word wrap to `width` characters; a word longer than the width gets a line of its own.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::wrap_words;

    #[test]
    fn wraps_on_word_boundaries() {
        assert_eq!(wrap_words("aa bb cc", 5), ["aa bb", "cc"]);
        assert_eq!(
            wrap_words("averyverylongword x", 5),
            ["averyverylongword", "x"]
        );
        assert_eq!(wrap_words("", 5), [""]);
    }
}
