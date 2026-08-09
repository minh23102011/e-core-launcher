use std::collections::BTreeSet;
use std::error::Error;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Args;
use ecore_launcher::{
    diagnose, resolve_config_path, AppRegistry, ApplicationSettingsUpdate, AutostartState,
    CpuTopologyDetector, DesktopApplicationScanner, DirectCommandRunner, DiscoveredApplication,
    DiscoveryOptions, DoctorOptions, DoctorReport, DoctorStatus, IntegrationPaths, IoPriorityClass,
    RegisteredApplication, RegisteredApplicationAvailability, RegistryStore, SessionEnvironment,
    StartupManager, StartupStatus, TopologyClass,
};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::{Frame, Terminal};

const CORE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const STATUS_TTL: Duration = Duration::from_secs(6);

#[derive(Clone, Debug, Args)]
pub struct TuiArgs {
    /// Alternate CPU sysfs root for diagnostics or fixtures.
    #[arg(long, value_name = "PATH", default_value = "/sys/devices/system/cpu")]
    sysfs_root: PathBuf,

    /// Alternate procfs root for diagnostics or fixtures.
    #[arg(long, value_name = "PATH", default_value = "/proc")]
    proc_root: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Dashboard,
    Add,
    Configure,
    Doctor,
    Startup,
    Help,
    Confirm,
}

#[derive(Clone, Debug)]
enum PendingAction {
    Remove(String),
    StartupEnable { suppress_autostart: bool },
    StartupDisable,
}

#[derive(Clone, Debug)]
struct ConfigureDraft {
    desktop_id: String,
    delay_seconds: u64,
    nice: i8,
    io_class: IoPriorityClass,
    io_priority: Option<u8>,
    enforce_process_tree: bool,
    field: usize,
    reset_to_defaults: bool,
}

impl ConfigureDraft {
    fn from_application(application: &RegisteredApplication) -> Self {
        Self {
            desktop_id: application.desktop_id.clone(),
            delay_seconds: application.delay_seconds,
            nice: application.nice,
            io_class: application.io_class,
            io_priority: application.io_priority,
            enforce_process_tree: application.enforce_process_tree,
            field: 0,
            reset_to_defaults: false,
        }
    }

    fn move_field(&mut self, delta: i32) {
        self.field = cycle_index(self.field, 5, delta);
    }

    fn adjust(&mut self, delta: i32, coarse: bool) {
        self.reset_to_defaults = false;
        match self.field {
            0 => {
                let step = if coarse { 10 } else { 1 };
                self.delay_seconds = adjust_u64(self.delay_seconds, delta, step, 0, 3_600);
            }
            1 => {
                let step = if coarse { 5 } else { 1 };
                self.nice = adjust_i8(self.nice, delta, step, -20, 19);
            }
            2 => {
                self.io_class = cycle_io_class(self.io_class, delta);
                normalize_io_priority(&mut self.io_priority, self.io_class);
            }
            3 => {
                if matches!(self.io_class, IoPriorityClass::BestEffort | IoPriorityClass::Realtime)
                {
                    let current = self.io_priority.unwrap_or(4);
                    self.io_priority = Some(adjust_u8(current, delta, 1, 0, 7));
                }
            }
            4 => self.enforce_process_tree = !self.enforce_process_tree,
            _ => {}
        }
    }
}

#[derive(Clone, Debug)]
struct StatusMessage {
    text: String,
    is_error: bool,
    created_at: Instant,
}

struct TuiApp {
    config_path: PathBuf,
    sysfs_root: PathBuf,
    proc_root: PathBuf,
    registry: AppRegistry,
    availability: Vec<RegisteredApplicationAvailability>,
    topology_label: String,
    efficiency_cpus: Vec<u32>,
    startup_status: Option<StartupStatus>,
    startup_error: Option<String>,
    selected: usize,
    screen: Screen,
    add_candidates: Vec<DiscoveredApplication>,
    add_selected: BTreeSet<String>,
    add_index: usize,
    configure: Option<ConfigureDraft>,
    doctor: Option<DoctorReport>,
    doctor_index: usize,
    pending: Option<PendingAction>,
    status: Option<StatusMessage>,
    last_core_refresh: Instant,
    should_quit: bool,
}

impl TuiApp {
    fn new(config_path: PathBuf, sysfs_root: PathBuf, proc_root: PathBuf) -> Self {
        Self {
            config_path,
            sysfs_root,
            proc_root,
            registry: AppRegistry::default(),
            availability: Vec::new(),
            topology_label: "unknown".to_owned(),
            efficiency_cpus: Vec::new(),
            startup_status: None,
            startup_error: None,
            selected: 0,
            screen: Screen::Dashboard,
            add_candidates: Vec::new(),
            add_selected: BTreeSet::new(),
            add_index: 0,
            configure: None,
            doctor: None,
            doctor_index: 0,
            pending: None,
            status: None,
            last_core_refresh: Instant::now(),
            should_quit: false,
        }
    }

