use std::{
    collections::{HashMap, HashSet},
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::Show,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Table, TableState, Tabs, Wrap,
    },
};

use crate::{
    cli, command,
    config::{Config, Tool},
    datetime, detach, doctor,
    error::{Error, Result},
    job::{CommandSpec, JobStatus, JobStore},
    state::StateDirs,
    worker,
};

const TICK_RATE: Duration = Duration::from_millis(100);
const MAX_ACTIVITY_LINES: usize = 1_000;
const ACCENT: Color = Color::Rgb(103, 232, 249);
const DIM: Color = Color::Rgb(153, 153, 153);
const SUBTLE: Color = Color::Rgb(110, 110, 120);
const SURFACE: Color = Color::Rgb(25, 25, 33);
const BORDER: Color = Color::Rgb(66, 66, 80);
const SELECTION_BG: Color = Color::Rgb(52, 52, 66);
const PANEL_BG: Color = Color::Rgb(28, 28, 36);
const BACKDROP_BG: Color = Color::Rgb(12, 12, 16);
const SUCCESS: Color = Color::Rgb(78, 186, 101);
const ERROR_COLOR: Color = Color::Rgb(255, 107, 128);
const WARNING_COLOR: Color = Color::Rgb(255, 193, 7);

/// Runs the interactive terminal interface.
pub fn run(state: StateDirs, config_path: Option<PathBuf>) -> Result<u8> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(Error::Message(
            "the TUI requires an interactive terminal; use `dvup list` or `dvup update` in scripts"
                .to_owned(),
        ));
    }

    enable_raw_mode()?;
    let mut restore = TerminalRestore { armed: true };
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let app_result = terminal
        .clear()
        .map_err(Error::from)
        .and_then(|()| App::new(state, config_path))
        .and_then(|mut app| run_app(&mut terminal, &mut app));

    drop(terminal);
    let restore_result = restore_terminal();
    restore.armed = false;
    match app_result {
        Err(error) => Err(error),
        Ok(code) => {
            restore_result?;
            Ok(code)
        }
    }
}

struct TerminalRestore {
    armed: bool,
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        if self.armed {
            let _ = restore_terminal();
        }
    }
}

fn restore_terminal() -> io::Result<()> {
    let raw_result = disable_raw_mode();
    let mut stdout = io::stdout();
    let screen_result = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen, Show);
    raw_result.and(screen_result)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Availability {
    Installed,
    Missing,
    Unsupported,
}

impl Availability {
    fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Installed, Language::English) => "installed",
            (Self::Installed, Language::Chinese) => "已安装",
            (Self::Missing, Language::English) => "missing",
            (Self::Missing, Language::Chinese) => "未安装",
            (Self::Unsupported, Language::English) => "unsupported",
            (Self::Unsupported, Language::Chinese) => "不支持",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Installed => Style::default().fg(SUCCESS),
            Self::Missing => Style::default().fg(SUBTLE),
            Self::Unsupported => Style::default().fg(WARNING_COLOR),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunState {
    Idle,
    Running,
    Updated,
    Queued,
    Failed,
}

impl RunState {
    fn label(self, frame: u64, language: Language) -> &'static str {
        match (self, language) {
            (Self::Idle, _) => "",
            (Self::Running, Language::English) if frame % 2 == 0 => "running ·",
            (Self::Running, Language::English) => "running •",
            (Self::Running, Language::Chinese) if frame % 2 == 0 => "运行中 ·",
            (Self::Running, Language::Chinese) => "运行中 •",
            (Self::Updated, Language::English) => "updated",
            (Self::Updated, Language::Chinese) => "已更新",
            (Self::Queued, Language::English) => "queued",
            (Self::Queued, Language::Chinese) => "已排队",
            (Self::Failed, Language::English) => "failed",
            (Self::Failed, Language::Chinese) => "失败",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Idle => Style::default(),
            Self::Running => Style::default().fg(ACCENT),
            Self::Updated => Style::default().fg(SUCCESS),
            Self::Queued => Style::default().fg(WARNING_COLOR),
            Self::Failed => Style::default().fg(ERROR_COLOR),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivityTone {
    Normal,
    Start,
    Success,
    Queued,
    Error,
    Hint,
    Metadata,
}

fn activity_tone(text: &str) -> ActivityTone {
    let unprefixed = datetime::strip_timestamp_prefix(text);
    let line = unprefixed.trim().trim_matches('=').trim();
    let lower = line.to_ascii_lowercase();

    if lower.starts_with("error:")
        || lower.starts_with("failure:")
        || line.starts_with("错误：")
        || line.starts_with("刷新失败：")
        || lower.contains(": failed ===")
        || lower.ends_with(": failed")
        || line.contains(": 失败 ===")
        || line.ends_with(": 失败")
        || lower.contains(" failed with error")
        || lower.contains(" failed (exit code")
        || lower.contains("httpforbidden")
        || (lower.starts_with("complete:") && !lower.contains("0 failed"))
        || (line.starts_with("完成：") && !line.contains("0 项失败"))
    {
        ActivityTone::Error
    } else if lower.contains(": queued ===")
        || lower.ends_with(": queued")
        || line.contains(": 已排队 ===")
        || line.ends_with(": 已排队")
        || lower.starts_with("queued ")
        || line.starts_with("已排队")
        || lower.contains("waiting on process policy")
        || line.contains("等待进程策略")
    {
        ActivityTone::Queued
    } else if lower.contains(": ok ===")
        || lower.ends_with(": ok")
        || line.contains(": 成功 ===")
        || line.ends_with(": 成功")
        || lower.starts_with("updated ")
        || lower.starts_with("added ")
        || lower.starts_with("removed ")
        || line.starts_with("已更新")
        || line.starts_with("已添加")
        || line.starts_with("已删除")
        || (lower.starts_with("complete:") && lower.contains("0 failed"))
        || (line.starts_with("完成：") && line.contains("0 项失败"))
    {
        ActivityTone::Success
    } else if lower.starts_with(">>> ") {
        ActivityTone::Start
    } else if lower.starts_with("please ")
        || lower.starts_with("powershell ")
        || lower.starts_with("suggested ")
        || lower.starts_with("process policy:")
        || line.starts_with("请")
        || line.starts_with("进程策略：")
    {
        ActivityTone::Hint
    } else if lower.starts_with("job:")
        || lower.starts_with("inspect:")
        || lower.starts_with("command:")
        || lower.starts_with("resource:")
        || lower.starts_with("exit:")
        || line.starts_with("任务：")
        || line.starts_with("查看：")
        || line.starts_with("命令：")
        || line.starts_with("资源：")
        || line.starts_with("退出码：")
    {
        ActivityTone::Metadata
    } else {
        ActivityTone::Normal
    }
}

fn activity_style(text: &str) -> Style {
    let style = match activity_tone(text) {
        ActivityTone::Normal => Style::default(),
        ActivityTone::Start => Style::default().fg(ACCENT),
        ActivityTone::Success => Style::default().fg(SUCCESS),
        ActivityTone::Queued | ActivityTone::Hint => Style::default().fg(WARNING_COLOR),
        ActivityTone::Error => Style::default().fg(ERROR_COLOR),
        ActivityTone::Metadata => Style::default().fg(Color::Rgb(120, 170, 210)),
    };
    if text.trim_start().starts_with("===") || text.trim_start().starts_with(">>>") {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn activity_outcome_label(success: bool, queued: bool, language: Language) -> &'static str {
    match (success, queued, language) {
        (false, _, Language::English) => "FAILED",
        (false, _, Language::Chinese) => "失败",
        (true, true, Language::English) => "QUEUED",
        (true, true, Language::Chinese) => "已排队",
        (true, false, Language::English) => "OK",
        (true, false, Language::Chinese) => "成功",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Language {
    English,
    Chinese,
}

impl Language {
    fn toggle(self) -> Self {
        match self {
            Self::English => Self::Chinese,
            Self::Chinese => Self::English,
        }
    }

    fn text(self, english: &'static str, chinese: &'static str) -> &'static str {
        match self {
            Self::English => english,
            Self::Chinese => chinese,
        }
    }

    fn job_status(self, status: &JobStatus) -> &'static str {
        match (self, status) {
            (Self::English, JobStatus::Pending) => "pending",
            (Self::Chinese, JobStatus::Pending) => "等待执行",
            (Self::English, JobStatus::WaitingForLocks { .. }) => "waiting_for_locks",
            (Self::Chinese, JobStatus::WaitingForLocks { .. }) => "等待进程退出",
            (Self::English, JobStatus::TerminatingProcesses { .. }) => "terminating_processes",
            (Self::Chinese, JobStatus::TerminatingProcesses { .. }) => "正在终止进程",
            (Self::English, JobStatus::Running { .. }) => "running",
            (Self::Chinese, JobStatus::Running { .. }) => "运行中",
            (Self::English, JobStatus::Succeeded { .. }) => "succeeded",
            (Self::Chinese, JobStatus::Succeeded { .. }) => "成功",
            (Self::English, JobStatus::Failed { .. }) => "failed",
            (Self::Chinese, JobStatus::Failed { .. }) => "失败",
        }
    }
}

#[derive(Clone, Debug)]
enum VersionState {
    Loading,
    Available(String),
    Unavailable,
}

impl VersionState {
    fn label(&self) -> &str {
        match self {
            Self::Loading => "…",
            Self::Available(version) => version,
            Self::Unavailable => "—",
        }
    }

    fn style(&self) -> Style {
        match self {
            Self::Available(_) => Style::default().fg(ACCENT),
            Self::Loading | Self::Unavailable => Style::default().fg(SUBTLE),
        }
    }
}

#[derive(Clone, Debug)]
struct ToolItem {
    name: String,
    command: String,
    version: VersionState,
    version_command: CommandSpec,
    version_probe_id: u64,
    availability: Availability,
    custom: bool,
    selected: bool,
    run_state: RunState,
    elapsed: Option<Duration>,
}

#[derive(Clone, Debug)]
struct JobItem {
    id: String,
    name: String,
    status: JobStatus,
    updated_at_unix_ms: u128,
}

#[derive(Clone, Copy, Debug)]
struct ModalInputHitbox {
    area: Rect,
    field: usize,
    visible_start: usize,
    visible_end: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tab {
    Tools,
    Activity,
    Jobs,
    Doctor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessStrategy {
    Wait,
    Terminate,
}

impl ProcessStrategy {
    fn toggle(self) -> Self {
        match self {
            Self::Wait => Self::Terminate,
            Self::Terminate => Self::Wait,
        }
    }

    fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Wait, Language::English) => "WAIT",
            (Self::Wait, Language::Chinese) => "等待",
            (Self::Terminate, Language::English) => "TERMINATE",
            (Self::Terminate, Language::Chinese) => "终止",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Wait => Style::default().fg(WARNING_COLOR),
            Self::Terminate => Style::default()
                .fg(ERROR_COLOR)
                .add_modifier(Modifier::BOLD),
        }
    }

    fn terminates(self) -> bool {
        self == Self::Terminate
    }
}

impl Tab {
    fn next(self) -> Self {
        match self {
            Self::Tools => Self::Activity,
            Self::Activity => Self::Jobs,
            Self::Jobs => Self::Doctor,
            Self::Doctor => Self::Tools,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Tools => Self::Doctor,
            Self::Activity => Self::Tools,
            Self::Jobs => Self::Activity,
            Self::Doctor => Self::Jobs,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Tools => 0,
            Self::Activity => 1,
            Self::Jobs => 2,
            Self::Doctor => 3,
        }
    }

    fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Tools),
            1 => Some(Self::Activity),
            2 => Some(Self::Jobs),
            3 => Some(Self::Doctor),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct TextInput {
    value: String,
    cursor: usize,
    selection_anchor: Option<usize>,
}

impl TextInput {
    fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            cursor: value.len(),
            value,
            selection_anchor: None,
        }
    }

    fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        (anchor != self.cursor).then_some({
            if anchor < self.cursor {
                (anchor, self.cursor)
            } else {
                (self.cursor, anchor)
            }
        })
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    fn select_all(&mut self) {
        self.selection_anchor = (!self.value.is_empty()).then_some(0);
        self.cursor = self.value.len();
    }

    fn move_left(&mut self, extend_selection: bool) {
        if !extend_selection && let Some((start, _)) = self.selection() {
            self.cursor = start;
            self.clear_selection();
            return;
        }
        let target = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.move_to(target, extend_selection);
    }

    fn move_right(&mut self, extend_selection: bool) {
        if !extend_selection && let Some((_, end)) = self.selection() {
            self.cursor = end;
            self.clear_selection();
            return;
        }
        let target = self.value[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(offset, _)| self.cursor + offset)
            .unwrap_or(self.value.len());
        self.move_to(target, extend_selection);
    }

    fn move_home(&mut self, extend_selection: bool) {
        self.move_to(0, extend_selection);
    }

    fn move_end(&mut self, extend_selection: bool) {
        self.move_to(self.value.len(), extend_selection);
    }

