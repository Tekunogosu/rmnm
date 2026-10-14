//! Application state and event handling. Rendering lives in `ui`; network work runs on
//! background threads that report back through `Msg`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use crate::api::{Api, GameVersion, LoginOutcome, TokenStatus, TwoFactorChallenge};
use crate::db::{Db, EnvVar, InstallRecord, SessionSource, StoredSession};
use crate::feeds::{self, NewsItem, PatchNote, PatchNoteSummary};
use crate::launch::LaunchPlan;
use crate::patcher::{self, Cancelled, Mode, Progress, Report};
use crate::paths;
use crate::session::{self, now};
use crate::settings::{Setting, Settings};
use crate::ui;

/// Set once the first-start import from the official launcher has been attempted, so a
/// later logout is not undone by importing the same token again on the next start.
const AUTO_IMPORT_DONE_KEY: &str = "auto_import_done";

pub enum Loadable<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    News,
    PatchNotes,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::News, Tab::PatchNotes, Tab::Settings];

    pub fn title(self) -> &'static str {
        match self {
            Self::News => "News",
            Self::PatchNotes => "Patch Notes",
            Self::Settings => "Settings",
        }
    }

    fn offset(self, by: isize) -> Tab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0) as isize;
        Self::ALL[(i + by).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Play,
    Patch,
    Repair,
    Reinstall,
    Logout,
}