    fn selected_application(&self) -> Option<&RegisteredApplication> {
        self.registry.apps.get(self.selected)
    }

    fn refresh_core(&mut self) {
        let store = RegistryStore::new(&self.config_path);
        match store.load() {
            Ok(registry) => {
                self.registry = registry;
                self.selected = clamp_index(self.selected, self.registry.apps.len());
            }
            Err(error) => {
                self.set_error(format!("registry refresh failed: {error}"));
                return;
            }
        }

        let mut options = DiscoveryOptions::from_environment();
        options.include_no_display = true;
        match DesktopApplicationScanner::from_options(options).discover() {
            Ok(report) => {
                self.availability = self
                    .registry
                    .resolve_against(Some(&report))
                    .into_iter()
                    .map(|status| status.availability)
                    .collect();
            }
            Err(error) => {
                self.availability = vec![
                    RegisteredApplicationAvailability::Unknown;
                    self.registry.apps.len()
                ];
                self.set_error(format!("desktop discovery failed: {error}"));
            }
        }

        match CpuTopologyDetector::new(&self.sysfs_root).detect() {
            Ok(topology) => {
                self.topology_label = topology.classification.to_string();
                self.efficiency_cpus = topology.efficiency_cpus;
            }
            Err(error) => {
                self.topology_label = format!("error: {error}");
                self.efficiency_cpus.clear();
            }
        }
        self.last_core_refresh = Instant::now();
    }

    fn refresh_startup(&mut self) {
        match startup_manager(&self.config_path) {
            Ok((manager, store)) => match store.load() {
                Ok(registry) => match manager.status(&registry) {
                    Ok(status) => {
                        self.startup_status = Some(status);
                        self.startup_error = None;
                    }
                    Err(error) => {
                        self.startup_status = None;
                        self.startup_error = Some(error.to_string());
                    }
                },
                Err(error) => {
                    self.startup_status = None;
                    self.startup_error = Some(error.to_string());
                }
            },
            Err(error) => {
                self.startup_status = None;
                self.startup_error = Some(error.to_string());
            }
        }
    }

    fn refresh_all(&mut self) {
        self.refresh_core();
        self.refresh_startup();
    }

    fn maybe_refresh(&mut self) {
        if self.last_core_refresh.elapsed() >= CORE_REFRESH_INTERVAL {
            self.refresh_core();
        }
        if self
            .status
            .as_ref()
            .is_some_and(|message| message.created_at.elapsed() >= STATUS_TTL)
        {
            self.status = None;
        }
    }