    fn insert(&mut self, character: char) {
        self.delete_selection();
        self.value.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn backspace(&mut self) {
        if self.delete_selection() || self.cursor == 0 {
            return;
        }
        let start = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete(&mut self) {
        if self.delete_selection() || self.cursor == self.value.len() {
            return;
        }
        let end = self.value[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(offset, _)| self.cursor + offset)
            .unwrap_or(self.value.len());
        self.value.replace_range(self.cursor..end, "");
    }

    fn visible_range(&self, max_width: usize) -> (usize, usize) {
        if max_width == 0 {
            return (self.cursor, self.cursor);
        }
        let mut start = 0;
        while display_width(&self.value[start..self.cursor]) > max_width {
            start += self.value[start..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(0);
        }
        let mut end = self.value.len();
        while end > self.cursor && display_width(&self.value[start..end]) > max_width {
            end = self.value[..end]
                .char_indices()
                .next_back()
                .map(|(index, _)| index)
                .unwrap_or(start);
        }
        (start, end.max(self.cursor))
    }

    fn move_to(&mut self, target: usize, extend_selection: bool) {
        if extend_selection {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.clear_selection();
        }
        self.cursor = target;
        if self.selection_anchor == Some(self.cursor) {
            self.clear_selection();
        }
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        self.value.replace_range(start..end, "");
        self.cursor = start;
        self.clear_selection();
        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandFormMode {
    Add,
    Edit,
}

impl CommandFormMode {
    fn operation(self) -> Operation {
        match self {
            Self::Add => Operation::Add,
            Self::Edit => Operation::Edit,
        }
    }
}

#[derive(Clone, Debug)]
enum Modal {
    None,
    ConfirmUpdate {
        tools: Vec<String>,
    },
    AddCommand {
        mode: CommandFormMode,
        original_name: Option<String>,
        field: usize,
        name: TextInput,
        command: TextInput,
    },
    ConfirmAdd {
        mode: CommandFormMode,
        original_name: Option<String>,
        name: String,
        command: String,
    },
    ConfirmDelete {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operation {
    Update,
    Add,
    Edit,
    Delete,
}

impl Operation {
    fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Update, Language::English) => "update",
            (Self::Update, Language::Chinese) => "更新",
            (Self::Add, Language::English) => "save",
            (Self::Add, Language::Chinese) => "保存",
            (Self::Edit, Language::English) => "edit",
            (Self::Edit, Language::Chinese) => "编辑",
            (Self::Delete, Language::English) => "remove",
            (Self::Delete, Language::Chinese) => "删除",
        }
    }

    fn completion_message(
        self,
        name: &str,
        success: bool,
        elapsed: Duration,
        running: usize,
        language: Language,
    ) -> String {
        match (self, success, language) {
            (Self::Add, true, Language::English) => {
                format!("Added {name}; it has not been run. Press Enter to update it")
            }
            (Self::Add, true, Language::Chinese) => {
                format!("已添加 {name}，尚未执行；按 Enter 可更新")
            }
            (Self::Edit, true, Language::English) => {
                format!("Updated {name}; the command has not been run")
            }
            (Self::Edit, true, Language::Chinese) => {
                format!("已更新 {name} 的命令，尚未执行")
            }
            (Self::Delete, true, Language::English) => format!("Removed {name}"),
            (Self::Delete, true, Language::Chinese) => format!("已删除 {name}"),
            (Self::Add, false, Language::English) => {
                format!("Could not add {name}; open Activity to view the error")
            }
            (Self::Add, false, Language::Chinese) => {
                format!("无法添加 {name}；请在活动页查看错误")
            }
            (Self::Edit, false, Language::English) => {
                format!("Could not edit {name}; open Activity to view the error")
            }
            (Self::Edit, false, Language::Chinese) => {
                format!("无法编辑 {name}；请在活动页查看错误")
            }
            (Self::Delete, false, Language::English) => {
                format!("Could not remove {name}; open Activity to view the error")
            }
            (Self::Delete, false, Language::Chinese) => {
                format!("无法删除 {name}；请在活动页查看错误")
            }
            (Self::Update, _, Language::English) => format!(
                "{name} {} in {:.1}s. {running} operation(s) still running.",
                if success { "finished" } else { "failed" },
                elapsed.as_secs_f64(),
            ),
            (Self::Update, _, Language::Chinese) => format!(
                "{name} {}，耗时 {:.1} 秒；仍有 {running} 项操作在运行。",
                if success { "已完成" } else { "失败" },
                elapsed.as_secs_f64(),
            ),
        }
    }
}

#[derive(Debug)]
enum AppEvent {
    Finished {
        name: String,
        success: bool,
        output: String,
        operation: Operation,
        elapsed: Duration,
    },
    VersionResolved {
        name: String,
        probe_id: u64,
        version: Option<String>,
    },
    DoctorResolved {
        probe_id: u64,
        diagnoses: Vec<doctor::ToolDiagnosis>,
        error: Option<String>,
    },
}

struct UpdateBatch {
    started: Instant,
    total: usize,
}

struct App {
    state: StateDirs,
    config_path: Option<PathBuf>,
    executable: PathBuf,
    tools: Vec<ToolItem>,
    tool_index: usize,
    tool_hitboxes: Vec<(Rect, usize)>,
    jobs: Vec<JobItem>,
    job_index: usize,
    activity: Vec<String>,
    activity_timestamps: Vec<String>,
    activity_scroll: usize,
    activity_rendered_height: usize,
    expanded_activity: HashSet<usize>,
    activity_hitboxes: Vec<(Rect, usize)>,
    hovered_activity: Option<usize>,
    tab_hitboxes: Vec<(Rect, usize)>,
    hovered_tab: Option<usize>,
    tab: Tab,
    language: Language,
    process_strategy: ProcessStrategy,
    modal: Modal,
    message: String,
    running: usize,
    next_version_probe_id: u64,
    update_batch: Option<UpdateBatch>,
    frame: u64,
    last_job_refresh: Instant,
    expanded_job: Option<String>,
    job_log: Vec<String>,
    job_log_scroll: usize,
    job_hitboxes: Vec<(Rect, usize)>,
    job_detail_area: Option<Rect>,
    doctor_diagnoses: Vec<doctor::ToolDiagnosis>,
    doctor_index: usize,
    doctor_hitboxes: Vec<(Rect, usize)>,
    expanded_doctor: Option<String>,
    doctor_detail_scroll: usize,
    doctor_detail_area: Option<Rect>,
    doctor_loading: bool,
    doctor_probe_id: u64,
    next_doctor_probe_id: u64,
    doctor_checked_at: Option<String>,
    modal_input_hitboxes: Vec<ModalInputHitbox>,
    modal_drag: Option<(usize, usize)>,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    should_quit: bool,
}

impl App {
    fn new(state: StateDirs, config_path: Option<PathBuf>) -> Result<Self> {
        let executable = std::env::current_exe()?;
        let (tx, rx) = mpsc::channel();
        let started_at = datetime::now();
        let mut app = Self {
            state,
            config_path,
            executable,
            tools: Vec::new(),
            tool_index: 0,
            tool_hitboxes: Vec::new(),
            jobs: Vec::new(),
            job_index: 0,
            activity: vec![
                "Welcome to dvup.".to_owned(),
                "Select tools with Space and press Enter to update.".to_owned(),
            ],
            activity_timestamps: vec![started_at.clone(), started_at],
            activity_scroll: 0,
            activity_rendered_height: 2,
            expanded_activity: HashSet::new(),
            activity_hitboxes: Vec::new(),
            hovered_activity: None,
            tab_hitboxes: Vec::new(),
            hovered_tab: None,
            tab: Tab::Tools,
            language: Language::English,
            process_strategy: ProcessStrategy::Wait,
            modal: Modal::None,
            message: "Ready".to_owned(),
            running: 0,
            next_version_probe_id: 0,
            update_batch: None,
            frame: 0,
            last_job_refresh: Instant::now(),
            expanded_job: None,
            job_log: Vec::new(),
            job_log_scroll: 0,
            job_hitboxes: Vec::new(),
            job_detail_area: None,
            doctor_diagnoses: Vec::new(),
            doctor_index: 0,
            doctor_hitboxes: Vec::new(),
            expanded_doctor: None,
            doctor_detail_scroll: 0,
            doctor_detail_area: None,
            doctor_loading: false,
            doctor_probe_id: 0,
            next_doctor_probe_id: 0,
            doctor_checked_at: None,
            modal_input_hitboxes: Vec::new(),
            modal_drag: None,
            tx,
            rx,
            should_quit: false,
        };
        app.refresh_tools()?;
        app.refresh_jobs()?;
        Ok(app)
    }

    fn refresh_tools(&mut self) -> Result<()> {
        let previous: HashMap<_, _> = self
            .tools
            .iter()
            .map(|tool| {
                (
                    tool.name.clone(),
                    (tool.selected, tool.run_state, tool.elapsed),
                )
            })
            .collect();
        let (manifest, working_directory, _) =
            cli::load_manifest(self.config_path.clone(), &self.state)?;
        let custom_names = load_custom_names(&self.state)?;
        self.tools = manifest
            .tools
            .into_iter()
            .map(|(name, tool)| {
                let command_spec = CommandSpec {
                    program: tool.program.clone(),
                    args: tool.args.clone(),
                    working_directory: working_directory.clone(),
                };
                let availability = if !tool.supports_current_platform() {
                    Availability::Unsupported
                } else if command::is_available(&command_spec) {
                    Availability::Installed
                } else {
                    Availability::Missing
                };
                let (selected, run_state, elapsed) =
                    previous
                        .get(&name)
                        .copied()
                        .unwrap_or((false, RunState::Idle, None));
                let actual_command = format_command(&tool.program, &tool.args);
                let version_command =
                    version_command_for_tool(&name, &tool, working_directory.clone());
                ToolItem {
                    command: cli::display_command(&name, &actual_command),
                    custom: custom_names.contains(&name),
                    name,
                    availability,
                    version: if availability == Availability::Unsupported {
                        VersionState::Unavailable
                    } else {
                        VersionState::Loading
                    },
                    version_command,
                    version_probe_id: 0,
                    selected,
                    run_state,
                    elapsed,
                }
            })
            .collect();
        self.tool_index = self.tool_index.min(self.tools.len().saturating_sub(1));
        for index in 0..self.tools.len() {
            if self.tools[index].availability != Availability::Unsupported {
                self.start_version_probe(index);
            }
        }
        Ok(())
    }

    fn start_version_probe(&mut self, index: usize) {
        let Some(tool) = self.tools.get_mut(index) else {
            return;
        };
        self.next_version_probe_id = self.next_version_probe_id.wrapping_add(1).max(1);
        let probe_id = self.next_version_probe_id;
        tool.version = VersionState::Loading;
        tool.version_probe_id = probe_id;
        spawn_version_probe(
            self.tx.clone(),
            tool.name.clone(),
            probe_id,
            tool.version_command.clone(),
        );
    }

    fn refresh_tool_version(&mut self, name: &str) {
        if let Some(index) = self.tools.iter().position(|tool| tool.name == name)
            && self.tools[index].availability != Availability::Unsupported
        {
            self.start_version_probe(index);
        }
    }

    fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        if tab == Tab::Doctor
            && self.doctor_probe_id == 0
            && let Err(error) = self.refresh_doctor()
        {
            self.message = match self.language {
                Language::English => format!("Diagnostics failed: {error}"),
                Language::Chinese => format!("诊断失败：{error}"),
            };
        }
    }

    fn refresh_doctor(&mut self) -> Result<()> {
        let (manifest, working_directory, _) =
            cli::load_manifest(self.config_path.clone(), &self.state)?;
        self.next_doctor_probe_id = self.next_doctor_probe_id.wrapping_add(1).max(1);
        let probe_id = self.next_doctor_probe_id;
        self.doctor_probe_id = probe_id;
        self.doctor_loading = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let (diagnoses, error) = match doctor::diagnose(&manifest, &working_directory, None) {
                Ok(diagnoses) => (diagnoses, None),
                Err(error) => (Vec::new(), Some(error.to_string())),
            };
            let _ = tx.send(AppEvent::DoctorResolved {
                probe_id,
                diagnoses,
                error,
            });
        });
        Ok(())
    }

    fn toggle_doctor_detail(&mut self) {
        let Some(diagnosis) = self.doctor_diagnoses.get(self.doctor_index) else {
            return;
        };
        if self.expanded_doctor.as_deref() == Some(&diagnosis.name) {
            self.expanded_doctor = None;
        } else {
            self.expanded_doctor = Some(diagnosis.name.clone());
        }
        self.doctor_detail_scroll = 0;
    }

    fn refresh_jobs(&mut self) -> Result<()> {
        let store = JobStore::new(self.state.clone())?;
        let previously_active = self
            .jobs
            .iter()
            .filter(|job| !job.status.is_terminal())
            .map(|job| job.id.clone())
            .collect::<HashSet<_>>();
        let jobs = store
            .list()?
            .into_iter()
            .map(|job| JobItem {
                id: job.id,
                name: job.name,
                status: job.status,
                updated_at_unix_ms: job.updated_at_unix_ms,
            })
            .collect::<Vec<_>>();
        let completed_tools = jobs
            .iter()
            .filter(|job| {
                previously_active.contains(&job.id)
                    && matches!(job.status, JobStatus::Succeeded { .. })
            })
            .map(|job| job.name.clone())
            .collect::<HashSet<_>>();
        let should_refresh_doctor = !completed_tools.is_empty() && self.doctor_probe_id > 0;
        self.jobs = jobs;
        self.job_index = self.job_index.min(self.jobs.len().saturating_sub(1));
        if let Some(id) = self.expanded_job.clone() {
            if self.jobs.iter().any(|job| job.id == id) {
                self.job_log =
                    sanitize_terminal_output(&String::from_utf8_lossy(&store.read_log(&id)?));
            } else {
                self.expanded_job = None;
                self.job_log.clear();
                self.job_log_scroll = 0;
            }
        }
        for name in completed_tools {
            self.refresh_tool_version(&name);
        }
        if should_refresh_doctor {
            self.refresh_doctor()?;
        }
        self.last_job_refresh = Instant::now();
        Ok(())
    }

    fn terminate_active_job_waits(&mut self) -> Result<(usize, usize, usize, usize, usize)> {
        self.terminate_active_job_waits_with(detach::ensure_worker)
    }

    fn terminate_active_job_waits_with<F>(
        &mut self,
        mut ensure_worker: F,
    ) -> Result<(usize, usize, usize, usize, usize)>
    where
        F: FnMut(&crate::job::Job, &StateDirs) -> Result<detach::WorkerLaunch>,
    {
        let store = JobStore::new(self.state.clone())?;
        let mut active_jobs = 0;
        let mut changed_rules = 0;
        let mut stopped_processes = 0;
        let mut restarted_jobs = 0;
        let mut skipped_jobs = 0;
        for mut job in store.list()? {
            let needs_worker_recovery = matches!(
                job.status,
                JobStatus::WaitingForLocks { .. } | JobStatus::TerminatingProcesses { .. }
            );
            if !matches!(
                job.status,
                JobStatus::Pending
                    | JobStatus::WaitingForLocks { .. }
                    | JobStatus::TerminatingProcesses { .. }
            ) {
                continue;
            }
            active_jobs += 1;
            match job.terminate_waiting_processes() {
                Ok(changed) => {
                    if changed > 0 {
                        store.save(&job)?;
                    }
                    changed_rules += changed;
                    match worker::apply_terminate_rules(&mut job, &store) {
                        Ok(stopped) => stopped_processes += stopped,
                        Err(_) => {
                            skipped_jobs += 1;
                            continue;
                        }
                    }

                    let latest = store.load(&job.id)?;
                    if matches!(
                        latest.status,
                        JobStatus::Pending
                            | JobStatus::WaitingForLocks { .. }
                            | JobStatus::TerminatingProcesses { .. }
                    ) {
                        job = latest;
                        job.set_status(JobStatus::Pending);
                        store.save(&job)?;
                    } else {
                        continue;
                    }

                    if needs_worker_recovery
                        && ensure_worker(&job, store.dirs())? == detach::WorkerLaunch::Spawned
                    {
                        restarted_jobs += 1;
                    }
                }
                Err(_) => skipped_jobs += 1,
            }
        }
        self.refresh_jobs()?;
        Ok((
            active_jobs,
            changed_rules,
            stopped_processes,
            restarted_jobs,
            skipped_jobs,
        ))
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                AppEvent::Finished {
                    name,
                    success,
                    output,
                    operation,
                    elapsed,
                } => {
                    self.running = self.running.saturating_sub(1);
                    let queued = operation == Operation::Update
                        && success
                        && output_was_queued(&name, &output);
                    if operation == Operation::Update {
                        if let Some(tool) = self.tools.iter_mut().find(|tool| tool.name == name) {
                            tool.run_state = if success {
                                if queued {
                                    RunState::Queued
                                } else {
                                    RunState::Updated
                                }
                            } else {
                                RunState::Failed
                            };
                            tool.elapsed = Some(elapsed);
                        }
                    }
                    self.push_activity(format!(
                        "\n=== {} {name}: {} ===",
                        operation.label(self.language),
                        activity_outcome_label(success, queued, self.language)
                    ));
                    self.push_activity_output(&output);
                    if operation != Operation::Update {
                        if let Err(error) = self.refresh_tools() {
                            let line = match self.language {
                                Language::English => format!("refresh failed: {error}"),
                                Language::Chinese => format!("刷新失败：{error}"),
                            };
                            self.push_activity(line);
                        }
                        if self.doctor_probe_id > 0
                            && let Err(error) = self.refresh_doctor()
                        {
                            let line = match self.language {
                                Language::English => {
                                    format!("diagnostics refresh failed: {error}")
                                }
                                Language::Chinese => format!("诊断刷新失败：{error}"),
                            };
                            self.push_activity(line);
                        }
                    }
                    if matches!(operation, Operation::Add | Operation::Edit) && success {
                        if let Some(index) = self.tools.iter().position(|tool| tool.name == name) {
                            self.tool_index = index;
                        }
                    }
                    self.message = operation.completion_message(
                        &name,
                        success,
                        elapsed,
                        self.running,
                        self.language,
                    );
                    if operation == Operation::Update && success && !queued {
                        self.refresh_tool_version(&name);
                    }
                    let _ = self.refresh_jobs();
                    if self.running == 0 {
                        self.finish_update_batch();
                    }
                }
                AppEvent::VersionResolved {
                    name,
                    probe_id,
                    version,
                } => {
                    if let Some(tool) = self
                        .tools
                        .iter_mut()
                        .find(|tool| tool.name == name && tool.version_probe_id == probe_id)
                    {
                        tool.version = version
                            .map(VersionState::Available)
                            .unwrap_or(VersionState::Unavailable);
                    }
                }
                AppEvent::DoctorResolved {
                    probe_id,
                    diagnoses,
                    error,
                } => {
                    if self.doctor_probe_id != probe_id {
                        continue;
                    }
                    self.doctor_loading = false;
                    self.doctor_checked_at = Some(datetime::now());
                    if let Some(error) = error {
                        self.message = match self.language {
                            Language::English => format!("Diagnostics failed: {error}"),
                            Language::Chinese => format!("诊断失败：{error}"),
                        };
                        continue;
                    }
                    self.doctor_diagnoses = diagnoses;
                    self.doctor_index = self
                        .doctor_index
                        .min(self.doctor_diagnoses.len().saturating_sub(1));
                    if self.expanded_doctor.as_ref().is_some_and(|name| {
                        !self
                            .doctor_diagnoses
                            .iter()
                            .any(|diagnosis| &diagnosis.name == name)
                    }) {
                        self.expanded_doctor = None;
                        self.doctor_detail_scroll = 0;
                    }
                    let conflicts = self
                        .doctor_diagnoses
                        .iter()
                        .filter(|diagnosis| diagnosis.has_conflict())
                        .count();
                    self.message = match self.language {
                        Language::English => format!(
                            "Diagnostics complete: {} tool(s), {conflicts} conflict(s)",
                            self.doctor_diagnoses.len()
                        ),
                        Language::Chinese => format!(
                            "诊断完成：{} 个工具，{conflicts} 项冲突",
                            self.doctor_diagnoses.len()
                        ),
                    };
                }
            }
        }
    }

    fn finish_update_batch(&mut self) {
        let Some(batch) = self.update_batch.take() else {
            return;
        };
        let updated = self
            .tools
            .iter()
            .filter(|tool| tool.run_state == RunState::Updated)
            .count();
        let queued = self
            .tools
            .iter()
            .filter(|tool| tool.run_state == RunState::Queued)
            .count();
        let failed = self
            .tools
            .iter()
            .filter(|tool| tool.run_state == RunState::Failed)
            .count();
        let summary = match self.language {
            Language::English => format!(
                "Complete: {updated} updated, {queued} queued, {failed} failed ({} total) in {:.1}s",
                batch.total,
                batch.started.elapsed().as_secs_f64()
            ),
            Language::Chinese => format!(
                "完成：{updated} 项已更新，{queued} 项已排队，{failed} 项失败（共 {} 项），耗时 {:.1} 秒",
                batch.total,
                batch.started.elapsed().as_secs_f64()
            ),
        };
        self.push_activity(format!("\n=== {summary} ==="));
        self.message = if failed == 0 {
            summary
        } else {
            match self.language {
                Language::English => {
                    format!("{summary}. See Activity for command output and errors")
                }
                Language::Chinese => format!("{summary}。请在活动页查看命令输出和错误"),
            }
        };
        if self.doctor_probe_id > 0
            && let Err(error) = self.refresh_doctor()
        {
            self.push_activity(match self.language {
                Language::English => format!("diagnostics refresh failed: {error}"),
                Language::Chinese => format!("诊断刷新失败：{error}"),
            });
        }
    }

    fn push_activity(&mut self, line: String) {
        self.activity.push(line);
        self.activity_timestamps.push(datetime::now());
        if self.activity.len() > MAX_ACTIVITY_LINES {
            let remove = self.activity.len() - MAX_ACTIVITY_LINES;
            self.activity.drain(0..remove);
            let timestamp_remove = remove.min(self.activity_timestamps.len());
            self.activity_timestamps.drain(0..timestamp_remove);
            self.expanded_activity = self
                .expanded_activity
                .drain()
                .filter_map(|index| index.checked_sub(remove))
                .collect();
        }
        self.activity_scroll = self.activity.len().saturating_sub(1);
    }

    fn push_activity_output(&mut self, output: &str) {
        for line in sanitize_terminal_output(output) {
            self.push_activity(line);
        }
    }

    fn selected_for_update(&self) -> Vec<String> {
        let selected: Vec<_> = self
            .tools
            .iter()
            .filter(|tool| {
                tool.selected
                    && tool.availability == Availability::Installed
                    && tool.run_state != RunState::Running
            })
            .map(|tool| tool.name.clone())
            .collect();
        if !selected.is_empty() {
            return selected;
        }
        self.tools
            .get(self.tool_index)
            .filter(|tool| {
                tool.availability == Availability::Installed && tool.run_state != RunState::Running
            })
            .map(|tool| vec![tool.name.clone()])
            .unwrap_or_default()
    }

    fn start_updates(&mut self, tools: Vec<String>) {
        let process_strategy = self.process_strategy;
        for tool in &mut self.tools {
            tool.run_state = RunState::Idle;
            tool.elapsed = None;
        }
        self.update_batch = Some(UpdateBatch {
            started: Instant::now(),
            total: tools.len(),
        });
        self.push_activity(format!(
            "\nprocess policy: {}{}",
            process_strategy.label(self.language),
            match (process_strategy.terminates(), self.language) {
                (true, Language::English) => " matching processes will be stopped",
                (false, Language::English) => " matching processes will be waited on",
                (true, Language::Chinese) => "；将终止匹配的进程",
                (false, Language::Chinese) => "；将等待匹配的进程退出",
            }
        ));
        for name in tools {
            if let Some(tool) = self.tools.iter_mut().find(|tool| tool.name == name) {
                tool.run_state = RunState::Running;
                tool.selected = false;
            }
            self.running += 1;
            self.push_activity(match self.language {
                Language::English => format!("\n>>> starting {name}"),
                Language::Chinese => format!("\n>>> 正在启动 {name}"),
            });
            spawn_dvup(
                self.tx.clone(),
                self.executable.clone(),
                self.state.root().to_path_buf(),
                update_arguments(
                    &name,
                    self.config_path.as_deref(),
                    process_strategy.terminates(),
                ),
                name,
                Operation::Update,
                self.language,
            );
        }
        self.message = match self.language {
            Language::English => format!("Started {} update(s) in parallel", self.running),
            Language::Chinese => format!("已并行启动 {} 项更新", self.running),
        };
        self.tab = Tab::Activity;
    }

    fn open_edit_command(&mut self) {
        if self.running > 0 {
            self.message = self
                .language
                .text(
                    "Wait for the current operation to finish",
                    "请等待当前操作完成",
                )
                .to_owned();
            return;
        }
        if self.config_path.is_some() {
            self.message = self
                .language
                .text(
                    "Custom commands are disabled with --config",
                    "使用 --config 时不能编辑自定义命令",
                )
                .to_owned();
            return;
        }
        let Some(selected) = self.tools.get(self.tool_index).cloned() else {
            return;
        };
        if !selected.custom {
            self.message = self
                .language
                .text("Built-in tools cannot be edited", "不能编辑内置工具")
                .to_owned();
            return;
        }
        let custom = match Config::load(&self.state.custom_config_path()) {
            Ok(custom) => custom,
            Err(error) => {
                self.message = match self.language {
                    Language::English => format!("Could not load custom command: {error}"),
                    Language::Chinese => format!("无法加载自定义命令：{error}"),
                };
                return;
            }
        };
        let Some(tool) = custom.tools.get(&selected.name) else {
            self.message = self
                .language
                .text("Custom command no longer exists", "自定义命令已不存在")
                .to_owned();
            return;
        };
        self.modal = Modal::AddCommand {
            mode: CommandFormMode::Edit,
            original_name: Some(selected.name.clone()),
            field: 1,
            name: TextInput::new(selected.name),
            command: TextInput::new(format_editable_command(&tool.program, &tool.args)),
        };
    }

    fn start_add(
        &mut self,
        mode: CommandFormMode,
        original_name: Option<String>,
        name: String,
        command_line: String,
    ) {
        if self.config_path.is_some() {
            self.message = self
                .language
                .text(
                    "Add is disabled with an explicit --config manifest",
                    "使用显式 --config 配置时不能添加命令",
                )
                .to_owned();
            return;
        }
        let command = match split_command_line(&command_line, self.language) {
            Ok(command) if !command.is_empty() => command,
            Ok(_) => {
                self.message = self
                    .language
                    .text("Command cannot be empty", "命令不能为空")
                    .to_owned();
                return;
            }
            Err(error) => {
                self.message = error;
                return;
            }
        };
        let operation = mode.operation();
        let arguments = match mode {
            CommandFormMode::Add => add_arguments(&name, command, false),
            CommandFormMode::Edit => {
                let Some(original_name) = original_name.as_deref() else {
                    self.message = self
                        .language
                        .text("Original command name is missing", "缺少原命令名称")
                        .to_owned();
                    return;
                };
                edit_arguments(original_name, &name, command)
            }
        };
        self.running += 1;
        self.message = match (mode, self.language) {
            (CommandFormMode::Add, Language::English) => {
                format!("Saving {name}; the command will not be run")
            }
            (CommandFormMode::Add, Language::Chinese) => {
                format!("正在保存 {name}；不会执行该命令")
            }
            (CommandFormMode::Edit, Language::English) => {
                format!("Updating {name}; the command will not be run")
            }
            (CommandFormMode::Edit, Language::Chinese) => {
                format!("正在更新 {name}；不会执行该命令")
            }
        };
        self.push_activity(match (mode, self.language) {
            (CommandFormMode::Add, Language::English) => {
                format!("\n>>> saving {name} (not running): {command_line}")
            }
            (CommandFormMode::Add, Language::Chinese) => {
                format!("\n>>> 正在保存 {name}（不执行）：{command_line}")
            }
            (CommandFormMode::Edit, Language::English) => {
                format!("\n>>> updating {name} (not running): {command_line}")
            }
            (CommandFormMode::Edit, Language::Chinese) => {
                format!("\n>>> 正在更新 {name}（不执行）：{command_line}")
            }
        });
        spawn_dvup(
            self.tx.clone(),
            self.executable.clone(),
            self.state.root().to_path_buf(),
            arguments,
            name,
            operation,
            self.language,
        );
        self.tab = Tab::Tools;
    }

    fn start_delete(&mut self, name: String) {
        self.running += 1;
        self.push_activity(match self.language {
            Language::English => format!("\n>>> removing {name}"),
            Language::Chinese => format!("\n>>> 正在删除 {name}"),
        });
        spawn_dvup(
            self.tx.clone(),
            self.executable.clone(),
            self.state.root().to_path_buf(),
            vec!["remove".to_owned(), name.clone()],
            name,
            Operation::Delete,
            self.language,
        );
        self.tab = Tab::Tools;
    }

    fn toggle_job_log(&mut self) {
        let Some(job) = self.jobs.get(self.job_index).cloned() else {
            self.message = self
                .language
                .text("No job selected", "未选择任务")
                .to_owned();
            return;
        };
        if self.expanded_job.as_deref() == Some(&job.id) {
            self.expanded_job = None;
            self.job_log.clear();
            self.job_log_scroll = 0;
            return;
        }
        match JobStore::new(self.state.clone()).and_then(|store| store.read_log(&job.id)) {
            Ok(log) => {
                self.expanded_job = Some(job.id);
                self.job_log = sanitize_terminal_output(&String::from_utf8_lossy(&log));
                self.job_log_scroll = 0;
            }
            Err(error) => {
                self.message = match self.language {
                    Language::English => format!("Failed to load job log: {error}"),
                    Language::Chinese => format!("无法加载任务日志：{error}"),
                }
            }
        }
    }
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<u8> {
    while !app.should_quit {
        app.frame = app.frame.wrapping_add(1);
        app.process_events();
        if app.last_job_refresh.elapsed() >= Duration::from_secs(1) {
            let _ = app.refresh_jobs();
        }
        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(TICK_RATE)? {
            let event = event::read()?;
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key),
                Event::Mouse(mouse) => handle_mouse(app, mouse),
                _ => {}
            }
        }
    }
    Ok(0)
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if !matches!(app.modal, Modal::None) {
        handle_modal_key(app, key);
        return;
    }
    if is_ctrl_c(&key) {
        request_quit(app);
        return;
    }
    if is_shift_tab(&key) {
        toggle_process_strategy(app);
        return;
    }
    if is_language_toggle(&app.modal, &key) {
        app.language = app.language.toggle();
        for line in &mut app.activity {
            if matches!(line.as_str(), "Welcome to dvup." | "欢迎使用 dvup。") {
                *line = app
                    .language
                    .text("Welcome to dvup.", "欢迎使用 dvup。")
                    .to_owned();
            } else if matches!(
                line.as_str(),
                "Select tools with Space and press Enter to update."
                    | "按 Space 选择工具，然后按 Enter 更新。"
            ) {
                *line = app
                    .language
                    .text(
                        "Select tools with Space and press Enter to update.",
                        "按 Space 选择工具，然后按 Enter 更新。",
                    )
                    .to_owned();
            }
        }
        app.message = app
            .language
            .text("Language switched to English", "语言已切换为中文")
            .to_owned();
        return;
    }

    handle_normal_key(app, key);
}