impl Action {
    pub const ALL: [Action; 5] = [
        Action::Play,
        Action::Patch,
        Action::Repair,
        Action::Reinstall,
        Action::Logout,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::Patch => "Patch",
            Self::Repair => "Repair",
            Self::Reinstall => "Reinstall",
            Self::Logout => "Logout",
        }
    }

    /// The patcher job this action starts, if it starts one.
    pub fn mode(self) -> Option<Mode> {
        match self {
            Self::Patch => Some(Mode::Patch),
            Self::Repair => Some(Mode::Repair),
            Self::Reinstall => Some(Mode::Reinstall),
            Self::Play | Self::Logout => None,
        }
    }

    pub fn key(self) -> char {
        match self {
            Self::Play => 'p',
            Self::Patch => 'u',
            Self::Repair => 'r',
            Self::Reinstall => 'x',
            Self::Logout => 'l',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginField {
    Email,
    Password,
    Code,
}

pub enum Auth {
    Checking,
    SignedOut,
    SignedIn(StoredSession),
}

#[derive(Default)]
pub struct LoginForm {
    pub email: TextInput,
    pub password: TextInput,
    pub code: TextInput,
    pub focus: Option<LoginField>,
    pub challenge: Option<TwoFactorChallenge>,
    pub busy: bool,
    pub error: Option<String>,
}

impl LoginForm {
    fn with_email(email: &str) -> Self {
        let focus = if email.is_empty() {
            LoginField::Email
        } else {
            LoginField::Password
        };
        Self {
            email: TextInput::new(email),
            focus: Some(focus),
            ..Self::default()
        }
    }

    pub fn fields(&self) -> &'static [LoginField] {
        if self.challenge.is_some() {
            &[LoginField::Code]
        } else {
            &[LoginField::Email, LoginField::Password]
        }
    }

    pub fn focused(&self) -> LoginField {
        self.focus
            .filter(|f| self.fields().contains(f))
            .unwrap_or(self.fields()[0])
    }

    fn input_mut(&mut self, field: LoginField) -> &mut TextInput {
        match field {
            LoginField::Email => &mut self.email,
            LoginField::Password => &mut self.password,
            LoginField::Code => &mut self.code,
        }
    }

    fn move_focus(&mut self, by: isize) {
        let fields = self.fields();
        let i = fields
            .iter()
            .position(|f| *f == self.focused())
            .unwrap_or(0) as isize;
        self.focus = Some(fields[(i + by).rem_euclid(fields.len() as isize) as usize]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    Setting(Setting),
    EnvVar(usize),
    AddEnvVar,
    ImportToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    Setting(Setting),
    NewEnvVar,
    EnvVar(i64),
}

pub struct Editor {
    pub target: EditTarget,
    pub input: TextInput,
    pub error: Option<String>,
}

/// Single-line text input with a character cursor.
#[derive(Debug, Default, Clone)]
pub struct TextInput {
    chars: Vec<char>,
    cursor: usize,
}

impl TextInput {
    pub fn new(value: &str) -> Self {
        let chars: Vec<char> = value.chars().collect();
        Self {
            cursor: chars.len(),
            chars,
        }
    }

    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Applies an editing key; returns false when the key is not an editing key.
    fn handle(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

/// Clickable screen regions, recorded during each draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    Tab(Tab),
    Action(Action),
    News(usize),
    PatchNote(usize),
    PatchDetail,
    SettingsRow(usize),
    LoginField(LoginField),
    LoginSubmit,
}

enum ImportResult {
    NotFound,
    Expired,
    Rejected(String),
    Imported(StoredSession),
}

enum Msg {
    News(Result<Vec<NewsItem>>),
    PatchIndex(Result<Vec<PatchNoteSummary>>),
    PatchNote(u32, Result<PatchNote>),
    SessionChecked {
        token: String,
        result: Result<TokenStatus>,
    },
    Imported {
        manual: bool,
        result: Result<ImportResult>,
    },
    Login {
        email: String,
        result: Result<LoginOutcome>,
    },
    TwoFactorResent(Result<()>),
    GameVersion(Result<GameVersion>),
    PatchProgress(Progress),
    PatchDone(Mode, Result<Report>),
    /// The token check that runs before the game starts.
    PlayChecked {
        token: String,
        result: Result<TokenStatus>,
    },
}

/// A Patch, Repair or Reinstall running on a background thread.
pub struct RunningJob {
    pub mode: Mode,
    pub progress: Option<Progress>,
    cancel: Arc<AtomicBool>,
    /// Recent (time, bytes done) samples for the transfer rate, newest last.
    samples: VecDeque<(Instant, u64)>,
}

impl RunningJob {
    /// Bytes per second over the last few seconds of the current phase.
    pub fn rate(&self) -> Option<f64> {
        let (t0, b0) = *self.samples.front()?;
        let (t1, b1) = *self.samples.back()?;
        let secs = t1.duration_since(t0).as_secs_f64();
        (secs >= 1.0).then(|| b1.saturating_sub(b0) as f64 / secs)
    }

    fn record(&mut self, progress: Progress) {
        let phase_changed = self
            .progress
            .as_ref()
            .is_some_and(|p| p.phase != progress.phase);
        if phase_changed {
            self.samples.clear();
        }
        let now = Instant::now();
        self.samples.push_back((now, progress.done));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(5))
        {
            self.samples.pop_front();
        }
        self.progress = Some(progress);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallState {
    /// The server's current build has not been fetched yet (or could not be).
    Unknown,
    NotInstalled,
    /// A previous patch stopped before finishing.
    Incomplete,
    UpdateAvailable,
    UpToDate,
}

pub struct App {
    db: Db,
    api: Arc<Api>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,

    pub auth: Auth,
    pub login: LoginForm,
    pub tab: Tab,
    pub status: Option<String>,
    pub hits: Vec<(Rect, Click)>,

    pub news: Loadable<Vec<NewsItem>>,
    pub news_state: ListState,

    pub patch_index: Loadable<Vec<PatchNoteSummary>>,
    pub patch_state: ListState,
    pub patch_notes: HashMap<u32, Loadable<PatchNote>>,
    pub patch_scroll: u16,

    pub settings: Settings,
    pub env_vars: Vec<EnvVar>,
    pub settings_state: ListState,
    pub editor: Option<Editor>,

    pub game_version: Loadable<GameVersion>,
    pub install: Option<InstallRecord>,
    pub job: Option<RunningJob>,
    /// An action waiting for the user to confirm it.
    pub confirm: Option<Action>,
    quit_armed: bool,
    /// The game process rmnm started, while it runs.
    game: Option<Child>,
    play_pending: bool,
}

impl App {
    pub fn new(db: Db, api: Api) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let settings = Settings::load(&db)?;
        let env_vars = db.env_vars()?;
        let mut app = Self {
            db,
            api: Arc::new(api),
            tx,
            rx,
            quit: false,
            auth: Auth::Checking,
            login: LoginForm::default(),
            tab: Tab::News,
            status: None,
            hits: Vec::new(),
            news: Loadable::Loading,
            news_state: ListState::default().with_selected(Some(0)),
            patch_index: Loadable::Loading,
            patch_state: ListState::default().with_selected(Some(0)),
            patch_notes: HashMap::new(),
            patch_scroll: 0,
            settings,
            env_vars,
            settings_state: ListState::default().with_selected(Some(0)),
            editor: None,
            game_version: Loadable::Loading,
            install: None,
            job: None,
            confirm: None,
            quit_armed: false,
            game: None,
            play_pending: false,
        };
        app.reload_install()?;
        app.restore_session()?;
        app.refresh_feeds();
        Ok(app)
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| ui::draw(frame, self))?;
            if event::poll(Duration::from_millis(100))? {
                self.handle_event(event::read()?)?;
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.handle_msg(msg)?;
            }
            self.poll_game();
        }
        Ok(())
    }

    fn spawn(&self, job: impl FnOnce(&Api) -> Msg + Send + 'static) {
        let api = Arc::clone(&self.api);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(job(&api));
        });
    }

    // ---- session ----------------------------------------------------------------------

    fn restore_session(&mut self) -> Result<()> {
        match self.db.session()? {
            Some(stored) if stored.expires_at > now() => {
                let token = stored.token.clone();
                self.auth = Auth::SignedIn(stored);
                self.refresh_game_version();
                self.spawn(move |api| {
                    let result = api.token_status(&token);
                    Msg::SessionChecked { token, result }
                });
            }
            stored => {
                if stored.is_some() {
                    self.db.clear_session()?;
                }
                if self.db.setting(AUTO_IMPORT_DONE_KEY)?.is_none() {
                    self.db.set_setting(AUTO_IMPORT_DONE_KEY, "1")?;
                    self.auth = Auth::Checking;
                    self.start_import(false);
                } else {
                    let reason = stored
                        .is_some()
                        .then_some("Your session expired. Please sign in again.");
                    self.sign_out(stored.map(|s| s.email).as_deref(), reason);
                }
            }
        }
        Ok(())
    }

    fn start_import(&mut self, manual: bool) {
        let path = PathBuf::from(self.settings.get(Setting::OfficialDb));
        if manual {
            self.status = Some(format!("Importing token from {}…", path.display()));
        }
        self.spawn(move |api| Msg::Imported {
            manual,
            result: import_official(api, &path),
        });
    }

    fn sign_in(&mut self, session: StoredSession) -> Result<()> {
        self.db.save_session(&session, now())?;
        self.auth = Auth::SignedIn(session);
        self.login = LoginForm::default();
        self.refresh_game_version();
        Ok(())
    }

    fn token(&self) -> Option<String> {
        match &self.auth {
            Auth::SignedIn(s) => Some(s.token.clone()),
            _ => None,
        }
    }

    // ---- game install -----------------------------------------------------------------

    fn refresh_game_version(&mut self) {
        let Some(token) = self.token() else { return };
        self.game_version = Loadable::Loading;
        self.spawn(move |api| {
            Msg::GameVersion(api.game_versions(&token).and_then(pick_game_version))
        });
    }

    pub fn install_dir(&self) -> PathBuf {
        PathBuf::from(self.settings.get(Setting::InstallDir))
    }

    fn reload_install(&mut self) -> Result<()> {
        self.install = self.db.install(&self.install_dir().display().to_string())?;
        Ok(())
    }

    pub fn install_state(&self) -> InstallState {
        let Loadable::Ready(server) = &self.game_version else {
            return InstallState::Unknown;
        };
        let has_game = self.install_dir().join("mnm.exe").is_file();
        match &self.install {
            _ if !has_game => InstallState::NotInstalled,
            Some(record) if record.patch_in_progress => InstallState::Incomplete,
            Some(record) if record.version.as_deref() == Some(server.version.as_str()) => {
                InstallState::UpToDate
            }
            // Files present but never patched by rmnm: only a check can tell.
            _ => InstallState::UpdateAvailable,
        }
    }

    pub fn game_running(&self) -> bool {
        self.game.is_some()
    }

    fn poll_game(&mut self) {
        let Some(child) = &mut self.game else { return };
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                self.game = None;
                self.status = Some(if status.success() {
                    "The game has exited.".to_string()
                } else {
                    format!(
                        "The game exited with {status}. Its output is in {}.",
                        game_log().display()
                    )
                });
            }
            Err(e) => {
                self.game = None;
                self.status = Some(format!("Lost track of the game process: {e}"));
            }
        }
    }

    fn request_play(&mut self) {
        if self.game_running() {
            self.status = Some("The game is already running.".into());
            return;
        }
        if self.job.is_some() {
            self.status = Some("Wait for the current job to finish, or cancel it.".into());
            return;
        }
        if self.play_pending {
            return;
        }
        match self.install_state() {
            InstallState::NotInstalled => {
                self.status =
                    Some("The game is not installed here yet. Press U to install it.".into());
                return;
            }
            InstallState::Incomplete => {
                self.status = Some("The last patch did not finish. Press U to complete it.".into());
                return;
            }
            InstallState::UpdateAvailable => {
                self.status =
                    Some("An update is available. Press U to patch before playing.".into());
                return;
            }
            InstallState::Unknown | InstallState::UpToDate => {}
        }
        if let Err(e) = LaunchPlan::new(&self.settings, &self.env_vars) {
            self.status = Some(format!("Cannot start the game: {e:#}"));
            return;
        }
        let Some(token) = self.token() else { return };
        self.play_pending = true;
        self.status = Some("Checking your session…".into());
        self.spawn(move |api| {
            let result = api.token_status(&token);
            Msg::PlayChecked { token, result }
        });
    }

    fn launch_game(&mut self, token: &str) {
        let log = game_log();
        let started = LaunchPlan::new(&self.settings, &self.env_vars)
            .and_then(|plan| Ok((plan.describe(), plan.spawn(token, &log)?)));
        self.status = Some(match started {
            Ok((command, child)) => {
                self.game = Some(child);
                format!("Started {command} · output in {}", log.display())
            }
            Err(e) => format!("Cannot start the game: {e:#}"),
        });
    }

    fn request_job(&mut self, mode: Mode) {
        if self.game_running() {
            self.status = Some("Close the game before patching or repairing.".into());
            return;
        }
        if let Some(job) = &self.job {
            if job.mode == mode {
                job.cancel.store(true, Ordering::Relaxed);
                self.status = Some("Cancelling…".into());
            } else {
                self.status = Some("Another job is running; cancel it first.".into());
            }
            return;
        }
        match &self.game_version {
            Loadable::Ready(_) => {
                self.confirm = Action::ALL.into_iter().find(|a| a.mode() == Some(mode))
            }
            Loadable::Loading => {
                self.status = Some("Still asking the server for the current build…".into())
            }
            Loadable::Failed(e) => {
                self.status = Some(format!("Could not get the current build: {e}. Retrying…"));
                self.refresh_game_version();
            }
        }
    }

    /// Asks the server for the current build again before the job runs, so a build that was
    /// replaced or rolled back since sign-in is not installed.
    fn start_job(&mut self, mode: Mode) -> Result<()> {
        let Some(token) = self.token() else {
            return Ok(());
        };
        let install_dir = self.install_dir();
        let db_path = paths::database_file()?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(RunningJob {
            mode,
            progress: None,
            cancel: Arc::clone(&cancel),
            samples: VecDeque::new(),
        });
        self.status = None;
        let tx = self.tx.clone();
        self.spawn(move |api| {
            let version = match api.game_versions(&token).and_then(pick_game_version) {
                Ok(version) => version,
                Err(e) => {
                    return Msg::PatchDone(mode, Err(e.context("getting the current build")));
                }
            };
            let _ = tx.send(Msg::GameVersion(Ok(version.clone())));
            let job = patcher::Job {
                api,
                db_path: &db_path,
                install_dir: &install_dir,
                version: &version,
                mode,
                cancel: &cancel,
            };
            Msg::PatchDone(mode, job.run(&mut |p| drop(tx.send(Msg::PatchProgress(p)))))
        });
        Ok(())
    }

    fn finish_job(&mut self, mode: Mode, result: Result<Report>) -> Result<()> {
        self.job = None;
        self.reload_install()?;
        let verb = mode.label();
        self.status = Some(match result {
            Ok(report) => {
                let mut text = format!(
                    "{verb} complete: {} ({} files, {} rewritten, {} downloaded).",
                    display_version(&report.version),
                    report.files,
                    report.rewritten,
                    human_bytes(report.downloaded),
                );
                if !report.unlisted.is_empty() {
                    let shown: Vec<&str> =
                        report.unlisted.iter().take(3).map(String::as_str).collect();
                    let more = report.unlisted.len().saturating_sub(shown.len());
                    text.push_str(&format!(
                        " Left alone, not in the manifest: {}{}.",
                        shown.join(", "),
                        if more > 0 {
                            format!(" and {more} more")
                        } else {
                            String::new()
                        }
                    ));
                }
                text
            }
            Err(e) if e.is::<Cancelled>() => {
                format!("{verb} cancelled. Downloaded bundles are kept and reused next time.")
            }
            Err(e) => format!("{verb} failed: {e:#}"),
        });
        Ok(())
    }

    fn sign_out(&mut self, email: Option<&str>, reason: Option<&str>) {
        self.auth = Auth::SignedOut;
        self.login = LoginForm::with_email(email.unwrap_or_default());
        self.login.error = reason.map(str::to_string);
    }

    fn logout(&mut self) -> Result<()> {
        let email = match &self.auth {
            Auth::SignedIn(s) => Some(s.email.clone()),
            _ => None,
        };
        self.db.clear_session()?;
        self.sign_out(email.as_deref(), None);
        Ok(())
    }

    fn submit_login(&mut self) {
        if self.login.busy {
            return;
        }
        self.login.error = None;
        if let Some(challenge) = self.login.challenge.clone() {
            let code = self.login.code.value().trim().to_string();
            if code.is_empty() {
                self.login.error = Some("Enter the code you were sent.".into());
                return;
            }
            let email = self.login.email.value();
            self.login.busy = true;
            self.spawn(move |api| Msg::Login {
                email,
                result: api
                    .submit_two_factor(&challenge, &code)
                    .map(LoginOutcome::Token),
            });
            return;
        }
        let email = self.login.email.value().trim().to_string();
        let password = self.login.password.value();
        if email.is_empty() || password.is_empty() {
            self.login.error = Some("Enter your email and password.".into());
            return;
        }
        self.login.busy = true;
        self.spawn(move |api| {
            let result = api.login(&email, &password);
            Msg::Login { email, result }
        });
    }

    // ---- feeds ------------------------------------------------------------------------

    fn refresh_feeds(&mut self) {
        self.news = Loadable::Loading;
        self.patch_index = Loadable::Loading;
        self.patch_notes.clear();
        self.spawn(|api| {
            Msg::News(
                api.get_text(feeds::NEWS_URL)
                    .and_then(|xml| feeds::parse_news(&xml)),
            )
        });
        self.spawn(|api| {
            Msg::PatchIndex(
                api.get_text(feeds::PATCH_NOTES_URL)
                    .and_then(|html| feeds::parse_patch_note_index(&html)),
            )
        });
    }

    pub fn selected_patch_note(&self) -> Option<&PatchNoteSummary> {
        match &self.patch_index {
            Loadable::Ready(notes) => notes.get(self.patch_state.selected()?),
            _ => None,
        }
    }

    fn load_selected_patch_note(&mut self) {
        self.patch_scroll = 0;
        let Some(id) = self.selected_patch_note().map(|n| n.id) else {
            return;
        };
        if self.patch_notes.contains_key(&id) {
            return;
        }
        self.patch_notes.insert(id, Loadable::Loading);
        self.spawn(move |api| {
            Msg::PatchNote(
                id,
                api.get_text(&feeds::patch_note_url(id))
                    .and_then(|html| feeds::parse_patch_note(&html)),
            )
        });
    }

    // ---- settings ---------------------------------------------------------------------

    pub fn settings_rows(&self) -> Vec<SettingsRow> {
        Setting::ALL
            .into_iter()
            .map(SettingsRow::Setting)
            .chain((0..self.env_vars.len()).map(SettingsRow::EnvVar))
            .chain([SettingsRow::AddEnvVar, SettingsRow::ImportToken])
            .collect()
    }

    fn selected_settings_row(&self) -> Option<SettingsRow> {
        self.settings_rows()
            .get(self.settings_state.selected()?)
            .copied()
    }

    fn reload_settings(&mut self) -> Result<()> {
        self.settings = Settings::load(&self.db)?;
        self.env_vars = self.db.env_vars()?;
        self.reload_install()?;
        let rows = self.settings_rows().len();
        if self.settings_state.selected().is_some_and(|i| i >= rows) {
            self.settings_state.select(Some(rows - 1));
        }
        Ok(())
    }

    fn activate_settings_row(&mut self, row: SettingsRow) {
        let (target, initial) = match row {
            SettingsRow::Setting(s) => (EditTarget::Setting(s), self.settings.get(s)),
            SettingsRow::EnvVar(i) => {
                let var = &self.env_vars[i];
                (
                    EditTarget::EnvVar(var.id),
                    format!("{}={}", var.name, var.value),
                )
            }
            SettingsRow::AddEnvVar => (EditTarget::NewEnvVar, String::new()),
            SettingsRow::ImportToken => {
                self.start_import(true);
                return;
            }
        };
        self.editor = Some(Editor {
            target,
            input: TextInput::new(&initial),
            error: None,
        });
    }

    fn commit_editor(&mut self) -> Result<()> {
        let Some(editor) = &mut self.editor else {
            return Ok(());
        };
        let value = editor.input.value();
        let outcome = match editor.target {
            EditTarget::Setting(setting) => self.settings.set(&self.db, setting, &value),
            EditTarget::NewEnvVar => {
                parse_env_assignment(&value).and_then(|(n, v)| self.db.add_env_var(&n, &v))
            }
            EditTarget::EnvVar(id) => {
                parse_env_assignment(&value).and_then(|(n, v)| self.db.update_env_var(id, &n, &v))
            }
        };
        match outcome {
            Ok(()) => {
                self.editor = None;
                self.reload_settings()?;
            }
            Err(e) => editor.error = Some(format!("{e:#}")),
        }
        Ok(())
    }

    fn reset_or_delete_settings_row(&mut self, row: SettingsRow) -> Result<()> {
        match row {
            SettingsRow::Setting(s) => Settings::reset(&self.db, s)?,
            SettingsRow::EnvVar(i) => self.db.delete_env_var(self.env_vars[i].id)?,
            SettingsRow::AddEnvVar | SettingsRow::ImportToken => return Ok(()),
        }
        self.reload_settings()
    }

    fn toggle_env_var(&mut self, row: SettingsRow) -> Result<()> {
        if let SettingsRow::EnvVar(i) = row {
            let var = &self.env_vars[i];
            self.db.set_env_var_enabled(var.id, !var.enabled)?;
            self.reload_settings()?;
        }
        Ok(())
    }

    // ---- input ------------------------------------------------------------------------

    fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => Ok(()),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Ok(());
        }
        if let Some(action) = self.confirm {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.confirm = None;
                    if let Some(mode) = action.mode() {
                        self.start_job(mode)?;
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') => self.confirm = None,
                _ => {}
            }
            return Ok(());
        }
        if let Some(editor) = &mut self.editor {
            match key.code {
                KeyCode::Enter => self.commit_editor()?,
                KeyCode::Esc => self.editor = None,
                _ => {
                    editor.input.handle(key);
                }
            }
            return Ok(());
        }
        match self.auth {
            Auth::Checking => {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    self.quit = true;
                }
                Ok(())
            }
            Auth::SignedOut => {
                self.handle_login_key(key);
                Ok(())
            }
            Auth::SignedIn(_) => self.handle_main_key(key),
        }
    }

    fn handle_login_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if self.login.challenge.is_some() => {
                self.login.challenge = None;
                self.login.code = TextInput::default();
                self.login.error = None;
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('r') if ctrl => {
                if let Some(challenge) = self.login.challenge.clone() {
                    self.spawn(move |api| Msg::TwoFactorResent(api.resend_two_factor(&challenge)));
                }
            }
            KeyCode::Tab | KeyCode::Down => self.login.move_focus(1),
            KeyCode::BackTab | KeyCode::Up => self.login.move_focus(-1),
            KeyCode::Enter => {
                if self.login.focused() == *self.login.fields().last().unwrap_or(&LoginField::Code)
                {
                    self.submit_login();
                } else {
                    self.login.move_focus(1);
                }
            }
            _ if !self.login.busy => {
                let field = self.login.focused();
                self.login.input_mut(field).handle(key);
            }
            _ => {}
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Result<()> {
        if let KeyCode::Char(c) = key.code {
            let c = c.to_ascii_lowercase();
            if let Some(action) = Action::ALL.into_iter().find(|a| a.key() == c) {
                return self.perform(action);
            }
        }
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Tab => self.tab = self.tab.offset(1),
            KeyCode::BackTab => self.tab = self.tab.offset(-1),
            KeyCode::Char('1') => self.tab = Tab::News,
            KeyCode::Char('2') => self.tab = Tab::PatchNotes,
            KeyCode::Char('3') => self.tab = Tab::Settings,
            KeyCode::F(5) => self.refresh_feeds(),
            _ => match self.tab {
                Tab::News => self.handle_news_key(key),
                Tab::PatchNotes => self.handle_patch_notes_key(key),
                Tab::Settings => self.handle_settings_key(key)?,
            },
        }
        Ok(())
    }

    fn request_quit(&mut self) {
        match &self.job {
            Some(_) if !self.quit_armed => {
                self.quit_armed = true;
                self.status = Some("A job is running. Press q again to cancel it and quit.".into());
            }
            Some(job) => {
                job.cancel.store(true, Ordering::Relaxed);
                self.quit = true;
            }
            None => self.quit = true,
        }
    }

    fn handle_news_key(&mut self, key: KeyEvent) {
        let len = match &self.news {
            Loadable::Ready(items) => items.len(),
            _ => return,
        };
        match key.code {
            KeyCode::Enter => self.open_selected_news(),
            code => move_selection(&mut self.news_state, len, code),
        }
    }

    fn handle_patch_notes_key(&mut self, key: KeyEvent) {
        let len = match &self.patch_index {
            Loadable::Ready(notes) => notes.len(),
            _ => return,
        };
        match key.code {
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.patch_scroll = self.patch_scroll.saturating_add(10)
            }
            KeyCode::PageUp => self.patch_scroll = self.patch_scroll.saturating_sub(10),
            KeyCode::Enter => {
                if let Some(id) = self.selected_patch_note().map(|n| n.id) {
                    self.open_link(&feeds::patch_note_url(id));
                }
            }
            code => {
                let before = self.patch_state.selected();
                move_selection(&mut self.patch_state, len, code);
                if self.patch_state.selected() != before {
                    self.load_selected_patch_note();
                }
            }
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(row) = self.selected_settings_row() else {
            return Ok(());
        };
        match key.code {
            KeyCode::Enter => self.activate_settings_row(row),
            KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('d') => {
                self.reset_or_delete_settings_row(row)?
            }
            KeyCode::Char(' ') => self.toggle_env_var(row)?,
            KeyCode::Char('a') => self.activate_settings_row(SettingsRow::AddEnvVar),
            KeyCode::Char('i') => self.start_import(true),
            code => {
                let len = self.settings_rows().len();
                move_selection(&mut self.settings_state, len, code);
            }
        }
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        let at = Position::new(mouse.column, mouse.row);
        let hit = self
            .hits
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(at))
            .map(|(_, c)| *c);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => match hit {
                Some(click) if self.editor.is_none() => self.click(click)?,
                _ => {}
            },
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if self.editor.is_none() => {
                let down = mouse.kind == MouseEventKind::ScrollDown;
                if hit == Some(Click::PatchDetail) {
                    self.patch_scroll = if down {
                        self.patch_scroll.saturating_add(3)
                    } else {
                        self.patch_scroll.saturating_sub(3)
                    };
                } else if matches!(self.auth, Auth::SignedIn(_)) {
                    let code = if down { KeyCode::Down } else { KeyCode::Up };
                    self.handle_main_key(KeyEvent::new(code, KeyModifiers::NONE))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn click(&mut self, click: Click) -> Result<()> {
        match click {
            Click::Tab(tab) => self.tab = tab,
            Click::Action(action) => self.perform(action)?,
            Click::News(i) => {
                if self.news_state.selected() == Some(i) {
                    self.open_selected_news();
                } else {
                    self.news_state.select(Some(i));
                }
            }
            Click::PatchNote(i) => {
                if self.patch_state.selected() != Some(i) {
                    self.patch_state.select(Some(i));
                    self.load_selected_patch_note();
                }
            }
            Click::PatchDetail => {}
            Click::SettingsRow(i) => {
                if self.settings_state.selected() == Some(i) {
                    if let Some(row) = self.selected_settings_row() {
                        self.activate_settings_row(row);
                    }
                } else {
                    self.settings_state.select(Some(i));
                }
            }
            Click::LoginField(field) => self.login.focus = Some(field),
            Click::LoginSubmit => self.submit_login(),
        }
        Ok(())
    }

    fn perform(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Logout => self.logout()?,
            Action::Play => self.request_play(),
            Action::Patch => self.request_job(Mode::Patch),
            Action::Repair => self.request_job(Mode::Repair),
            Action::Reinstall => self.request_job(Mode::Reinstall),
        }
        Ok(())
    }

    fn open_selected_news(&mut self) {
        let link = match &self.news {
            Loadable::Ready(items) => self
                .news_state
                .selected()
                .and_then(|i| items.get(i))
                .map(|n| n.link.clone()),
            _ => None,
        };
        if let Some(link) = link {
            self.open_link(&link);
        }
    }

    fn open_link(&mut self, url: &str) {
        self.status = Some(match open_in_browser(url) {
            Ok(()) => format!("Opened {url}"),
            Err(e) => format!("Could not open {url}: {e:#}"),
        });
    }

    // ---- background results -----------------------------------------------------------

    fn handle_msg(&mut self, msg: Msg) -> Result<()> {
        match msg {
            Msg::News(result) => self.news = loaded(result),
            Msg::PatchIndex(result) => {
                self.patch_index = loaded(result);
                self.load_selected_patch_note();
            }
            Msg::PatchNote(id, result) => {
                self.patch_notes.insert(id, loaded(result));
            }
            Msg::SessionChecked { token, result } => {
                let current = matches!(&self.auth, Auth::SignedIn(s) if s.token == token);
                match result {
                    _ if !current => {}
                    Ok(TokenStatus::Valid) => self.db.mark_session_validated(now())?,
                    Ok(TokenStatus::Rejected(reason)) => {
                        let email = match &self.auth {
                            Auth::SignedIn(s) => s.email.clone(),
                            _ => String::new(),
                        };
                        self.db.clear_session()?;
                        self.sign_out(Some(&email), Some(&reason));
                    }
                    Err(e) => self.status = Some(format!("Could not verify your session: {e:#}")),
                }
            }
            Msg::Imported { manual, result } => self.handle_import(manual, result)?,
            Msg::Login { email, result } => {
                self.login.busy = false;
                match result {
                    Ok(LoginOutcome::Token(token)) => {
                        match session_from_token(&email, token, SessionSource::Login) {
                            Ok(session) => self.sign_in(session)?,
                            Err(e) => self.login.error = Some(format!("{e:#}")),
                        }
                    }
                    Ok(LoginOutcome::TwoFactor(challenge)) => {
                        self.login.challenge = Some(challenge);
                        self.login.focus = Some(LoginField::Code);
                    }
                    Err(e) => self.login.error = Some(format!("{e:#}")),
                }
            }
            Msg::GameVersion(result) => self.game_version = loaded(result),
            Msg::PatchProgress(progress) => {
                if let Some(job) = &mut self.job {
                    job.record(progress);
                }
            }
            Msg::PatchDone(mode, result) => self.finish_job(mode, result)?,
            Msg::PlayChecked { token, result } => {
                self.play_pending = false;
                let current = matches!(&self.auth, Auth::SignedIn(s) if s.token == token);
                match result {
                    _ if !current => {}
                    Ok(TokenStatus::Valid) => self.launch_game(&token),
                    Ok(TokenStatus::Rejected(reason)) => {
                        let email = match &self.auth {
                            Auth::SignedIn(s) => s.email.clone(),
                            _ => String::new(),
                        };
                        self.db.clear_session()?;
                        self.sign_out(Some(&email), Some(&reason));
                    }
                    Err(e) => self.status = Some(format!("Could not verify your session: {e:#}")),
                }
            }
            Msg::TwoFactorResent(result) => {
                self.login.error = Some(match result {
                    Ok(()) => "A new code was sent.".into(),
                    Err(e) => format!("Could not resend the code: {e:#}"),
                });
            }
        }
        Ok(())
    }

    fn handle_import(&mut self, manual: bool, result: Result<ImportResult>) -> Result<()> {
        let message = match result {
            Ok(ImportResult::Imported(session)) => {
                let message = format!(
                    "Imported the official launcher's session for {} (expires {}).",
                    session.email,
                    session::format_date(session.expires_at)
                );
                self.sign_in(session)?;
                self.status = Some(message);
                return Ok(());
            }
            Ok(ImportResult::NotFound) => {
                "No signed-in official launcher session was found.".to_string()
            }
            Ok(ImportResult::Expired) => "The official launcher's token has expired.".to_string(),
            Ok(ImportResult::Rejected(reason)) => {
                format!("The official launcher's token was refused: {reason}")
            }
            Err(e) => format!("Could not import the official launcher's token: {e:#}"),
        };
        if manual {
            self.status = Some(message);
        } else if matches!(self.auth, Auth::Checking) {
            self.sign_out(None, None);
        }
        Ok(())
    }
}