    fn set_ok(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            text: text.into(),
            is_error: false,
            created_at: Instant::now(),
        });
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            text: text.into(),
            is_error: true,
            created_at: Instant::now(),
        });
    }

    fn move_selection(&mut self, delta: i32) {
        self.selected = move_index(self.selected, self.registry.apps.len(), delta);
    }

    fn toggle_selected(&mut self) {
        let Some(application) = self.selected_application() else {
            self.set_error("no registered application selected");
            return;
        };
        let desktop_id = application.desktop_id.clone();
        let desired = !application.enabled;
        let store = RegistryStore::new(&self.config_path);
        match store.mutate(|registry| registry.set_enabled(std::slice::from_ref(&desktop_id), desired))
        {
            Ok(_) => {
                self.refresh_core();
                self.set_ok(format!(
                    "{} {desktop_id}",
                    if desired { "enabled" } else { "disabled" }
                ));
            }
            Err(error) => self.set_error(error.to_string()),
        }
    }

    fn open_add(&mut self) {
        let mut options = DiscoveryOptions::from_environment();
        options.include_no_display = false;
        match DesktopApplicationScanner::from_options(options).discover() {
            Ok(report) => {
                let registered: BTreeSet<&str> = self
                    .registry
                    .apps
                    .iter()
                    .map(|application| application.desktop_id.as_str())
                    .collect();
                self.add_candidates = report
                    .applications
                    .into_iter()
                    .filter(|application| !registered.contains(application.desktop_id.as_str()))
                    .collect();
                self.add_candidates
                    .sort_by(|left, right| left.name.cmp(&right.name).then(left.desktop_id.cmp(&right.desktop_id)));
                self.add_selected.clear();
                self.add_index = 0;
                self.screen = Screen::Add;
            }
            Err(error) => self.set_error(format!("discovery failed: {error}")),
        }
    }

    fn apply_add(&mut self) {
        if self.add_selected.is_empty() {
            self.set_error("select at least one application with Space");
            return;
        }
        let selected: Vec<_> = self
            .add_candidates
            .iter()
            .filter(|application| self.add_selected.contains(&application.desktop_id))
            .cloned()
            .collect();
        let store = RegistryStore::new(&self.config_path);
        match store.mutate(|registry| registry.add_discovered(&selected)) {
            Ok(result) => {
                let count = result.added.len();
                self.screen = Screen::Dashboard;
                self.refresh_core();
                self.set_ok(format!("added {count} application(s)"));
            }
            Err(error) => self.set_error(error.to_string()),
        }
    }

    fn open_configure(&mut self) {
        let Some(application) = self.selected_application() else {
            self.set_error("no registered application selected");
            return;
        };
        self.configure = Some(ConfigureDraft::from_application(application));
        self.screen = Screen::Configure;
    }

    fn save_configure(&mut self) {
        let Some(draft) = self.configure.clone() else {
            return;
        };
        let update = if draft.reset_to_defaults {
            ApplicationSettingsUpdate {
                reset_to_defaults: true,
                ..ApplicationSettingsUpdate::default()
            }
        } else {
            ApplicationSettingsUpdate {
                delay_seconds: Some(draft.delay_seconds),
                nice: Some(draft.nice),
                io_class: Some(draft.io_class),
                io_priority: Some(draft.io_priority),
                enforce_process_tree: Some(draft.enforce_process_tree),
                reset_to_defaults: false,
            }
        };
        let store = RegistryStore::new(&self.config_path);
        match store.mutate(|registry| registry.configure(&draft.desktop_id, &update)) {
            Ok(_) => {
                self.configure = None;
                self.screen = Screen::Dashboard;
                self.refresh_core();
                self.set_ok(format!("configured {}", draft.desktop_id));
            }
            Err(error) => self.set_error(error.to_string()),
        }
    }

    fn request_remove(&mut self) {
        let Some(application) = self.selected_application() else {
            self.set_error("no registered application selected");
            return;
        };
        self.pending = Some(PendingAction::Remove(application.desktop_id.clone()));
        self.screen = Screen::Confirm;
    }

    fn remove_application(&mut self, desktop_id: String) {
        let store = RegistryStore::new(&self.config_path);
        match store.mutate(|registry| registry.remove(std::slice::from_ref(&desktop_id))) {
            Ok(_) => {
                self.refresh_core();
                self.set_ok(format!("removed {desktop_id}"));
            }
            Err(error) => self.set_error(error.to_string()),
        }
    }

    fn spawn_selected(&mut self, supervise: bool) {
        let Some(application) = self.selected_application() else {
            self.set_error("no registered application selected");
            return;
        };
        if !application.enabled {
            self.set_error("selected application is disabled");
            return;
        }
        let desktop_id = application.desktop_id.clone();
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.set_error(error.to_string());
                return;
            }
        };
        let command_name = if supervise { "supervise" } else { "run" };
        match Command::new(executable)
            .arg("--config")
            .arg(&self.config_path)
            .arg(command_name)
            .arg(&desktop_id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => self.set_ok(format!(
                "started {command_name} request for {desktop_id} (PID {})",
                child.id()
            )),
            Err(error) => self.set_error(format!("failed to start {command_name}: {error}")),
        }
    }

    fn run_doctor(&mut self) {
        let options = match doctor_options(&self.config_path, &self.sysfs_root, &self.proc_root) {
            Ok(options) => options,
            Err(error) => {
                self.set_error(error.to_string());
                return;
            }
        };
        self.doctor = Some(diagnose(&options));
        self.doctor_index = 0;
        self.screen = Screen::Doctor;
    }

    fn request_startup(&mut self, suppress_autostart: bool) {
        self.pending = Some(PendingAction::StartupEnable { suppress_autostart });
        self.screen = Screen::Confirm;
    }

    fn request_startup_disable(&mut self) {
        self.pending = Some(PendingAction::StartupDisable);
        self.screen = Screen::Confirm;
    }

    fn apply_pending(&mut self) {
        let Some(action) = self.pending.take() else {
            self.screen = Screen::Dashboard;
            return;
        };
        self.screen = Screen::Dashboard;
        match action {
            PendingAction::Remove(desktop_id) => self.remove_application(desktop_id),
            PendingAction::StartupEnable { suppress_autostart } => {
                match startup_manager(&self.config_path) {
                    Ok((manager, store)) => match store.load() {
                        Ok(registry) => match manager.enable(&registry, suppress_autostart) {
                            Ok(change) => {
                                self.refresh_startup();
                                self.set_ok(format!(
                                    "startup enabled; {} autostart override(s) changed",
                                    change.autostart_overrides_changed.len()
                                ));
                            }
                            Err(error) => self.set_error(error.to_string()),
                        },
                        Err(error) => self.set_error(error.to_string()),
                    },
                    Err(error) => self.set_error(error.to_string()),
                }
            }
            PendingAction::StartupDisable => match startup_manager(&self.config_path) {
                Ok((manager, _store)) => match manager.disable() {
                    Ok(change) => {
                        self.refresh_startup();
                        self.set_ok(format!(
                            "startup disabled; {} owned autostart override(s) removed",
                            change.autostart_overrides_changed.len()
                        ));
                    }
                    Err(error) => self.set_error(error.to_string()),
                },
                Err(error) => self.set_error(error.to_string()),
            },
        }
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(mut terminal) => {
                terminal.clear()?;
                Ok(Self { terminal })
            }
            Err(error) => {
                let _ = disable_raw_mode();
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen);
                Err(error)
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

pub fn run(arguments: &TuiArgs, config: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let config_path = make_absolute(resolve_config_path(config)?)?;
    let mut app = TuiApp::new(
        config_path,
        arguments.sysfs_root.clone(),
        arguments.proc_root.clone(),
    );
    app.refresh_all();

    let mut terminal = TerminalGuard::enter()?;
    while !app.should_quit {
        app.maybe_refresh();
        terminal.terminal.draw(|frame| draw(frame, &app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(&mut app, key);
                }
            }
        }
    }
    Ok(())
}