fn handle_text_input_key(input: &mut TextInput, key: KeyEvent) -> bool {
    let extend_selection = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Left => input.move_left(extend_selection),
        KeyCode::Right => input.move_right(extend_selection),
        KeyCode::Home => input.move_home(extend_selection),
        KeyCode::End => input.move_end(extend_selection),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Char('a' | 'A') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.select_all()
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            input.insert(character)
        }
        _ => return false,
    }
    true
}

fn handle_modal_key(app: &mut App, key: KeyEvent) {
    let key = if is_ctrl_c(&key) {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    } else {
        key
    };
    match app.modal.clone() {
        Modal::ConfirmUpdate { tools } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                app.modal = Modal::None;
                app.start_updates(tools);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.modal = Modal::None,
            _ => {}
        },
        Modal::ConfirmAdd {
            mode,
            original_name,
            name,
            command,
        } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                app.modal = Modal::None;
                app.start_add(mode, original_name, name, command);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.modal = Modal::AddCommand {
                    mode,
                    original_name,
                    field: 1,
                    name: TextInput::new(name),
                    command: TextInput::new(command),
                };
                app.message = app
                    .language
                    .text(
                        "Edit the command or press Esc to cancel",
                        "请修改命令，或按 Esc 取消",
                    )
                    .to_owned();
            }
            _ => {}
        },
        Modal::ConfirmDelete { name } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                app.modal = Modal::None;
                app.start_delete(name);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.modal = Modal::None,
            _ => {}
        },
        Modal::AddCommand {
            mode,
            original_name,
            mut field,
            mut name,
            mut command,
        } => {
            match key.code {
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    return;
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    field = usize::from(field == 0);
                    name.clear_selection();
                    command.clear_selection();
                }
                KeyCode::Up | KeyCode::Down => {
                    let next_field = usize::from(key.code == KeyCode::Down);
                    if field != next_field {
                        field = next_field;
                        name.clear_selection();
                        command.clear_selection();
                    }
                }
                KeyCode::Enter if field == 0 => field = 1,
                KeyCode::Enter => {
                    let trimmed_name = name.value.trim().to_owned();
                    let trimmed_command = command.value.trim().to_owned();
                    if trimmed_name.is_empty() {
                        app.message = app
                            .language
                            .text("Name cannot be empty", "名称不能为空")
                            .to_owned();
                        field = 0;
                    } else if trimmed_command.is_empty() {
                        app.message = app
                            .language
                            .text("Command cannot be empty", "命令不能为空")
                            .to_owned();
                    } else {
                        match split_command_line(&trimmed_command, app.language) {
                            Ok(parts) if !parts.is_empty() => {
                                app.modal = Modal::ConfirmAdd {
                                    mode,
                                    original_name,
                                    name: trimmed_name,
                                    command: trimmed_command,
                                };
                                return;
                            }
                            Ok(_) => {
                                app.message = app
                                    .language
                                    .text("Command cannot be empty", "命令不能为空")
                                    .to_owned()
                            }
                            Err(error) => app.message = error,
                        }
                    }
                }
                _ if field == 0 => {
                    handle_text_input_key(&mut name, key);
                }
                _ => {
                    handle_text_input_key(&mut command, key);
                }
            }
            app.modal = Modal::AddCommand {
                mode,
                original_name,
                field,
                name,
                command,
            };
        }
        Modal::None => {}
    }
}

fn handle_normal_key(app: &mut App, key: KeyEvent) {
    if let Some(tab) = navigated_tab(app.tab, &key.code) {
        app.select_tab(tab);
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => request_quit(app),
        KeyCode::Char('r') | KeyCode::Char('R') => {
            let refresh = app.refresh_tools().and_then(|_| app.refresh_jobs());
            let refresh = if refresh.is_ok() && app.doctor_probe_id > 0 {
                app.refresh_doctor()
            } else {
                refresh
            };
            if let Err(error) = refresh {
                app.message = match app.language {
                    Language::English => format!("Refresh failed: {error}"),
                    Language::Chinese => format!("刷新失败：{error}"),
                };
            } else {
                app.message = app.language.text("Refreshed", "已刷新").to_owned();
            }
        }
        _ => match app.tab {
            Tab::Tools => handle_tools_key(app, key),
            Tab::Activity => handle_activity_key(app, key),
            Tab::Jobs => handle_jobs_key(app, key),
            Tab::Doctor => handle_doctor_key(app, key),
        },
    }
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_shift_tab(key: &KeyEvent) -> bool {
    key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
}

fn is_language_toggle(modal: &Modal, key: &KeyEvent) -> bool {
    !matches!(modal, Modal::AddCommand { .. })
        && matches!(key.code, KeyCode::Char('l') | KeyCode::Char('L'))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn toggle_process_strategy(app: &mut App) {
    if app.running > 0 {
        app.message = app
            .language
            .text(
                "Wait for running operations before changing process policy",
                "请等待运行中的操作结束后再切换进程策略",
            )
            .to_owned();
        return;
    }
    app.process_strategy = app.process_strategy.toggle();
    app.message = match app.process_strategy {
        ProcessStrategy::Wait => app
            .language
            .text(
                "Process policy: WAIT for matching processes (Shift+Tab to change)",
                "进程策略：等待匹配进程退出（按 Shift+Tab 切换）",
            )
            .to_owned(),
        ProcessStrategy::Terminate => match app.terminate_active_job_waits() {
            Ok((jobs, rules, stopped, restarted, skipped)) => {
                app.push_activity(match app.language {
                    Language::English => format!(
                        "process policy: TERMINATE; checked {jobs} active job(s), changed {rules} wait rule(s), stopped {stopped} matching process(es), restarted {restarted} orphaned job(s)"
                    ),
                    Language::Chinese => format!(
                        "进程策略：终止；已检查 {jobs} 个活动任务，修改 {rules} 条等待规则，停止 {stopped} 个匹配进程，并重启 {restarted} 个孤儿任务"
                    ),
                });
                if skipped == 0 {
                    match app.language {
                        Language::English => format!(
                            "Process policy: TERMINATE; checked {jobs} active job(s), stopped {stopped} process(es), restarted {restarted} orphaned job(s)"
                        ),
                        Language::Chinese => {
                            format!(
                                "进程策略：终止；已检查 {jobs} 个活动任务，停止 {stopped} 个进程，重启 {restarted} 个孤儿任务"
                            )
                        }
                    }
                } else {
                    match app.language {
                        Language::English => format!(
                            "Process policy: TERMINATE; checked {jobs} job(s), restarted {restarted}, skipped {skipped} unsafe or failed job(s)"
                        ),
                        Language::Chinese => format!(
                            "进程策略：终止；已检查 {jobs} 个任务，重启 {restarted} 个，跳过 {skipped} 个不安全或处理失败的任务"
                        ),
                    }
                }
            }
            Err(error) => match app.language {
                Language::English => format!("Could not update active job policies: {error}"),
                Language::Chinese => format!("无法更新活动任务的进程策略：{error}"),
            },
        },
    };
}

fn request_quit(app: &mut App) {
    if app.running == 0 {
        app.should_quit = true;
    } else {
        app.message = match app.language {
            Language::English => format!(
                "Wait for {} running operation(s) before quitting with q or Ctrl+C",
                app.running
            ),
            Language::Chinese => {
                format!(
                    "请等待 {} 项运行中的操作结束后再按 q 或 Ctrl+C 退出",
                    app.running
                )
            }
        };
    }
}

fn navigated_tab(tab: Tab, code: &KeyCode) -> Option<Tab> {
    match code {
        KeyCode::Right => Some(tab.next()),
        KeyCode::Left => Some(tab.previous()),
        _ => None,
    }
}

fn handle_tools_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.tool_index = previous_index(app.tool_index, app.tools.len());
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.tool_index = next_index(app.tool_index, app.tools.len());
        }
        KeyCode::Char(' ') => {
            toggle_tool_selection(app, app.tool_index);
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            let should_select = app
                .tools
                .iter()
                .any(|tool| tool.availability == Availability::Installed && !tool.selected);
            for tool in &mut app.tools {
                if tool.availability == Availability::Installed {
                    tool.selected = should_select;
                }
            }
        }
        KeyCode::Enter => {
            if app.running > 0 {
                app.message = app
                    .language
                    .text(
                        "Wait for the current operation to finish",
                        "请等待当前操作完成",
                    )
                    .to_owned();
                return;
            }
            let tools = app.selected_for_update();
            if tools.is_empty() {
                app.message = app
                    .language
                    .text("Select an installed tool first", "请先选择一个已安装的工具")
                    .to_owned();
            } else {
                app.modal = Modal::ConfirmUpdate { tools };
            }
        }
        KeyCode::Char('c') | KeyCode::Char('C') => {
            if app.running > 0 {
                app.message = app
                    .language
                    .text(
                        "Wait for the current operation to finish",
                        "请等待当前操作完成",
                    )
                    .to_owned();
            } else if app.config_path.is_some() {
                app.message = app
                    .language
                    .text(
                        "Custom commands are disabled with --config",
                        "使用 --config 时不能添加自定义命令",
                    )
                    .to_owned();
            } else {
                app.modal = Modal::AddCommand {
                    mode: CommandFormMode::Add,
                    original_name: None,
                    field: 0,
                    name: TextInput::new(String::new()),
                    command: TextInput::new(String::new()),
                };
            }
        }
        KeyCode::Char('e') | KeyCode::Char('E') => app.open_edit_command(),
        KeyCode::Char('d') | KeyCode::Char('D') => {
            if app.running > 0 {
                app.message = app
                    .language
                    .text(
                        "Wait for the current operation to finish",
                        "请等待当前操作完成",
                    )
                    .to_owned();
                return;
            }
            if let Some(tool) = app.tools.get(app.tool_index) {
                if tool.custom {
                    app.modal = Modal::ConfirmDelete {
                        name: tool.name.clone(),
                    };
                } else {
                    app.message = app
                        .language
                        .text("Built-in tools cannot be deleted", "不能删除内置工具")
                        .to_owned();
                }
            }
        }
        _ => {}
    }
}