fn game_log() -> PathBuf {
    paths::data_dir()
        .unwrap_or_default()
        .join("logs")
        .join("game.log")
}

/// The main game's build, as the official launcher picks it: slug `mnm`, else the first listed.
fn pick_game_version(versions: Vec<GameVersion>) -> Result<GameVersion> {
    let mut versions = versions.into_iter();
    let first = versions
        .next()
        .context("the server listed no game builds")?;
    if first.slug == "mnm" {
        return Ok(first);
    }
    Ok(versions.find(|v| v.slug == "mnm").unwrap_or(first))
}

/// `publish-0.30.2.4-1a719bcf…` -> `0.30.2.4`; other forms are shown unchanged.
pub fn display_version(version: &str) -> &str {
    let mut parts = version.split('-');
    match (parts.next(), parts.next()) {
        (Some("publish"), Some(number)) => number,
        _ => version,
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn loaded<T>(result: Result<T>) -> Loadable<T> {
    match result {
        Ok(value) => Loadable::Ready(value),
        Err(e) => Loadable::Failed(format!("{e:#}")),
    }
}

fn move_selection(state: &mut ListState, len: usize, code: KeyCode) {
    if len == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0);
    let next = match code {
        KeyCode::Down | KeyCode::Char('j') => (current + 1).min(len - 1),
        KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => len - 1,
        _ => return,
    };
    state.select(Some(next));
}

fn session_from_token(email: &str, token: String, source: SessionSource) -> Result<StoredSession> {
    session::ensure_token_shape(&token)?;
    let claims = session::decode_claims(&token)?;
    let email = claims
        .email
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| email.to_string());
    Ok(StoredSession {
        email,
        token,
        expires_at: claims.expires_at,
        source,
    })
}