fn handle_key(app: &mut TuiApp, key: KeyEvent) {
    match app.screen {
        Screen::Dashboard => handle_dashboard_key(app, key),
        Screen::Add => handle_add_key(app, key),
        Screen::Configure => handle_configure_key(app, key),
        Screen::Doctor => handle_doctor_key(app, key),
        Screen::Startup => handle_startup_key(app, key),
        Screen::Help => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')) {
                app.screen = Screen::Dashboard;
            }
        }
        Screen::Confirm => match key.code {
            KeyCode::Enter | KeyCode::Char('y') => app.apply_pending(),
            KeyCode::Esc | KeyCode::Char('n') => {
                app.pending = None;
                app.screen = Screen::Dashboard;
            }
            _ => {}
        },
    }
}

fn handle_dashboard_key(app: &mut TuiApp, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Home | KeyCode::Char('g') => app.selected = 0,
        KeyCode::End | KeyCode::Char('G') => {
            app.selected = app.registry.apps.len().saturating_sub(1)
        }
        KeyCode::Char(' ') => app.toggle_selected(),
        KeyCode::Char('a') => app.open_add(),
        KeyCode::Char('c') => app.open_configure(),
        KeyCode::Char('x') => app.request_remove(),
        KeyCode::Char('r') => app.spawn_selected(false),
        KeyCode::Char('s') => app.spawn_selected(true),
        KeyCode::Char('d') => app.run_doctor(),
        KeyCode::Char('u') => {
            app.refresh_startup();
            app.screen = Screen::Startup;
        }
        KeyCode::Char('f') => {
            app.refresh_all();
            app.set_ok("refreshed dashboard state");
        }
        KeyCode::Char('?') => app.screen = Screen::Help,
        _ => {}
    }
}

fn handle_add_key(app: &mut TuiApp, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.screen = Screen::Dashboard,
        KeyCode::Down | KeyCode::Char('j') => {
            app.add_index = move_index(app.add_index, app.add_candidates.len(), 1)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.add_index = move_index(app.add_index, app.add_candidates.len(), -1)
        }
        KeyCode::Char(' ') => {
            if let Some(application) = app.add_candidates.get(app.add_index) {
                let desktop_id = application.desktop_id.clone();
                if !app.add_selected.insert(desktop_id.clone()) {
                    app.add_selected.remove(&desktop_id);
                }
            }
        }
        KeyCode::Enter => app.apply_add(),
        _ => {}
    }
}

fn handle_configure_key(app: &mut TuiApp, key: KeyEvent) {
    let Some(draft) = app.configure.as_mut() else {
        app.screen = Screen::Dashboard;
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.configure = None;
            app.screen = Screen::Dashboard;
        }
        KeyCode::Up | KeyCode::Char('k') => draft.move_field(-1),
        KeyCode::Down | KeyCode::Char('j') => draft.move_field(1),
        KeyCode::Left | KeyCode::Char('h') => {
            draft.adjust(-1, key.modifiers.contains(KeyModifiers::SHIFT))
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
            draft.adjust(1, key.modifiers.contains(KeyModifiers::SHIFT))
        }
        KeyCode::Char('r') => draft.reset_to_defaults = true,
        KeyCode::Enter => app.save_configure(),
        _ => {}
    }
}