fn toggle_tool_selection(app: &mut App, index: usize) {
    let Some(tool) = app.tools.get_mut(index) else {
        return;
    };
    if tool.availability == Availability::Installed {
        tool.selected = !tool.selected;
    } else {
        app.message = match app.language {
            Language::English => format!("{} is not available", tool.name),
            Language::Chinese => format!("{} 当前不可用", tool.name),
        };
    }
}

fn handle_activity_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.activity_scroll = app.activity_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.activity_scroll =
                (app.activity_scroll + 1).min(app.activity_rendered_height.saturating_sub(1));
        }
        KeyCode::Home => app.activity_scroll = 0,
        KeyCode::End => app.activity_scroll = app.activity_rendered_height.saturating_sub(1),
        _ => {}
    }
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    if !matches!(app.modal, Modal::None) {
        handle_modal_mouse(app, mouse);
        return;
    }
    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
        && let Some(index) = hitbox_target(&app.tab_hitboxes, mouse.column, mouse.row)
    {
        if let Some(tab) = Tab::from_index(index) {
            app.select_tab(tab);
        }
        return;
    }

    match mouse.kind {
        MouseEventKind::Moved => {
            app.hovered_tab = hitbox_target(&app.tab_hitboxes, mouse.column, mouse.row);
            app.hovered_activity = None;
            match app.tab {
                Tab::Tools => {
                    if let Some(index) = hitbox_target(&app.tool_hitboxes, mouse.column, mouse.row)
                    {
                        app.tool_index = index;
                    }
                }
                Tab::Jobs => {
                    if let Some(index) = hitbox_target(&app.job_hitboxes, mouse.column, mouse.row) {
                        app.job_index = index;
                    }
                }
                Tab::Activity => {
                    app.hovered_activity =
                        hitbox_target(&app.activity_hitboxes, mouse.column, mouse.row);
                }
                Tab::Doctor => {
                    if let Some(index) =
                        hitbox_target(&app.doctor_hitboxes, mouse.column, mouse.row)
                    {
                        app.doctor_index = index;
                    }
                }
            }
        }
        MouseEventKind::Down(MouseButton::Left) => match app.tab {
            Tab::Activity => {
                if let Some(index) = hitbox_target(&app.activity_hitboxes, mouse.column, mouse.row)
                {
                    if !app.expanded_activity.remove(&index) {
                        app.expanded_activity.insert(index);
                    }
                }
            }
            Tab::Jobs => {
                if let Some(index) = hitbox_target(&app.job_hitboxes, mouse.column, mouse.row) {
                    app.job_index = index;
                    app.toggle_job_log();
                }
            }
            Tab::Tools => {
                if let Some(index) = hitbox_target(&app.tool_hitboxes, mouse.column, mouse.row) {
                    app.tool_index = index;
                    toggle_tool_selection(app, index);
                }
            }
            Tab::Doctor => {
                if let Some(index) = hitbox_target(&app.doctor_hitboxes, mouse.column, mouse.row) {
                    app.doctor_index = index;
                    app.toggle_doctor_detail();
                }
            }
        },
        MouseEventKind::ScrollUp => match app.tab {
            Tab::Activity => app.activity_scroll = app.activity_scroll.saturating_sub(3),
            Tab::Jobs if contains(app.job_detail_area, mouse.column, mouse.row) => {
                app.job_log_scroll = app.job_log_scroll.saturating_sub(3);
            }
            Tab::Doctor if contains(app.doctor_detail_area, mouse.column, mouse.row) => {
                app.doctor_detail_scroll = app.doctor_detail_scroll.saturating_sub(3);
            }
            _ => {}
        },
        MouseEventKind::ScrollDown => match app.tab {
            Tab::Activity => app.activity_scroll = app.activity_scroll.saturating_add(3),
            Tab::Jobs if contains(app.job_detail_area, mouse.column, mouse.row) => {
                app.job_log_scroll = app.job_log_scroll.saturating_add(3);
            }
            Tab::Doctor if contains(app.doctor_detail_area, mouse.column, mouse.row) => {
                app.doctor_detail_scroll = app.doctor_detail_scroll.saturating_add(3);
            }
            _ => {}
        },
        _ => {}
    }
}

fn handle_modal_mouse(app: &mut App, mouse: MouseEvent) {
    let hitbox = app
        .modal_input_hitboxes
        .iter()
        .copied()
        .find(|hitbox| contains(Some(hitbox.area), mouse.column, mouse.row));
    match mouse.kind {
        MouseEventKind::Moved => {
            if let Some(hitbox) = hitbox
                && modal_field_is_editable(&app.modal, hitbox.field)
                && let Modal::AddCommand { field, .. } = &mut app.modal
            {
                *field = hitbox.field;
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(hitbox) = hitbox else {
                app.modal_drag = None;
                return;
            };
            if !modal_field_is_editable(&app.modal, hitbox.field) {
                return;
            }
            let Some(target) = modal_cursor_at(app, hitbox, mouse.column) else {
                return;
            };
            if let Modal::AddCommand {
                field,
                name,
                command,
                ..
            } = &mut app.modal
            {
                *field = hitbox.field;
                let input = if hitbox.field == 0 { name } else { command };
                input.cursor = target;
                input.clear_selection();
                app.modal_drag = Some((hitbox.field, target));
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some((field, anchor)) = app.modal_drag else {
                return;
            };
            let Some(hitbox) = app
                .modal_input_hitboxes
                .iter()
                .copied()
                .find(|hitbox| hitbox.field == field)
            else {
                return;
            };
            let column = mouse
                .column
                .clamp(hitbox.area.x, hitbox.area.right().saturating_sub(1));
            let Some(target) = modal_cursor_at(app, hitbox, column) else {
                return;
            };
            if let Modal::AddCommand { name, command, .. } = &mut app.modal {
                let input = if field == 0 { name } else { command };
                input.cursor = target;
                input.selection_anchor = (target != anchor).then_some(anchor);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => app.modal_drag = None,
        _ => {}
    }
}

fn modal_field_is_editable(modal: &Modal, _field: usize) -> bool {
    matches!(modal, Modal::AddCommand { .. })
}

fn modal_cursor_at(app: &App, hitbox: ModalInputHitbox, column: u16) -> Option<usize> {
    let Modal::AddCommand { name, command, .. } = &app.modal else {
        return None;
    };
    let input = if hitbox.field == 0 { name } else { command };
    let relative_column = usize::from(column.saturating_sub(hitbox.area.x));
    let mut width = 0;
    for (offset, character) in input.value[hitbox.visible_start..hitbox.visible_end].char_indices()
    {
        let character_width = display_width(&character.to_string());
        if relative_column < width + character_width.div_ceil(2) {
            return Some(hitbox.visible_start + offset);
        }
        width += character_width;
        if relative_column < width {
            return Some(hitbox.visible_start + offset + character.len_utf8());
        }
    }
    Some(hitbox.visible_end)
}

fn contains(area: Option<Rect>, column: u16, row: u16) -> bool {
    area.is_some_and(|area| {
        column >= area.x
            && column < area.x.saturating_add(area.width)
            && row >= area.y
            && row < area.y.saturating_add(area.height)
    })
}

fn hitbox_target(hitboxes: &[(Rect, usize)], column: u16, row: u16) -> Option<usize> {
    hitboxes
        .iter()
        .find_map(|(area, target)| contains(Some(*area), column, row).then_some(*target))
}

fn handle_jobs_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.job_index = previous_index(app.job_index, app.jobs.len());
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.job_index = next_index(app.job_index, app.jobs.len());
        }
        KeyCode::Enter => app.toggle_job_log(),
        KeyCode::PageUp if app.expanded_job.is_some() => {
            app.job_log_scroll = app.job_log_scroll.saturating_sub(5);
        }
        KeyCode::PageDown if app.expanded_job.is_some() => {
            app.job_log_scroll = app.job_log_scroll.saturating_add(5);
        }
        _ => {}
    }
}

fn handle_doctor_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.doctor_index = previous_index(app.doctor_index, app.doctor_diagnoses.len());
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.doctor_index = next_index(app.doctor_index, app.doctor_diagnoses.len());
        }
        KeyCode::Enter => app.toggle_doctor_detail(),
        KeyCode::PageUp if app.expanded_doctor.is_some() => {
            app.doctor_detail_scroll = app.doctor_detail_scroll.saturating_sub(5);
        }
        KeyCode::PageDown if app.expanded_doctor.is_some() => {
            app.doctor_detail_scroll = app.doctor_detail_scroll.saturating_add(5);
        }
        _ => {}
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(4),
        ])
        .split(area);

    let titles = match app.language {
        Language::English => ["Tools", "Activity", "Jobs", "Doctor"],
        Language::Chinese => ["工具", "活动", "任务", "诊断"],
    }
    .into_iter()
    .map(Line::from)
    .collect::<Vec<_>>();
    app.tab_hitboxes = tab_hitboxes(chunks[0], &titles);
    let title = Line::from(vec![
        Span::raw(match app.language {
            Language::English => {
                format!(" dvup — {} running — policy: ", app.running)
            }
            Language::Chinese => {
                format!(" dvup — {} 项运行中 — 策略：", app.running)
            }
        }),
        Span::styled(
            app.process_strategy.label(app.language),
            app.process_strategy.style(),
        ),
        Span::raw(match app.language {
            Language::English => " — language: EN ",
            Language::Chinese => " — 语言：中文 ",
        }),
        Span::styled(
            format!("— {} ", datetime::now()),
            Style::default().fg(SUBTLE),
        ),
    ]);
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(BORDER))
                .style(Style::default().bg(SURFACE))
                .title(title),
        )
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, chunks[0]);
    if let Some(index) = app.hovered_tab
        && let Some((area, _)) = app.tab_hitboxes.iter().find(|(_, target)| *target == index)
    {
        frame.buffer_mut().set_style(
            *area,
            Style::default()
                .fg(ACCENT)
                .bg(SELECTION_BG)
                .add_modifier(Modifier::BOLD),
        );
    }

    match app.tab {
        Tab::Tools => draw_tools(frame, app, chunks[1]),
        Tab::Activity => draw_activity(frame, app, chunks[1]),
        Tab::Jobs => draw_jobs(frame, app, chunks[1]),
        Tab::Doctor => draw_doctor(frame, app, chunks[1]),
    }

    let help = match (app.tab, app.language) {
        (Tab::Tools, Language::English) => [
            "↑↓/hover move · click/Space select · a all · Enter update",
            "c add · e edit · d del · r refresh · L 中/EN · ←/→ or click tab · Shift+Tab policy · q quit",
        ],
        (Tab::Tools, Language::Chinese) => [
            "↑↓/悬停 移动 · 点击/Space 选择 · a 全选 · Enter 更新",
            "c 添加 · e 编辑 · d 删除 · r 刷新 · L 中/EN · ←/→ 或点击页签 · Shift+Tab 策略 · q 退出",
        ],
        (Tab::Activity, Language::English) => [
            "↑↓ scroll · click execution to expand · Home/End · r refresh",
            "←/→ or click tab · L 中/EN · Shift+Tab policy · q quit",
        ],
        (Tab::Activity, Language::Chinese) => [
            "↑↓ 滚动 · 点击执行展开 · Home/End · r 刷新",
            "←/→ 或点击页签 · L 中/EN · Shift+Tab 策略 · q 退出",
        ],
        (Tab::Jobs, Language::English) => [
            "↑↓/hover move · click/Enter expand result · PgUp/PgDn scroll · r refresh",
            "←/→ or click tab · L 中/EN · Shift+Tab policy · q quit",
        ],
        (Tab::Jobs, Language::Chinese) => [
            "↑↓/悬停 移动 · 点击/Enter 展开结果 · PgUp/PgDn 滚动 · r 刷新",
            "←/→ 或点击页签 · L 中/EN · Shift+Tab 策略 · q 退出",
        ],
        (Tab::Doctor, Language::English) => [
            "↑↓/hover move · click/Enter expand diagnosis · PgUp/PgDn scroll · r rescan",
            "active wins PATH · shadowed is hidden · ←/→ or click tab · L 中/EN · q quit",
        ],
        (Tab::Doctor, Language::Chinese) => [
            "↑↓/悬停 移动 · 点击/Enter 展开诊断 · PgUp/PgDn 滚动 · r 重新扫描",
            "active 为当前生效项 · shadowed 为被遮蔽项 · ←/→ 或点击页签 · q 退出",
        ],
    };
    let footer = Paragraph::new(vec![
        Line::from(Span::styled(
            format!(" {}", app.message),
            Style::default()
                .fg(WARNING_COLOR)
                .add_modifier(Modifier::BOLD),
        )),
        Line::styled(help[0], Style::default().fg(DIM)),
        Line::styled(help[1], Style::default().fg(SUBTLE)),
    ])
    .style(Style::default().bg(SURFACE))
    .block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER)),
    );
    frame.render_widget(footer, chunks[2]);

    draw_modal(frame, app, area);
}

fn tab_hitboxes(area: Rect, titles: &[Line<'_>]) -> Vec<(Rect, usize)> {
    if area.width <= 2 || area.height <= 2 {
        return Vec::new();
    }
    let mut hitboxes = Vec::with_capacity(titles.len());
    let mut x = area.x.saturating_add(1);
    let right = area.right().saturating_sub(1);
    for (index, title) in titles.iter().enumerate() {
        let tab_width = u16::try_from(title.width().saturating_add(2)).unwrap_or(u16::MAX);
        let width = tab_width.min(right.saturating_sub(x));
        if width == 0 {
            break;
        }
        hitboxes.push((Rect::new(x, area.y.saturating_add(1), width, 1), index));
        x = x.saturating_add(tab_width);
        if index + 1 < titles.len() {
            x = x.saturating_add(1);
        }
    }
    hitboxes
}

fn draw_tools(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.tools.iter().map(|tool| {
        let checked = if tool.selected { "[x]" } else { "[ ]" };
        let kind = match (tool.custom, app.language) {
            (true, Language::English) => "custom",
            (true, Language::Chinese) => "自定义",
            (false, Language::English) => "built-in",
            (false, Language::Chinese) => "内置",
        };
        Row::new(vec![
            Cell::from(checked),
            Cell::from(tool.name.clone()),
            Cell::from(tool.availability.label(app.language)).style(tool.availability.style()),
            Cell::from(tool.version.label().to_owned()).style(tool.version.style()),
            Cell::from(format_run_result(tool, app.frame, app.language))
                .style(tool.run_state.style()),
            Cell::from(kind),
            Cell::from(tool.command.clone()),
        ])
    });
    let header = Row::new(match app.language {
        Language::English => [
            "",
            "TOOL",
            "AVAILABLE",
            "VERSION",
            "RESULT",
            "TYPE",
            "COMMAND",
        ],
        Language::Chinese => ["", "工具", "可用性", "版本", "结果", "类型", "命令"],
    })
    .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD))
    .bottom_margin(1);
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(18),
            Constraint::Length(14),
            Constraint::Length(8),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .title(Span::styled(
                app.language.text(" Tools ", " 工具 "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
    )
    .row_highlight_style(
        Style::default()
            .bg(SELECTION_BG)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ");
    let mut state = TableState::default().with_selected(Some(app.tool_index));
    frame.render_stateful_widget(table, area, &mut state);

    let first_row = area.y.saturating_add(3);
    let visible_rows = area.height.saturating_sub(4) as usize;
    let offset = state.offset();
    app.tool_hitboxes = (offset..app.tools.len().min(offset.saturating_add(visible_rows)))
        .enumerate()
        .map(|(visible_index, tool_index)| {
            (
                Rect {
                    x: area.x.saturating_add(1),
                    y: first_row.saturating_add(visible_index as u16),
                    width: area.width.saturating_sub(2),
                    height: 1,
                },
                tool_index,
            )
        })
        .collect();
    render_scrollbar(frame, area, app.tools.len(), visible_rows, offset);
}

fn draw_activity(frame: &mut Frame, app: &mut App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    let rendered = activity_render_lines(app);
    let content_width = area.width.saturating_sub(2).max(1);
    let mut visual_row = 0;
    let positioned = rendered
        .iter()
        .map(|(line, target)| {
            let line_height = Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(content_width)
                .max(1);
            let start = visual_row;
            visual_row += line_height;
            (start, line_height, *target)
        })
        .collect::<Vec<_>>();
    app.activity_rendered_height = visual_row;
    let max_scroll = visual_row.saturating_sub(height);
    app.activity_scroll = app.activity_scroll.min(max_scroll);
    let scroll = app.activity_scroll;
    app.activity_hitboxes = positioned
        .iter()
        .filter_map(|(start, line_height, target)| {
            let target = (*target)?;
            let visible_start = (*start).max(scroll);
            let visible_end = start
                .saturating_add(*line_height)
                .min(scroll.saturating_add(height));
            (visible_start < visible_end).then(|| {
                (
                    Rect {
                        x: area.x.saturating_add(1),
                        y: area
                            .y
                            .saturating_add(1)
                            .saturating_add((visible_start - scroll) as u16),
                        width: area.width.saturating_sub(2),
                        height: (visible_end - visible_start) as u16,
                    },
                    target,
                )
            })
        })
        .collect();
    let lines = rendered
        .into_iter()
        .map(|(line, _)| line)
        .collect::<Vec<_>>();
    let title = Line::from(vec![
        Span::raw(app.language.text(" Activity  ", " 活动  ")),
        Span::styled(
            app.language.text("● success", "● 成功"),
            Style::default().fg(SUCCESS),
        ),
        Span::raw("  "),
        Span::styled(
            app.language.text("● queued", "● 已排队"),
            Style::default().fg(WARNING_COLOR),
        ),
        Span::raw("  "),
        Span::styled(
            app.language.text("● failed ", "● 失败 "),
            Style::default().fg(ERROR_COLOR),
        ),
    ]);
    let activity = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(BORDER))
                .title(title),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    frame.render_widget(activity, area);
    render_scrollbar(frame, area, visual_row, height, scroll);
}

fn activity_render_lines(app: &App) -> Vec<(Line<'static>, Option<usize>)> {
    let mut lines = Vec::new();
    let mut index = 0;
    while index < app.activity.len() {
        let text = &app.activity[index];
        if is_activity_execution_header(text) {
            if text.starts_with('\n') {
                lines.push((Line::default(), None));
            }
            let expanded = app.expanded_activity.contains(&index);
            let marker = if expanded { "▾ " } else { "▸ " };
            let header = text.trim_start_matches(['\r', '\n']);
            let header_line = Line::from(vec![
                activity_timestamp_span(app, index),
                Span::styled(marker, Style::default().fg(ACCENT)),
                Span::styled(header.to_owned(), activity_style(header)),
            ])
            .style(if app.hovered_activity == Some(index) {
                Style::default().bg(SELECTION_BG)
            } else {
                Style::default()
            });
            lines.push((header_line, Some(index)));

            let mut next = index + 1;
            while next < app.activity.len() && !app.activity[next].starts_with('\n') {
                if expanded {
                    for output_line in app.activity[next].split('\n') {
                        lines.push((
                            Line::from(vec![
                                activity_timestamp_span(app, next),
                                Span::raw("  "),
                                Span::styled(output_line.to_owned(), activity_style(output_line)),
                            ]),
                            None,
                        ));
                    }
                }
                next += 1;
            }
            index = next;
            continue;
        }

        for (line_index, plain) in text.split('\n').enumerate() {
            if line_index > 0 || !plain.is_empty() {
                lines.push((
                    Line::from(vec![
                        activity_timestamp_span(app, index),
                        Span::styled(plain.to_owned(), activity_style(plain)),
                    ]),
                    None,
                ));
            } else {
                lines.push((Line::default(), None));
            }
        }
        index += 1;
    }
    lines
}

fn activity_timestamp_span(app: &App, index: usize) -> Span<'static> {
    app.activity_timestamps
        .get(index)
        .map(|timestamp| Span::styled(format!("[{timestamp}] "), Style::default().fg(SUBTLE)))
        .unwrap_or_default()
}