/// Reads the official launcher's token and keeps it only if it is unexpired and the server accepts it.
fn import_official(api: &Api, path: &std::path::Path) -> Result<ImportResult> {
    let Some(found) = session::read_official_token(path)? else {
        return Ok(ImportResult::NotFound);
    };
    let session = session_from_token(
        found.username.as_deref().unwrap_or_default(),
        found.token,
        SessionSource::OfficialLauncher,
    )?;
    if session.expires_at <= now() {
        return Ok(ImportResult::Expired);
    }
    Ok(match api.token_status(&session.token)? {
        TokenStatus::Valid => ImportResult::Imported(session),
        TokenStatus::Rejected(reason) => ImportResult::Rejected(reason),
    })
}

/// Parses `NAME=VALUE`, requiring a shell-style variable name.
fn parse_env_assignment(input: &str) -> Result<(String, String)> {
    let (name, value) = input.split_once('=').context("use the form NAME=VALUE")?;
    let name = name.trim();
    let valid = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!("{name:?} is not a valid variable name");
    }
    Ok((name.to_string(), value.to_string()))
}

/// Hands an http(s) link from a feed to the desktop's browser. Other schemes are refused,
/// since feed content comes from outside and `xdg-open` would act on any URL it is given.
fn open_in_browser(url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("refusing a non-web link");
    }
    let mut child = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting xdg-open")?;
    thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_versions_and_sizes() {
        assert_eq!(
            display_version("publish-0.30.2.4-1a719bcf4379132306ae37b774d4cf3e536bcc02"),
            "0.30.2.4"
        );
        assert_eq!(display_version("dev-build"), "dev-build");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(9_304_182_965), "8.7 GiB");
    }

    #[test]
    fn env_assignment_validation() {
        assert_eq!(
            parse_env_assignment("DXVK_HUD=fps,gpuload").unwrap(),
            ("DXVK_HUD".into(), "fps,gpuload".into())
        );
        assert_eq!(
            parse_env_assignment("A=").unwrap(),
            ("A".into(), String::new())
        );
        assert_eq!(parse_env_assignment("A=b=c").unwrap().1, "b=c");
        assert!(parse_env_assignment("noequals").is_err());
        assert!(parse_env_assignment("1A=x").is_err());
        assert!(parse_env_assignment("A B=x").is_err());
    }

    #[test]
    fn browser_refuses_non_web_links() {
        assert!(open_in_browser("file:///etc/passwd").is_err());
        assert!(open_in_browser("--help").is_err());
    }

    #[test]
    fn text_input_edits_at_cursor() {
        let mut input = TextInput::new("ac");
        input.handle(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        input.handle(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(input.value(), "abc");
        input.handle(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        input.handle(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(input.value(), "bc");
        assert_eq!(input.cursor(), 0);
    }
}