fn handle_doctor_key(app: &mut TuiApp, key: KeyEvent) {
    let len = app.doctor.as_ref().map_or(0, |report| report.checks.len());
    match key.code {
        KeyCode::Esc => app.screen = Screen::Dashboard,
        KeyCode::Down | KeyCode::Char('j') => {
            app.doctor_index = move_index(app.doctor_index, len, 1)
        }
        KeyCode::Up | KeyCode::Char('k') => app.doctor_index = move_index(app.doctor_index, len, -1),
        KeyCode::Char('r') => app.run_doctor(),
        _ => {}
    }
}

fn handle_startup_key(app: &mut TuiApp, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.screen = Screen::Dashboard,
        KeyCode::Char('e') => app.request_startup(false),
        KeyCode::Char('a') => app.request_startup(true),
        KeyCode::Char('d') => app.request_startup_disable(),
        KeyCode::Char('r') => app.refresh_startup(),
        _ => {}
    }
}

fn draw(frame: &mut Frame<'_>, app: &TuiApp) {
    let area = frame.area();
    if area.width < 72 || area.height < 20 {
        let text = format!(
            "ecore-launcher TUI needs at least 72x20.\nCurrent terminal: {}x{}\n\nResize the terminal or press q to quit.",
            area.width, area.height
        );
        frame.render_widget(
            Paragraph::new(text)
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title("Terminal too small")),
            area,
        );
        return;
    }

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(area);
    draw_header(frame, app, vertical[0]);
    draw_dashboard(frame, app, vertical[1]);
    draw_footer(frame, app, vertical[2]);

    match app.screen {
        Screen::Dashboard => {}
        Screen::Add => draw_add_modal(frame, app, centered(area, 82, 78)),
        Screen::Configure => draw_configure_modal(frame, app, centered(area, 64, 68)),
        Screen::Doctor => draw_doctor_modal(frame, app, centered(area, 88, 82)),
        Screen::Startup => draw_startup_modal(frame, app, centered(area, 76, 76)),
        Screen::Help => draw_help_modal(frame, centered(area, 72, 76)),
        Screen::Confirm => draw_confirm_modal(frame, app, centered(area, 64, 34)),
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    let startup = app.startup_status.as_ref().map_or_else(
        || "startup ?".to_owned(),
        |status| match status.enabled {
            Some(true) => "startup enabled".to_owned(),
            Some(false) => "startup disabled".to_owned(),
            None => "startup unknown".to_owned(),
        },
    );
    let title = Line::from(vec![
        Span::styled(" ecore-launcher ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(&app.topology_label, topology_style(&app.topology_label)),
        Span::raw(format!("  E-cores {}  {startup}", format_cpus(&app.efficiency_cpus))),
    ]);
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_dashboard(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    draw_applications(frame, app, horizontal[0]);
    draw_details(frame, app, horizontal[1]);
}

fn draw_applications(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    let items: Vec<ListItem<'_>> = if app.registry.apps.is_empty() {
        vec![ListItem::new("No registered applications. Press a to add one.")]
    } else {
        app.registry
            .apps
            .iter()
            .enumerate()
            .map(|(index, application)| {
                let enabled = if application.enabled { "[x]" } else { "[ ]" };
                let availability = availability_label(app.availability.get(index));
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{enabled} "),
                        if application.enabled {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        },
                    ),
                    Span::raw(format!("{:<24}", truncate(&application.name, 23))),
                    Span::styled(availability.0, availability.1),
                ]))
            })
            .collect()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Applications "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !app.registry.apps.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_details(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    let mut lines = Vec::new();
    if let Some(application) = app.selected_application() {
        lines.push(Line::from(vec![
            Span::styled("Name: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(application.name.clone()),
        ]));
        lines.push(Line::from(format!("ID: {}", application.desktop_id)));
        lines.push(Line::from(format!(
            "State: {}",
            if application.enabled { "enabled" } else { "disabled" }
        )));
        lines.push(Line::from(format!(
            "Availability: {}",
            availability_label(app.availability.get(app.selected)).0
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(format!("Delay: {}s", application.delay_seconds)));
        lines.push(Line::from(format!("Nice: {}", application.nice)));
        lines.push(Line::from(format!("I/O class: {}", application.io_class)));
        lines.push(Line::from(format!(
            "I/O priority: {}",
            application
                .io_priority
                .map_or_else(|| "none".to_owned(), |value| value.to_string())
        )));
        lines.push(Line::from(format!(
            "Process tree: {}",
            if application.enforce_process_tree {
                "enforced by supervise"
            } else {
                "inherit only"
            }
        )));
        if let Some(path) = &application.desktop_file {
            lines.push(Line::from(""));
            lines.push(Line::from(format!("Desktop: {}", path.display())));
        }
    } else {
        lines.push(Line::from("Select or add an application."));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Details ")),
        area,
    );
}

fn draw_footer(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    let message = if let Some(status) = &app.status {
        Line::from(Span::styled(
            status.text.clone(),
            if status.is_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            },
        ))
    } else {
        Line::from("a Add  Space Toggle  c Configure  r Run  s Supervise  d Doctor  u Startup  x Remove  f Refresh  ? Help  q Quit")
    };
    frame.render_widget(
        Paragraph::new(message)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" Keyboard ")),
        area,
    );
}