fn is_activity_execution_header(text: &str) -> bool {
    let line = text.trim();
    let lower = line.to_ascii_lowercase();
    lower.starts_with("=== update ")
        || lower.starts_with("=== save ")
        || lower.starts_with("=== remove ")
        || line.starts_with("=== 更新 ")
        || line.starts_with("=== 保存 ")
        || line.starts_with("=== 删除 ")
}

fn draw_jobs(frame: &mut Frame, app: &mut App, area: Rect) {
    let (table_area, detail_area) = if app.expanded_job.is_some() && area.height >= 8 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };
    let rows = app.jobs.iter().map(|job| {
        let marker = if app.expanded_job.as_deref() == Some(&job.id) {
            "▾"
        } else {
            "▸"
        };
        Row::new(vec![
            Cell::from(format!("{marker} {}", job.name)),
            Cell::from(app.language.job_status(&job.status)),
            Cell::from(datetime::format_unix_ms(job.updated_at_unix_ms)),
            Cell::from(job.id.clone()),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Length(19),
            Constraint::Min(24),
        ],
    )
    .header(
        Row::new(match app.language {
            Language::English => ["TOOL", "STATUS", "UPDATED", "JOB"],
            Language::Chinese => ["工具", "状态", "更新时间", "任务"],
        })
        .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD))
        .bottom_margin(1),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .title(Span::styled(
                app.language.text(" Background jobs ", " 后台任务 "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
    )
    .row_highlight_style(
        Style::default()
            .bg(SELECTION_BG)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ");
    let mut state = TableState::default().with_selected(Some(app.job_index));
    frame.render_stateful_widget(table, table_area, &mut state);

    let first_row = table_area.y.saturating_add(3);
    let visible_rows = table_area.height.saturating_sub(4) as usize;
    let offset = state.offset();
    app.job_hitboxes = (offset..app.jobs.len().min(offset.saturating_add(visible_rows)))
        .enumerate()
        .map(|(visible_index, job_index)| {
            (
                Rect {
                    x: table_area.x.saturating_add(1),
                    y: first_row.saturating_add(visible_index as u16),
                    width: table_area.width.saturating_sub(2),
                    height: 1,
                },
                job_index,
            )
        })
        .collect();
    render_scrollbar(frame, table_area, app.jobs.len(), visible_rows, offset);

    app.job_detail_area = detail_area;
    let Some(detail_area) = detail_area else {
        return;
    };
    let detail_lines = if app.job_log.is_empty() {
        vec![Line::styled(
            app.language
                .text("No output has been recorded yet.", "尚未记录任何输出。"),
            Style::default().fg(SUBTLE),
        )]
    } else {
        app.job_log
            .iter()
            .map(|text| Line::styled(text.clone(), activity_style(text)))
            .collect()
    };
    let detail_height = detail_area.height.saturating_sub(2) as usize;
    let detail_width = detail_area.width.saturating_sub(2).max(1);
    let detail_rendered_height = detail_lines
        .iter()
        .map(|line| {
            Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(detail_width)
                .max(1)
        })
        .sum::<usize>();
    let max_scroll = detail_rendered_height.saturating_sub(detail_height);
    app.job_log_scroll = app.job_log_scroll.min(max_scroll);
    let job = app
        .expanded_job
        .as_deref()
        .and_then(|id| app.jobs.iter().find(|job| job.id == id));
    let detail_title = match (job, app.language) {
        (Some(job), Language::English) => format!(" Result — {} / {} ", job.name, job.id),
        (Some(job), Language::Chinese) => format!(" 结果 — {} / {} ", job.name, job.id),
        (None, Language::English) => " Result ".to_owned(),
        (None, Language::Chinese) => " 结果 ".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(detail_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(BORDER))
                    .title(Span::styled(
                        detail_title,
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.job_log_scroll as u16, 0)),
        detail_area,
    );
    render_scrollbar(
        frame,
        detail_area,
        detail_rendered_height,
        detail_height,
        app.job_log_scroll,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DoctorStatus {
    Ok,
    Conflict,
    Missing,
    Unsupported,
}

impl DoctorStatus {
    fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Ok, Language::English) => "OK",
            (Self::Ok, Language::Chinese) => "正常",
            (Self::Conflict, Language::English) => "WARN",
            (Self::Conflict, Language::Chinese) => "冲突",
            (Self::Missing, Language::English) => "NOT FOUND",
            (Self::Missing, Language::Chinese) => "未找到",
            (Self::Unsupported, Language::English) => "UNSUPPORTED",
            (Self::Unsupported, Language::Chinese) => "不支持",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Ok => Style::default().fg(SUCCESS),
            Self::Conflict => Style::default()
                .fg(WARNING_COLOR)
                .add_modifier(Modifier::BOLD),
            Self::Missing => Style::default().fg(SUBTLE),
            Self::Unsupported => Style::default().fg(WARNING_COLOR),
        }
    }
}

fn doctor_status(diagnosis: &doctor::ToolDiagnosis) -> DoctorStatus {
    if !diagnosis.supported {
        DoctorStatus::Unsupported
    } else if diagnosis.has_conflict() {
        DoctorStatus::Conflict
    } else if diagnosis.target.candidates.is_empty() {
        DoctorStatus::Missing
    } else {
        DoctorStatus::Ok
    }
}

fn draw_doctor(frame: &mut Frame, app: &mut App, area: Rect) {
    let (table_area, detail_area) = if app.expanded_doctor.is_some() && area.height >= 10 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };
    let rows = app.doctor_diagnoses.iter().map(|diagnosis| {
        let status = doctor_status(diagnosis);
        let marker = if app.expanded_doctor.as_deref() == Some(&diagnosis.name) {
            "▾"
        } else {
            "▸"
        };
        let active = diagnosis
            .target
            .candidates
            .first()
            .map(|candidate| candidate.path.display().to_string())
            .unwrap_or_else(|| "—".to_owned());
        let version = diagnosis
            .target
            .candidates
            .first()
            .and_then(|candidate| candidate.version.clone())
            .unwrap_or_else(|| "—".to_owned());
        let updater = diagnosis
            .updater
            .as_ref()
            .map(|updater| updater.program.clone())
            .unwrap_or_else(|| "—".to_owned());
        Row::new(vec![
            Cell::from(format!("{marker} {}", diagnosis.name)),
            Cell::from(status.label(app.language)).style(status.style()),
            Cell::from(active),
            Cell::from(version),
            Cell::from(diagnosis.target.candidates.len().to_string()),
            Cell::from(updater),
        ])
    });
    let conflicts = app
        .doctor_diagnoses
        .iter()
        .filter(|diagnosis| diagnosis.has_conflict())
        .count();
    let title = Line::from(vec![
        Span::styled(
            app.language.text(" Doctor  ", " 安装诊断  "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if app.doctor_loading {
                app.language.text("● scanning", "● 扫描中")
            } else {
                app.language.text("● ready", "● 已完成")
            },
            if app.doctor_loading {
                Style::default().fg(ACCENT)
            } else {
                Style::default().fg(SUCCESS)
            },
        ),
        Span::styled(
            match app.language {
                Language::English => {
                    format!("  {} tools · {conflicts} warn", app.doctor_diagnoses.len())
                }
                Language::Chinese => {
                    format!("  {} 工具 · {conflicts} 冲突", app.doctor_diagnoses.len())
                }
            },
            Style::default().fg(if conflicts == 0 {
                SUBTLE
            } else {
                WARNING_COLOR
            }),
        ),
        Span::styled(
            app.doctor_checked_at
                .as_ref()
                .map(|checked| format!("  {checked} "))
                .unwrap_or_default(),
            Style::default().fg(SUBTLE),
        ),
    ]);
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(11),
            Constraint::Min(22),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(match app.language {
            Language::English => ["TOOL", "STATUS", "ACTIVE", "VERSION", "COPIES", "UPDATER"],
            Language::Chinese => ["工具", "状态", "当前生效", "版本", "安装数", "更新器"],
        })
        .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD))
        .bottom_margin(1),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .title(title),
    )
    .row_highlight_style(
        Style::default()
            .bg(SELECTION_BG)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ");
    let mut state = TableState::default().with_selected(Some(app.doctor_index));
    frame.render_stateful_widget(table, table_area, &mut state);

    let first_row = table_area.y.saturating_add(3);
    let visible_rows = table_area.height.saturating_sub(4) as usize;
    let offset = state.offset();
    app.doctor_hitboxes = (offset
        ..app
            .doctor_diagnoses
            .len()
            .min(offset.saturating_add(visible_rows)))
        .enumerate()
        .map(|(visible_index, diagnosis_index)| {
            (
                Rect {
                    x: table_area.x.saturating_add(1),
                    y: first_row.saturating_add(visible_index as u16),
                    width: table_area.width.saturating_sub(2),
                    height: 1,
                },
                diagnosis_index,
            )
        })
        .collect();
    render_scrollbar(
        frame,
        table_area,
        app.doctor_diagnoses.len(),
        visible_rows,
        offset,
    );

    app.doctor_detail_area = detail_area;
    let Some(detail_area) = detail_area else {
        return;
    };
    let diagnosis = app
        .expanded_doctor
        .as_deref()
        .and_then(|name| app.doctor_diagnoses.iter().find(|item| item.name == name));
    let detail_lines = diagnosis
        .map(|diagnosis| doctor_detail_lines(diagnosis, app.language))
        .unwrap_or_else(|| {
            vec![Line::styled(
                app.language
                    .text("No diagnosis selected.", "未选择诊断项。"),
                Style::default().fg(SUBTLE),
            )]
        });
    let detail_height = detail_area.height.saturating_sub(2) as usize;
    let detail_width = detail_area.width.saturating_sub(2).max(1);
    let rendered_height = detail_lines
        .iter()
        .map(|line| {
            Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(detail_width)
                .max(1)
        })
        .sum::<usize>();
    let max_scroll = rendered_height.saturating_sub(detail_height);
    app.doctor_detail_scroll = app.doctor_detail_scroll.min(max_scroll);
    let detail_title = match (diagnosis, app.language) {
        (Some(diagnosis), Language::English) => format!(" Diagnosis — {} ", diagnosis.name),
        (Some(diagnosis), Language::Chinese) => format!(" 诊断详情 — {} ", diagnosis.name),
        (None, Language::English) => " Diagnosis ".to_owned(),
        (None, Language::Chinese) => " 诊断详情 ".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(detail_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(BORDER))
                    .title(Span::styled(
                        detail_title,
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.doctor_detail_scroll as u16, 0)),
        detail_area,
    );
    render_scrollbar(
        frame,
        detail_area,
        rendered_height,
        detail_height,
        app.doctor_detail_scroll,
    );
}

fn doctor_detail_lines(
    diagnosis: &doctor::ToolDiagnosis,
    language: Language,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let status = doctor_status(diagnosis);
    lines.push(Line::from(vec![
        Span::styled(
            language.text("status: ", "状态："),
            Style::default().fg(SUBTLE),
        ),
        Span::styled(status.label(language), status.style()),
    ]));
    if !diagnosis.supported {
        lines.push(Line::styled(
            match language {
                Language::English => format!("unsupported on {}", std::env::consts::OS),
                Language::Chinese => format!("当前平台 {} 不支持此工具", std::env::consts::OS),
            },
            Style::default().fg(WARNING_COLOR),
        ));
        return lines;
    }
    append_doctor_executable_lines(&mut lines, &diagnosis.target, false, language);
    if let Some(updater) = &diagnosis.updater {
        lines.push(Line::default());
        append_doctor_executable_lines(&mut lines, updater, true, language);
    }
    if diagnosis.has_conflict() {
        lines.push(Line::default());
        let versions_differ = diagnosis.target.versions_differ()
            || diagnosis
                .updater
                .as_ref()
                .is_some_and(doctor::ExecutableDiagnosis::versions_differ);
        lines.push(Line::styled(
            if versions_differ {
                language.text(
                    "conflict: PATH candidates report different versions",
                    "冲突：PATH 候选项报告了不同版本",
                )
            } else {
                language.text(
                    "conflict: multiple installations are visible in PATH",
                    "冲突：PATH 中存在多个可见安装",
                )
            },
            Style::default()
                .fg(WARNING_COLOR)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            language.text(
                "fix: remove the stale installation from PATH or move the intended one first",
                "建议：从 PATH 移除旧安装，或把预期安装移动到最前面",
            ),
            Style::default().fg(DIM),
        ));
    }
    lines
}

fn append_doctor_executable_lines(
    lines: &mut Vec<Line<'static>>,
    executable: &doctor::ExecutableDiagnosis,
    updater: bool,
    language: Language,
) {
    let label = match (updater, language) {
        (false, Language::English) => "command: ",
        (false, Language::Chinese) => "命令：",
        (true, Language::English) => "updater: ",
        (true, Language::Chinese) => "更新器：",
    };
    lines.push(Line::from(vec![
        Span::styled(label, Style::default().fg(SUBTLE)),
        Span::styled(
            executable.program.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ]));
    if executable.candidates.is_empty() {
        lines.push(Line::styled(
            language.text("active: not found", "当前生效：未找到"),
            Style::default().fg(SUBTLE),
        ));
        return;
    }
    for (index, candidate) in executable.candidates.iter().enumerate() {
        let prefix = match (index == 0, language) {
            (true, Language::English) => "active: ",
            (true, Language::Chinese) => "当前生效：",
            (false, Language::English) => "shadowed: ",
            (false, Language::Chinese) => "被遮蔽：",
        };
        let version = candidate.version.as_deref().unwrap_or("unknown");
        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(if index == 0 { SUCCESS } else { WARNING_COLOR }),
            ),
            Span::raw(candidate.path.display().to_string()),
            Span::styled(
                format!("  [{}]  ", candidate.source),
                Style::default().fg(DIM),
            ),
            Span::styled(
                match language {
                    Language::English => format!("version {version}"),
                    Language::Chinese => format!("版本 {version}"),
                },
                Style::default().fg(ACCENT),
            ),
        ]));
    }
}

fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    content_length: usize,
    viewport_length: usize,
    position: usize,
) {
    if area.width == 0 || area.height <= 2 || content_length <= viewport_length {
        return;
    }
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol("█")
        .thumb_style(Style::default().fg(SUBTLE))
        .track_symbol(Some("│"))
        .track_style(Style::default().fg(BORDER))
        .begin_symbol(None)
        .end_symbol(None);
    let positions = content_length
        .saturating_sub(viewport_length)
        .saturating_add(1);
    let mut state = ScrollbarState::new(positions)
        .position(position)
        .viewport_content_length(viewport_length);
    frame.render_stateful_widget(
        scrollbar,
        Rect {
            x: area.right().saturating_sub(1),
            y: area.y.saturating_add(1),
            width: 1,
            height: area.height.saturating_sub(2),
        },
        &mut state,
    );
}