fn draw_add_modal(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    frame.render_widget(Clear, area);
    let items: Vec<ListItem<'_>> = if app.add_candidates.is_empty() {
        vec![ListItem::new("No unregistered desktop applications found.")]
    } else {
        app.add_candidates
            .iter()
            .map(|application| {
                let selected = if app.add_selected.contains(&application.desktop_id) {
                    "[x]"
                } else {
                    "[ ]"
                };
                ListItem::new(format!(
                    "{selected} {:<28} {}",
                    truncate(&application.name, 27),
                    application.desktop_id
                ))
            })
            .collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Add applications — Space select · Enter add · Esc cancel "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !app.add_candidates.is_empty() {
        state.select(Some(app.add_index));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_configure_modal(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    frame.render_widget(Clear, area);
    let Some(draft) = &app.configure else {
        return;
    };
    let fields = [
        ("Delay", format!("{}s", draft.delay_seconds)),
        ("Nice", draft.nice.to_string()),
        ("I/O class", draft.io_class.to_string()),
        (
            "I/O priority",
            draft
                .io_priority
                .map_or_else(|| "none".to_owned(), |value| value.to_string()),
        ),
        (
            "Process tree",
            if draft.enforce_process_tree {
                "enforce".to_owned()
            } else {
                "inherit only".to_owned()
            },
        ),
    ];
    let mut lines = vec![Line::from(format!("{}", draft.desktop_id)), Line::from("")];
    for (index, (name, value)) in fields.iter().enumerate() {
        let prefix = if index == draft.field { ">" } else { " " };
        let style = if index == draft.field {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix} {name:<16} {value}"),
            style,
        )));
    }
    lines.push(Line::from(""));
    if draft.reset_to_defaults {
        lines.push(Line::from(Span::styled(
            "Reset to registry defaults is armed; Enter applies it.",
            Style::default().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(
        "j/k field · h/l adjust · Shift+h/l coarse · r reset · Enter save · Esc cancel",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Configure ")),
        area,
    );
}

fn draw_doctor_modal(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    frame.render_widget(Clear, area);
    let Some(report) = &app.doctor else {
        frame.render_widget(
            Paragraph::new("Doctor report unavailable.")
                .block(Block::default().borders(Borders::ALL).title(" Doctor ")),
            area,
        );
        return;
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(6), Constraint::Length(3)])
        .split(area);
    let summary = format!(
        "Overall: {:?} · {} checks",
        report.status,
        report.checks.len()
    );
    frame.render_widget(
        Paragraph::new(summary)
            .style(doctor_style(report.status))
            .block(Block::default().borders(Borders::ALL).title(" Doctor ")),
        vertical[0],
    );
    let items: Vec<ListItem<'_>> = report
        .checks
        .iter()
        .map(|check| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:?} ", check.status), doctor_style(check.status)),
                Span::styled(format!("{:<24}", check.id), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(check.summary.clone()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Checks "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !report.checks.is_empty() {
        state.select(Some(app.doctor_index));
    }
    frame.render_stateful_widget(list, vertical[1], &mut state);
    let detail = report.checks.get(app.doctor_index).map_or_else(
        || "Esc close · r refresh".to_owned(),
        |check| {
            let details = check
                .details
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(" · ");
            format!("{}\n{}\nEsc close · r refresh", check.summary, details)
        },
    );
    frame.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" Selected check ")),
        vertical[2],
    );
}

fn draw_startup_modal(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    if let Some(status) = &app.startup_status {
        lines.push(Line::from(format!("Unit: {}", status.unit_path.display())));
        lines.push(Line::from(format!("Present: {}", status.unit_present)));
        lines.push(Line::from(format!("Launcher-owned: {}", status.unit_owned)));
        lines.push(Line::from(format!("Current: {}", status.unit_current)));
        lines.push(Line::from(format!(
            "Enabled: {}",
            status
                .enabled
                .map_or("unknown", |enabled| if enabled { "yes" } else { "no" })
        )));
        lines.push(Line::from(format!(
            "Manager environment ready: {}",
            status
                .manager_environment
                .as_ref()
                .is_some_and(|environment| environment.is_ready())
        )));
        lines.push(Line::from(""));
        lines.push(Line::from("Autostart:"));
        if status.autostart.is_empty() {
            lines.push(Line::from("  no enabled registered applications"));
        } else {
            for assessment in &status.autostart {
                lines.push(Line::from(format!(
                    "  {:<30} {}",
                    assessment.desktop_id,
                    autostart_label(assessment.state)
                )));
            }
        }
    } else if let Some(error) = &app.startup_error {
        lines.push(Line::from(Span::styled(
            format!("Startup status unavailable: {error}"),
            Style::default().fg(Color::Red),
        )));
    } else {
        lines.push(Line::from("Startup status has not been loaded."));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "e enable · a enable + suppress duplicate autostart · d disable · r refresh · Esc close",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" User startup ")),
        area,
    );
}