fn draw_modal(frame: &mut Frame, app: &mut App, area: Rect) {
    app.modal_input_hitboxes.clear();
    if matches!(app.modal, Modal::None) {
        return;
    }
    render_modal_backdrop(frame, area);

    match &app.modal {
        Modal::None => {}
        Modal::ConfirmUpdate { tools } => {
            let inner = modal_panel(
                frame,
                area,
                app.language.text("Confirm update", "确认更新"),
                74,
                14,
            );
            let policy_detail = match app.process_strategy {
                ProcessStrategy::Wait => app.language.text(
                    "Matching processes will be waited on.",
                    "将等待匹配的进程退出。",
                ),
                ProcessStrategy::Terminate => app.language.text(
                    "Matching processes will be stopped before update.",
                    "将在更新前终止匹配的进程。",
                ),
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        match app.language {
                            Language::English => format!("Update {} tool(s)?", tools.len()),
                            Language::Chinese => format!("更新 {} 个工具？", tools.len()),
                        },
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    labeled_value(
                        app.language.text("Selected", "已选择"),
                        &tools.join(", "),
                        ACCENT,
                    ),
                    Line::raw(""),
                    Line::from(vec![
                        Span::styled(
                            format!("{}  ", app.language.text("Process policy", "进程策略")),
                            Style::default().fg(DIM),
                        ),
                        Span::styled(
                            app.process_strategy.label(app.language),
                            app.process_strategy.style().add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::styled(policy_detail, Style::default().fg(DIM)),
                    Line::raw(""),
                    modal_actions(
                        app.language,
                        app.language.text("confirm", "确认"),
                        app.language.text("cancel", "取消"),
                    ),
                ])
                .style(Style::default().bg(PANEL_BG))
                .wrap(Wrap { trim: false }),
                inner,
            );
        }
        Modal::ConfirmAdd {
            mode,
            name,
            command,
            ..
        } => {
            let inner = modal_panel(
                frame,
                area,
                match mode {
                    CommandFormMode::Add => app.language.text("Confirm add", "确认添加"),
                    CommandFormMode::Edit => app.language.text("Confirm edit", "确认编辑"),
                },
                74,
                14,
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        match mode {
                            CommandFormMode::Add => app.language.text(
                                "Save this custom update command?",
                                "保存这条自定义更新命令？",
                            ),
                            CommandFormMode::Edit => app.language.text(
                                "Replace this custom update command?",
                                "更新这条自定义更新命令？",
                            ),
                        },
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    labeled_value(app.language.text("Name", "名称"), name, ACCENT),
                    labeled_value(app.language.text("Command", "命令"), command, Color::White),
                    Line::raw(""),
                    Line::styled(
                        app.language.text(
                            "This saves the command only; it will not run now.",
                            "这里只保存命令，本次不会执行。",
                        ),
                        Style::default()
                            .fg(WARNING_COLOR)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    modal_actions(
                        app.language,
                        app.language.text("save", "保存"),
                        app.language.text("go back", "返回"),
                    ),
                ])
                .style(Style::default().bg(PANEL_BG))
                .wrap(Wrap { trim: false }),
                inner,
            );
        }
        Modal::ConfirmDelete { name } => {
            let inner = modal_panel(
                frame,
                area,
                app.language.text("Confirm delete", "确认删除"),
                62,
                9,
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        app.language.text(
                            "Remove this custom update command?",
                            "删除这条自定义更新命令？",
                        ),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    Line::styled(name.clone(), Style::default().fg(ERROR_COLOR)),
                    Line::raw(""),
                    modal_actions(
                        app.language,
                        app.language.text("remove", "删除"),
                        app.language.text("cancel", "取消"),
                    ),
                ])
                .style(Style::default().bg(PANEL_BG))
                .wrap(Wrap { trim: false }),
                inner,
            );
        }
        Modal::AddCommand {
            mode,
            field,
            name,
            command,
            ..
        } => {
            let inner = modal_panel(
                frame,
                area,
                match mode {
                    CommandFormMode::Add => app.language.text("Add command", "添加命令"),
                    CommandFormMode::Edit => app.language.text("Edit command", "编辑命令"),
                },
                80,
                16,
            );
            let name_label = app.language.text("Name", "名称");
            let command_label = app.language.text("Command", "命令");
            let name_width = input_value_width(inner.width, name_label);
            let command_width = input_value_width(inner.width, command_label);
            let lines = vec![
                Line::styled(
                    match mode {
                        CommandFormMode::Add => app.language.text(
                            "Create a user-level update command.",
                            "创建一条用户级更新命令。",
                        ),
                        CommandFormMode::Edit => app.language.text(
                            "Edit or rename the selected custom command.",
                            "编辑或重命名选中的自定义命令。",
                        ),
                    },
                    Style::default().fg(DIM),
                ),
                Line::raw(""),
                modal_input_line(*field == 0, name_label, name, "claude", name_width),
                modal_input_line(
                    *field == 1,
                    command_label,
                    command,
                    "claude install",
                    command_width,
                ),
                Line::raw(""),
                Line::styled(
                    app.language.text("Examples", "示例"),
                    Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                ),
                Line::styled("  claude install", Style::default().fg(SUBTLE)),
                Line::styled(
                    "  npm install -g package@latest",
                    Style::default().fg(SUBTLE),
                ),
                Line::styled("  pnpm add -g package@latest", Style::default().fg(SUBTLE)),
                Line::styled("  brew upgrade ripgrep", Style::default().fg(SUBTLE)),
                Line::raw(""),
                modal_form_actions(app.language, true),
            ];
            frame.render_widget(
                Paragraph::new(lines)
                    .style(Style::default().bg(PANEL_BG))
                    .wrap(Wrap { trim: false }),
                inner,
            );

            for (field, label, input, row, value_width) in [
                (0, name_label, name, 2, name_width),
                (1, command_label, command, 3, command_width),
            ] {
                if inner.height <= row || value_width == 0 {
                    continue;
                }
                let label_width = u16::try_from(display_width(label)).unwrap_or(u16::MAX);
                let (visible_start, visible_end) = input.visible_range(value_width);
                app.modal_input_hitboxes.push(ModalInputHitbox {
                    area: Rect::new(
                        inner
                            .x
                            .saturating_add(2)
                            .saturating_add(label_width)
                            .saturating_add(2),
                        inner.y.saturating_add(row),
                        u16::try_from(value_width).unwrap_or(u16::MAX),
                        1,
                    ),
                    field,
                    visible_start,
                    visible_end,
                });
            }

            let (label, input, row, value_width) = if *field == 0 {
                (name_label, name, 2, name_width)
            } else {
                (command_label, command, 3, command_width)
            };
            if inner.width > 0 && inner.height > row {
                let label_width = u16::try_from(display_width(label)).unwrap_or(u16::MAX);
                let (visible_start, _) = input.visible_range(value_width);
                let cursor_width =
                    u16::try_from(display_width(&input.value[visible_start..input.cursor]))
                        .unwrap_or(u16::MAX);
                let cursor_x = inner
                    .x
                    .saturating_add(2)
                    .saturating_add(label_width)
                    .saturating_add(2)
                    .saturating_add(cursor_width)
                    .min(inner.right().saturating_sub(1));
                frame.set_cursor_position(Position::new(cursor_x, inner.y.saturating_add(row)));
            }
        }
    }
}

fn render_modal_backdrop(frame: &mut Frame, area: Rect) {
    let buffer = frame.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buffer[(x, y)].set_bg(BACKDROP_BG).set_fg(SUBTLE);
        }
    }
}

fn modal_panel(frame: &mut Frame, area: Rect, title: &str, width: u16, height: u16) -> Rect {
    let rect = centered_rect(width, height, area);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let inset = 2.min(inner.width / 2);
    Rect {
        x: inner.x.saturating_add(inset),
        width: inner.width.saturating_sub(inset.saturating_mul(2)),
        ..inner
    }
}

fn labeled_value(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}  "), Style::default().fg(DIM)),
        Span::styled(value.to_owned(), Style::default().fg(color)),
    ])
}

fn display_width(value: &str) -> usize {
    Line::from(value).width()
}

fn input_value_width(line_width: u16, label: &str) -> usize {
    usize::from(line_width).saturating_sub(4 + display_width(label))
}

fn modal_input_line(
    active: bool,
    label: &str,
    input: &TextInput,
    placeholder: &str,
    max_value_width: usize,
) -> Line<'static> {
    let marker_style = if active {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(SUBTLE)
    };
    let value_spans = if input.value.is_empty() {
        vec![Span::styled(
            placeholder.to_owned(),
            Style::default().fg(SUBTLE),
        )]
    } else {
        let (visible_start, visible_end) = input.visible_range(max_value_width);
        let base_style = if active {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let Some((selected_start, selected_end)) = input.selection() else {
            return Line::from(vec![
                Span::styled(if active { "› " } else { "  " }, marker_style),
                Span::styled(
                    format!("{label}  "),
                    if active {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    },
                ),
                Span::styled(
                    input.value[visible_start..visible_end].to_owned(),
                    base_style,
                ),
            ])
            .style(if active {
                Style::default().bg(SURFACE)
            } else {
                Style::default().bg(PANEL_BG)
            });
        };
        let selected_start = selected_start.max(visible_start).min(visible_end);
        let selected_end = selected_end.max(visible_start).min(visible_end);
        let mut spans = Vec::new();
        if visible_start < selected_start {
            spans.push(Span::styled(
                input.value[visible_start..selected_start].to_owned(),
                base_style,
            ));
        }
        if selected_start < selected_end {
            spans.push(Span::styled(
                input.value[selected_start..selected_end].to_owned(),
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if selected_end < visible_end {
            spans.push(Span::styled(
                input.value[selected_end..visible_end].to_owned(),
                base_style,
            ));
        }
        spans
    };
    let mut spans = vec![
        Span::styled(if active { "› " } else { "  " }, marker_style),
        Span::styled(
            format!("{label}  "),
            if active {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            },
        ),
    ];
    spans.extend(value_spans);
    Line::from(spans).style(if active {
        Style::default().bg(SURFACE)
    } else {
        Style::default().bg(PANEL_BG)
    })
}

fn modal_actions(language: Language, primary: &str, secondary: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "[Enter/y]",
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {primary}    ")),
        Span::styled(
            "[Esc/n]",
            Style::default()
                .fg(ERROR_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {secondary}")),
        Span::styled(
            match language {
                Language::English => "    modal input only",
                Language::Chinese => "    仅响应当前窗口",
            },
            Style::default().fg(SUBTLE),
        ),
    ])
}

fn modal_form_actions(language: Language, multiple_fields: bool) -> Line<'static> {
    let mut spans = Vec::new();
    if multiple_fields {
        spans.extend([
            Span::styled("[Tab/↑↓]", Style::default().fg(ACCENT)),
            Span::raw(language.text(" field    ", " 切换栏    ")),
        ]);
    }
    spans.extend([
        Span::styled("[Shift+←/→]", Style::default().fg(ACCENT)),
        Span::raw(language.text(" select    ", " 选择    ")),
        Span::styled("[Ctrl+A]", Style::default().fg(ACCENT)),
        Span::raw(language.text(" all    ", " 全选    ")),
        Span::styled("[Enter]", Style::default().fg(SUCCESS)),
        Span::raw(language.text(" review    ", " 预览    ")),
        Span::styled("[Esc]", Style::default().fg(ERROR_COLOR)),
        Span::raw(language.text(" cancel", " 取消")),
    ]);
    Line::from(spans)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let available_width = area.width.saturating_sub(4).max(area.width.min(4));
    let available_height = area.height.saturating_sub(2).max(area.height.min(2));
    let width = width.min(available_width);
    let height = height.min(available_height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn version_command_for_tool(name: &str, tool: &Tool, working_directory: PathBuf) -> CommandSpec {
    let updater = executable_name(&tool.program);
    if updater == "brew"
        && tool
            .args
            .first()
            .is_some_and(|argument| argument == "upgrade")
        && let Some(package) = tool
            .args
            .iter()
            .rev()
            .find(|argument| argument.as_str() != "upgrade" && !argument.starts_with('-'))
    {
        let mut args = vec!["list".to_owned()];
        if tool.args.iter().any(|argument| argument == "--cask") {
            args.push("--cask".to_owned());
        }
        args.extend(["--versions".to_owned(), package.clone()]);
        return CommandSpec {
            program: tool.program.clone(),
            args,
            working_directory,
        };
    }

    let known_tool = matches!(
        name.to_ascii_lowercase().as_str(),
        "brew" | "bun" | "codex" | "rustup" | "scoop" | "uv"
    );
    let package_manager = matches!(updater.as_str(), "brew" | "bun" | "npm" | "pnpm" | "scoop");
    CommandSpec {
        program: if known_tool || package_manager {
            name.to_owned()
        } else {
            tool.program.clone()
        },
        args: vec!["--version".to_owned()],
        working_directory,
    }
}

fn executable_name(program: &str) -> String {
    let executable = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    [".exe", ".cmd", ".ps1", ".bat"]
        .into_iter()
        .find_map(|suffix| executable.strip_suffix(suffix))
        .unwrap_or(&executable)
        .to_owned()
}

fn spawn_version_probe(
    tx: Sender<AppEvent>,
    name: String,
    probe_id: u64,
    command_spec: CommandSpec,
) {
    if cfg!(test) {
        return;
    }
    thread::spawn(move || {
        let version = command::run(&command_spec)
            .ok()
            .filter(|result| result.status.success())
            .and_then(|result| version_from_output(&result.stdout, &result.stderr));
        let _ = tx.send(AppEvent::VersionResolved {
            name,
            probe_id,
            version,
        });
    });
}

fn version_from_output(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    let mut output = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.is_empty() {
        output.push('\n');
        output.push_str(&String::from_utf8_lossy(stderr));
    }
    let lines = sanitize_terminal_output(&output)
        .into_iter()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if let Some(version) = lines
        .iter()
        .flat_map(|line| line.split_whitespace())
        .find_map(version_token)
    {
        return Some(version);
    }
    let first = lines.first()?;
    let version = if first.ends_with(':') {
        lines
            .get(1)
            .map(|second| format!("{first} {second}"))
            .unwrap_or_else(|| first.clone())
    } else {
        first.clone()
    };
    Some(truncate_version(&version, 48))
}

fn version_token(token: &str) -> Option<String> {
    let candidate = token.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '.' | '-' | '+' | '_')
    });
    let numeric = candidate
        .strip_prefix('v')
        .or_else(|| candidate.strip_prefix('V'))
        .unwrap_or(candidate);
    (numeric.contains('.')
        && numeric.chars().any(|character| character.is_ascii_digit())
        && numeric.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+' | '_')
        }))
    .then(|| candidate.to_owned())
}

fn truncate_version(version: &str, max_characters: usize) -> String {
    if version.chars().count() <= max_characters {
        return version.to_owned();
    }
    let mut truncated = version
        .chars()
        .take(max_characters.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn spawn_dvup(
    tx: Sender<AppEvent>,
    executable: PathBuf,
    state_root: PathBuf,
    arguments: Vec<String>,
    name: String,
    operation: Operation,
    language: Language,
) {
    thread::spawn(move || {
        let started = Instant::now();
        let mut child = Command::new(executable);
        child.arg("--state-dir").arg(state_root).args(arguments);
        command::configure_no_window(&mut child);
        let output = child.output();
        let event = match output {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.is_empty() {
                    if !text.ends_with('\n') && !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&stderr);
                }
                AppEvent::Finished {
                    name,
                    success: output.status.success(),
                    output: text,
                    operation,
                    elapsed: started.elapsed(),
                }
            }
            Err(error) => AppEvent::Finished {
                name,
                success: false,
                output: match language {
                    Language::English => format!("failed to start dvup subprocess: {error}"),
                    Language::Chinese => format!("无法启动 dvup 子进程：{error}"),
                },
                operation,
                elapsed: started.elapsed(),
            },
        };
        let _ = tx.send(event);
    });
}

fn format_run_result(tool: &ToolItem, frame: u64, language: Language) -> String {
    let label = tool.run_state.label(frame, language);
    match tool.elapsed {
        Some(elapsed) if tool.run_state != RunState::Running => {
            format!("{label} {:.1}s", elapsed.as_secs_f64())
        }
        _ => label.to_owned(),
    }
}

fn update_arguments(
    name: &str,
    config_path: Option<&Path>,
    terminate_locking_processes: bool,
) -> Vec<String> {
    let mut arguments = vec![
        "update".to_owned(),
        "--background".to_owned(),
        "auto".to_owned(),
    ];
    if terminate_locking_processes {
        arguments.push("--terminate-locking-processes".to_owned());
    }
    if let Some(config_path) = config_path {
        arguments.push("--config".to_owned());
        arguments.push(config_path.to_string_lossy().into_owned());
    }
    arguments.push(name.to_owned());
    arguments
}

fn add_arguments(name: &str, command: Vec<String>, force: bool) -> Vec<String> {
    let mut arguments = vec!["add".to_owned()];
    if force {
        arguments.push("--force".to_owned());
    }
    arguments.push(name.to_owned());
    arguments.extend(command);
    arguments
}

fn edit_arguments(original_name: &str, name: &str, command: Vec<String>) -> Vec<String> {
    let mut arguments = vec!["edit".to_owned(), original_name.to_owned(), name.to_owned()];
    arguments.extend(command);
    arguments
}

fn output_was_queued(name: &str, output: &str) -> bool {
    let queued = format!("queued {name}:");
    let updated = format!("updated {name}:");
    for line in output.lines().rev().map(str::trim) {
        if line.starts_with(&updated) {
            return false;
        }
        if line.starts_with(&queued) {
            return true;
        }
    }
    false
}

fn sanitize_terminal_output(output: &str) -> Vec<String> {
    let output = strip_terminal_sequences(output);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut characters = output.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\r' if characters.peek() == Some(&'\n') => {
                characters.next();
                lines.push(std::mem::take(&mut line));
            }
            '\r' => line.clear(),
            '\n' => lines.push(std::mem::take(&mut line)),
            '\u{8}' => {
                line.pop();
            }
            '\t' => line.push_str("    "),
            value if value.is_control() => {}
            value => line.push(value),
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }

    lines
        .into_iter()
        .map(|line| line.trim_end().to_owned())
        .filter(|line| !is_progress_artifact(line))
        .collect()
}

fn strip_terminal_sequences(output: &str) -> String {
    let mut plain = String::with_capacity(output.len());
    let mut characters = output.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => match characters.next() {
                Some('[') => consume_control_sequence(&mut characters),
                Some(']' | 'P' | 'X' | '^' | '_') => consume_terminal_string(&mut characters),
                Some(_) | None => {}
            },
            '\u{9b}' => consume_control_sequence(&mut characters),
            value => plain.push(value),
        }
    }

    plain
}

fn consume_control_sequence(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for character in characters.by_ref() {
        if ('@'..='~').contains(&character) {
            break;
        }
    }
}

fn consume_terminal_string(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(character) = characters.next() {
        if character == '\u{7}' {
            break;
        }
        if character == '\u{1b}' && characters.peek() == Some(&'\\') {
            characters.next();
            break;
        }
    }
}

fn is_progress_artifact(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("% total") && lower.contains("% received") {
        return true;
    }

    let mut has_progress_marker = false;
    let only_progress_characters = trimmed.chars().all(|character| {
        if matches!(
            character,
            '#' | '=' | '%' | '|' | '/' | '\\' | '*' | 'O' | 'o'
        ) {
            has_progress_marker = true;
            true
        } else {
            character.is_ascii_digit()
                || character.is_ascii_whitespace()
                || matches!(
                    character,
                    '.' | '-' | ':' | '<' | '>' | '[' | ']' | '(' | ')'
                )
        }
    });
    only_progress_characters && has_progress_marker
}

fn load_custom_names(state: &StateDirs) -> Result<HashSet<String>> {
    let path = state.custom_config_path();
    if !path.is_file() {
        return Ok(HashSet::new());
    }
    Ok(Config::load(&path)?.tools.into_keys().collect())
}

fn format_command(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_editable_command(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(quote_editable_argument)
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_editable_argument(value: &str) -> String {
    if !value.is_empty()
        && !value.chars().any(char::is_whitespace)
        && !value.contains('"')
        && !value.contains('\'')
    {
        return value.to_owned();
    }
    if !value.contains('"') {
        format!("\"{value}\"")
    } else if !value.contains('\'') {
        format!("'{value}'")
    } else {
        value.to_owned()
    }
}

fn split_command_line(input: &str, language: Language) -> std::result::Result<Vec<String>, String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for character in input.chars() {
        match (quote, character) {
            (Some(expected), value) if value == expected => quote = None,
            (Some(_), value) => {
                current.push(value);
                started = true;
            }
            (None, '\'' | '"') => {
                quote = Some(character);
                started = true;
            }
            (None, value) if value.is_whitespace() => {
                if started {
                    arguments.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, value) => {
                current.push(value);
                started = true;
            }
        }
    }
    if let Some(quote) = quote {
        return Err(match language {
            Language::English => format!("Unclosed {quote} quote in command"),
            Language::Chinese => format!("命令中有未闭合的 {quote} 引号"),
        });
    }
    if started {
        arguments.push(current);
    }
    Ok(arguments)
}

fn previous_index(current: usize, length: usize) -> usize {
    if length == 0 {
        0
    } else if current == 0 {
        length - 1
    } else {
        current - 1
    }
}

fn next_index(current: usize, length: usize) -> usize {
    if length == 0 {
        0
    } else {
        (current + 1) % length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_version_command(name: &str) -> CommandSpec {
        CommandSpec {
            program: name.to_owned(),
            args: vec!["--version".to_owned()],
            working_directory: PathBuf::from("."),
        }
    }

    fn test_diagnosis(name: &str, paths: &[(&str, Option<&str>)]) -> doctor::ToolDiagnosis {
        doctor::ToolDiagnosis {
            name: name.to_owned(),
            supported: true,
            target: doctor::ExecutableDiagnosis {
                program: name.to_owned(),
                candidates: paths
                    .iter()
                    .map(|(path, version)| doctor::ExecutableCandidate {
                        path: PathBuf::from(path),
                        source: "PATH",
                        version: version.map(str::to_owned),
                    })
                    .collect(),
            },
            updater: None,
        }
    }

    #[test]
    fn splits_custom_command_with_quoted_path() {
        assert_eq!(
            split_command_line(
                r#""C:\Program Files\Claude\claude.exe" install --channel stable"#,
                Language::English,
            )
            .expect("split command"),
            [
                r#"C:\Program Files\Claude\claude.exe"#,
                "install",
                "--channel",
                "stable"
            ]
        );

        let editable = format_editable_command(
            r#"C:\Program Files\Example\example.exe"#,
            &[
                "update".to_owned(),
                "two words".to_owned(),
                "a\"b".to_owned(),
            ],
        );
        assert_eq!(
            split_command_line(&editable, Language::English).expect("editable command"),
            [
                r#"C:\Program Files\Example\example.exe"#,
                "update",
                "two words",
                "a\"b"
            ]
        );
    }

    #[test]
    fn extracts_concise_versions_from_command_output() {
        assert_eq!(
            version_from_output(b"\x1b[32muv 0.8.0\x1b[0m\n", b""),
            Some("0.8.0".to_owned())
        );
        assert_eq!(
            version_from_output(b"Current Scoop version:\n0.5.2 - released\n", b""),
            Some("0.5.2".to_owned())
        );
        assert_eq!(version_from_output(b"\n", b"\n"), None);
    }

    #[test]
    fn builds_version_commands_for_builtins_and_homebrew_packages() {
        let working_directory = PathBuf::from("workspace");
        let uv = Config::starter().tools.remove("uv").expect("uv preset");
        let uv_version = version_command_for_tool("uv", &uv, working_directory.clone());
        assert_eq!(uv_version.program, "uv");
        assert_eq!(uv_version.args, ["--version"]);

        let ripgrep = Tool::custom(
            "ripgrep",
            "/opt/homebrew/bin/brew".to_owned(),
            vec!["upgrade".to_owned(), "ripgrep".to_owned()],
        );
        let ripgrep_version =
            version_command_for_tool("ripgrep", &ripgrep, working_directory.clone());
        assert_eq!(ripgrep_version.program, "/opt/homebrew/bin/brew");
        assert_eq!(ripgrep_version.args, ["list", "--versions", "ripgrep"]);

        let claude = Tool::custom(
            "claude-custom",
            "claude".to_owned(),
            vec!["install".to_owned()],
        );
        let claude_version = version_command_for_tool("claude-custom", &claude, working_directory);
        assert_eq!(claude_version.program, "claude");
        assert_eq!(claude_version.args, ["--version"]);
    }

    #[test]
    fn ignores_stale_version_probe_results() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let tool = app.tools.first_mut().expect("built-in tool");
        let name = tool.name.clone();
        let current_probe = tool.version_probe_id;
        tool.version = VersionState::Loading;

        app.tx
            .send(AppEvent::VersionResolved {
                name: name.clone(),
                probe_id: current_probe.wrapping_sub(1),
                version: Some("stale 0.0.1".to_owned()),
            })
            .expect("stale result");
        app.process_events();
        assert!(matches!(app.tools[0].version, VersionState::Loading));

        app.tx
            .send(AppEvent::VersionResolved {
                name,
                probe_id: current_probe,
                version: Some("current 1.0.0".to_owned()),
            })
            .expect("current result");
        app.process_events();
        assert!(matches!(
            &app.tools[0].version,
            VersionState::Available(version) if version == "current 1.0.0"
        ));
    }

    #[test]
    fn ignores_stale_doctor_results_and_accepts_the_current_scan() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.doctor_probe_id = 2;
        app.doctor_loading = true;

        app.tx
            .send(AppEvent::DoctorResolved {
                probe_id: 1,
                diagnoses: vec![test_diagnosis("stale", &[("stale/tool", Some("0.1.0"))])],
                error: None,
            })
            .expect("stale doctor result");
        app.process_events();
        assert!(app.doctor_diagnoses.is_empty());
        assert!(app.doctor_loading);

        app.tx
            .send(AppEvent::DoctorResolved {
                probe_id: 2,
                diagnoses: vec![test_diagnosis(
                    "uv",
                    &[("first/uv", Some("1.0.0")), ("second/uv", Some("0.9.0"))],
                )],
                error: None,
            })
            .expect("current doctor result");
        app.process_events();

        assert!(!app.doctor_loading);
        assert_eq!(app.doctor_diagnoses.len(), 1);
        assert!(app.doctor_diagnoses[0].has_conflict());
        assert!(app.doctor_checked_at.is_some());
        assert!(app.message.contains("1 conflict"));
    }

    #[test]
    fn doctor_rows_follow_mouse_focus_and_expand_without_switching_tabs() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tab = Tab::Doctor;
        app.doctor_diagnoses = vec![
            test_diagnosis("codex", &[("first/codex", Some("1.0.0"))]),
            test_diagnosis("uv", &[("first/uv", Some("1.0.0"))]),
        ];
        app.doctor_hitboxes = vec![(Rect::new(1, 4, 50, 1), 0), (Rect::new(1, 5, 50, 1), 1)];

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 2,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.doctor_index, 1);
        assert!(app.expanded_doctor.is_none());

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.tab, Tab::Doctor);
        assert_eq!(app.expanded_doctor.as_deref(), Some("uv"));

        handle_doctor_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.expanded_doctor.is_none());
    }

    #[test]
    fn doctor_view_renders_shadowed_installations_and_checked_time() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tab = Tab::Doctor;
        app.doctor_diagnoses = vec![test_diagnosis(
            "uv",
            &[("first/uv", Some("1.0.0")), ("second/uv", Some("0.9.0"))],
        )];
        app.expanded_doctor = Some("uv".to_owned());
        app.doctor_checked_at = Some("2026-07-12 14:30:00".to_owned());
        let backend = TestBackend::new(80, 26);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render doctor view");

        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("1 tools · 1 warn"), "screen: {screen}");
        assert!(screen.contains("shadowed:"), "screen: {screen}");
        assert!(screen.contains("2026-07-12 14:30:00"), "screen: {screen}");
    }

    #[test]
    fn rejects_unclosed_quote() {
        assert!(split_command_line("claude 'install", Language::English).is_err());
        assert!(
            split_command_line("claude 'install", Language::Chinese)
                .expect_err("unclosed quote")
                .contains("未闭合")
        );
    }

    #[test]
    fn l_switches_languages_but_remains_text_in_the_add_form() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let lower_l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);

        handle_key(&mut app, lower_l);
        assert_eq!(app.language, Language::Chinese);
        assert_eq!(app.message, "语言已切换为中文");
        assert_eq!(app.activity[0], "欢迎使用 dvup。");
        assert_eq!(Availability::Installed.label(app.language), "已安装");
        assert_eq!(RunState::Updated.label(0, app.language), "已更新");
        assert_eq!(app.language.job_status(&JobStatus::Pending), "等待执行");

        app.modal = Modal::AddCommand {
            mode: CommandFormMode::Add,
            original_name: None,
            field: 0,
            name: TextInput::new(String::new()),
            command: TextInput::new(String::new()),
        };
        handle_key(&mut app, lower_l);
        assert_eq!(app.language, Language::Chinese);
        assert!(matches!(
            &app.modal,
            Modal::AddCommand { name, .. } if name.value == "l"
        ));
    }

    #[test]
    fn modal_keyboard_input_never_reaches_the_underlying_view() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::ConfirmUpdate {
            tools: vec!["example".to_owned()],
        };
        let original_tab = app.tab;
        let original_language = app.language;
        let original_strategy = app.process_strategy;

        for key in [
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        ] {
            handle_key(&mut app, key);
        }

        assert_eq!(app.tab, original_tab);
        assert_eq!(app.language, original_language);
        assert_eq!(app.process_strategy, original_strategy);
        assert!(!app.should_quit);
        assert!(matches!(app.modal, Modal::ConfirmUpdate { .. }));

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn modal_mouse_input_never_reaches_the_underlying_view() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tools = vec![ToolItem {
            name: "available".to_owned(),
            command: "available update".to_owned(),
            version: VersionState::Available("1.2.3".to_owned()),
            version_command: test_version_command("available"),
            version_probe_id: 1,
            availability: Availability::Installed,
            custom: false,
            selected: false,
            run_state: RunState::Idle,
            elapsed: None,
        }];
        app.tool_hitboxes = vec![(Rect::new(1, 4, 50, 1), 0)];
        app.tab_hitboxes = vec![(Rect::new(10, 1, 8, 1), Tab::Jobs.index())];
        app.modal = Modal::ConfirmDelete {
            name: "available".to_owned(),
        };

        for (column, row) in [(2, 4), (11, 1)] {
            handle_mouse(
                &mut app,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                },
            );
        }

        assert_eq!(app.tab, Tab::Tools);
        assert!(!app.tools[0].selected);

        app.tab = Tab::Activity;
        app.activity_scroll = 10;
        app.activity_hitboxes = vec![(Rect::new(1, 4, 50, 1), 0)];
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::ScrollDown,
        ] {
            handle_mouse(
                &mut app,
                MouseEvent {
                    kind,
                    column: 2,
                    row: 4,
                    modifiers: KeyModifiers::NONE,
                },
            );
        }
        assert_eq!(app.activity_scroll, 10);
        assert!(app.expanded_activity.is_empty());
        assert!(app.hovered_activity.is_none());
        assert!(matches!(app.modal, Modal::ConfirmDelete { .. }));
    }

    #[test]
    fn add_command_input_supports_mouse_selection_and_replacement() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::AddCommand {
            mode: CommandFormMode::Add,
            original_name: None,
            field: 1,
            name: TextInput::new("example"),
            command: TextInput::new("abcd"),
        };
        app.modal_input_hitboxes = vec![ModalInputHitbox {
            area: Rect::new(10, 5, 10, 1),
            field: 1,
            visible_start: 0,
            visible_end: 4,
        }];

        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), 11),
            (MouseEventKind::Drag(MouseButton::Left), 13),
            (MouseEventKind::Up(MouseButton::Left), 13),
        ] {
            handle_mouse(
                &mut app,
                MouseEvent {
                    kind,
                    column,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                },
            );
        }
        handle_modal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE),
        );

        assert!(matches!(
            &app.modal,
            Modal::AddCommand { command, .. } if command.value == "aXd"
        ));
    }

    #[test]
    fn modal_render_dims_the_view_and_raises_a_panel() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::ConfirmDelete {
            name: "example".to_owned(),
        };
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render modal");

        let buffer = terminal.backend().buffer();
        let panel = centered_rect(62, 9, buffer.area);
        assert_eq!(buffer[(0, 0)].bg, BACKDROP_BG);
        assert_eq!(buffer[(panel.x + 1, panel.y + 1)].bg, PANEL_BG);
        let screen = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Confirm delete"));
        assert!(screen.contains("[Enter/y]"));
    }

    #[test]
    fn add_command_modal_remains_renderable_in_a_small_terminal() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::AddCommand {
            mode: CommandFormMode::Add,
            original_name: None,
            field: 1,
            name: TextInput::new("example"),
            command: TextInput::new("example update"),
        };
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render small modal");
    }

    #[test]
    fn add_command_input_inserts_at_the_moved_cursor() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::AddCommand {
            mode: CommandFormMode::Add,
            original_name: None,
            field: 1,
            name: TextInput::new("example"),
            command: TextInput::new("abcd"),
        };

        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        handle_modal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE),
        );

        assert!(matches!(
            &app.modal,
            Modal::AddCommand { command, .. } if command.value == "abcXd"
        ));
    }

    #[test]
    fn add_command_input_replaces_a_keyboard_selection() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::AddCommand {
            mode: CommandFormMode::Add,
            original_name: None,
            field: 1,
            name: TextInput::new("example"),
            command: TextInput::new("old command"),
        };

        handle_modal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        handle_modal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE),
        );

        assert!(matches!(
            &app.modal,
            Modal::AddCommand { command, .. } if command.value == "X"
        ));
    }

    #[test]
    fn add_command_input_replaces_a_shift_arrow_selection() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::AddCommand {
            mode: CommandFormMode::Add,
            original_name: None,
            field: 1,
            name: TextInput::new("example"),
            command: TextInput::new("abcd"),
        };

        for (code, modifiers) in [
            (KeyCode::Left, KeyModifiers::NONE),
            (KeyCode::Left, KeyModifiers::SHIFT),
            (KeyCode::Left, KeyModifiers::SHIFT),
            (KeyCode::Char('X'), KeyModifiers::NONE),
        ] {
            handle_modal_key(&mut app, KeyEvent::new(code, modifiers));
        }

        assert!(matches!(
            &app.modal,
            Modal::AddCommand { command, .. } if command.value == "aXd"
        ));
    }

    #[test]
    fn edit_key_opens_the_selected_custom_command() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        state.ensure().expect("state directories");
        let mut custom = Config::empty();
        custom.tools.insert(
            "example".to_owned(),
            crate::config::Tool::custom(
                "example",
                "example-cli".to_owned(),
                vec!["update".to_owned(), "two words".to_owned()],
            ),
        );
        custom
            .save(&state.custom_config_path())
            .expect("save custom command");
        let mut app = App::new(state, None).expect("app");
        app.tool_index = app
            .tools
            .iter()
            .position(|tool| tool.name == "example")
            .expect("custom tool row");

        handle_tools_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );

        assert!(matches!(
            &app.modal,
            Modal::AddCommand {
                mode: CommandFormMode::Edit,
                original_name: Some(original_name),
                field: 1,
                name,
                command,
            } if original_name == "example"
                && name.value == "example"
                && command.value == r#"example-cli update "two words""#
        ));

        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(matches!(&app.modal, Modal::AddCommand { field: 0, .. }));
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(&app.modal, Modal::AddCommand { field: 1, .. }));

        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(&app.modal, Modal::AddCommand { field: 0, .. }));

        handle_modal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        for character in "renamed".chars() {
            handle_modal_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            );
        }
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &app.modal,
            Modal::ConfirmAdd {
                mode: CommandFormMode::Edit,
                original_name: Some(original_name),
                name,
                ..
            } if original_name == "example" && name == "renamed"
        ));
    }

    #[test]
    fn sanitizes_dynamic_terminal_output_before_rendering_activity() {
        let output = concat!(
            "\u{1b}[?25l#=#=#\r##O#-#\r 79.0%\u{1b}[?25h\n",
            "\u{1b}[32mBun 1.3.14 was installed successfully!\u{1b}[0m\n",
            "coverage: 100% complete\n",
            "updated bun: Bun official installer\n",
        );

        assert_eq!(
            sanitize_terminal_output(output),
            [
                "Bun 1.3.14 was installed successfully!",
                "coverage: 100% complete",
                "updated bun: Bun official installer",
            ]
        );
    }

    #[test]
    fn activity_executions_are_collapsed_until_their_header_is_activated() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.activity = vec![
            "Welcome".to_owned(),
            "\n=== update bun: OK ===".to_owned(),
            "downloaded package".to_owned(),
            "updated bun".to_owned(),
            "\n=== Complete: 1 updated, 0 queued, 0 failed (1 total) in 1.0s ===".to_owned(),
        ];

        let collapsed = activity_render_lines(&app);
        assert!(collapsed.iter().any(|(_, target)| *target == Some(1)));
        assert!(!collapsed.iter().any(|(line, _)| {
            line.spans
                .iter()
                .any(|span| span.content.contains("downloaded package"))
        }));

        app.activity_hitboxes = vec![(Rect::new(2, 3, 30, 1), 1)];
        app.tab = Tab::Activity;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row: 3,
                modifiers: KeyModifiers::NONE,
            },
        );

        let expanded = activity_render_lines(&app);
        assert!(app.expanded_activity.contains(&1));
        assert!(expanded.iter().any(|(line, _)| {
            line.spans
                .iter()
                .any(|span| span.content.contains("downloaded package"))
        }));
    }

    #[test]
    fn activity_lines_render_their_recorded_datetime() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.activity = vec!["recorded event".to_owned()];
        app.activity_timestamps = vec!["2026-07-12 10:20:30".to_owned()];

        let rendered = activity_render_lines(&app);
        let text = rendered[0]
            .0
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "[2026-07-12 10:20:30] recorded event");
    }

    #[test]
    fn clicking_tool_rows_toggles_only_available_tools() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tools = vec![
            ToolItem {
                name: "available".to_owned(),
                command: "available update".to_owned(),
                version: VersionState::Available("1.2.3".to_owned()),
                version_command: test_version_command("available"),
                version_probe_id: 1,
                availability: Availability::Installed,
                custom: false,
                selected: false,
                run_state: RunState::Idle,
                elapsed: None,
            },
            ToolItem {
                name: "missing".to_owned(),
                command: "missing update".to_owned(),
                version: VersionState::Unavailable,
                version_command: test_version_command("missing"),
                version_probe_id: 2,
                availability: Availability::Missing,
                custom: false,
                selected: false,
                run_state: RunState::Idle,
                elapsed: None,
            },
        ];
        app.tab = Tab::Tools;
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render tools");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("VERSION"), "screen: {screen}");
        assert!(screen.contains("1.2.3"), "screen: {screen}");

        let available_area = app.tool_hitboxes[0].0;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: available_area.x,
                row: available_area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.tool_index, 0);
        assert!(app.tools[0].selected);

        let missing_area = app.tool_hitboxes[1].0;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: missing_area.x,
                row: missing_area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.tool_index, 1);
        assert!(!app.tools[1].selected);
        assert_eq!(app.message, "missing is not available");
    }

    #[test]
    fn mouse_movement_follows_focus_without_activating_items() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tools = ["first", "second"]
            .into_iter()
            .map(|name| ToolItem {
                name: name.to_owned(),
                command: format!("{name} update"),
                version: VersionState::Available("1.0.0".to_owned()),
                version_command: test_version_command(name),
                version_probe_id: 1,
                availability: Availability::Installed,
                custom: false,
                selected: false,
                run_state: RunState::Idle,
                elapsed: None,
            })
            .collect();
        app.tool_hitboxes = vec![(Rect::new(1, 4, 50, 1), 0), (Rect::new(1, 5, 50, 1), 1)];
        app.tab_hitboxes = vec![(Rect::new(10, 1, 8, 1), Tab::Jobs.index())];

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 2,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.tool_index, 1);
        assert!(app.tools.iter().all(|tool| !tool.selected));

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 11,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.hovered_tab, Some(Tab::Jobs.index()));
        assert_eq!(app.tab, Tab::Tools);

        app.tab = Tab::Activity;
        app.activity = vec!["\n=== update example: OK ===".to_owned()];
        app.activity_hitboxes = vec![(Rect::new(1, 4, 50, 1), 0)];
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 2,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.hovered_activity, Some(0));
        assert!(app.expanded_activity.is_empty());
        assert_eq!(
            activity_render_lines(&app)[1].0.style.bg,
            Some(SELECTION_BG)
        );

        app.jobs = ["job-1", "job-2"]
            .into_iter()
            .map(|id| JobItem {
                id: id.to_owned(),
                name: id.to_owned(),
                status: JobStatus::Pending,
                updated_at_unix_ms: 0,
            })
            .collect();
        app.tab = Tab::Jobs;
        app.job_hitboxes = vec![(Rect::new(1, 4, 50, 1), 0), (Rect::new(1, 5, 50, 1), 1)];
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 2,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.job_index, 1);
        assert!(app.expanded_job.is_none());
    }

    #[test]
    fn renders_two_expanded_activity_executions_in_the_same_frame() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tab = Tab::Activity;
        app.activity = vec![
            "Welcome".to_owned(),
            "\n=== update first: OK ===".to_owned(),
            "first output ".repeat(50),
            "\n=== update second: FAILED ===".to_owned(),
            "second output ".repeat(50),
            "\n=== Complete: 1 updated, 0 queued, 1 failed (2 total) in 1.0s ===".to_owned(),
        ];
        app.expanded_activity.extend([1, 3]);
        let backend = TestBackend::new(42, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render two expanded executions");

        assert_eq!(app.expanded_activity.len(), 2);
        assert_eq!(app.activity_hitboxes.len(), 1);
    }

    #[test]
    fn activity_end_scroll_reaches_the_last_word_wrapped_line() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tab = Tab::Activity;
        app.activity = vec!["abcdefghijk ".repeat(5); 8];
        app.activity.push("BOTTOM_MARKER".to_owned());
        app.activity_scroll = usize::MAX;
        let backend = TestBackend::new(32, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render activity at end");

        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("BOTTOM_MARKER"), "screen: {screen}");
    }

    #[test]
    fn job_results_expand_in_jobs_without_switching_to_activity() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let store = JobStore::new(state.clone()).expect("job store");
        let tool = crate::config::Tool::custom(
            "result-test",
            "Write-Output".to_owned(),
            vec!["done".to_owned()],
        );
        let job = crate::job::Job::from_tool(
            "result-test".to_owned(),
            tool,
            temporary.path().to_path_buf(),
        );
        let job_id = job.id.clone();
        store.save(&job).expect("save job");
        store
            .append_log(&job_id, b"job output\n")
            .expect("append log");
        let mut app = App::new(state, None).expect("app");
        app.tab = Tab::Jobs;
        app.job_index = app
            .jobs
            .iter()
            .position(|item| item.id == job_id)
            .expect("job row");
        app.job_hitboxes = vec![(Rect::new(1, 4, 50, 1), app.job_index)];

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert_eq!(app.tab, Tab::Jobs);
        assert_eq!(app.expanded_job.as_deref(), Some(job_id.as_str()));
        assert_eq!(app.job_log, ["job output"]);

        handle_jobs_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tab, Tab::Jobs);
        assert!(app.expanded_job.is_none());
    }

    #[test]
    fn completed_background_job_reprobes_the_tool_version() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let store = JobStore::new(state.clone()).expect("job store");
        let tool = Config::starter()
            .tools
            .remove("rustup")
            .expect("rustup preset");
        let mut job =
            crate::job::Job::from_tool("rustup".to_owned(), tool, temporary.path().to_path_buf());
        let job_id = job.id.clone();
        store.save(&job).expect("save pending job");
        let mut app = App::new(state, None).expect("app");
        let index = app
            .tools
            .iter()
            .position(|tool| tool.name == "rustup")
            .expect("rustup row");
        let previous_probe = app.tools[index].version_probe_id;

        job.set_status(JobStatus::Succeeded { exit_code: 0 });
        store.save(&job).expect("complete job");
        app.refresh_jobs().expect("refresh completed job");

        assert!(app.tools[index].version_probe_id > previous_probe);
        assert!(matches!(app.tools[index].version, VersionState::Loading));
        assert!(app.jobs.iter().any(|job| job.id == job_id));
    }

    #[test]
    fn wraps_navigation_indices() {
        assert_eq!(previous_index(0, 3), 2);
        assert_eq!(next_index(2, 3), 0);
        assert_eq!(next_index(0, 0), 0);
    }

    #[test]
    fn tabs_move_in_both_directions() {
        assert_eq!(Tab::Tools.next(), Tab::Activity);
        assert_eq!(Tab::Tools.previous(), Tab::Doctor);
        assert_eq!(Tab::Jobs.next(), Tab::Doctor);
        assert_eq!(Tab::Jobs.previous(), Tab::Activity);
        assert_eq!(Tab::Doctor.next(), Tab::Tools);
        assert_eq!(Tab::Doctor.previous(), Tab::Jobs);
    }

    #[test]
    fn clicking_tab_titles_switches_views() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render tabs");
        assert_eq!(app.tab_hitboxes.len(), 4);

        for (index, expected) in [Tab::Tools, Tab::Activity, Tab::Jobs, Tab::Doctor]
            .into_iter()
            .enumerate()
        {
            let area = app.tab_hitboxes[index].0;
            handle_mouse(
                &mut app,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                },
            );
            assert_eq!(app.tab, expected);
        }
    }

    #[test]
    fn ctrl_c_quits_and_number_keys_do_not_switch_tabs() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let plain_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        let shift_backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        let shifted_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        let ctrl_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL);

        assert!(is_ctrl_c(&ctrl_c));
        assert!(!is_ctrl_c(&plain_c));
        assert!(is_shift_tab(&shift_backtab));
        assert!(is_shift_tab(&shifted_tab));
        assert!(!is_shift_tab(&ctrl_tab));
        assert_eq!(ProcessStrategy::Wait.toggle(), ProcessStrategy::Terminate);
        assert_eq!(ProcessStrategy::Terminate.toggle(), ProcessStrategy::Wait);
        assert_eq!(
            navigated_tab(Tab::Tools, &KeyCode::Right),
            Some(Tab::Activity)
        );
        assert_eq!(navigated_tab(Tab::Tools, &KeyCode::Left), Some(Tab::Doctor));
        for code in [
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Char('h'),
            KeyCode::Char('H'),
            KeyCode::Char('l'),
            KeyCode::Char('L'),
            KeyCode::Char('1'),
            KeyCode::Char('2'),
            KeyCode::Char('3'),
        ] {
            assert_eq!(navigated_tab(Tab::Tools, &code), None);
        }
    }

    #[test]
    fn terminate_toggle_updates_existing_pending_jobs() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let store = JobStore::new(state.clone()).expect("job store");
        let tool = crate::config::Tool::custom(
            "policy-test",
            "Write-Output".to_owned(),
            vec!["done".to_owned()],
        );
        let job = crate::job::Job::from_tool(
            "policy-test".to_owned(),
            tool,
            temporary.path().to_path_buf(),
        );
        let job_id = job.id.clone();
        store.save(&job).expect("save pending job");
        let running_tool = crate::config::Tool::custom(
            "running-test",
            "Write-Output".to_owned(),
            vec!["running".to_owned()],
        );
        let mut running_job = crate::job::Job::from_tool(
            "running-test".to_owned(),
            running_tool,
            temporary.path().to_path_buf(),
        );
        running_job.set_status(JobStatus::Running { attempt: 1 });
        let running_job_id = running_job.id.clone();
        store.save(&running_job).expect("save running job");
        let mut app = App::new(state, None).expect("app");

        toggle_process_strategy(&mut app);

        let updated = store.load(&job_id).expect("updated job");
        assert_eq!(app.process_strategy, ProcessStrategy::Terminate);
        assert_eq!(
            updated.process_rules[0].action,
            crate::config::ProcessAction::Terminate
        );
        assert_eq!(
            store
                .load(&running_job_id)
                .expect("running job")
                .process_rules[0]
                .action,
            crate::config::ProcessAction::Wait
        );
        assert!(app.message.contains("1 active job(s)"));
    }

    #[test]
    fn terminate_restarts_waiting_job_even_when_rules_are_already_terminate() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let store = JobStore::new(state.clone()).expect("job store");
        let mut job = crate::job::Job::from_tool(
            "policy-test".to_owned(),
            crate::config::Tool::custom(
                "policy-test",
                "Write-Output".to_owned(),
                vec!["done".to_owned()],
            ),
            temporary.path().to_path_buf(),
        );
        job.process_rules[0].action = crate::config::ProcessAction::Terminate;
        job.set_status(JobStatus::WaitingForLocks {
            processes: vec![crate::job::LockingProcess {
                pid: u32::MAX,
                name: "policy-test.exe".to_owned(),
                start_time: 1,
            }],
        });
        let job_id = job.id.clone();
        store.save(&job).expect("save waiting job");
        let mut app = App::new(state, None).expect("app");
        let mut ensured = Vec::new();

        let summary = app
            .terminate_active_job_waits_with(|job, _| {
                ensured.push(job.id.clone());
                Ok(detach::WorkerLaunch::Spawned)
            })
            .expect("terminate waiting jobs");

        assert_eq!(summary, (1, 0, 0, 1, 0));
        assert_eq!(ensured.as_slice(), std::slice::from_ref(&job_id));
        assert!(matches!(
            store.load(&job_id).expect("recovered job").status,
            JobStatus::Pending
        ));
    }

    #[test]
    fn terminate_restarts_job_left_in_terminating_state() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let store = JobStore::new(state.clone()).expect("job store");
        let mut job = crate::job::Job::from_tool(
            "terminating-test".to_owned(),
            crate::config::Tool::custom(
                "terminating-test",
                "Write-Output".to_owned(),
                vec!["done".to_owned()],
            ),
            temporary.path().to_path_buf(),
        );
        job.process_rules[0].action = crate::config::ProcessAction::Terminate;
        job.set_status(JobStatus::TerminatingProcesses {
            processes: vec![crate::job::LockingProcess {
                pid: u32::MAX,
                name: "terminating-test.exe".to_owned(),
                start_time: 1,
            }],
        });
        let job_id = job.id.clone();
        store.save(&job).expect("save terminating job");
        let mut app = App::new(state, None).expect("app");
        let mut ensured = Vec::new();

        let summary = app
            .terminate_active_job_waits_with(|job, _| {
                ensured.push(job.id.clone());
                Ok(detach::WorkerLaunch::Spawned)
            })
            .expect("recover terminating jobs");

        assert_eq!(summary, (1, 0, 0, 1, 0));
        assert_eq!(ensured.as_slice(), std::slice::from_ref(&job_id));
        assert!(matches!(
            store.load(&job_id).expect("recovered job").status,
            JobStatus::Pending
        ));
    }

    #[test]
    fn builds_update_subprocess_arguments() {
        assert_eq!(
            update_arguments("claude", Some(Path::new("custom.toml")), false),
            [
                "update",
                "--background",
                "auto",
                "--config",
                "custom.toml",
                "claude"
            ]
        );
        assert_eq!(
            update_arguments("claude", None, true),
            [
                "update",
                "--background",
                "auto",
                "--terminate-locking-processes",
                "claude"
            ]
        );
    }

    #[test]
    fn adding_a_command_only_builds_a_save_operation() {
        let arguments = add_arguments(
            "sentinel",
            vec!["Write-Output".to_owned(), "must-not-run".to_owned()],
            false,
        );

        assert_eq!(
            arguments,
            ["add", "sentinel", "Write-Output", "must-not-run"]
        );
        assert!(!arguments.iter().any(|argument| argument == "update"));
        assert!(
            Operation::Add
                .completion_message("sentinel", true, Duration::ZERO, 0, Language::English)
                .contains("has not been run")
        );
        assert!(
            Operation::Add
                .completion_message("sentinel", true, Duration::ZERO, 0, Language::Chinese)
                .contains("尚未执行")
        );

        assert_eq!(
            add_arguments(
                "sentinel",
                vec!["Write-Output".to_owned(), "replacement".to_owned()],
                true,
            ),
            ["add", "--force", "sentinel", "Write-Output", "replacement"]
        );
        assert!(
            Operation::Edit
                .completion_message("sentinel", true, Duration::ZERO, 0, Language::English)
                .contains("Updated")
        );
        assert_eq!(
            edit_arguments(
                "sentinel",
                "renamed",
                vec!["Write-Output".to_owned(), "replacement".to_owned()],
            ),
            ["edit", "sentinel", "renamed", "Write-Output", "replacement"]
        );
    }

    #[test]
    fn classifies_only_the_final_cli_status_as_queued() {
        assert!(output_was_queued(
            "example",
            "some output\nqueued example: waiting\njob: 123"
        ));
        assert!(!output_was_queued(
            "example",
            "queued example: text from the tool\nupdated example: command"
        ));
        assert!(!output_was_queued(
            "example",
            "the word queued in ordinary output\nupdated example: command"
        ));
    }

    #[test]
    fn colors_activity_lines_by_outcome() {
        assert_eq!(activity_tone(">>> starting bun"), ActivityTone::Start);
        assert_eq!(
            activity_tone("=== update codex: QUEUED ==="),
            ActivityTone::Queued
        );
        assert_eq!(
            activity_tone("queued codex: waiting on process policy: codex.exe"),
            ActivityTone::Queued
        );
        assert_eq!(
            activity_tone("=== update bun: FAILED ==="),
            ActivityTone::Error
        );
        assert_eq!(
            activity_tone("Bun upgrade failed with error: HTTPForbidden"),
            ActivityTone::Error
        );
        assert_eq!(
            activity_tone("error: bun failed (exit code 1)"),
            ActivityTone::Error
        );
        assert_eq!(
            activity_tone("Please upgrade manually:"),
            ActivityTone::Hint
        );
        assert_eq!(activity_tone("job: 123"), ActivityTone::Metadata);
        assert_eq!(
            activity_tone("inspect: dvup jobs 123 --log"),
            ActivityTone::Metadata
        );
        assert_eq!(
            activity_tone("=== update rustup: OK ==="),
            ActivityTone::Success
        );
        assert_eq!(
            activity_outcome_label(true, true, Language::English),
            "QUEUED"
        );
        assert_eq!(activity_outcome_label(true, false, Language::English), "OK");
        assert_eq!(
            activity_outcome_label(false, false, Language::English),
            "FAILED"
        );
        assert_eq!(
            activity_style("=== update bun: FAILED ===").fg,
            Some(ERROR_COLOR)
        );
        assert_eq!(
            activity_style("=== update codex: QUEUED ===").fg,
            Some(WARNING_COLOR)
        );
        assert_eq!(
            activity_style("=== update rustup: OK ===").fg,
            Some(SUCCESS)
        );
        assert_eq!(
            activity_tone("=== 更新 codex: 已排队 ==="),
            ActivityTone::Queued
        );
        assert_eq!(activity_tone("=== 更新 bun: 失败 ==="), ActivityTone::Error);
        assert_eq!(
            activity_tone("=== 更新 rustup: 成功 ==="),
            ActivityTone::Success
        );
        assert_eq!(activity_tone("进程策略：等待"), ActivityTone::Hint);
        assert_eq!(activity_tone("任务：123"), ActivityTone::Metadata);
        assert_eq!(
            activity_tone("=== 完成：2 项已更新，0 项已排队，0 项失败 ==="),
            ActivityTone::Success
        );
        assert_eq!(
            activity_tone("=== 完成：1 项已更新，0 项已排队，1 项失败 ==="),
            ActivityTone::Error
        );
        assert_eq!(
            activity_outcome_label(true, true, Language::Chinese),
            "已排队"
        );
    }
}