fn draw_help_modal(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Clear, area);
    let text = vec![
        Line::from(Span::styled(
            "ecore-launcher TUI",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("j/k or arrows   move selection"),
        Line::from("Space           enable / disable selected app"),
        Line::from("a               discover and add applications"),
        Line::from("c               configure launch policy"),
        Line::from("r               start a one-shot run request"),
        Line::from("s               start a supervised run request"),
        Line::from("d               read-only doctor diagnostics"),
        Line::from("u               user startup / autostart integration"),
        Line::from("x               remove selected registry entry"),
        Line::from("f               refresh dashboard and startup state"),
        Line::from("?               help"),
        Line::from("q / Esc         quit from dashboard"),
        Line::from(""),
        Line::from("All controls are keyboard-only. No shell is invoked."),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Help — Esc to close ")),
        area,
    );
}

fn draw_confirm_modal(frame: &mut Frame<'_>, app: &TuiApp, area: Rect) {
    frame.render_widget(Clear, area);
    let prompt = match app.pending.as_ref() {
        Some(PendingAction::Remove(desktop_id)) => {
            format!("Remove {desktop_id} from the explicit registry?\nRunning processes are untouched.")
        }
        Some(PendingAction::StartupEnable {
            suppress_autostart: false,
        }) => "Enable launcher-owned user startup?\nNo application is launched immediately.".to_owned(),
        Some(PendingAction::StartupEnable {
            suppress_autostart: true,
        }) => "Enable user startup and create owned Hidden=true overrides for matching duplicate desktop autostarts?\nExisting user overrides are never overwritten.".to_owned(),
        Some(PendingAction::StartupDisable) => "Disable and remove only launcher-owned startup/autostart integration?\nRegistry and running applications are untouched.".to_owned(),
        None => "No pending action.".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(format!("{prompt}\n\nEnter / y confirm · Esc / n cancel"))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" Confirm ")),
        area,
    );
}

fn startup_manager(
    config_path: &Path,
) -> Result<(StartupManager<DirectCommandRunner>, RegistryStore), Box<dyn Error>> {
    let paths = IntegrationPaths::from_environment(None, &[], None)?;
    let registry_path = make_absolute(config_path.to_owned())?;
    let store = RegistryStore::new(&registry_path);
    let manager = StartupManager::new(
        paths,
        std::env::current_exe()?,
        registry_path,
        PathBuf::from("systemctl"),
        DirectCommandRunner,
    )?;
    Ok((manager, store))
}

fn doctor_options(
    config_path: &Path,
    sysfs_root: &Path,
    proc_root: &Path,
) -> Result<DoctorOptions, Box<dyn Error>> {
    let mut discovery = DiscoveryOptions::from_environment();
    discovery.include_no_display = true;
    Ok(DoctorOptions {
        registry_path: make_absolute(config_path.to_owned())?,
        discovery,
        sysfs_root: sysfs_root.to_owned(),
        proc_root: proc_root.to_owned(),
        integration_paths: IntegrationPaths::from_environment(None, &[], None)?,
        launcher_executable: std::env::current_exe()?,
        systemctl_executable: PathBuf::from("systemctl"),
        session: SessionEnvironment::from_environment(),
    })
}

fn make_absolute(path: PathBuf) -> Result<PathBuf, io::Error> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn move_index(current: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        current.saturating_add(delta as usize).min(len - 1)
    }
}

fn clamp_index(current: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        current.min(len - 1)
    }
}

fn cycle_index(current: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as i32;
    (current as i32 + delta).rem_euclid(len) as usize
}

fn adjust_u64(value: u64, direction: i32, step: u64, min: u64, max: u64) -> u64 {
    if direction < 0 {
        value.saturating_sub(step).max(min)
    } else {
        value.saturating_add(step).min(max)
    }
}

fn adjust_u8(value: u8, direction: i32, step: u8, min: u8, max: u8) -> u8 {
    if direction < 0 {
        value.saturating_sub(step).max(min)
    } else {
        value.saturating_add(step).min(max)
    }
}

fn adjust_i8(value: i8, direction: i32, step: i8, min: i8, max: i8) -> i8 {
    if direction < 0 {
        value.saturating_sub(step).max(min)
    } else {
        value.saturating_add(step).min(max)
    }
}

fn cycle_io_class(current: IoPriorityClass, delta: i32) -> IoPriorityClass {
    let classes = [
        IoPriorityClass::None,
        IoPriorityClass::BestEffort,
        IoPriorityClass::Realtime,
        IoPriorityClass::Idle,
    ];
    let index = classes
        .iter()
        .position(|candidate| *candidate == current)
        .unwrap_or(0);
    classes[cycle_index(index, classes.len(), delta)]
}

fn normalize_io_priority(priority: &mut Option<u8>, class: IoPriorityClass) {
    match class {
        IoPriorityClass::None | IoPriorityClass::Idle => *priority = None,
        IoPriorityClass::BestEffort | IoPriorityClass::Realtime => {
            if priority.is_none() {
                *priority = Some(4);
            }
        }
    }
}

fn availability_label(
    availability: Option<&RegisteredApplicationAvailability>,
) -> (&'static str, Style) {
    match availability {
        Some(RegisteredApplicationAvailability::Available { .. }) => {
            ("available", Style::default().fg(Color::Green))
        }
        Some(RegisteredApplicationAvailability::Unavailable) => {
            ("unavailable", Style::default().fg(Color::Red))
        }
        _ => ("unknown", Style::default().fg(Color::Yellow)),
    }
}

fn topology_style(label: &str) -> Style {
    if label == TopologyClass::Hybrid.to_string() {
        Style::default().fg(Color::Green)
    } else if label.starts_with("error:") {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Yellow)
    }
}

fn doctor_style(status: DoctorStatus) -> Style {
    match status {
        DoctorStatus::Ok => Style::default().fg(Color::Green),
        DoctorStatus::Warning => Style::default().fg(Color::Yellow),
        DoctorStatus::Error => Style::default().fg(Color::Red),
    }
}

fn autostart_label(state: AutostartState) -> &'static str {
    match state {
        AutostartState::NotPresent => "none",
        AutostartState::DuplicateRisk => "duplicate risk",
        AutostartState::SuppressedByLauncher => "suppressed by launcher",
        AutostartState::UserOverride => "user override",
    }
}

fn format_cpus(cpus: &[u32]) -> String {
    if cpus.is_empty() {
        return "none".to_owned();
    }
    let mut ranges = Vec::new();
    let mut start = cpus[0];
    let mut previous = cpus[0];
    for &cpu in &cpus[1..] {
        if cpu == previous.saturating_add(1) {
            previous = cpu;
            continue;
        }
        ranges.push(format_cpu_range(start, previous));
        start = cpu;
        previous = cpu;
    }
    ranges.push(format_cpu_range(start, previous));
    ranges.join(",")
}

fn format_cpu_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }
    let mut truncated: String = value.chars().take(max_chars - 1).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::{
        adjust_i8, adjust_u64, clamp_index, cycle_io_class, format_cpus, move_index,
        normalize_io_priority, IoPriorityClass,
    };

    #[test]
    fn selection_helpers_stay_in_bounds() {
        assert_eq!(move_index(0, 3, -1), 0);
        assert_eq!(move_index(0, 3, 1), 1);
        assert_eq!(move_index(2, 3, 1), 2);
        assert_eq!(clamp_index(9, 3), 2);
        assert_eq!(clamp_index(9, 0), 0);
    }

    #[test]
    fn configure_adjustments_respect_registry_bounds() {
        assert_eq!(adjust_u64(0, -1, 1, 0, 3_600), 0);
        assert_eq!(adjust_u64(3_600, 1, 1, 0, 3_600), 3_600);
        assert_eq!(adjust_i8(-20, -1, 1, -20, 19), -20);
        assert_eq!(adjust_i8(19, 1, 1, -20, 19), 19);
    }

    #[test]
    fn io_class_cycle_and_priority_normalization_are_deterministic() {
        assert_eq!(
            cycle_io_class(IoPriorityClass::None, 1),
            IoPriorityClass::BestEffort
        );
        let mut priority = Some(7);
        normalize_io_priority(&mut priority, IoPriorityClass::Idle);
        assert_eq!(priority, None);
        normalize_io_priority(&mut priority, IoPriorityClass::Realtime);
        assert_eq!(priority, Some(4));
    }

    #[test]
    fn cpu_lists_render_compactly() {
        assert_eq!(format_cpus(&[]), "none");
        assert_eq!(format_cpus(&[1, 2, 3, 5, 7, 8]), "1-3,5,7-8");
    }
}
