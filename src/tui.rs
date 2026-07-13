use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

#[cfg(not(test))]
use std::sync::{Arc, Mutex};

use crossterm::{
    cursor::Show,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Gauge, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table, TableState, Tabs, Wrap,
    },
};
use zeroize::Zeroize;

use crate::{
    cli, command,
    config::{
        GithubReleaseMonitor, LatestVersionSource, ReleaseAssetFormat, ReleaseUpdatePolicy, Tool,
        UserConfig, UserTool,
    },
    credential, datetime, detach, doctor,
    error::{Error, Result},
    job::{CommandSpec, JobStatus, JobStore},
    release::{self, MonitorOutcome, MonitorStatus},
    settings::{AppSettings, Language, NetworkSettings, ProxyMode},
    state::StateDirs,
    version, worker,
};

#[cfg(test)]
use crate::config::Config;

const TICK_RATE: Duration = Duration::from_millis(100);
const ACTIVE_JOB_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_JOB_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const GITHUB_RATE_LIMIT_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
#[cfg(not(test))]
const MAX_CONCURRENT_TUI_PROBES: usize = 2;
const MOUSE_WHEEL_ROWS: isize = 1;
const SETTINGS_ROW_COUNT: usize = 8;
const GITHUB_MONITOR_FORM_FIELD_COUNT: usize = 12;
const DEFAULT_MONITOR_MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MONITOR_MAX_EXTRACTED_BYTES: u64 = 2_048 * 1024 * 1024;
const DEFAULT_MONITOR_MAX_EXTRACTED_FILES: usize = 10_000;
const MAX_ACTIVITY_LINES: usize = 1_000;
const TOML_HISTORY_LIMIT: usize = 100;
const TOML_HISTORY_BYTE_LIMIT: usize = 8 * 1024 * 1024;
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
const TOML_KEY: Color = ACCENT;
const TOML_STRING: Color = Color::Rgb(134, 239, 172);
const TOML_NUMBER: Color = Color::Rgb(251, 191, 36);
const TOML_BOOLEAN: Color = Color::Rgb(232, 121, 249);
const TOML_DATE_TIME: Color = Color::Rgb(96, 165, 250);
const TOML_COMMENT: Color = SUBTLE;

/// Runs the interactive terminal interface.
pub fn run(
    state: StateDirs,
    config_path: Option<PathBuf>,
    editor_path: Option<PathBuf>,
) -> Result<u8> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(Error::Message(
            "the TUI requires an interactive terminal; use `dvup list` or `dvup update` in scripts"
                .to_owned(),
        ));
    }

    enable_raw_mode()?;
    let mut restore = TerminalRestore { armed: true };
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let app_result = terminal.clear().map_err(Error::from).and_then(|()| {
        let direct_editor = editor_path.is_some();
        let mut app = App::new_loading(state, if direct_editor { None } else { config_path })?;
        if let Some(path) = editor_path {
            app.open_toml_file(path)?;
        }
        run_app(&mut terminal, &mut app)
    });

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
    let screen_result = execute!(
        stdout,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
    raw_result.and(screen_result)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Availability {
    Installed,
    Missing,
    UpdaterMissing,
    Unsupported,
}

impl Availability {
    fn allows_version_checks(self) -> bool {
        matches!(self, Self::Installed | Self::UpdaterMissing)
    }

    fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Installed, Language::English) => "installed",
            (Self::Installed, Language::Chinese) => "已安装",
            (Self::Missing, Language::English) => "missing",
            (Self::Missing, Language::Chinese) => "未安装",
            (Self::UpdaterMissing, Language::English) => "no updater",
            (Self::UpdaterMissing, Language::Chinese) => "缺少更新器",
            (Self::Unsupported, Language::English) => "unsupported",
            (Self::Unsupported, Language::Chinese) => "不支持",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Installed => Style::default().fg(SUCCESS),
            Self::Missing => Style::default().fg(SUBTLE),
            Self::UpdaterMissing => Style::default().fg(ERROR_COLOR),
            Self::Unsupported => Style::default().fg(WARNING_COLOR),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunState {
    Idle,
    Running,
    UpToDate,
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
            (Self::UpToDate, Language::English) => "up to date",
            (Self::UpToDate, Language::Chinese) => "已是最新",
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
            Self::UpToDate | Self::Updated => Style::default().fg(SUCCESS),
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum VersionState {
    Loading,
    Available(String),
    Failed(version::LatestVersionError),
    Unavailable,
}

impl VersionState {
    fn label(&self) -> &str {
        match self {
            Self::Loading => "…",
            Self::Available(version) => version,
            Self::Failed(_) => "!",
            Self::Unavailable => "—",
        }
    }

    fn style(&self) -> Style {
        match self {
            Self::Available(_) => Style::default().fg(ACCENT),
            Self::Failed(error) if error.kind == version::LatestVersionErrorKind::RateLimited => {
                Style::default().fg(WARNING_COLOR)
            }
            Self::Failed(_) => Style::default().fg(ERROR_COLOR),
            Self::Loading | Self::Unavailable => Style::default().fg(SUBTLE),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolKind {
    BuiltIn,
    Custom,
}

impl ToolKind {
    fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::BuiltIn, Language::English) => "built-in",
            (Self::BuiltIn, Language::Chinese) => "内置",
            (Self::Custom, Language::English) => "custom",
            (Self::Custom, Language::Chinese) => "自定义",
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
    latest_version: VersionState,
    latest_source: Option<LatestVersionSource>,
    latest_probe_id: u64,
    supports_target_version: bool,
    availability: Availability,
    kind: ToolKind,
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

#[derive(Clone, Copy, Debug)]
struct TomlEditorHitbox {
    area: Rect,
}

#[derive(Clone, Copy, Debug, Default)]
struct ListViewport {
    area: Option<Rect>,
    length: usize,
    offset: usize,
}

impl ListViewport {
    fn offset(self) -> usize {
        self.offset
    }

    fn update(&mut self, area: Rect, length: usize, offset: usize) {
        let maximum = length.saturating_sub(usize::from(area.height));
        self.area = Some(area);
        self.length = length;
        self.offset = offset.min(maximum);
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn scroll_at(&mut self, column: u16, row: u16, delta: isize) -> Option<usize> {
        let area = self.area.filter(|area| {
            contains(Some(*area), column, row) && area.height > 0 && self.length > 0
        })?;
        let maximum = self.length.saturating_sub(usize::from(area.height));
        self.offset = self.offset.saturating_add_signed(delta).min(maximum);
        let slot = usize::from(row.saturating_sub(area.y));
        Some(
            self.offset
                .saturating_add(slot)
                .min(self.length.saturating_sub(1)),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tab {
    Tools,
    Activity,
    Jobs,
    Doctor,
    Settings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolView {
    Commands,
    Github,
}

impl ToolView {
    fn toggle(self) -> Self {
        match self {
            Self::Commands => Self::Github,
            Self::Github => Self::Commands,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Commands => 0,
            Self::Github => 1,
        }
    }
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
            Self::Doctor => Self::Settings,
            Self::Settings => Self::Tools,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Tools => Self::Settings,
            Self::Activity => Self::Tools,
            Self::Jobs => Self::Activity,
            Self::Doctor => Self::Jobs,
            Self::Settings => Self::Doctor,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Tools => 0,
            Self::Activity => 1,
            Self::Jobs => 2,
            Self::Doctor => 3,
            Self::Settings => 4,
        }
    }

    fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Tools),
            1 => Some(Self::Activity),
            2 => Some(Self::Jobs),
            3 => Some(Self::Doctor),
            4 => Some(Self::Settings),
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

    fn insert_text(&mut self, text: &str) {
        self.delete_selection();
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
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

#[derive(Clone, Copy, Debug)]
struct TomlHighlight {
    start: usize,
    end: usize,
    style: Style,
}

#[derive(Clone, Debug)]
struct TomlEditorSnapshot {
    text: String,
    cursor: usize,
    selection_anchor: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TomlCommentAction {
    Commented,
    Uncommented,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TomlEditorMode {
    Standard,
    VimNormal,
    VimInsert,
    VimVisual,
}

impl TomlEditorMode {
    fn is_vim(self) -> bool {
        self != Self::Standard
    }

    fn label(self) -> &'static str {
        match self {
            Self::Standard => "STANDARD",
            Self::VimNormal => "VIM NORMAL",
            Self::VimInsert => "VIM INSERT",
            Self::VimVisual => "VIM VISUAL",
        }
    }
}

#[derive(Clone, Debug)]
struct TomlEditor {
    path: PathBuf,
    text: String,
    mode: TomlEditorMode,
    vim_pending: Option<char>,
    vim_register: String,
    vim_register_linewise: bool,
    cursor: usize,
    selection_anchor: Option<usize>,
    scroll_y: usize,
    scroll_x: usize,
    preferred_column: Option<usize>,
    follow_cursor: bool,
    dirty: bool,
    saved_text: String,
    revision: u64,
    highlighted_revision: u64,
    highlights: Vec<TomlHighlight>,
    undo_stack: Vec<TomlEditorSnapshot>,
    redo_stack: Vec<TomlEditorSnapshot>,
}

impl TomlEditor {
    fn new(path: PathBuf, text: String) -> Self {
        let saved_text = text.clone();
        Self {
            path,
            text,
            mode: TomlEditorMode::Standard,
            vim_pending: None,
            vim_register: String::new(),
            vim_register_linewise: false,
            cursor: 0,
            selection_anchor: None,
            scroll_y: 0,
            scroll_x: 0,
            preferred_column: None,
            follow_cursor: true,
            dirty: false,
            saved_text,
            revision: 0,
            highlighted_revision: u64::MAX,
            highlights: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    fn snapshot(&self) -> TomlEditorSnapshot {
        TomlEditorSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
        }
    }

    fn push_history(stack: &mut Vec<TomlEditorSnapshot>, snapshot: TomlEditorSnapshot) {
        stack.push(snapshot);
        let mut total_bytes = stack
            .iter()
            .map(|snapshot| snapshot.text.len())
            .sum::<usize>();
        while stack.len() > TOML_HISTORY_LIMIT
            || (stack.len() > 1 && total_bytes > TOML_HISTORY_BYTE_LIMIT)
        {
            total_bytes = total_bytes.saturating_sub(stack[0].text.len());
            stack.remove(0);
        }
    }

    fn begin_edit(&mut self) {
        let snapshot = self.snapshot();
        Self::push_history(&mut self.undo_stack, snapshot);
        self.redo_stack.clear();
    }

    fn finish_edit(&mut self) {
        self.preferred_column = None;
        self.follow_cursor = true;
        self.dirty = self.text != self.saved_text;
        self.revision = self.revision.wrapping_add(1);
    }

    fn restore_snapshot(&mut self, snapshot: TomlEditorSnapshot) {
        self.text = snapshot.text;
        self.cursor = snapshot.cursor;
        self.selection_anchor = snapshot.selection_anchor;
        self.dirty = self.text != self.saved_text;
        self.preferred_column = None;
        self.follow_cursor = true;
        self.revision = self.revision.wrapping_add(1);
    }

    fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo_stack.pop() else {
            return false;
        };
        let current = self.snapshot();
        Self::push_history(&mut self.redo_stack, current);
        self.restore_snapshot(snapshot);
        true
    }

    fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo_stack.pop() else {
            return false;
        };
        let current = self.snapshot();
        Self::push_history(&mut self.undo_stack, current);
        self.restore_snapshot(snapshot);
        true
    }

    fn mark_saved(&mut self) {
        self.saved_text.clone_from(&self.text);
        self.dirty = false;
    }

    fn toggle_vim_mode(&mut self) {
        self.mode = if self.mode.is_vim() {
            TomlEditorMode::Standard
        } else {
            TomlEditorMode::VimNormal
        };
        self.vim_pending = None;
        self.clear_selection();
    }

    fn enter_vim_mode(&mut self, mode: TomlEditorMode) {
        self.mode = mode;
        self.vim_pending = None;
        if mode != TomlEditorMode::VimVisual {
            self.clear_selection();
        }
    }

    fn line_ranges(&self) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        let mut start = 0;
        for (index, character) in self.text.char_indices() {
            if character == '\n' {
                ranges.push((start, index));
                start = index + 1;
            }
        }
        ranges.push((start, self.text.len()));
        ranges
    }

    fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        (anchor != self.cursor).then_some(if anchor < self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(start, end)| &self.text[start..end])
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    fn select_all(&mut self) {
        self.selection_anchor = (!self.text.is_empty()).then_some(0);
        self.cursor = self.text.len();
        self.preferred_column = None;
        self.follow_cursor = true;
    }

    fn cursor_line_column(&self) -> (usize, usize) {
        let ranges = self.line_ranges();
        let line = ranges
            .iter()
            .position(|(_, end)| self.cursor <= *end)
            .unwrap_or_else(|| ranges.len().saturating_sub(1));
        let (start, end) = ranges[line];
        let cursor = self.cursor.min(end);
        (line, display_width(&self.text[start..cursor]))
    }

    fn byte_at_column(&self, line: usize, column: usize) -> usize {
        let ranges = self.line_ranges();
        let (start, end) = ranges[line.min(ranges.len().saturating_sub(1))];
        let mut width = 0;
        for (offset, character) in self.text[start..end].char_indices() {
            let character_width = display_width(&character.to_string());
            if column < width + character_width.div_ceil(2) {
                return start + offset;
            }
            width += character_width;
            if column < width {
                return start + offset + character.len_utf8();
            }
        }
        end
    }

    fn move_to(&mut self, target: usize, extend_selection: bool) {
        if extend_selection {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.clear_selection();
        }
        self.cursor = target;
        self.follow_cursor = true;
        if self.selection_anchor == Some(self.cursor) {
            self.clear_selection();
        }
    }

    fn move_left(&mut self, extend_selection: bool) {
        self.preferred_column = None;
        if !extend_selection && let Some((start, _)) = self.selection() {
            self.move_to(start, false);
            return;
        }
        let target = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.move_to(target, extend_selection);
    }

    fn move_right(&mut self, extend_selection: bool) {
        self.preferred_column = None;
        if !extend_selection && let Some((_, end)) = self.selection() {
            self.move_to(end, false);
            return;
        }
        let target = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(offset, _)| self.cursor + offset)
            .unwrap_or(self.text.len());
        self.move_to(target, extend_selection);
    }

    fn move_vertical(&mut self, delta: isize, extend_selection: bool) {
        let (line, column) = self.cursor_line_column();
        let column = *self.preferred_column.get_or_insert(column);
        let last_line = self.line_ranges().len().saturating_sub(1);
        let target_line = line.saturating_add_signed(delta).min(last_line);
        let target = self.byte_at_column(target_line, column);
        self.move_to(target, extend_selection);
    }

    fn move_home(&mut self, document: bool, extend_selection: bool) {
        self.preferred_column = None;
        let target = if document {
            0
        } else {
            let (line, _) = self.cursor_line_column();
            self.line_ranges()[line].0
        };
        self.move_to(target, extend_selection);
    }

    fn move_end(&mut self, document: bool, extend_selection: bool) {
        self.preferred_column = None;
        let target = if document {
            self.text.len()
        } else {
            let (line, _) = self.cursor_line_column();
            self.line_ranges()[line].1
        };
        self.move_to(target, extend_selection);
    }

    fn move_word_forward(&mut self, extend_selection: bool) {
        if self.cursor >= self.text.len() {
            return;
        }
        let characters = self.text[self.cursor..].char_indices().collect::<Vec<_>>();
        let first_class = vim_word_class(characters[0].1);
        let mut target = self.text.len();
        let mut index = 0;
        while index < characters.len() && vim_word_class(characters[index].1) == first_class {
            index += 1;
        }
        if first_class != 0 {
            while index < characters.len() && vim_word_class(characters[index].1) == 0 {
                index += 1;
            }
        }
        if let Some((offset, _)) = characters.get(index) {
            target = self.cursor + offset;
        }
        self.preferred_column = None;
        self.move_to(target, extend_selection);
    }

    fn move_word_backward(&mut self, extend_selection: bool) {
        if self.cursor == 0 {
            return;
        }
        let characters = self.text[..self.cursor].char_indices().collect::<Vec<_>>();
        let mut index = characters.len();
        while index > 0 && vim_word_class(characters[index - 1].1) == 0 {
            index -= 1;
        }
        if index == 0 {
            self.move_to(0, extend_selection);
            return;
        }
        let class = vim_word_class(characters[index - 1].1);
        while index > 0 && vim_word_class(characters[index - 1].1) == class {
            index -= 1;
        }
        let target = characters
            .get(index)
            .map(|(offset, _)| *offset)
            .unwrap_or(0);
        self.preferred_column = None;
        self.move_to(target, extend_selection);
    }

    fn select_current_line(&mut self) {
        let (line, _) = self.cursor_line_column();
        let (start, end) = self.line_ranges()[line];
        self.selection_anchor = Some(start);
        self.cursor = end;
        self.preferred_column = None;
        self.follow_cursor = true;
    }

    fn vim_yank_current_line(&mut self) {
        let (line, _) = self.cursor_line_column();
        let (start, end) = self.line_ranges()[line];
        self.vim_register = format!("{}\n", &self.text[start..end]);
        self.vim_register_linewise = true;
    }

    fn vim_yank_selection(&mut self) -> bool {
        let Some(selected) = self.selected_text().map(str::to_owned) else {
            return false;
        };
        self.vim_register = selected;
        self.vim_register_linewise = false;
        self.enter_vim_mode(TomlEditorMode::VimNormal);
        true
    }

    fn vim_delete_current_line(&mut self) {
        let (line, _) = self.cursor_line_column();
        let (start, end) = self.line_ranges()[line];
        self.vim_register = format!("{}\n", &self.text[start..end]);
        self.vim_register_linewise = true;
        let (remove_start, remove_end) = if end < self.text.len() {
            (start, end + 1)
        } else if start > 0 {
            (start - 1, end)
        } else {
            (start, end)
        };
        if remove_start == remove_end {
            return;
        }
        self.begin_edit();
        self.text.replace_range(remove_start..remove_end, "");
        self.cursor = remove_start.min(self.text.len());
        self.clear_selection();
        self.finish_edit();
    }

    fn vim_delete_selection(&mut self, insert_after: bool) -> bool {
        let Some(selected) = self.selected_text().map(str::to_owned) else {
            return false;
        };
        self.vim_register = selected;
        self.vim_register_linewise = false;
        self.begin_edit();
        self.delete_selection();
        self.finish_edit();
        self.enter_vim_mode(if insert_after {
            TomlEditorMode::VimInsert
        } else {
            TomlEditorMode::VimNormal
        });
        true
    }

    fn vim_paste(&mut self) {
        if self.vim_register.is_empty() {
            return;
        }
        if self.vim_register_linewise {
            let (line, _) = self.cursor_line_column();
            let (_, end) = self.line_ranges()[line];
            self.clear_selection();
            if end < self.text.len() {
                self.cursor = end + 1;
                let register = self.vim_register.clone();
                self.insert_text(&register);
            } else {
                self.cursor = end;
                let register = self.vim_register.trim_end_matches('\n').to_owned();
                self.insert_text(&format!("\n{register}"));
            }
        } else {
            self.move_right(false);
            let register = self.vim_register.clone();
            self.insert_text(&register);
        }
        self.enter_vim_mode(TomlEditorMode::VimNormal);
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.clear_selection();
        true
    }

    fn insert_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        self.begin_edit();
        self.delete_selection();
        self.text.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.finish_edit();
    }

    fn backspace(&mut self) {
        self.preferred_column = None;
        if self.selection().is_some() {
            self.begin_edit();
            self.delete_selection();
            self.finish_edit();
            return;
        }
        if self.cursor == 0 {
            return;
        }
        self.begin_edit();
        let start = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.finish_edit();
    }

    fn delete(&mut self) {
        self.preferred_column = None;
        if self.selection().is_some() {
            self.begin_edit();
            self.delete_selection();
            self.finish_edit();
            return;
        }
        if self.cursor == self.text.len() {
            return;
        }
        self.begin_edit();
        let end = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(offset, _)| self.cursor + offset)
            .unwrap_or(self.text.len());
        self.text.replace_range(self.cursor..end, "");
        self.finish_edit();
    }

    fn toggle_line_comments(&mut self) -> Option<TomlCommentAction> {
        let ranges = self.line_ranges();
        let selection = self.selection();
        let (selection_start, selection_end) = selection.unwrap_or((self.cursor, self.cursor));
        let first_line = ranges
            .iter()
            .position(|(_, end)| selection_start <= *end)
            .unwrap_or_else(|| ranges.len().saturating_sub(1));
        let end_probe = if selection.is_some() && selection_end > selection_start {
            selection_end.saturating_sub(1)
        } else {
            selection_end
        };
        let last_line = ranges
            .iter()
            .position(|(_, end)| end_probe <= *end)
            .unwrap_or_else(|| ranges.len().saturating_sub(1));
        let selected_ranges = &ranges[first_line..=last_line];

        let line_info = selected_ranges
            .iter()
            .map(|&(start, end)| {
                let line = &self.text[start..end];
                let indentation = line
                    .char_indices()
                    .take_while(|(_, character)| matches!(character, ' ' | '\t'))
                    .map(|(offset, character)| offset + character.len_utf8())
                    .last()
                    .unwrap_or(0);
                let content = &line[indentation..];
                (start, end, indentation, content)
            })
            .collect::<Vec<_>>();
        let has_nonblank = line_info
            .iter()
            .any(|(_, _, _, content)| !content.is_empty());
        let uncomment = has_nonblank
            && line_info
                .iter()
                .filter(|(_, _, _, content)| !content.is_empty())
                .all(|(_, _, _, content)| content.starts_with('#'));
        let comment_single_blank = line_info.len() == 1;

        let mut edits = Vec::new();
        for &(start, _, indentation, content) in &line_info {
            let marker = start + indentation;
            if uncomment {
                if let Some(after_hash) = content.strip_prefix('#') {
                    let remove = if after_hash.starts_with(' ') { 2 } else { 1 };
                    edits.push((marker, marker + remove, String::new()));
                }
            } else if !content.is_empty() || comment_single_blank {
                edits.push((marker, marker, "# ".to_owned()));
            }
        }
        if edits.is_empty() {
            return None;
        }

        let original_cursor = self.cursor;
        let original_anchor = self.selection_anchor;
        let block_start = selected_ranges[0].0;
        let block_end = selected_ranges[selected_ranges.len() - 1].1;
        let delta = edits
            .iter()
            .fold(0isize, |total, (start, end, replacement)| {
                total + replacement.len() as isize - (*end - *start) as isize
            });
        let mapped_cursor = map_toml_edit_position(original_cursor, &edits);

        self.begin_edit();
        for (start, end, replacement) in edits.iter().rev() {
            self.text.replace_range(*start..*end, replacement);
        }
        if let Some(anchor) = original_anchor {
            let new_block_end = block_end.saturating_add_signed(delta);
            if anchor <= original_cursor {
                self.selection_anchor = Some(block_start);
                self.cursor = new_block_end;
            } else {
                self.selection_anchor = Some(new_block_end);
                self.cursor = block_start;
            }
        } else {
            self.selection_anchor = None;
            self.cursor = mapped_cursor;
        }
        self.finish_edit();

        Some(if uncomment {
            TomlCommentAction::Uncommented
        } else {
            TomlCommentAction::Commented
        })
    }

    fn ensure_cursor_visible(&mut self, height: usize, width: usize) {
        if height == 0 || width == 0 {
            return;
        }
        let (line, column) = self.cursor_line_column();
        if line < self.scroll_y {
            self.scroll_y = line;
        } else if line >= self.scroll_y + height {
            self.scroll_y = line + 1 - height;
        }
        if column < self.scroll_x {
            self.scroll_x = column;
        } else if column >= self.scroll_x + width {
            self.scroll_x = column + 1 - width;
        }
    }

    fn scroll_vertical(&mut self, delta: isize, height: usize) {
        let max_scroll = self.line_ranges().len().saturating_sub(height.max(1));
        self.scroll_y = self.scroll_y.saturating_add_signed(delta).min(max_scroll);
        self.follow_cursor = false;
    }

    fn refresh_highlights(&mut self) {
        if self.highlighted_revision == self.revision {
            return;
        }

        let syntax = taplo::parser::parse(&self.text).into_syntax();
        self.highlights = syntax
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
            .map(|token| {
                use taplo::syntax::SyntaxKind;

                let kind = token.kind();
                let is_key = token.parent().is_some_and(|parent| {
                    parent
                        .ancestors()
                        .any(|ancestor| ancestor.kind() == SyntaxKind::KEY)
                });
                let style = toml_token_style(kind, is_key);
                let range = token.text_range();
                TomlHighlight {
                    start: u32::from(range.start()) as usize,
                    end: u32::from(range.end()) as usize,
                    style,
                }
            })
            .collect();
        self.highlighted_revision = self.revision;
    }
}

fn vim_word_class(character: char) -> u8 {
    if character.is_whitespace() {
        0
    } else if character.is_alphanumeric() || character == '_' {
        1
    } else {
        2
    }
}

fn map_toml_edit_position(position: usize, edits: &[(usize, usize, String)]) -> usize {
    let mut delta = 0isize;
    for (start, end, replacement) in edits {
        if position < *start {
            break;
        }
        if position <= *end {
            return start
                .saturating_add_signed(delta)
                .saturating_add(replacement.len());
        }
        delta += replacement.len() as isize - (*end - *start) as isize;
    }
    position.saturating_add_signed(delta)
}

fn toml_token_style(kind: taplo::syntax::SyntaxKind, is_key: bool) -> Style {
    use taplo::syntax::SyntaxKind;

    let base = Style::default().fg(Color::White).bg(SURFACE);
    if is_key
        && matches!(
            kind,
            SyntaxKind::IDENT
                | SyntaxKind::IDENT_WITH_GLOB
                | SyntaxKind::STRING
                | SyntaxKind::MULTI_LINE_STRING
                | SyntaxKind::STRING_LITERAL
                | SyntaxKind::MULTI_LINE_STRING_LITERAL
                | SyntaxKind::PERIOD
        )
    {
        return base.fg(TOML_KEY).add_modifier(Modifier::BOLD);
    }

    match kind {
        SyntaxKind::COMMENT => base.fg(TOML_COMMENT).add_modifier(Modifier::ITALIC),
        SyntaxKind::STRING
        | SyntaxKind::MULTI_LINE_STRING
        | SyntaxKind::STRING_LITERAL
        | SyntaxKind::MULTI_LINE_STRING_LITERAL => base.fg(TOML_STRING),
        SyntaxKind::INTEGER
        | SyntaxKind::INTEGER_HEX
        | SyntaxKind::INTEGER_OCT
        | SyntaxKind::INTEGER_BIN
        | SyntaxKind::FLOAT => base.fg(TOML_NUMBER),
        SyntaxKind::BOOL => base.fg(TOML_BOOLEAN).add_modifier(Modifier::BOLD),
        SyntaxKind::DATE_TIME_OFFSET
        | SyntaxKind::DATE_TIME_LOCAL
        | SyntaxKind::DATE
        | SyntaxKind::TIME => base.fg(TOML_DATE_TIME),
        SyntaxKind::BRACKET_START
        | SyntaxKind::BRACKET_END
        | SyntaxKind::BRACE_START
        | SyntaxKind::BRACE_END
        | SyntaxKind::PERIOD
        | SyntaxKind::COMMA
        | SyntaxKind::EQ => base.fg(DIM),
        SyntaxKind::ERROR => base
            .fg(ERROR_COLOR)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        _ => base,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandFormMode {
    Add,
    Edit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MonitorFormMode {
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

#[derive(Clone)]
enum Modal {
    None,
    ConfirmUpdate {
        tools: Vec<String>,
        target_version: Option<String>,
        current_tools: Vec<String>,
    },
    ConfirmGithubMonitorUpdate {
        monitors: Vec<String>,
    },
    TargetVersion {
        name: String,
        version: TextInput,
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
    TomlEditor {
        editor: TomlEditor,
    },
    NetworkProxy {
        proxy_mode: ProxyMode,
        field: usize,
        proxy_url: TextInput,
        no_proxy: TextInput,
    },
    GithubApiKey {
        api_key: TextInput,
    },
    GithubMonitorForm {
        mode: MonitorFormMode,
        original_index: Option<usize>,
        field: usize,
        name: TextInput,
        repository: TextInput,
        asset_regex: TextInput,
        target_directory: TextInput,
        format: ReleaseAssetFormat,
        update_policy: ReleaseUpdatePolicy,
        cleanup_installer: bool,
        max_download_bytes: TextInput,
        max_extracted_bytes: TextInput,
        max_extracted_files: TextInput,
        strip_components: TextInput,
        enabled: bool,
    },
    ConfirmDeleteGithubMonitor {
        index: usize,
    },
    GithubPollInterval {
        seconds: TextInput,
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
    InitialLoadProgress(InitialLoadProgress),
    InitialLoadFinished(std::result::Result<InitialLoadData, String>),
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
    LatestVersionResolved {
        name: String,
        probe_id: u64,
        result: std::result::Result<String, version::LatestVersionError>,
    },
    DoctorResolved {
        probe_id: u64,
        diagnoses: Vec<doctor::ToolDiagnosis>,
        error: Option<String>,
    },
    NetworkTestResolved {
        probe_id: u64,
        results: std::result::Result<Vec<version::NetworkTestResult>, String>,
    },
    ReleaseMonitorsProbed(std::result::Result<Vec<MonitorStatus>, String>),
    ReleaseMonitorsResolved(std::result::Result<Vec<MonitorOutcome>, String>),
    GithubCredentialResolved {
        probe_id: u64,
        result: std::result::Result<bool, String>,
    },
    GithubRateLimitResolved {
        probe_id: u64,
        result: std::result::Result<version::GithubRateLimit, String>,
    },
}

enum ProbeTask {
    InstalledVersion {
        name: String,
        probe_id: u64,
        command_spec: CommandSpec,
        network: NetworkSettings,
    },
    LatestVersion {
        name: String,
        probe_id: u64,
        source: LatestVersionSource,
        agent: ureq::Agent,
        encrypted_github_api_key: Option<String>,
    },
}

impl ProbeTask {
    fn run(self, tx: &Sender<AppEvent>) {
        match self {
            Self::InstalledVersion {
                name,
                probe_id,
                command_spec,
                network,
            } => {
                let version = command::run_with_network(&command_spec, &network)
                    .ok()
                    .filter(|result| result.status.success())
                    .and_then(|result| version_from_output(&result.stdout, &result.stderr));
                let _ = tx.send(AppEvent::VersionResolved {
                    name,
                    probe_id,
                    version,
                });
            }
            Self::LatestVersion {
                name,
                probe_id,
                source,
                agent,
                encrypted_github_api_key,
            } => {
                let api_key = if matches!(
                    source,
                    LatestVersionSource::GithubRelease { .. }
                        | LatestVersionSource::GithubTag { .. }
                ) {
                    credential::github_api_key(encrypted_github_api_key.as_deref())
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let result = version::fetch_latest(
                    &source,
                    &agent,
                    api_key.as_ref().map(|key| key.as_str()),
                );
                let _ = tx.send(AppEvent::LatestVersionResolved {
                    name,
                    probe_id,
                    result,
                });
            }
        }
    }
}

#[derive(Clone)]
struct ProbeScheduler {
    tx: Sender<ProbeTask>,
}

impl ProbeScheduler {
    fn new(event_tx: Sender<AppEvent>) -> Self {
        let (tx, rx) = mpsc::channel::<ProbeTask>();
        #[cfg(not(test))]
        {
            let rx = Arc::new(Mutex::new(rx));
            for _ in 0..MAX_CONCURRENT_TUI_PROBES {
                let rx = Arc::clone(&rx);
                let event_tx = event_tx.clone();
                thread::spawn(move || {
                    loop {
                        let task = {
                            let rx = rx.lock().expect("TUI probe queue lock");
                            rx.recv()
                        };
                        let Ok(task) = task else {
                            break;
                        };
                        task.run(&event_tx);
                    }
                });
            }
        }
        #[cfg(test)]
        {
            let run = ProbeTask::run;
            let _ = (event_tx, rx, run);
        }
        Self { tx }
    }

    fn schedule(&self, task: ProbeTask) {
        let _ = self.tx.send(task);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitialLoadPhase {
    Configuration,
    Tools,
    Jobs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InitialLoadProgress {
    phase: InitialLoadPhase,
    completed: usize,
    total: usize,
}

impl InitialLoadProgress {
    fn percentage(self) -> u16 {
        if self.total == 0 {
            return 0;
        }
        u16::try_from(self.completed.saturating_mul(100) / self.total)
            .unwrap_or(100)
            .min(100)
    }

    fn label(self, language: Language) -> &'static str {
        match (self.phase, language) {
            (InitialLoadPhase::Configuration, Language::English) => "Loading configuration",
            (InitialLoadPhase::Configuration, Language::Chinese) => "正在加载配置",
            (InitialLoadPhase::Tools, Language::English) => "Checking installed tools",
            (InitialLoadPhase::Tools, Language::Chinese) => "正在检查已安装工具",
            (InitialLoadPhase::Jobs, Language::English) => "Loading job history",
            (InitialLoadPhase::Jobs, Language::Chinese) => "正在加载任务历史",
        }
    }
}

#[derive(Debug)]
struct InitialLoadData {
    tools: Vec<ToolItem>,
    github_monitors: Vec<GithubReleaseMonitor>,
    jobs: Vec<JobItem>,
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
    visible_tool_indices: Vec<usize>,
    tool_index: usize,
    tool_hitboxes: Vec<(Rect, usize)>,
    tool_viewport: ListViewport,
    tool_view: ToolView,
    tool_view_hitboxes: Vec<(Rect, usize)>,
    github_monitor_index: usize,
    github_monitor_hitboxes: Vec<(Rect, usize)>,
    github_monitor_viewport: ListViewport,
    github_monitors: Vec<GithubReleaseMonitor>,
    selected_github_monitors: HashSet<String>,
    jobs: Vec<JobItem>,
    job_index: usize,
    job_viewport: ListViewport,
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
    next_latest_probe_id: u64,
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
    doctor_viewport: ListViewport,
    expanded_doctor: Option<String>,
    doctor_detail_scroll: usize,
    doctor_detail_area: Option<Rect>,
    doctor_loading: bool,
    doctor_probe_id: u64,
    next_doctor_probe_id: u64,
    doctor_checked_at: Option<String>,
    settings: AppSettings,
    settings_index: usize,
    settings_hitboxes: Vec<(Rect, usize)>,
    network_test_loading: bool,
    network_test_probe_id: u64,
    next_network_test_probe_id: u64,
    network_test_results: Vec<version::NetworkTestResult>,
    github_api_key_configured: bool,
    github_credential_error: Option<String>,
    github_credential_probe_id: u64,
    github_rate_limit: Option<version::GithubRateLimit>,
    github_rate_limit_error: Option<String>,
    github_rate_limit_loading: bool,
    github_rate_limit_probe_id: u64,
    last_github_rate_limit_refresh: Instant,
    release_probe_running: bool,
    release_monitor_running: bool,
    release_monitor_statuses: Vec<MonitorStatus>,
    last_release_refresh: Instant,
    latest_agent: Option<(NetworkSettings, ureq::Agent)>,
    probe_scheduler: ProbeScheduler,
    initial_load: Option<InitialLoadProgress>,
    initial_load_error: Option<String>,
    modal_input_hitboxes: Vec<ModalInputHitbox>,
    modal_drag: Option<(usize, usize)>,
    toml_editor_hitbox: Option<TomlEditorHitbox>,
    toml_editor_drag: Option<usize>,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    ctrl_c_armed: bool,
    should_quit: bool,
}

impl App {
    #[cfg(test)]
    fn new(state: StateDirs, config_path: Option<PathBuf>) -> Result<Self> {
        let mut app = Self::empty(state, config_path)?;
        let data = load_initial_data(app.state.clone(), app.config_path.clone(), |_| {})?;
        app.apply_initial_load(data);
        Ok(app)
    }

    fn new_loading(state: StateDirs, config_path: Option<PathBuf>) -> Result<Self> {
        let mut app = Self::empty(state, config_path)?;
        app.initial_load = Some(InitialLoadProgress {
            phase: InitialLoadPhase::Configuration,
            completed: 0,
            total: 1,
        });
        app.start_initial_load();
        app.start_github_credential_check();
        Ok(app)
    }

    fn empty(state: StateDirs, config_path: Option<PathBuf>) -> Result<Self> {
        let executable = std::env::current_exe()?;
        let settings = AppSettings::load(&state.settings_path())?;
        let language = settings.language;
        let (tx, rx) = mpsc::channel();
        let probe_scheduler = ProbeScheduler::new(tx.clone());
        let started_at = datetime::now();
        let app = Self {
            state,
            config_path,
            executable,
            tools: Vec::new(),
            visible_tool_indices: Vec::new(),
            tool_index: 0,
            tool_hitboxes: Vec::new(),
            tool_viewport: ListViewport::default(),
            tool_view: ToolView::Commands,
            tool_view_hitboxes: Vec::new(),
            github_monitor_index: 0,
            github_monitor_hitboxes: Vec::new(),
            github_monitor_viewport: ListViewport::default(),
            github_monitors: Vec::new(),
            selected_github_monitors: HashSet::new(),
            jobs: Vec::new(),
            job_index: 0,
            job_viewport: ListViewport::default(),
            activity: vec![
                language
                    .text("Welcome to dvup.", "欢迎使用 dvup。")
                    .to_owned(),
                language
                    .text(
                        "Select tools with Space and press Enter to update.",
                        "按 Space 选择工具，然后按 Enter 更新。",
                    )
                    .to_owned(),
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
            language,
            process_strategy: ProcessStrategy::Wait,
            modal: Modal::None,
            message: language.text("Ready", "就绪").to_owned(),
            running: 0,
            next_version_probe_id: 0,
            next_latest_probe_id: 0,
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
            doctor_viewport: ListViewport::default(),
            expanded_doctor: None,
            doctor_detail_scroll: 0,
            doctor_detail_area: None,
            doctor_loading: false,
            doctor_probe_id: 0,
            next_doctor_probe_id: 0,
            doctor_checked_at: None,
            settings,
            settings_index: 0,
            settings_hitboxes: Vec::new(),
            network_test_loading: false,
            network_test_probe_id: 0,
            next_network_test_probe_id: 0,
            network_test_results: Vec::new(),
            github_api_key_configured: false,
            github_credential_error: None,
            github_credential_probe_id: 0,
            github_rate_limit: None,
            github_rate_limit_error: None,
            github_rate_limit_loading: false,
            github_rate_limit_probe_id: 0,
            last_github_rate_limit_refresh: Instant::now(),
            release_probe_running: false,
            release_monitor_running: false,
            release_monitor_statuses: Vec::new(),
            last_release_refresh: Instant::now(),
            latest_agent: None,
            probe_scheduler,
            initial_load: None,
            initial_load_error: None,
            modal_input_hitboxes: Vec::new(),
            modal_drag: None,
            toml_editor_hitbox: None,
            toml_editor_drag: None,
            tx,
            rx,
            ctrl_c_armed: false,
            should_quit: false,
        };
        Ok(app)
    }

    fn start_initial_load(&self) {
        let state = self.state.clone();
        let config_path = self.config_path.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = load_initial_data(state, config_path, move |progress| {
                let _ = progress_tx.send(AppEvent::InitialLoadProgress(progress));
            })
            .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::InitialLoadFinished(result));
        });
    }

    fn start_github_credential_check(&mut self) {
        self.github_credential_probe_id = self.github_credential_probe_id.wrapping_add(1).max(1);
        let probe_id = self.github_credential_probe_id;
        let encrypted_api_key = self.settings.github.encrypted_api_key.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = credential::has_github_api_key(encrypted_api_key.as_deref())
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::GithubCredentialResolved { probe_id, result });
        });
    }

    fn start_github_rate_limit_refresh(&mut self) {
        if !self.github_api_key_configured || self.github_rate_limit_loading {
            return;
        }
        self.github_rate_limit_probe_id = self.github_rate_limit_probe_id.wrapping_add(1).max(1);
        let probe_id = self.github_rate_limit_probe_id;
        self.github_rate_limit_loading = true;
        self.github_rate_limit_error = None;
        self.last_github_rate_limit_refresh = Instant::now();
        if cfg!(test) {
            return;
        }
        let network = self.settings.network.clone();
        let encrypted_api_key = self.settings.github.encrypted_api_key.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = (|| {
                let api_key = credential::github_api_key(encrypted_api_key.as_deref())?
                    .ok_or_else(|| {
                        Error::Message("GitHub API key is no longer configured".to_owned())
                    })?;
                let agent = version::network_agent(&network)?;
                version::fetch_github_rate_limit(&agent, api_key.as_str())
            })()
            .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::GithubRateLimitResolved { probe_id, result });
        });
    }

    fn clear_github_rate_limit(&mut self) {
        self.github_rate_limit_probe_id = self.github_rate_limit_probe_id.wrapping_add(1);
        self.github_rate_limit = None;
        self.github_rate_limit_error = None;
        self.github_rate_limit_loading = false;
        self.last_github_rate_limit_refresh = Instant::now();
    }

    fn apply_initial_load(&mut self, data: InitialLoadData) {
        self.tools = data.tools;
        self.github_monitors = data.github_monitors;
        self.jobs = data.jobs;
        self.rebuild_visible_tool_indices(None);
        self.job_index = self.job_index.min(self.jobs.len().saturating_sub(1));
        self.last_job_refresh = Instant::now();
        self.initial_load = None;
        self.message = self.language.text("Ready", "就绪").to_owned();
        self.start_all_tool_version_probes();
        if self.settings.auto_diagnose_on_startup {
            let _ = self.refresh_doctor();
        }
    }

    fn refresh_tools(&mut self) -> Result<()> {
        let focused_name = self.focused_tool().map(|tool| tool.name.clone());
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
        let (tools, github_monitors) =
            load_tool_items(&self.state, self.config_path.clone(), |_| {})?;
        self.tools = tools;
        self.github_monitors = github_monitors;
        self.selected_github_monitors.retain(|name| {
            self.github_monitors
                .iter()
                .any(|monitor| &monitor.name == name)
        });
        self.release_monitor_statuses.retain(|status| {
            self.github_monitors
                .iter()
                .any(|monitor| monitor.name == status.name)
        });
        self.github_monitor_index = self
            .github_monitor_index
            .min(self.github_monitors.len().saturating_sub(1));
        for tool in &mut self.tools {
            let (selected, run_state, elapsed) =
                previous
                    .get(&tool.name)
                    .copied()
                    .unwrap_or((false, RunState::Idle, None));
            tool.selected = selected;
            tool.run_state = run_state;
            tool.elapsed = elapsed;
        }
        self.rebuild_visible_tool_indices(focused_name.as_deref());
        self.start_all_tool_version_probes();
        Ok(())
    }

    fn rebuild_visible_tool_indices(&mut self, preferred_name: Option<&str>) {
        let previous_position = self.tool_index;
        let hide_unavailable = self.settings.hide_unsupported_and_missing_tools;
        self.visible_tool_indices = self
            .tools
            .iter()
            .enumerate()
            .filter_map(|(index, tool)| {
                let hidden = hide_unavailable
                    && matches!(
                        tool.availability,
                        Availability::Unsupported | Availability::Missing
                    );
                (!hidden).then_some(index)
            })
            .collect();
        self.tool_index = preferred_name
            .and_then(|name| {
                self.visible_tool_indices
                    .iter()
                    .position(|&index| self.tools[index].name == name)
            })
            .unwrap_or_else(|| {
                previous_position.min(self.visible_tool_indices.len().saturating_sub(1))
            });
    }

    fn focused_tool_index(&self) -> Option<usize> {
        self.visible_tool_indices.get(self.tool_index).copied()
    }

    fn focused_tool(&self) -> Option<&ToolItem> {
        self.focused_tool_index()
            .and_then(|index| self.tools.get(index))
    }

    fn focus_tool_named(&mut self, name: &str) {
        if let Some(position) = self
            .visible_tool_indices
            .iter()
            .position(|&index| self.tools[index].name == name)
        {
            self.tool_index = position;
        }
    }

    fn start_tool_version_probes(&mut self, index: usize) {
        let Some(tool) = self.tools.get_mut(index) else {
            return;
        };
        if !tool.availability.allows_version_checks() {
            tool.version = VersionState::Unavailable;
            tool.latest_version = VersionState::Unavailable;
            return;
        }

        self.start_version_probe(index);
        self.start_latest_version_probe(index);
    }

    fn start_all_tool_version_probes(&mut self) {
        for index in 0..self.tools.len() {
            if self.tools[index].availability.allows_version_checks() {
                self.start_version_probe(index);
            } else {
                self.tools[index].version = VersionState::Unavailable;
                self.tools[index].latest_version = VersionState::Unavailable;
            }
        }
        for index in 0..self.tools.len() {
            if self.tools[index].availability.allows_version_checks() {
                self.start_latest_version_probe(index);
            }
        }
    }

    fn start_version_probe(&mut self, index: usize) {
        let Some(tool) = self.tools.get_mut(index) else {
            return;
        };
        self.next_version_probe_id = self.next_version_probe_id.wrapping_add(1).max(1);
        let probe_id = self.next_version_probe_id;
        tool.version = VersionState::Loading;
        tool.version_probe_id = probe_id;
        self.probe_scheduler.schedule(ProbeTask::InstalledVersion {
            name: tool.name.clone(),
            probe_id,
            command_spec: tool.version_command.clone(),
            network: self.settings.network.clone(),
        });
    }

    fn start_latest_version_probe(&mut self, index: usize) {
        let Some(source) = self
            .tools
            .get(index)
            .and_then(|tool| tool.latest_source.clone())
        else {
            if let Some(tool) = self.tools.get_mut(index) {
                tool.latest_version = VersionState::Unavailable;
            }
            return;
        };
        let Ok(agent) = self.latest_version_agent() else {
            let Some(tool) = self.tools.get_mut(index) else {
                return;
            };
            tool.latest_version = VersionState::Unavailable;
            return;
        };
        let Some(tool) = self.tools.get_mut(index) else {
            return;
        };
        let encrypted_github_api_key = self.settings.github.encrypted_api_key.clone();
        self.next_latest_probe_id = self.next_latest_probe_id.wrapping_add(1).max(1);
        let probe_id = self.next_latest_probe_id;
        tool.latest_version = VersionState::Loading;
        tool.latest_probe_id = probe_id;
        self.probe_scheduler.schedule(ProbeTask::LatestVersion {
            name: tool.name.clone(),
            probe_id,
            source,
            agent,
            encrypted_github_api_key,
        });
    }

    fn latest_version_agent(&mut self) -> Result<ureq::Agent> {
        let network = &self.settings.network;
        if self
            .latest_agent
            .as_ref()
            .is_none_or(|(cached, _)| cached != network)
        {
            self.latest_agent = Some((network.clone(), version::network_agent(network)?));
        }
        Ok(self
            .latest_agent
            .as_ref()
            .expect("latest-version agent initialized")
            .1
            .clone())
    }

    fn refresh_tool_version(&mut self, name: &str) {
        if let Some(index) = self.tools.iter().position(|tool| tool.name == name) {
            self.start_tool_version_probes(index);
        }
    }

    fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        if tab == Tab::Doctor && self.doctor_never_scanned() {
            self.message = self
                .language
                .text(
                    "Diagnostics have not been run. Press Enter to scan",
                    "尚未运行诊断，按 Enter 开始扫描",
                )
                .to_owned();
        }
    }

    fn select_tool_view(&mut self, view: ToolView) {
        self.tool_view = view;
        self.message = match (view, self.language) {
            (ToolView::Commands, Language::English) => "Command tools".to_owned(),
            (ToolView::Commands, Language::Chinese) => "命令工具".to_owned(),
            (ToolView::Github, Language::English) => "GitHub repositories".to_owned(),
            (ToolView::Github, Language::Chinese) => "GitHub 仓库".to_owned(),
        };
        if view == ToolView::Github
            && self.release_monitor_statuses.is_empty()
            && !self.github_monitors.is_empty()
        {
            self.start_release_probe(false);
        }
    }

    fn focused_github_monitor(&self) -> Option<&GithubReleaseMonitor> {
        self.github_monitors.get(self.github_monitor_index)
    }

    fn doctor_never_scanned(&self) -> bool {
        !self.doctor_loading && self.doctor_checked_at.is_none() && self.doctor_diagnoses.is_empty()
    }

    fn doctor_diagnosis_visible(&self, diagnosis: &doctor::ToolDiagnosis) -> bool {
        diagnosis_is_visible(diagnosis, self.settings.hide_unsupported_and_missing_tools)
    }

    fn visible_doctor_diagnoses(&self) -> impl Iterator<Item = &doctor::ToolDiagnosis> {
        self.doctor_diagnoses
            .iter()
            .filter(|diagnosis| self.doctor_diagnosis_visible(diagnosis))
    }

    fn visible_doctor_count(&self) -> usize {
        self.visible_doctor_diagnoses().count()
    }

    fn focused_doctor_diagnosis(&self) -> Option<&doctor::ToolDiagnosis> {
        self.visible_doctor_diagnoses().nth(self.doctor_index)
    }

    fn reconcile_doctor_selection(&mut self, preferred_name: Option<&str>) {
        let previous_position = self.doctor_index;
        let visible_count = self.visible_doctor_count();
        self.doctor_index = preferred_name
            .and_then(|name| {
                self.visible_doctor_diagnoses()
                    .position(|diagnosis| diagnosis.name == name)
            })
            .unwrap_or_else(|| previous_position.min(visible_count.saturating_sub(1)));

        if let Some(expanded_name) = self.expanded_doctor.clone()
            && !self
                .visible_doctor_diagnoses()
                .any(|diagnosis| diagnosis.name == expanded_name)
        {
            self.expanded_doctor = None;
            self.doctor_detail_scroll = 0;
        }
    }

    fn toggle_setting(&mut self, index: usize) {
        match index {
            0 => {
                let previous = self.settings.auto_diagnose_on_startup;
                self.settings.auto_diagnose_on_startup = !previous;
                if let Err(error) = self.settings.save(&self.state.settings_path()) {
                    self.settings.auto_diagnose_on_startup = previous;
                    self.report_settings_save_error(error);
                    return;
                }
                self.message = match (self.settings.auto_diagnose_on_startup, self.language) {
                    (true, Language::English) => {
                        "Automatic startup diagnostics enabled; applies next time TUI starts"
                            .to_owned()
                    }
                    (true, Language::Chinese) => {
                        "已启用启动自动诊断；下次进入 TUI 时生效".to_owned()
                    }
                    (false, Language::English) => {
                        "Automatic startup diagnostics disabled".to_owned()
                    }
                    (false, Language::Chinese) => "已关闭启动自动诊断".to_owned(),
                };
            }
            1 => {
                let focused_name = self.focused_tool().map(|tool| tool.name.clone());
                let focused_doctor_name = self
                    .focused_doctor_diagnosis()
                    .map(|diagnosis| diagnosis.name.clone());
                let previous = self.settings.hide_unsupported_and_missing_tools;
                self.settings.hide_unsupported_and_missing_tools = !previous;
                if let Err(error) = self.settings.save(&self.state.settings_path()) {
                    self.settings.hide_unsupported_and_missing_tools = previous;
                    self.report_settings_save_error(error);
                    return;
                }
                self.rebuild_visible_tool_indices(focused_name.as_deref());
                self.reconcile_doctor_selection(focused_doctor_name.as_deref());
                self.message = match (
                    self.settings.hide_unsupported_and_missing_tools,
                    self.language,
                ) {
                    (true, Language::English) => {
                        "Unsupported and uninstalled tools are now hidden".to_owned()
                    }
                    (true, Language::Chinese) => "已隐藏不支持或未安装的工具".to_owned(),
                    (false, Language::English) => "All configured tools are now shown".to_owned(),
                    (false, Language::Chinese) => "已显示全部配置工具".to_owned(),
                };
            }
            2 => self.open_network_editor(0),
            3 | 4 => {
                if self.settings.network.proxy_mode == ProxyMode::Explicit {
                    self.open_network_editor(index - 2);
                } else {
                    self.message = self
                        .language
                        .text(
                            "Proxy URL and bypass rules are only used in explicit mode",
                            "代理地址和绕过规则仅用于显式代理模式",
                        )
                        .to_owned();
                }
            }
            5 => self.start_network_test(),
            6 => {
                self.modal = Modal::GithubApiKey {
                    api_key: TextInput::new(String::new()),
                };
                self.message = self
                    .language
                    .text(
                        "Enter a GitHub API key; leave empty and press Enter to remove it",
                        "请输入 GitHub API Key；留空并按 Enter 可删除现有密钥",
                    )
                    .to_owned();
            }
            7 => {
                self.modal = Modal::GithubPollInterval {
                    seconds: TextInput::new(self.settings.github.poll_interval_secs.to_string()),
                };
                self.message = self
                    .language
                    .text(
                        "Set the GitHub monitor interval in seconds (60–86400)",
                        "设置 GitHub 监控间隔秒数（60–86400）",
                    )
                    .to_owned();
            }
            _ => {}
        }
    }

    fn open_github_monitor_form(&mut self, mode: MonitorFormMode, index: Option<usize>) {
        let monitor = index.and_then(|index| self.github_monitors.get(index));
        self.modal = Modal::GithubMonitorForm {
            mode,
            original_index: index,
            field: 0,
            name: TextInput::new(monitor.map(|value| value.name.clone()).unwrap_or_default()),
            repository: TextInput::new(
                monitor
                    .map(|value| value.repository.clone())
                    .unwrap_or_default(),
            ),
            asset_regex: TextInput::new(
                monitor
                    .map(|value| value.asset_regex.clone())
                    .unwrap_or_default(),
            ),
            target_directory: TextInput::new(
                monitor
                    .map(|value| value.target_directory.display().to_string())
                    .unwrap_or_default(),
            ),
            format: monitor
                .map(|value| value.format)
                .unwrap_or(ReleaseAssetFormat::Zip),
            update_policy: monitor.map(|value| value.update_policy).unwrap_or_default(),
            cleanup_installer: monitor.map(|value| value.cleanup_installer).unwrap_or(true),
            max_download_bytes: TextInput::new(
                monitor
                    .map(|value| value.max_download_bytes.to_string())
                    .unwrap_or_else(|| DEFAULT_MONITOR_MAX_DOWNLOAD_BYTES.to_string()),
            ),
            max_extracted_bytes: TextInput::new(
                monitor
                    .map(|value| value.max_extracted_bytes.to_string())
                    .unwrap_or_else(|| DEFAULT_MONITOR_MAX_EXTRACTED_BYTES.to_string()),
            ),
            max_extracted_files: TextInput::new(
                monitor
                    .map(|value| value.max_extracted_files.to_string())
                    .unwrap_or_else(|| DEFAULT_MONITOR_MAX_EXTRACTED_FILES.to_string()),
            ),
            strip_components: TextInput::new(
                monitor
                    .map(|value| value.strip_components.to_string())
                    .unwrap_or_else(|| "0".to_owned()),
            ),
            enabled: monitor.map(|value| value.enabled).unwrap_or(true),
        };
        self.message = match (mode, self.language) {
            (MonitorFormMode::Add, Language::English) => {
                "Enter an owner/repo and strict Release asset installation rules".to_owned()
            }
            (MonitorFormMode::Add, Language::Chinese) => {
                "填写 owner/repo 和严格的 Release 资产安装规则".to_owned()
            }
            (MonitorFormMode::Edit, Language::English) => {
                "Edit the selected GitHub repository monitor".to_owned()
            }
            (MonitorFormMode::Edit, Language::Chinese) => "编辑选中的 GitHub 仓库监控项".to_owned(),
        };
    }

    fn save_github_monitor(
        &mut self,
        original_index: Option<usize>,
        monitor: GithubReleaseMonitor,
    ) -> Result<usize> {
        if self.config_path.is_some() {
            return Err(Error::Message(
                "GitHub repository editing is disabled with --config".to_owned(),
            ));
        }
        let path = self.state.custom_config_path();
        let mut custom = if path.is_file() {
            UserConfig::load(&path)?
        } else {
            UserConfig::empty()
        };
        let selected = if let Some(index) = original_index {
            let Some(slot) = custom.github.monitors.get_mut(index) else {
                return Err(Error::Message(
                    "selected GitHub monitor no longer exists".to_owned(),
                ));
            };
            *slot = monitor;
            index
        } else {
            custom.github.monitors.push(monitor);
            custom.github.monitors.len() - 1
        };
        custom.save(&path)?;
        self.github_monitors = custom.github.monitors;
        Ok(selected)
    }

    fn delete_github_monitor(&mut self, index: usize) -> usize {
        let result = (|| {
            if self.config_path.is_some() {
                return Err(Error::Message(
                    "GitHub repository editing is disabled with --config".to_owned(),
                ));
            }
            let path = self.state.custom_config_path();
            let mut custom = UserConfig::load(&path)?;
            if index >= custom.github.monitors.len() {
                return Err(Error::Message(
                    "selected GitHub monitor no longer exists".to_owned(),
                ));
            }
            let name = custom.github.monitors.remove(index).name;
            if custom.is_empty() {
                std::fs::remove_file(&path)?;
            } else {
                custom.save(&path)?;
            }
            Ok((custom.github.monitors, name))
        })();
        match result {
            Ok((monitors, name)) => {
                self.github_monitors = monitors;
                self.release_monitor_statuses
                    .retain(|status| status.name != name);
                self.selected_github_monitors.remove(&name);
                self.message = match self.language {
                    Language::English => format!("Removed GitHub monitor {name}"),
                    Language::Chinese => format!("已删除 GitHub 监控 {name}"),
                };
            }
            Err(error) => self.report_custom_config_save_error(error),
        }
        index.min(self.github_monitors.len().saturating_sub(1))
    }

    fn start_release_probe(&mut self, manual: bool) {
        if self.release_probe_running || self.release_monitor_running {
            if manual {
                self.message = self
                    .language
                    .text(
                        "A GitHub repository operation is already running",
                        "GitHub 仓库操作正在进行中",
                    )
                    .to_owned();
            }
            return;
        }
        if self.github_monitors.is_empty() {
            if manual {
                self.message = self
                    .language
                    .text("No GitHub repositories configured", "尚未配置 GitHub 仓库")
                    .to_owned();
            }
            self.last_release_refresh = Instant::now();
            return;
        }
        self.release_probe_running = true;
        self.last_release_refresh = Instant::now();
        if manual {
            self.message = self
                .language
                .text(
                    "Refreshing GitHub repository status…",
                    "正在刷新 GitHub 仓库状态…",
                )
                .to_owned();
        }
        let monitors = self.github_monitors.clone();
        let github = self.settings.github.clone();
        let network = self.settings.network.clone();
        let state_path = self.state.release_state_path();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = release::probe_monitors(&monitors, &github, &network, &state_path)
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::ReleaseMonitorsProbed(result));
        });
    }

    fn start_release_updates(&mut self, names: Vec<String>) {
        if self.release_probe_running || self.release_monitor_running {
            self.message = self
                .language
                .text(
                    "A GitHub repository operation is already running",
                    "GitHub 仓库操作正在进行中",
                )
                .to_owned();
            return;
        }
        if names.is_empty() {
            return;
        }
        self.release_monitor_running = true;
        self.message = match self.language {
            Language::English => format!("Installing {} GitHub repository update(s)…", names.len()),
            Language::Chinese => format!("正在安装 {} 项 GitHub 仓库更新…", names.len()),
        };
        let monitors = self.github_monitors.clone();
        let github = self.settings.github.clone();
        let network = self.settings.network.clone();
        let state_path = self.state.release_state_path();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result =
                release::run_selected_monitors(&monitors, &github, &network, &state_path, &names)
                    .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::ReleaseMonitorsResolved(result));
        });
    }

    fn open_network_editor(&mut self, requested_field: usize) {
        let proxy_mode = self.settings.network.proxy_mode;
        self.modal = Modal::NetworkProxy {
            proxy_mode,
            field: if proxy_mode == ProxyMode::Explicit {
                requested_field.min(2)
            } else {
                0
            },
            proxy_url: TextInput::new(self.settings.network.proxy_url.clone().unwrap_or_default()),
            no_proxy: TextInput::new(self.settings.network.no_proxy.join(", ")),
        };
        self.message = self
            .language
            .text(
                "Choose a network mode; explicit mode accepts an HTTP/HTTPS proxy",
                "请选择网络模式；显式模式可填写 HTTP/HTTPS 代理",
            )
            .to_owned();
    }

    fn save_network_settings(
        &mut self,
        proxy_mode: ProxyMode,
        proxy_url: String,
        no_proxy: String,
    ) -> bool {
        let network = if proxy_mode == ProxyMode::Explicit {
            NetworkSettings {
                proxy_mode,
                proxy_url: Some(proxy_url.trim().to_owned()),
                no_proxy: no_proxy
                    .split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_owned)
                    .collect(),
            }
        } else {
            NetworkSettings {
                proxy_mode,
                proxy_url: None,
                no_proxy: Vec::new(),
            }
        };
        if let Err(error) = network.validate() {
            self.message = match self.language {
                Language::English => format!("Invalid proxy settings: {error}"),
                Language::Chinese => format!("代理设置无效：{error}"),
            };
            return false;
        }
        let previous = std::mem::replace(&mut self.settings.network, network);
        if let Err(error) = self.settings.save(&self.state.settings_path()) {
            self.settings.network = previous;
            self.report_settings_save_error(error);
            return false;
        }
        self.network_settings_changed();
        true
    }

    fn network_settings_changed(&mut self) {
        self.network_test_results.clear();
        self.network_test_loading = false;
        self.network_test_probe_id = self.network_test_probe_id.wrapping_add(1);
        self.clear_github_rate_limit();
        self.start_github_rate_limit_refresh();
        if let Err(error) = self.refresh_tools() {
            self.message = match self.language {
                Language::English => format!("Network settings saved, but refresh failed: {error}"),
                Language::Chinese => format!("网络设置已保存，但刷新失败：{error}"),
            };
        } else {
            self.message = match self.language {
                Language::English => format!(
                    "Network mode changed to {}",
                    self.settings.network.proxy_mode.label()
                ),
                Language::Chinese => format!(
                    "网络模式已切换为 {}",
                    self.settings.network.proxy_mode.label()
                ),
            };
        }
    }

    fn start_network_test(&mut self) {
        if self.network_test_loading {
            self.message = self
                .language
                .text("Network test is already running", "网络测试正在进行中")
                .to_owned();
            return;
        }
        self.next_network_test_probe_id = self.next_network_test_probe_id.wrapping_add(1).max(1);
        let probe_id = self.next_network_test_probe_id;
        self.network_test_probe_id = probe_id;
        self.network_test_loading = true;
        self.network_test_results.clear();
        self.message = self
            .language
            .text("Testing four registry endpoints…", "正在测试四个仓库端点…")
            .to_owned();
        if cfg!(test) {
            return;
        }
        let tx = self.tx.clone();
        let network = self.settings.network.clone();
        thread::spawn(move || {
            let results = version::test_network(&network).map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::NetworkTestResolved { probe_id, results });
        });
    }

    fn report_settings_save_error(&mut self, error: Error) {
        self.message = match self.language {
            Language::English => format!("Could not save settings: {error}"),
            Language::Chinese => format!("无法保存设置：{error}"),
        };
    }

    fn report_custom_config_save_error(&mut self, error: Error) {
        self.message = match self.language {
            Language::English => format!("Could not save custom configuration: {error}"),
            Language::Chinese => format!("无法保存自定义配置：{error}"),
        };
    }

    fn report_refresh_error(&mut self, error: Error) {
        self.message = match self.language {
            Language::English => format!("Refresh failed: {error}"),
            Language::Chinese => format!("刷新失败：{error}"),
        };
    }

    fn refresh_doctor(&mut self) -> Result<()> {
        if self.doctor_loading {
            self.message = self
                .language
                .text("Diagnostics are already running", "诊断正在进行中，请稍候")
                .to_owned();
            return Ok(());
        }
        let (manifest, working_directory, _) =
            cli::load_manifest(self.config_path.clone(), &self.state)?;
        self.next_doctor_probe_id = self.next_doctor_probe_id.wrapping_add(1).max(1);
        let probe_id = self.next_doctor_probe_id;
        self.doctor_probe_id = probe_id;
        self.doctor_loading = true;
        self.message = self
            .language
            .text("Scanning installation conflicts…", "正在扫描安装冲突…")
            .to_owned();
        let tx = self.tx.clone();
        let network = self.settings.network.clone();
        thread::spawn(move || {
            let (diagnoses, error) =
                match doctor::diagnose(&manifest, &working_directory, None, &network) {
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
        let Some(name) = self
            .focused_doctor_diagnosis()
            .map(|diagnosis| diagnosis.name.clone())
        else {
            return;
        };
        if self.expanded_doctor.as_deref() == Some(&name) {
            self.expanded_doctor = None;
        } else {
            self.expanded_doctor = Some(name);
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
        self.last_job_refresh = Instant::now();
        Ok(())
    }

    fn job_refresh_interval(&self) -> Duration {
        if self.jobs.iter().any(|job| !job.status.is_terminal()) {
            ACTIVE_JOB_REFRESH_INTERVAL
        } else {
            IDLE_JOB_REFRESH_INTERVAL
        }
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
                AppEvent::InitialLoadProgress(progress) => {
                    if self.initial_load.is_some() {
                        self.initial_load = Some(progress);
                    }
                }
                AppEvent::InitialLoadFinished(result) => match result {
                    Ok(data) => self.apply_initial_load(data),
                    Err(error) => {
                        self.initial_load = None;
                        self.initial_load_error = Some(error);
                    }
                },
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
                    }
                    if matches!(operation, Operation::Add | Operation::Edit) && success {
                        self.focus_tool_named(&name);
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
                AppEvent::LatestVersionResolved {
                    name,
                    probe_id,
                    result,
                } => {
                    if let Some(tool) = self
                        .tools
                        .iter_mut()
                        .find(|tool| tool.name == name && tool.latest_probe_id == probe_id)
                    {
                        tool.latest_version = match result {
                            Ok(version) => VersionState::Available(version),
                            Err(error) => {
                                self.message =
                                    latest_version_error_message(&name, &error, self.language);
                                VersionState::Failed(error)
                            }
                        };
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
                    let focused_name = self
                        .focused_doctor_diagnosis()
                        .map(|diagnosis| diagnosis.name.clone());
                    self.doctor_diagnoses = diagnoses;
                    self.reconcile_doctor_selection(focused_name.as_deref());
                    let conflicts = self
                        .visible_doctor_diagnoses()
                        .filter(|diagnosis| diagnosis.has_conflict())
                        .count();
                    let visible_count = self.visible_doctor_count();
                    self.message = match self.language {
                        Language::English => format!(
                            "Diagnostics complete: {} tool(s), {conflicts} conflict(s)",
                            visible_count
                        ),
                        Language::Chinese => {
                            format!("诊断完成：{} 个工具，{conflicts} 项冲突", visible_count)
                        }
                    };
                }
                AppEvent::NetworkTestResolved { probe_id, results } => {
                    if self.network_test_probe_id != probe_id {
                        continue;
                    }
                    self.network_test_loading = false;
                    match results {
                        Ok(results) => {
                            let failed = results
                                .iter()
                                .filter(|result| result.error.is_some())
                                .count();
                            self.network_test_results = results;
                            self.message = match self.language {
                                Language::English => format!(
                                    "Network test complete: {} succeeded, {failed} failed",
                                    self.network_test_results.len().saturating_sub(failed)
                                ),
                                Language::Chinese => format!(
                                    "网络测试完成：{} 项成功，{failed} 项失败",
                                    self.network_test_results.len().saturating_sub(failed)
                                ),
                            };
                        }
                        Err(error) => {
                            self.network_test_results.clear();
                            self.message = match self.language {
                                Language::English => format!("Network test failed: {error}"),
                                Language::Chinese => format!("网络测试失败：{error}"),
                            };
                        }
                    }
                }
                AppEvent::ReleaseMonitorsProbed(results) => {
                    self.release_probe_running = false;
                    self.last_release_refresh = Instant::now();
                    match results {
                        Ok(statuses) => {
                            let automatic =
                                automatic_release_update_names(&self.github_monitors, &statuses);
                            let failed = statuses
                                .iter()
                                .filter(|status| status.error.is_some())
                                .count();
                            let available = statuses
                                .iter()
                                .filter(|status| {
                                    status.latest_tag.is_some()
                                        && !monitor_status_is_current(status)
                                })
                                .count();
                            self.release_monitor_statuses = statuses;
                            self.message = match self.language {
                                Language::English => format!(
                                    "GitHub repositories refreshed: {available} update(s), {failed} failed"
                                ),
                                Language::Chinese => format!(
                                    "GitHub 仓库刷新完成：{available} 项可更新，{failed} 项失败"
                                ),
                            };
                            if !automatic.is_empty() {
                                self.start_release_updates(automatic);
                            }
                        }
                        Err(error) => {
                            self.message = match self.language {
                                Language::English => {
                                    format!("GitHub repository refresh failed: {error}")
                                }
                                Language::Chinese => format!("GitHub 仓库刷新失败：{error}"),
                            };
                        }
                    }
                }
                AppEvent::ReleaseMonitorsResolved(results) => {
                    self.release_monitor_running = false;
                    self.last_release_refresh = Instant::now();
                    match results {
                        Ok(results) => {
                            let updated = results
                                .iter()
                                .filter(|outcome| matches!(outcome, MonitorOutcome::Updated { .. }))
                                .count();
                            let failed = results
                                .iter()
                                .filter(|outcome| matches!(outcome, MonitorOutcome::Failed { .. }))
                                .count();
                            for outcome in &results {
                                match outcome {
                                    MonitorOutcome::Updated { name, tag, asset } => {
                                        update_monitor_status(
                                            &mut self.release_monitor_statuses,
                                            name,
                                            tag,
                                            Some(asset),
                                            None,
                                        );
                                        self.push_activity(match self.language {
                                            Language::English => format!(
                                                "GitHub Release updated {name} to {tag} from {asset}"
                                            ),
                                            Language::Chinese => format!(
                                                "GitHub Release 已将 {name} 更新到 {tag}（{asset}）"
                                            ),
                                        });
                                    }
                                    MonitorOutcome::Failed { name, error } => {
                                        if let Some(status) = self
                                            .release_monitor_statuses
                                            .iter_mut()
                                            .find(|status| status.name == *name)
                                        {
                                            status.error = Some(error.clone());
                                        } else {
                                            self.release_monitor_statuses.push(MonitorStatus {
                                                name: name.clone(),
                                                installed_tag: None,
                                                latest_tag: None,
                                                asset: None,
                                                error: Some(error.clone()),
                                            });
                                        }
                                        self.push_activity(match self.language {
                                            Language::English => format!(
                                                "GitHub Release monitor {name} failed: {error}"
                                            ),
                                            Language::Chinese => {
                                                format!("GitHub Release 监控 {name} 失败：{error}")
                                            }
                                        });
                                    }
                                    MonitorOutcome::Current { name, tag } => {
                                        update_monitor_status(
                                            &mut self.release_monitor_statuses,
                                            name,
                                            tag,
                                            None,
                                            None,
                                        );
                                    }
                                }
                            }
                            self.selected_github_monitors.clear();
                            self.message = match self.language {
                                Language::English => format!(
                                    "Release check complete: {updated} updated, {failed} failed"
                                ),
                                Language::Chinese => {
                                    format!("Release 检查完成：{updated} 项更新，{failed} 项失败")
                                }
                            };
                        }
                        Err(error) => {
                            self.message = match self.language {
                                Language::English => format!("Release check failed: {error}"),
                                Language::Chinese => format!("Release 检查失败：{error}"),
                            };
                        }
                    }
                }
                AppEvent::GithubCredentialResolved { probe_id, result } => {
                    if probe_id != self.github_credential_probe_id {
                        continue;
                    }
                    match result {
                        Ok(configured) => {
                            self.github_api_key_configured = configured;
                            self.github_credential_error = None;
                            if configured {
                                self.start_github_rate_limit_refresh();
                            } else {
                                self.clear_github_rate_limit();
                            }
                        }
                        Err(error) => {
                            self.github_api_key_configured = false;
                            self.github_credential_error = Some(error);
                            self.clear_github_rate_limit();
                        }
                    }
                }
                AppEvent::GithubRateLimitResolved { probe_id, result } => {
                    if probe_id != self.github_rate_limit_probe_id {
                        continue;
                    }
                    self.github_rate_limit_loading = false;
                    match result {
                        Ok(rate_limit) => {
                            self.github_rate_limit = Some(rate_limit);
                            self.github_rate_limit_error = None;
                        }
                        Err(error) => {
                            self.github_rate_limit_error = Some(error);
                        }
                    }
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
        let up_to_date = self
            .tools
            .iter()
            .filter(|tool| tool.run_state == RunState::UpToDate)
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
        let self_update_queued = self
            .tools
            .iter()
            .any(|tool| tool.name == "dvup" && tool.run_state == RunState::Queued);
        let summary = match self.language {
            Language::English => format!(
                "Complete: {up_to_date} current, {updated} updated, {queued} queued, {failed} failed ({} total) in {:.1}s",
                batch.total,
                batch.started.elapsed().as_secs_f64()
            ),
            Language::Chinese => format!(
                "完成：{up_to_date} 项已是最新，{updated} 项已更新，{queued} 项已排队，{failed} 项失败（共 {} 项），耗时 {:.1} 秒",
                batch.total,
                batch.started.elapsed().as_secs_f64()
            ),
        };
        self.push_activity(format!("\n=== {summary} ==="));
        self.message = if self_update_queued {
            self.language
                .text(
                    "dvup self-update is queued; exit dvup to let the worker replace it",
                    "dvup 自更新已排队；请退出 dvup，让后台任务完成替换",
                )
                .to_owned()
        } else if failed == 0 {
            summary
        } else {
            match self.language {
                Language::English => {
                    format!("{summary}. See Activity for command output and errors")
                }
                Language::Chinese => format!("{summary}。请在活动页查看命令输出和错误"),
            }
        };
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
        self.focused_tool()
            .filter(|tool| {
                tool.availability == Availability::Installed && tool.run_state != RunState::Running
            })
            .map(|tool| vec![tool.name.clone()])
            .unwrap_or_default()
    }

    fn start_updates(
        &mut self,
        tools: Vec<String>,
        target_version: Option<String>,
        current_tools: Vec<String>,
    ) {
        debug_assert!(target_version.is_none() || tools.len() == 1);
        let process_strategy = self.process_strategy;
        for tool in &mut self.tools {
            tool.run_state = RunState::Idle;
            tool.elapsed = None;
        }
        self.mark_tools_up_to_date(&current_tools);
        self.update_batch = Some(UpdateBatch {
            started: Instant::now(),
            total: tools.len() + current_tools.len(),
        });
        if !current_tools.is_empty() {
            self.push_activity(match self.language {
                Language::English => {
                    format!("\n=== already up to date: {} ===", current_tools.join(", "))
                }
                Language::Chinese => {
                    format!("\n=== 已是最新版本：{} ===", current_tools.join("，"))
                }
            });
        }
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
            self.push_activity(match (&target_version, self.language) {
                (Some(version), Language::English) => {
                    format!("\n>>> starting {name} at version {version}")
                }
                (Some(version), Language::Chinese) => {
                    format!("\n>>> 正在将 {name} 更新到版本 {version}")
                }
                (None, Language::English) => format!("\n>>> starting {name}"),
                (None, Language::Chinese) => format!("\n>>> 正在启动 {name}"),
            });
            spawn_dvup(
                self.tx.clone(),
                self.executable.clone(),
                self.state.root().to_path_buf(),
                update_arguments(
                    &name,
                    self.config_path.as_deref(),
                    process_strategy.terminates(),
                    target_version.as_deref(),
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

    fn partition_latest_updates(&self, requested: Vec<String>) -> (Vec<String>, Vec<String>) {
        requested.into_iter().partition(|name| {
            self.tools
                .iter()
                .find(|tool| tool.name == *name)
                .is_none_or(|tool| !tool_is_up_to_date(tool))
        })
    }

    fn mark_tools_up_to_date(&mut self, names: &[String]) {
        for tool in &mut self.tools {
            if names.iter().any(|name| name == &tool.name) {
                tool.run_state = RunState::UpToDate;
                tool.selected = false;
                tool.elapsed = None;
            }
        }
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
        let Some(selected) = self.focused_tool().cloned() else {
            return;
        };
        match selected.kind {
            ToolKind::BuiltIn => {
                self.message = self
                    .language
                    .text("Built-in tools cannot be edited", "不能编辑内置工具")
                    .to_owned();
                return;
            }
            ToolKind::Custom => {}
        }
        let custom = match UserConfig::load(&self.state.custom_config_path()) {
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
        let (program, args) = tool
            .update
            .split_first()
            .expect("validated user update command");
        self.modal = Modal::AddCommand {
            mode: CommandFormMode::Edit,
            original_name: Some(selected.name.clone()),
            field: 1,
            name: TextInput::new(selected.name),
            command: TextInput::new(format_editable_command(program, args)),
        };
    }

    fn toml_editor_path(&self) -> Result<PathBuf> {
        if let Some(path) = &self.config_path {
            return Ok(path.clone());
        }
        Ok(self.state.custom_config_path())
    }

    fn toml_editor_text(&self, path: &Path) -> Result<String> {
        if path.is_file() {
            return Ok(std::fs::read_to_string(path)?);
        }

        let selected_name = self
            .focused_tool()
            .map(|tool| tool.name.clone())
            .ok_or_else(|| Error::Message("no tool is selected".to_owned()))?;
        let (manifest, _, _) = cli::load_manifest(self.config_path.clone(), &self.state)?;
        let selected = manifest
            .tools
            .get(&selected_name)
            .cloned()
            .ok_or_else(|| Error::ToolNotFound(selected_name.clone()))?;
        let mut seed = UserConfig::empty();
        seed.tools.insert(
            selected_name.clone(),
            UserTool::from_tool(&selected_name, &selected),
        );
        Ok(toml::to_string(&seed)?)
    }

    fn open_toml_editor(&mut self) {
        if self.running > 0 {
            self.message = self
                .language
                .text(
                    "Wait for the current operation before editing TOML",
                    "请等待当前操作完成后再编辑 TOML",
                )
                .to_owned();
            return;
        }
        let result = self
            .toml_editor_path()
            .and_then(|path| self.toml_editor_text(&path).map(|text| (path, text)));
        match result {
            Ok((path, text)) => self.show_toml_editor(path, text),
            Err(error) => {
                self.message = match self.language {
                    Language::English => format!("Could not open TOML editor: {error}"),
                    Language::Chinese => format!("无法打开 TOML 编辑器：{error}"),
                };
            }
        }
    }

    fn ensure_toml_editor_file(&self) -> Result<PathBuf> {
        let path = self.toml_editor_path()?;
        if !path.is_file() {
            let text = self.toml_editor_text(&path)?;
            UserConfig::save_text(&path, &text)?;
        }
        Ok(path)
    }

    fn open_toml_in_system_editor(&mut self) {
        if self.running > 0 {
            self.message = self
                .language
                .text(
                    "Wait for the current operation before editing TOML",
                    "请等待当前操作完成后再编辑 TOML",
                )
                .to_owned();
            return;
        }
        match self
            .ensure_toml_editor_file()
            .and_then(|path| launch_system_text_editor(&path).map(|()| path))
        {
            Ok(path) => {
                self.message = match self.language {
                    Language::English => {
                        format!("Opened {} in the system text editor", path.display())
                    }
                    Language::Chinese => {
                        format!("已使用系统文本编辑器打开 {}", path.display())
                    }
                };
            }
            Err(error) => {
                self.message = match self.language {
                    Language::English => {
                        format!("Could not open the system text editor: {error}")
                    }
                    Language::Chinese => format!("无法打开系统文本编辑器：{error}"),
                };
            }
        }
    }

    fn open_toml_file(&mut self, path: PathBuf) -> Result<()> {
        let text = std::fs::read_to_string(&path)?;
        self.config_path = Some(path.clone());
        self.show_toml_editor(path, text);
        Ok(())
    }

    fn show_toml_editor(&mut self, path: PathBuf, text: String) {
        self.message = match self.language {
            Language::English => format!("Editing {}", path.display()),
            Language::Chinese => format!("正在编辑 {}", path.display()),
        };
        self.modal = Modal::TomlEditor {
            editor: TomlEditor::new(path, text),
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

fn system_text_editor_command(path: &Path) -> (OsString, Vec<OsString>) {
    #[cfg(windows)]
    {
        (
            OsString::from("notepad.exe"),
            vec![path.as_os_str().to_owned()],
        )
    }
    #[cfg(target_os = "macos")]
    {
        (
            OsString::from("open"),
            vec![OsString::from("-t"), path.as_os_str().to_owned()],
        )
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        (
            OsString::from("xdg-open"),
            vec![path.as_os_str().to_owned()],
        )
    }
}

fn launch_system_text_editor(path: &Path) -> Result<()> {
    let (program, arguments) = system_text_editor_command(path);
    Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<u8> {
    while !app.should_quit {
        app.frame = app.frame.wrapping_add(1);
        app.process_events();
        if let Some(error) = app.initial_load_error.take() {
            return Err(Error::Message(error));
        }
        if app.initial_load.is_none()
            && app.last_job_refresh.elapsed() >= app.job_refresh_interval()
        {
            let _ = app.refresh_jobs();
        }
        if app.initial_load.is_none()
            && !app.release_probe_running
            && !app.release_monitor_running
            && app.last_release_refresh.elapsed()
                >= Duration::from_secs(app.settings.github.poll_interval_secs)
        {
            app.start_release_probe(false);
        }
        if app.initial_load.is_none()
            && app.github_api_key_configured
            && !app.github_rate_limit_loading
            && app.last_github_rate_limit_refresh.elapsed() >= GITHUB_RATE_LIMIT_REFRESH_INTERVAL
        {
            app.start_github_rate_limit_refresh();
        }
        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(TICK_RATE)? {
            let mut events = vec![event::read()?];
            while event::poll(Duration::ZERO)? {
                events.push(event::read()?);
            }
            handle_event_batch(app, events);
        }
    }
    Ok(0)
}

fn handle_event_batch(app: &mut App, events: impl IntoIterator<Item = Event>) {
    let mut pending_editor_text = String::new();

    for event in events {
        if matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Release) {
            continue;
        }
        if matches!(app.modal, Modal::TomlEditor { .. })
            && let Some(character) = toml_editor_input_character(&event)
        {
            pending_editor_text.push(character);
            continue;
        }
        flush_toml_editor_input(app, &mut pending_editor_text);
        dispatch_event(app, event);
    }

    flush_toml_editor_input(app, &mut pending_editor_text);
}

fn toml_editor_input_character(event: &Event) -> Option<char> {
    let Event::Key(key) = event else {
        return None;
    };
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(character) => Some(character),
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

fn flush_toml_editor_input(app: &mut App, pending: &mut String) {
    if pending.is_empty() {
        return;
    }
    if let Modal::TomlEditor { editor } = &mut app.modal {
        editor.insert_text(pending);
    }
    pending.clear();
}

fn dispatch_event(app: &mut App, event: Event) {
    if app.initial_load.is_some() {
        if let Event::Key(key) = event
            && key.kind != KeyEventKind::Release
        {
            if is_ctrl_c(&key) {
                request_ctrl_c_quit(app);
            } else {
                app.ctrl_c_armed = false;
            }
        }
        return;
    }
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => handle_key(app, key),
        Event::Paste(text) => handle_paste(app, &text),
        Event::Mouse(mouse) => handle_mouse(app, mouse),
        Event::Resize(_, _) => {
            if let Modal::TomlEditor { editor } = &mut app.modal {
                editor.follow_cursor = true;
            }
        }
        _ => {}
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if !matches!(app.modal, Modal::None) {
        handle_modal_key(app, key);
        return;
    }
    if is_ctrl_c(&key) {
        request_ctrl_c_quit(app);
        return;
    }
    app.ctrl_c_armed = false;
    if is_shift_tab(&key) {
        toggle_process_strategy(app);
        return;
    }
    if is_language_toggle(&app.modal, &key) {
        let previous_setting = app.settings.language;
        let language = app.language.toggle();
        app.settings.language = language;
        if let Err(error) = app.settings.save(&app.state.settings_path()) {
            app.settings.language = previous_setting;
            app.report_settings_save_error(error);
            return;
        }
        app.language = language;
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

fn github_api_key_submission(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn parse_u64_input(input: &TextInput, field: &str) -> Result<u64> {
    input
        .value
        .trim()
        .parse::<u64>()
        .map_err(|_| Error::InvalidConfig(format!("{field} must be a valid byte count")))
}

fn parse_usize_input(input: &TextInput, field: &str) -> Result<usize> {
    input
        .value
        .trim()
        .parse::<usize>()
        .map_err(|_| Error::InvalidConfig(format!("{field} must be a valid non-negative integer")))
}

#[allow(clippy::too_many_arguments)]
fn github_monitor_from_form(
    name: &TextInput,
    repository: &TextInput,
    asset_regex: &TextInput,
    target_directory: &TextInput,
    format: ReleaseAssetFormat,
    update_policy: ReleaseUpdatePolicy,
    cleanup_installer: bool,
    max_download_bytes: &TextInput,
    max_extracted_bytes: &TextInput,
    max_extracted_files: &TextInput,
    strip_components: &TextInput,
    enabled: bool,
) -> Result<GithubReleaseMonitor> {
    Ok(GithubReleaseMonitor {
        name: name.value.trim().to_owned(),
        repository: repository.value.trim().to_owned(),
        asset_regex: asset_regex.value.trim().to_owned(),
        target_directory: PathBuf::from(target_directory.value.trim()),
        format,
        update_policy,
        cleanup_installer,
        max_download_bytes: parse_u64_input(max_download_bytes, "max download size")?,
        max_extracted_bytes: parse_u64_input(max_extracted_bytes, "max extracted size")?,
        max_extracted_files: parse_usize_input(max_extracted_files, "max extracted files")?,
        strip_components: parse_usize_input(strip_components, "strip components")?,
        enabled,
    })
}

fn release_format_label(format: ReleaseAssetFormat) -> &'static str {
    match format {
        ReleaseAssetFormat::File => "file",
        ReleaseAssetFormat::Zip => "zip",
        ReleaseAssetFormat::TarGz => "tar_gz",
        ReleaseAssetFormat::Dmg => "dmg",
    }
}

fn next_release_format(format: ReleaseAssetFormat) -> ReleaseAssetFormat {
    match format {
        ReleaseAssetFormat::File => ReleaseAssetFormat::Zip,
        ReleaseAssetFormat::Zip => ReleaseAssetFormat::TarGz,
        ReleaseAssetFormat::TarGz => ReleaseAssetFormat::Dmg,
        ReleaseAssetFormat::Dmg => ReleaseAssetFormat::File,
    }
}

fn previous_release_format(format: ReleaseAssetFormat) -> ReleaseAssetFormat {
    match format {
        ReleaseAssetFormat::File => ReleaseAssetFormat::Dmg,
        ReleaseAssetFormat::Zip => ReleaseAssetFormat::File,
        ReleaseAssetFormat::TarGz => ReleaseAssetFormat::Zip,
        ReleaseAssetFormat::Dmg => ReleaseAssetFormat::TarGz,
    }
}

fn release_update_policy_label(policy: ReleaseUpdatePolicy, language: Language) -> &'static str {
    match (policy, language) {
        (ReleaseUpdatePolicy::Manual, Language::English) => "manual",
        (ReleaseUpdatePolicy::Manual, Language::Chinese) => "手动确认",
        (ReleaseUpdatePolicy::Automatic, Language::English) => "automatic",
        (ReleaseUpdatePolicy::Automatic, Language::Chinese) => "自动安装",
    }
}

fn next_release_update_policy(policy: ReleaseUpdatePolicy) -> ReleaseUpdatePolicy {
    match policy {
        ReleaseUpdatePolicy::Manual => ReleaseUpdatePolicy::Automatic,
        ReleaseUpdatePolicy::Automatic => ReleaseUpdatePolicy::Manual,
    }
}

fn automatic_release_update_names(
    monitors: &[GithubReleaseMonitor],
    statuses: &[MonitorStatus],
) -> Vec<String> {
    monitors
        .iter()
        .filter(|monitor| {
            monitor.enabled && monitor.update_policy == ReleaseUpdatePolicy::Automatic
        })
        .filter(|monitor| {
            statuses
                .iter()
                .find(|status| status.name == monitor.name)
                .is_some_and(|status| {
                    status.error.is_none()
                        && status.latest_tag.is_some()
                        && !monitor_status_is_current(status)
                })
        })
        .map(|monitor| monitor.name.clone())
        .collect()
}

fn monitor_status_is_current(status: &MonitorStatus) -> bool {
    match (&status.installed_tag, &status.latest_tag) {
        (Some(installed), Some(latest)) => release::release_versions_match(installed, latest),
        _ => false,
    }
}

fn update_monitor_status(
    statuses: &mut Vec<MonitorStatus>,
    name: &str,
    tag: &str,
    asset: Option<&str>,
    error: Option<String>,
) {
    if let Some(status) = statuses.iter_mut().find(|status| status.name == name) {
        status.installed_tag = Some(tag.to_owned());
        status.latest_tag = Some(tag.to_owned());
        if let Some(asset) = asset {
            status.asset = Some(asset.to_owned());
        }
        status.error = error;
        return;
    }
    statuses.push(MonitorStatus {
        name: name.to_owned(),
        installed_tag: Some(tag.to_owned()),
        latest_tag: Some(tag.to_owned()),
        asset: asset.map(str::to_owned),
        error,
    });
}

fn handle_modal_key(app: &mut App, key: KeyEvent) {
    if matches!(app.modal, Modal::TomlEditor { .. }) {
        handle_toml_editor_key(app, key);
        return;
    }
    if matches!(app.modal, Modal::GithubApiKey { .. }) {
        let Modal::GithubApiKey { mut api_key } = std::mem::replace(&mut app.modal, Modal::None)
        else {
            unreachable!();
        };
        let key = if is_ctrl_c(&key) {
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        } else {
            key
        };
        match key.code {
            KeyCode::Esc => {
                api_key.value.zeroize();
                app.message = app
                    .language
                    .text("GitHub API key unchanged", "GitHub API Key 未更改")
                    .to_owned();
            }
            KeyCode::Enter => {
                let submitted = github_api_key_submission(&api_key.value);
                let configured = submitted.is_some();
                let result: Result<AppSettings> = (|| {
                    let encrypted_api_key = submitted
                        .map(credential::encrypt_github_api_key)
                        .transpose()?;
                    credential::has_github_api_key(encrypted_api_key.as_deref())?;
                    let mut settings = app.settings.clone();
                    settings.github.encrypted_api_key = encrypted_api_key;
                    settings.save(&app.state.settings_path())?;
                    Ok(settings)
                })();
                match result {
                    Ok(settings) => {
                        api_key.value.zeroize();
                        app.github_credential_probe_id =
                            app.github_credential_probe_id.wrapping_add(1).max(1);
                        app.settings = settings;
                        app.github_api_key_configured = configured;
                        app.github_credential_error = None;
                        app.clear_github_rate_limit();
                        app.message = match (app.github_api_key_configured, app.language) {
                            (true, Language::English) => {
                                "GitHub API key encrypted and saved in settings.toml".to_owned()
                            }
                            (true, Language::Chinese) => {
                                "GitHub API Key 已加密保存到 settings.toml".to_owned()
                            }
                            (false, Language::English) => "GitHub API key removed".to_owned(),
                            (false, Language::Chinese) => "GitHub API Key 已删除".to_owned(),
                        };
                        if app.github_api_key_configured {
                            app.start_all_tool_version_probes();
                            app.start_github_rate_limit_refresh();
                        }
                    }
                    Err(error) => {
                        app.message = match (&error, app.language) {
                            (Error::SettingsWrite { path, .. }, Language::English) => format!(
                                "settings.toml is busy or not writable: {}. Close the editor or process using it, then press Enter to retry",
                                path.display()
                            ),
                            (Error::SettingsWrite { path, .. }, Language::Chinese) => format!(
                                "settings.toml 被占用或拒绝写入：{}。请关闭配置编辑器或其他占用进程后直接按 Enter 重试",
                                path.display()
                            ),
                            (_, Language::English) => format!(
                                "GitHub API key was not saved: {error}. Correct it and press Enter to retry, or press Esc"
                            ),
                            (_, Language::Chinese) => format!(
                                "GitHub API Key 未保存：{error}。请修正后按 Enter 重试，或按 Esc 取消"
                            ),
                        };
                        app.modal = Modal::GithubApiKey { api_key };
                    }
                }
            }
            _ => {
                handle_text_input_key(&mut api_key, key);
                app.modal = Modal::GithubApiKey { api_key };
            }
        }
        return;
    }
    let key = if is_ctrl_c(&key) {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    } else {
        key
    };
    match app.modal.clone() {
        Modal::ConfirmGithubMonitorUpdate { monitors } => match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                app.modal = Modal::None;
                app.start_release_updates(monitors);
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => app.modal = Modal::None,
            _ => {}
        },
        Modal::ConfirmUpdate {
            tools,
            target_version,
            current_tools,
        } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                app.modal = Modal::None;
                app.start_updates(tools, target_version, current_tools);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.modal = Modal::None,
            _ => {}
        },
        Modal::TargetVersion { name, mut version } => {
            match key.code {
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    return;
                }
                KeyCode::Enter => {
                    let value = version.value.trim().to_owned();
                    if value.is_empty() {
                        app.message = app
                            .language
                            .text("Version cannot be empty", "版本不能为空")
                            .to_owned();
                    } else {
                        app.modal = Modal::ConfirmUpdate {
                            tools: vec![name],
                            target_version: Some(value),
                            current_tools: Vec::new(),
                        };
                        return;
                    }
                }
                _ => {
                    handle_text_input_key(&mut version, key);
                }
            }
            app.modal = Modal::TargetVersion { name, version };
        }
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
        Modal::NetworkProxy {
            mut proxy_mode,
            mut field,
            mut proxy_url,
            mut no_proxy,
        } => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    return;
                }
                KeyCode::Char('s' | 'S') if ctrl => {
                    if app.save_network_settings(
                        proxy_mode,
                        proxy_url.value.clone(),
                        no_proxy.value.clone(),
                    ) {
                        app.modal = Modal::None;
                        return;
                    }
                }
                KeyCode::Left if field == 0 => proxy_mode = proxy_mode.previous(),
                KeyCode::Right | KeyCode::Char(' ') if field == 0 => {
                    proxy_mode = proxy_mode.next();
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    field = if proxy_mode == ProxyMode::Explicit {
                        if key.code == KeyCode::BackTab {
                            (field + 2) % 3
                        } else {
                            (field + 1) % 3
                        }
                    } else {
                        0
                    };
                    proxy_url.clear_selection();
                    no_proxy.clear_selection();
                }
                KeyCode::Up | KeyCode::Down => {
                    field = if proxy_mode == ProxyMode::Explicit {
                        if key.code == KeyCode::Up {
                            (field + 2) % 3
                        } else {
                            (field + 1) % 3
                        }
                    } else {
                        0
                    };
                    proxy_url.clear_selection();
                    no_proxy.clear_selection();
                }
                KeyCode::Enter if field == 0 && proxy_mode == ProxyMode::Explicit => field = 1,
                KeyCode::Enter if field == 1 => field = 2,
                KeyCode::Enter => {
                    if app.save_network_settings(
                        proxy_mode,
                        proxy_url.value.clone(),
                        no_proxy.value.clone(),
                    ) {
                        app.modal = Modal::None;
                        return;
                    }
                }
                _ if proxy_mode == ProxyMode::Explicit && field == 1 => {
                    handle_text_input_key(&mut proxy_url, key);
                }
                _ if proxy_mode == ProxyMode::Explicit && field == 2 => {
                    handle_text_input_key(&mut no_proxy, key);
                }
                _ => {}
            }
            app.modal = Modal::NetworkProxy {
                proxy_mode,
                field,
                proxy_url,
                no_proxy,
            };
        }
        Modal::GithubMonitorForm {
            mode,
            original_index,
            mut field,
            mut name,
            mut repository,
            mut asset_regex,
            mut target_directory,
            mut format,
            mut update_policy,
            mut cleanup_installer,
            mut max_download_bytes,
            mut max_extracted_bytes,
            mut max_extracted_files,
            mut strip_components,
            mut enabled,
        } => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let save = matches!(key.code, KeyCode::Enter)
                || ctrl && matches!(key.code, KeyCode::Char('s' | 'S'));
            match key.code {
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    return;
                }
                KeyCode::Tab | KeyCode::Down => {
                    field = (field + 1) % GITHUB_MONITOR_FORM_FIELD_COUNT;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    field = (field + GITHUB_MONITOR_FORM_FIELD_COUNT - 1)
                        % GITHUB_MONITOR_FORM_FIELD_COUNT;
                }
                KeyCode::Left if field == 4 => format = previous_release_format(format),
                KeyCode::Right | KeyCode::Char(' ') if field == 4 => {
                    format = next_release_format(format);
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if field == 5 => {
                    update_policy = next_release_update_policy(update_policy);
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if field == 6 => {
                    cleanup_installer = !cleanup_installer;
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if field == 11 => {
                    enabled = !enabled;
                }
                _ if !save => {
                    let input = match field {
                        0 => Some(&mut name),
                        1 => Some(&mut repository),
                        2 => Some(&mut asset_regex),
                        3 => Some(&mut target_directory),
                        7 => Some(&mut max_download_bytes),
                        8 => Some(&mut max_extracted_bytes),
                        9 => Some(&mut max_extracted_files),
                        10 => Some(&mut strip_components),
                        _ => None,
                    };
                    if let Some(input) = input {
                        handle_text_input_key(input, key);
                    }
                }
                _ => {}
            }
            if save {
                match github_monitor_from_form(
                    &name,
                    &repository,
                    &asset_regex,
                    &target_directory,
                    format,
                    update_policy,
                    cleanup_installer,
                    &max_download_bytes,
                    &max_extracted_bytes,
                    &max_extracted_files,
                    &strip_components,
                    enabled,
                )
                .and_then(|monitor| app.save_github_monitor(original_index, monitor))
                {
                    Ok(index) => {
                        app.github_monitor_index = index;
                        app.tool_view = ToolView::Github;
                        app.modal = Modal::None;
                        app.message = match (mode, app.language) {
                            (MonitorFormMode::Add, Language::English) => {
                                "GitHub repository monitor added".to_owned()
                            }
                            (MonitorFormMode::Add, Language::Chinese) => {
                                "已添加 GitHub 仓库监控".to_owned()
                            }
                            (MonitorFormMode::Edit, Language::English) => {
                                "GitHub repository monitor saved".to_owned()
                            }
                            (MonitorFormMode::Edit, Language::Chinese) => {
                                "已保存 GitHub 仓库监控".to_owned()
                            }
                        };
                        app.start_release_probe(false);
                        return;
                    }
                    Err(error) => {
                        app.message = match app.language {
                            Language::English => format!("GitHub monitor was not saved: {error}"),
                            Language::Chinese => format!("GitHub 监控未保存：{error}"),
                        };
                    }
                }
            }
            app.modal = Modal::GithubMonitorForm {
                mode,
                original_index,
                field,
                name,
                repository,
                asset_regex,
                target_directory,
                format,
                update_policy,
                cleanup_installer,
                max_download_bytes,
                max_extracted_bytes,
                max_extracted_files,
                strip_components,
                enabled,
            };
        }
        Modal::ConfirmDeleteGithubMonitor { index } => match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                app.github_monitor_index = app.delete_github_monitor(index);
                app.modal = Modal::None;
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => app.modal = Modal::None,
            _ => app.modal = Modal::ConfirmDeleteGithubMonitor { index },
        },
        Modal::GithubPollInterval { mut seconds } => {
            match key.code {
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    return;
                }
                KeyCode::Enter => match seconds.value.trim().parse::<u64>() {
                    Ok(value) if (60..=86_400).contains(&value) => {
                        let mut settings = app.settings.clone();
                        settings.github.poll_interval_secs = value;
                        match settings.save(&app.state.settings_path()) {
                            Ok(()) => {
                                app.settings = settings;
                                app.last_release_refresh = Instant::now();
                                app.message = match app.language {
                                    Language::English => {
                                        format!("GitHub monitor interval set to {value} seconds")
                                    }
                                    Language::Chinese => {
                                        format!("GitHub 监控间隔已设为 {value} 秒")
                                    }
                                };
                                app.modal = Modal::None;
                                return;
                            }
                            Err(error) => app.report_settings_save_error(error),
                        }
                    }
                    _ => {
                        app.message = app
                            .language
                            .text(
                                "Interval must be an integer from 60 to 86400 seconds",
                                "间隔必须是 60 到 86400 之间的整数秒数",
                            )
                            .to_owned();
                    }
                },
                _ => {
                    handle_text_input_key(&mut seconds, key);
                }
            }
            app.modal = Modal::GithubPollInterval { seconds };
        }
        Modal::GithubApiKey { .. } => unreachable!("GitHub API key keys are handled first"),
        Modal::TomlEditor { .. } => unreachable!("TOML editor keys are handled first"),
        Modal::None => {}
    }
}

fn handle_toml_editor_key(app: &mut App, key: KeyEvent) {
    let mut editor = match std::mem::replace(&mut app.modal, Modal::None) {
        Modal::TomlEditor { editor } => editor,
        modal => {
            app.modal = modal;
            return;
        }
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let extend_selection = key.modifiers.contains(KeyModifiers::SHIFT);

    if key.code == KeyCode::F(2) && !ctrl && !alt {
        editor.toggle_vim_mode();
        app.message = match (editor.mode.is_vim(), app.language) {
            (true, Language::English) => "Vim mode enabled — NORMAL".to_owned(),
            (true, Language::Chinese) => "已启用 Vim 模式 — NORMAL".to_owned(),
            (false, Language::English) => "Standard editor mode enabled".to_owned(),
            (false, Language::Chinese) => "已切换到标准编辑模式".to_owned(),
        };
        app.modal = Modal::TomlEditor { editor };
        return;
    }

    if editor.mode == TomlEditorMode::VimInsert && key.code == KeyCode::Esc {
        editor.enter_vim_mode(TomlEditorMode::VimNormal);
        app.message = app.language.text("Vim NORMAL", "Vim NORMAL").to_owned();
        app.modal = Modal::TomlEditor { editor };
        return;
    }

    if matches!(
        editor.mode,
        TomlEditorMode::VimNormal | TomlEditorMode::VimVisual
    ) && !ctrl
        && !alt
    {
        if let Some((english, chinese)) = handle_vim_toml_key(&mut editor, &key) {
            app.message = app.language.text(english, chinese).to_owned();
        }
        app.modal = Modal::TomlEditor { editor };
        return;
    }

    match key.code {
        KeyCode::Char('q' | 'Q') if ctrl => {
            app.modal = Modal::None;
            app.toml_editor_hitbox = None;
            app.toml_editor_drag = None;
            app.message = app
                .language
                .text("TOML editor closed", "已关闭 TOML 编辑器")
                .to_owned();
            return;
        }
        KeyCode::Esc => {
            app.modal = Modal::None;
            app.toml_editor_hitbox = None;
            app.toml_editor_drag = None;
            app.message = app
                .language
                .text("TOML editor closed", "已关闭 TOML 编辑器")
                .to_owned();
            return;
        }
        KeyCode::Char('z' | 'Z') if ctrl && extend_selection => {
            app.message = if editor.redo() {
                app.language
                    .text("Redid TOML edit", "已重做 TOML 编辑")
                    .to_owned()
            } else {
                app.language
                    .text("Nothing to redo", "没有可重做的编辑")
                    .to_owned()
            };
        }
        KeyCode::Char('z' | 'Z') if ctrl => {
            app.message = if editor.undo() {
                app.language
                    .text("Undid TOML edit", "已撤销 TOML 编辑")
                    .to_owned()
            } else {
                app.language
                    .text("Nothing to undo", "没有可撤销的编辑")
                    .to_owned()
            };
        }
        KeyCode::Char('y' | 'Y') if ctrl => {
            app.message = if editor.redo() {
                app.language
                    .text("Redid TOML edit", "已重做 TOML 编辑")
                    .to_owned()
            } else {
                app.language
                    .text("Nothing to redo", "没有可重做的编辑")
                    .to_owned()
            };
        }
        KeyCode::Char('r' | 'R') if ctrl && editor.mode.is_vim() => {
            app.message = if editor.redo() {
                app.language
                    .text("Redid TOML edit", "已重做 TOML 编辑")
                    .to_owned()
            } else {
                app.language
                    .text("Nothing to redo", "没有可重做的编辑")
                    .to_owned()
            };
        }
        _ if is_toml_comment_toggle(&key) => {
            app.message = match editor.toggle_line_comments() {
                Some(TomlCommentAction::Commented) => app
                    .language
                    .text("Commented TOML line(s)", "已注释 TOML 行")
                    .to_owned(),
                Some(TomlCommentAction::Uncommented) => app
                    .language
                    .text("Uncommented TOML line(s)", "已取消 TOML 行注释")
                    .to_owned(),
                None => app
                    .language
                    .text("No TOML line to comment", "没有可注释的 TOML 行")
                    .to_owned(),
            };
        }
        KeyCode::Char('s' | 'S') if ctrl => match UserConfig::save_text(&editor.path, &editor.text)
        {
            Ok(()) => {
                editor.mark_saved();
                app.message = match app.refresh_tools() {
                    Ok(()) => match app.language {
                        Language::English => format!("Saved {}", editor.path.display()),
                        Language::Chinese => format!("已保存 {}", editor.path.display()),
                    },
                    Err(error) => match app.language {
                        Language::English => format!("TOML saved but refresh failed: {error}"),
                        Language::Chinese => format!("TOML 已保存，但刷新失败：{error}"),
                    },
                };
            }
            Err(error) => {
                app.message = match app.language {
                    Language::English => format!("TOML was not saved: {error}"),
                    Language::Chinese => format!("TOML 未保存：{error}"),
                };
            }
        },
        KeyCode::Char('c' | 'C') if ctrl => {
            let Some(selected) = editor.selected_text() else {
                app.message = app
                    .language
                    .text(
                        "Select TOML text before copying",
                        "请先选择要复制的 TOML 文本",
                    )
                    .to_owned();
                app.modal = Modal::TomlEditor { editor };
                return;
            };
            match arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.set_text(selected.to_owned()))
            {
                Ok(()) => {
                    app.message = app
                        .language
                        .text("Copied selection", "已复制选区")
                        .to_owned()
                }
                Err(error) => {
                    app.message = match app.language {
                        Language::English => format!("Could not copy selection: {error}"),
                        Language::Chinese => format!("无法复制选区：{error}"),
                    }
                }
            }
        }
        KeyCode::Char('v' | 'V') if ctrl => {
            match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
                Ok(text) => {
                    editor.insert_text(&text);
                    app.message = app
                        .language
                        .text("Pasted clipboard text", "已粘贴剪贴板文本")
                        .to_owned();
                }
                Err(error) => {
                    app.message = match app.language {
                        Language::English => format!("Could not paste clipboard text: {error}"),
                        Language::Chinese => format!("无法粘贴剪贴板文本：{error}"),
                    }
                }
            }
        }
        KeyCode::Char('a' | 'A') if ctrl => editor.select_all(),
        KeyCode::Left => editor.move_left(extend_selection),
        KeyCode::Right => editor.move_right(extend_selection),
        KeyCode::Up => editor.move_vertical(-1, extend_selection),
        KeyCode::Down => editor.move_vertical(1, extend_selection),
        KeyCode::Home => editor.move_home(ctrl, extend_selection),
        KeyCode::End => editor.move_end(ctrl, extend_selection),
        KeyCode::PageUp => {
            if let Some(hitbox) = app.toml_editor_hitbox {
                let page = usize::from(hitbox.area.height).saturating_sub(1).max(1);
                editor.move_vertical(-(page as isize), extend_selection);
            }
        }
        KeyCode::PageDown => {
            if let Some(hitbox) = app.toml_editor_hitbox {
                let page = usize::from(hitbox.area.height).saturating_sub(1).max(1);
                editor.move_vertical(page as isize, extend_selection);
            }
        }
        KeyCode::Backspace => editor.backspace(),
        KeyCode::Delete => editor.delete(),
        KeyCode::Enter => editor.insert_text("\n"),
        KeyCode::Tab => editor.insert_text("\t"),
        KeyCode::Char(character) if !ctrl && !alt => editor.insert_text(&character.to_string()),
        _ => {}
    }

    app.modal = Modal::TomlEditor { editor };
}

fn handle_vim_toml_key(
    editor: &mut TomlEditor,
    key: &KeyEvent,
) -> Option<(&'static str, &'static str)> {
    if editor.mode == TomlEditorMode::VimNormal
        && let Some(pending) = editor.vim_pending.take()
    {
        match (pending, key.code) {
            ('g', KeyCode::Char('g')) => {
                editor.move_home(true, false);
                return Some(("Moved to start of file", "已移动到文件开头"));
            }
            ('d', KeyCode::Char('d')) => {
                editor.vim_delete_current_line();
                return Some(("Deleted TOML line", "已删除 TOML 行"));
            }
            ('y', KeyCode::Char('y')) => {
                editor.vim_yank_current_line();
                return Some(("Yanked TOML line", "已复制 TOML 行"));
            }
            _ => {}
        }
    }

    let visual = editor.mode == TomlEditorMode::VimVisual;
    if visual {
        match key.code {
            KeyCode::Esc | KeyCode::Char('v') => {
                editor.enter_vim_mode(TomlEditorMode::VimNormal);
                return Some(("Vim NORMAL", "Vim NORMAL"));
            }
            KeyCode::Char('V') => {
                editor.select_current_line();
                return Some(("Vim VISUAL LINE", "Vim VISUAL LINE"));
            }
            KeyCode::Char('y') => {
                if editor.vim_yank_selection() {
                    return Some(("Yanked selection", "已复制选区"));
                }
            }
            KeyCode::Char('d' | 'x') => {
                if editor.vim_delete_selection(false) {
                    return Some(("Deleted selection", "已删除选区"));
                }
            }
            KeyCode::Char('c') => {
                if editor.vim_delete_selection(true) {
                    return Some(("Vim INSERT", "Vim INSERT"));
                }
            }
            KeyCode::Left | KeyCode::Char('h') => editor.move_left(true),
            KeyCode::Down | KeyCode::Char('j') => editor.move_vertical(1, true),
            KeyCode::Up | KeyCode::Char('k') => editor.move_vertical(-1, true),
            KeyCode::Right | KeyCode::Char('l') => editor.move_right(true),
            KeyCode::Char('w') => editor.move_word_forward(true),
            KeyCode::Char('b') => editor.move_word_backward(true),
            KeyCode::Char('0') | KeyCode::Home => editor.move_home(false, true),
            KeyCode::Char('$') | KeyCode::End => editor.move_end(false, true),
            KeyCode::Char('G') => editor.move_end(true, true),
            KeyCode::PageUp => editor.move_vertical(-10, true),
            KeyCode::PageDown => editor.move_vertical(10, true),
            _ => {}
        }
        return None;
    }

    match key.code {
        KeyCode::Esc => {
            editor.vim_pending = None;
            Some(("Vim NORMAL", "Vim NORMAL"))
        }
        KeyCode::Left | KeyCode::Char('h') => {
            editor.move_left(false);
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            editor.move_vertical(1, false);
            None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            editor.move_vertical(-1, false);
            None
        }
        KeyCode::Right | KeyCode::Char('l') => {
            editor.move_right(false);
            None
        }
        KeyCode::Char('w') => {
            editor.move_word_forward(false);
            None
        }
        KeyCode::Char('b') => {
            editor.move_word_backward(false);
            None
        }
        KeyCode::Char('0') | KeyCode::Home => {
            editor.move_home(false, false);
            None
        }
        KeyCode::Char('$') | KeyCode::End => {
            editor.move_end(false, false);
            None
        }
        KeyCode::Char('G') => {
            editor.move_end(true, false);
            Some(("Moved to end of file", "已移动到文件末尾"))
        }
        KeyCode::Char('g' | 'd' | 'y') => {
            if let KeyCode::Char(command) = key.code {
                editor.vim_pending = Some(command);
            }
            None
        }
        KeyCode::Char('i') => {
            editor.enter_vim_mode(TomlEditorMode::VimInsert);
            Some(("Vim INSERT", "Vim INSERT"))
        }
        KeyCode::Char('a') => {
            editor.move_right(false);
            editor.enter_vim_mode(TomlEditorMode::VimInsert);
            Some(("Vim INSERT", "Vim INSERT"))
        }
        KeyCode::Char('I') => {
            editor.move_home(false, false);
            editor.enter_vim_mode(TomlEditorMode::VimInsert);
            Some(("Vim INSERT", "Vim INSERT"))
        }
        KeyCode::Char('A') => {
            editor.move_end(false, false);
            editor.enter_vim_mode(TomlEditorMode::VimInsert);
            Some(("Vim INSERT", "Vim INSERT"))
        }
        KeyCode::Char('v') => {
            editor.selection_anchor = Some(editor.cursor);
            editor.enter_vim_mode(TomlEditorMode::VimVisual);
            Some(("Vim VISUAL", "Vim VISUAL"))
        }
        KeyCode::Char('V') => {
            editor.enter_vim_mode(TomlEditorMode::VimVisual);
            editor.select_current_line();
            Some(("Vim VISUAL LINE", "Vim VISUAL LINE"))
        }
        KeyCode::Char('x') | KeyCode::Delete => {
            editor.delete();
            Some(("Deleted character", "已删除字符"))
        }
        KeyCode::Char('p') => {
            editor.vim_paste();
            Some(("Pasted Vim register", "已粘贴 Vim 寄存器"))
        }
        KeyCode::Char('u') => Some(if editor.undo() {
            ("Undid TOML edit", "已撤销 TOML 编辑")
        } else {
            ("Nothing to undo", "没有可撤销的编辑")
        }),
        KeyCode::PageUp => {
            editor.move_vertical(-10, false);
            None
        }
        KeyCode::PageDown => {
            editor.move_vertical(10, false);
            None
        }
        _ => None,
    }
}

fn handle_paste(app: &mut App, text: &str) {
    match &mut app.modal {
        Modal::TomlEditor { editor } => editor.insert_text(text),
        Modal::TargetVersion { version, .. } => version.insert_text(text),
        Modal::GithubApiKey { api_key } => api_key.insert_text(text),
        Modal::NetworkProxy {
            proxy_mode,
            field,
            proxy_url,
            no_proxy,
        } => {
            if *proxy_mode != ProxyMode::Explicit {
                return;
            }
            if *field == 1 {
                proxy_url.insert_text(text);
            } else if *field == 2 {
                no_proxy.insert_text(text);
            } else {
                return;
            }
        }
        Modal::GithubMonitorForm {
            field,
            name,
            repository,
            asset_regex,
            target_directory,
            max_download_bytes,
            max_extracted_bytes,
            max_extracted_files,
            strip_components,
            ..
        } => {
            let input = match *field {
                0 => Some(name),
                1 => Some(repository),
                2 => Some(asset_regex),
                3 => Some(target_directory),
                7 => Some(max_download_bytes),
                8 => Some(max_extracted_bytes),
                9 => Some(max_extracted_files),
                10 => Some(strip_components),
                _ => None,
            };
            let Some(input) = input else {
                return;
            };
            input.insert_text(text);
        }
        Modal::GithubPollInterval { seconds } => seconds.insert_text(text),
        _ => return,
    }
    app.message = app
        .language
        .text("Pasted terminal text", "已粘贴终端文本")
        .to_owned();
}

fn handle_normal_key(app: &mut App, key: KeyEvent) {
    if let Some(tab) = navigated_tab(app.tab, &key.code) {
        app.select_tab(tab);
        return;
    }

    match key.code {
        KeyCode::Char('r') | KeyCode::Char('R') => {
            if let Err(error) = app.refresh_tools() {
                app.report_refresh_error(error);
                return;
            }
            if app.tab == Tab::Doctor {
                request_doctor_refresh(app);
                return;
            }
            if app.tab == Tab::Tools && app.tool_view == ToolView::Github {
                app.start_release_probe(true);
                return;
            }
            if let Err(error) = app.refresh_jobs() {
                app.report_refresh_error(error);
            } else {
                app.message = app.language.text("Refreshed", "已刷新").to_owned();
            }
        }
        _ => match app.tab {
            Tab::Tools => handle_tools_key(app, key),
            Tab::Activity => handle_activity_key(app, key),
            Tab::Jobs => handle_jobs_key(app, key),
            Tab::Doctor => handle_doctor_key(app, key),
            Tab::Settings => handle_settings_key(app, key),
        },
    }
}

fn request_doctor_refresh(app: &mut App) {
    if let Err(error) = app.refresh_doctor() {
        app.message = match app.language {
            Language::English => format!("Diagnostics failed: {error}"),
            Language::Chinese => format!("诊断失败：{error}"),
        };
    }
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_toml_comment_toggle(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('/') | KeyCode::Char('7'))
}

fn is_shift_tab(key: &KeyEvent) -> bool {
    key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
}

fn is_language_toggle(modal: &Modal, key: &KeyEvent) -> bool {
    !matches!(
        modal,
        Modal::AddCommand { .. } | Modal::TargetVersion { .. } | Modal::NetworkProxy { .. }
    ) && matches!(key.code, KeyCode::Char('l') | KeyCode::Char('L'))
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

fn request_ctrl_c_quit(app: &mut App) {
    if app.ctrl_c_armed {
        app.should_quit = true;
        return;
    }

    app.ctrl_c_armed = true;
    app.message = app
        .language
        .text("Press Ctrl+C again to quit", "再次按 Ctrl+C 退出")
        .to_owned();
}

fn navigated_tab(tab: Tab, code: &KeyCode) -> Option<Tab> {
    match code {
        KeyCode::Right => Some(tab.next()),
        KeyCode::Left => Some(tab.previous()),
        _ => None,
    }
}

fn handle_tools_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Tab {
        app.select_tool_view(app.tool_view.toggle());
        return;
    }
    match app.tool_view {
        ToolView::Commands => handle_command_tools_key(app, key),
        ToolView::Github => handle_github_tools_key(app, key),
    }
}

fn handle_command_tools_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.tool_index = previous_index(app.tool_index, app.visible_tool_indices.len());
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.tool_index = next_index(app.tool_index, app.visible_tool_indices.len());
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
            let requested = app.selected_for_update();
            if requested.is_empty() {
                app.message = app
                    .language
                    .text("Select an installed tool first", "请先选择一个已安装的工具")
                    .to_owned();
            } else {
                let (tools, current_tools) = app.partition_latest_updates(requested);
                if tools.is_empty() {
                    app.mark_tools_up_to_date(&current_tools);
                    app.message = match app.language {
                        Language::English => format!(
                            "Already at the latest version: {}",
                            current_tools.join(", ")
                        ),
                        Language::Chinese => {
                            format!("已是最新版本：{}", current_tools.join("，"))
                        }
                    };
                } else {
                    app.modal = Modal::ConfirmUpdate {
                        tools,
                        target_version: None,
                        current_tools,
                    };
                }
            }
        }
        KeyCode::Char('v') | KeyCode::Char('V') => {
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
            let Some(tool) = app.focused_tool() else {
                app.message = app
                    .language
                    .text("Select an installed tool first", "请先选择一个已安装的工具")
                    .to_owned();
                return;
            };
            if tool.availability != Availability::Installed {
                app.message = app
                    .language
                    .text("Select an installed tool first", "请先选择一个已安装的工具")
                    .to_owned();
                return;
            }
            if !tool.supports_target_version {
                app.message = match app.language {
                    Language::English => format!(
                        "{} does not define an arbitrary-version update command",
                        tool.name
                    ),
                    Language::Chinese => {
                        format!("{} 未配置指定版本更新命令", tool.name)
                    }
                };
                return;
            }
            app.modal = Modal::TargetVersion {
                name: tool.name.clone(),
                version: TextInput::new(String::new()),
            };
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
        KeyCode::Char('t') | KeyCode::Char('T') => app.open_toml_editor(),
        KeyCode::Char('o') | KeyCode::Char('O') => app.open_toml_in_system_editor(),
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
            if app.config_path.is_some() {
                app.message = app
                    .language
                    .text(
                        "Delete custom tools from the explicit TOML editor (t)",
                        "请在显式 TOML 编辑器（t）中删除自定义工具",
                    )
                    .to_owned();
                return;
            }
            if let Some(tool) = app.focused_tool() {
                match tool.kind {
                    ToolKind::Custom => {
                        app.modal = Modal::ConfirmDelete {
                            name: tool.name.clone(),
                        };
                    }
                    ToolKind::BuiltIn => {
                        app.message = app
                            .language
                            .text("Built-in tools cannot be deleted", "不能删除内置工具")
                            .to_owned();
                    }
                }
            }
        }
        _ => {}
    }
}

fn handle_github_tools_key(app: &mut App, key: KeyEvent) {
    let monitor_count = app.github_monitors.len();
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.github_monitor_index = previous_index(app.github_monitor_index, monitor_count);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.github_monitor_index = next_index(app.github_monitor_index, monitor_count);
        }
        KeyCode::Char(' ') => {
            let Some(monitor) = app.focused_github_monitor() else {
                return;
            };
            let name = monitor.name.clone();
            if !monitor.enabled {
                app.message = match app.language {
                    Language::English => format!("{name} is disabled; edit it before selecting"),
                    Language::Chinese => format!("{name} 已停用；请先编辑并启用"),
                };
            } else if !app.selected_github_monitors.remove(&name) {
                app.selected_github_monitors.insert(name);
            }
        }
        KeyCode::Enter => {
            let mut requested = app
                .github_monitors
                .iter()
                .filter(|monitor| {
                    monitor.enabled && app.selected_github_monitors.contains(&monitor.name)
                })
                .map(|monitor| monitor.name.clone())
                .collect::<Vec<_>>();
            if requested.is_empty() {
                app.message = app
                    .language
                    .text(
                        "Select an enabled GitHub repository first",
                        "请先选择已启用的 GitHub 仓库",
                    )
                    .to_owned();
                return;
            }
            let current = requested
                .iter()
                .filter(|name| {
                    app.release_monitor_statuses
                        .iter()
                        .find(|status| status.name.as_str() == name.as_str())
                        .is_some_and(|status| {
                            status.latest_tag.is_some() && monitor_status_is_current(status)
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            requested.retain(|name| !current.contains(name));
            if requested.is_empty() {
                app.message = match app.language {
                    Language::English => format!(
                        "Already at the latest GitHub Release: {}",
                        current.join(", ")
                    ),
                    Language::Chinese => {
                        format!("GitHub Release 已是最新：{}", current.join("，"))
                    }
                };
                return;
            }
            app.modal = Modal::ConfirmGithubMonitorUpdate {
                monitors: requested,
            };
        }
        KeyCode::Char('a' | 'A') => {
            let should_select = app.github_monitors.iter().any(|monitor| {
                monitor.enabled && !app.selected_github_monitors.contains(&monitor.name)
            });
            app.selected_github_monitors = app
                .github_monitors
                .iter()
                .filter(|monitor| monitor.enabled && should_select)
                .map(|monitor| monitor.name.clone())
                .collect();
        }
        KeyCode::Char('c' | 'C') => {
            if app.config_path.is_some() {
                app.message = app
                    .language
                    .text(
                        "GitHub repository editing is disabled with --config",
                        "使用 --config 时不能编辑 GitHub 仓库",
                    )
                    .to_owned();
                return;
            }
            app.open_github_monitor_form(MonitorFormMode::Add, None);
        }
        KeyCode::Char('e' | 'E') => {
            if app.config_path.is_some() {
                app.message = app
                    .language
                    .text(
                        "GitHub repository editing is disabled with --config",
                        "使用 --config 时不能编辑 GitHub 仓库",
                    )
                    .to_owned();
                return;
            }
            if monitor_count > 0 {
                app.open_github_monitor_form(MonitorFormMode::Edit, Some(app.github_monitor_index));
            }
        }
        KeyCode::Char('t' | 'T') => app.open_toml_editor(),
        KeyCode::Char('o' | 'O') => app.open_toml_in_system_editor(),
        KeyCode::Char('d' | 'D') if monitor_count > 0 => {
            if app.config_path.is_some() {
                app.message = app
                    .language
                    .text(
                        "GitHub repository editing is disabled with --config",
                        "使用 --config 时不能编辑 GitHub 仓库",
                    )
                    .to_owned();
                return;
            }
            app.modal = Modal::ConfirmDeleteGithubMonitor {
                index: app.github_monitor_index,
            };
        }
        KeyCode::Char('r' | 'R') => app.start_release_probe(true),
        _ => {}
    }
}

fn toggle_tool_selection(app: &mut App, index: usize) {
    let Some(tool_index) = app.visible_tool_indices.get(index).copied() else {
        return;
    };
    let Some(tool) = app.tools.get_mut(tool_index) else {
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
                Tab::Tools => match app.tool_view {
                    ToolView::Commands => {
                        if let Some(index) =
                            hitbox_target(&app.tool_hitboxes, mouse.column, mouse.row)
                        {
                            app.tool_index = index;
                        }
                    }
                    ToolView::Github => {
                        if let Some(index) =
                            hitbox_target(&app.github_monitor_hitboxes, mouse.column, mouse.row)
                        {
                            app.github_monitor_index = index;
                        }
                    }
                },
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
                Tab::Settings => {
                    if let Some(index) =
                        hitbox_target(&app.settings_hitboxes, mouse.column, mouse.row)
                    {
                        app.settings_index = index;
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
                if let Some(index) = hitbox_target(&app.tool_view_hitboxes, mouse.column, mouse.row)
                {
                    app.select_tool_view(if index == 0 {
                        ToolView::Commands
                    } else {
                        ToolView::Github
                    });
                } else {
                    match app.tool_view {
                        ToolView::Commands => {
                            if let Some(index) =
                                hitbox_target(&app.tool_hitboxes, mouse.column, mouse.row)
                            {
                                app.tool_index = index;
                                toggle_tool_selection(app, index);
                            }
                        }
                        ToolView::Github => {
                            if let Some(index) =
                                hitbox_target(&app.github_monitor_hitboxes, mouse.column, mouse.row)
                            {
                                app.github_monitor_index = index;
                                handle_github_tools_key(
                                    app,
                                    KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
                                );
                            }
                        }
                    }
                }
            }
            Tab::Doctor => {
                if let Some(index) = hitbox_target(&app.doctor_hitboxes, mouse.column, mouse.row) {
                    app.doctor_index = index;
                    app.toggle_doctor_detail();
                }
            }
            Tab::Settings => {
                if let Some(index) = hitbox_target(&app.settings_hitboxes, mouse.column, mouse.row)
                {
                    app.settings_index = index;
                    app.toggle_setting(index);
                }
            }
        },
        MouseEventKind::ScrollUp => match app.tab {
            Tab::Activity => {
                app.activity_scroll = app
                    .activity_scroll
                    .saturating_sub(MOUSE_WHEEL_ROWS as usize);
            }
            Tab::Tools => match app.tool_view {
                ToolView::Commands => {
                    app.tool_index = app
                        .tool_viewport
                        .scroll_at(mouse.column, mouse.row, -MOUSE_WHEEL_ROWS)
                        .unwrap_or_else(|| {
                            scrolled_index(
                                app.tool_index,
                                app.visible_tool_indices.len(),
                                -MOUSE_WHEEL_ROWS,
                            )
                        });
                }
                ToolView::Github => {
                    app.github_monitor_index = app
                        .github_monitor_viewport
                        .scroll_at(mouse.column, mouse.row, -MOUSE_WHEEL_ROWS)
                        .unwrap_or_else(|| {
                            scrolled_index(
                                app.github_monitor_index,
                                app.github_monitors.len(),
                                -MOUSE_WHEEL_ROWS,
                            )
                        });
                }
            },
            Tab::Jobs => {
                if contains(app.job_detail_area, mouse.column, mouse.row) {
                    app.job_log_scroll =
                        app.job_log_scroll.saturating_sub(MOUSE_WHEEL_ROWS as usize);
                } else {
                    app.job_index = app
                        .job_viewport
                        .scroll_at(mouse.column, mouse.row, -MOUSE_WHEEL_ROWS)
                        .unwrap_or_else(|| {
                            scrolled_index(app.job_index, app.jobs.len(), -MOUSE_WHEEL_ROWS)
                        });
                }
            }
            Tab::Doctor => {
                if contains(app.doctor_detail_area, mouse.column, mouse.row) {
                    app.doctor_detail_scroll = app
                        .doctor_detail_scroll
                        .saturating_sub(MOUSE_WHEEL_ROWS as usize);
                } else {
                    app.doctor_index = app
                        .doctor_viewport
                        .scroll_at(mouse.column, mouse.row, -MOUSE_WHEEL_ROWS)
                        .unwrap_or_else(|| {
                            scrolled_index(
                                app.doctor_index,
                                app.visible_doctor_count(),
                                -MOUSE_WHEEL_ROWS,
                            )
                        });
                }
            }
            Tab::Settings => {
                app.settings_index = scrolled_index(app.settings_index, SETTINGS_ROW_COUNT, -1);
            }
        },
        MouseEventKind::ScrollDown => match app.tab {
            Tab::Activity => {
                app.activity_scroll = app
                    .activity_scroll
                    .saturating_add(MOUSE_WHEEL_ROWS as usize);
            }
            Tab::Tools => match app.tool_view {
                ToolView::Commands => {
                    app.tool_index = app
                        .tool_viewport
                        .scroll_at(mouse.column, mouse.row, MOUSE_WHEEL_ROWS)
                        .unwrap_or_else(|| {
                            scrolled_index(
                                app.tool_index,
                                app.visible_tool_indices.len(),
                                MOUSE_WHEEL_ROWS,
                            )
                        });
                }
                ToolView::Github => {
                    app.github_monitor_index = app
                        .github_monitor_viewport
                        .scroll_at(mouse.column, mouse.row, MOUSE_WHEEL_ROWS)
                        .unwrap_or_else(|| {
                            scrolled_index(
                                app.github_monitor_index,
                                app.github_monitors.len(),
                                MOUSE_WHEEL_ROWS,
                            )
                        });
                }
            },
            Tab::Jobs => {
                if contains(app.job_detail_area, mouse.column, mouse.row) {
                    app.job_log_scroll =
                        app.job_log_scroll.saturating_add(MOUSE_WHEEL_ROWS as usize);
                } else {
                    app.job_index = app
                        .job_viewport
                        .scroll_at(mouse.column, mouse.row, MOUSE_WHEEL_ROWS)
                        .unwrap_or_else(|| {
                            scrolled_index(app.job_index, app.jobs.len(), MOUSE_WHEEL_ROWS)
                        });
                }
            }
            Tab::Doctor => {
                if contains(app.doctor_detail_area, mouse.column, mouse.row) {
                    app.doctor_detail_scroll = app
                        .doctor_detail_scroll
                        .saturating_add(MOUSE_WHEEL_ROWS as usize);
                } else {
                    app.doctor_index = app
                        .doctor_viewport
                        .scroll_at(mouse.column, mouse.row, MOUSE_WHEEL_ROWS)
                        .unwrap_or_else(|| {
                            scrolled_index(
                                app.doctor_index,
                                app.visible_doctor_count(),
                                MOUSE_WHEEL_ROWS,
                            )
                        });
                }
            }
            Tab::Settings => {
                app.settings_index = scrolled_index(app.settings_index, SETTINGS_ROW_COUNT, 1);
            }
        },
        _ => {}
    }
}

fn handle_modal_mouse(app: &mut App, mouse: MouseEvent) {
    if matches!(app.modal, Modal::TomlEditor { .. }) {
        handle_toml_editor_mouse(app, mouse);
        return;
    }
    let hitbox = app
        .modal_input_hitboxes
        .iter()
        .copied()
        .find(|hitbox| contains(Some(hitbox.area), mouse.column, mouse.row));
    match mouse.kind {
        MouseEventKind::Moved => {
            if let Some(hitbox) = hitbox
                && modal_field_is_editable(&app.modal, hitbox.field)
            {
                if let Modal::AddCommand { field, .. } = &mut app.modal {
                    *field = hitbox.field;
                } else if let Modal::NetworkProxy { field, .. } = &mut app.modal {
                    *field = hitbox.field;
                } else if let Modal::GithubMonitorForm { field, .. } = &mut app.modal {
                    *field = hitbox.field;
                }
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
            if hitbox.field == 0
                && let Modal::NetworkProxy {
                    proxy_mode, field, ..
                } = &mut app.modal
            {
                *proxy_mode = proxy_mode.next();
                *field = 0;
                app.modal_drag = None;
                return;
            }
            if let Modal::GithubMonitorForm {
                field,
                format,
                update_policy,
                cleanup_installer,
                enabled,
                ..
            } = &mut app.modal
            {
                *field = hitbox.field;
                if hitbox.field == 4 {
                    *format = next_release_format(*format);
                    app.modal_drag = None;
                    return;
                }
                if hitbox.field == 5 {
                    *update_policy = next_release_update_policy(*update_policy);
                    app.modal_drag = None;
                    return;
                }
                if hitbox.field == 6 {
                    *cleanup_installer = !*cleanup_installer;
                    app.modal_drag = None;
                    return;
                }
                if hitbox.field == 11 {
                    *enabled = !*enabled;
                    app.modal_drag = None;
                    return;
                }
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
            } else if let Modal::TargetVersion { version, .. } = &mut app.modal {
                version.cursor = target;
                version.clear_selection();
                app.modal_drag = Some((0, target));
            } else if let Modal::GithubMonitorForm {
                field,
                name,
                repository,
                asset_regex,
                target_directory,
                max_download_bytes,
                max_extracted_bytes,
                max_extracted_files,
                strip_components,
                ..
            } = &mut app.modal
            {
                *field = hitbox.field;
                let input = match hitbox.field {
                    0 => name,
                    1 => repository,
                    2 => asset_regex,
                    3 => target_directory,
                    7 => max_download_bytes,
                    8 => max_extracted_bytes,
                    9 => max_extracted_files,
                    10 => strip_components,
                    _ => return,
                };
                input.cursor = target;
                input.clear_selection();
                app.modal_drag = Some((hitbox.field, target));
            } else if let Modal::GithubPollInterval { seconds } = &mut app.modal {
                seconds.cursor = target;
                seconds.clear_selection();
                app.modal_drag = Some((0, target));
            } else if let Modal::NetworkProxy {
                field,
                proxy_url,
                no_proxy,
                ..
            } = &mut app.modal
            {
                *field = hitbox.field;
                let input = if hitbox.field == 1 {
                    proxy_url
                } else {
                    no_proxy
                };
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
            } else if let Modal::TargetVersion { version, .. } = &mut app.modal {
                version.cursor = target;
                version.selection_anchor = (target != anchor).then_some(anchor);
            } else if let Modal::GithubMonitorForm {
                name,
                repository,
                asset_regex,
                target_directory,
                max_download_bytes,
                max_extracted_bytes,
                max_extracted_files,
                strip_components,
                ..
            } = &mut app.modal
            {
                let input = match field {
                    0 => name,
                    1 => repository,
                    2 => asset_regex,
                    3 => target_directory,
                    7 => max_download_bytes,
                    8 => max_extracted_bytes,
                    9 => max_extracted_files,
                    10 => strip_components,
                    _ => return,
                };
                input.cursor = target;
                input.selection_anchor = (target != anchor).then_some(anchor);
            } else if let Modal::GithubPollInterval { seconds } = &mut app.modal {
                seconds.cursor = target;
                seconds.selection_anchor = (target != anchor).then_some(anchor);
            } else if let Modal::NetworkProxy {
                proxy_url,
                no_proxy,
                ..
            } = &mut app.modal
            {
                let input = if field == 1 { proxy_url } else { no_proxy };
                input.cursor = target;
                input.selection_anchor = (target != anchor).then_some(anchor);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => app.modal_drag = None,
        _ => {}
    }
}

fn handle_toml_editor_mouse(app: &mut App, mouse: MouseEvent) {
    let Some(hitbox) = app.toml_editor_hitbox else {
        return;
    };
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if contains(Some(hitbox.area), mouse.column, mouse.row)
                && let Modal::TomlEditor { editor } = &mut app.modal
            {
                editor.scroll_vertical(-MOUSE_WHEEL_ROWS, usize::from(hitbox.area.height));
            }
        }
        MouseEventKind::ScrollDown => {
            if contains(Some(hitbox.area), mouse.column, mouse.row)
                && let Modal::TomlEditor { editor } = &mut app.modal
            {
                editor.scroll_vertical(MOUSE_WHEEL_ROWS, usize::from(hitbox.area.height));
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if !contains(Some(hitbox.area), mouse.column, mouse.row) {
                app.toml_editor_drag = None;
                return;
            }
            let Some(target) = toml_editor_cursor_at(app, hitbox, mouse.column, mouse.row) else {
                return;
            };
            if let Modal::TomlEditor { editor } = &mut app.modal {
                editor.move_to(target, false);
                editor.preferred_column = None;
                app.toml_editor_drag = Some(target);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(anchor) = app.toml_editor_drag else {
                return;
            };
            let column = mouse
                .column
                .clamp(hitbox.area.x, hitbox.area.right().saturating_sub(1));
            let row = mouse
                .row
                .clamp(hitbox.area.y, hitbox.area.bottom().saturating_sub(1));
            let Some(target) = toml_editor_cursor_at(app, hitbox, column, row) else {
                return;
            };
            if let Modal::TomlEditor { editor } = &mut app.modal {
                editor.cursor = target;
                editor.selection_anchor = (target != anchor).then_some(anchor);
                editor.preferred_column = None;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => app.toml_editor_drag = None,
        _ => {}
    }
}

fn toml_editor_cursor_at(
    app: &App,
    hitbox: TomlEditorHitbox,
    column: u16,
    row: u16,
) -> Option<usize> {
    let Modal::TomlEditor { editor } = &app.modal else {
        return None;
    };
    if hitbox.area.is_empty() {
        return None;
    }
    let line = editor
        .scroll_y
        .saturating_add(usize::from(row.saturating_sub(hitbox.area.y)))
        .min(editor.line_ranges().len().saturating_sub(1));
    let display_column = editor
        .scroll_x
        .saturating_add(usize::from(column.saturating_sub(hitbox.area.x)));
    Some(editor.byte_at_column(line, display_column))
}

fn modal_field_is_editable(modal: &Modal, field: usize) -> bool {
    match modal {
        Modal::AddCommand { .. } | Modal::TargetVersion { .. } => true,
        Modal::GithubMonitorForm { .. } => field < GITHUB_MONITOR_FORM_FIELD_COUNT,
        Modal::GithubPollInterval { .. } => field == 0,
        Modal::NetworkProxy { proxy_mode, .. } => {
            field == 0 || (*proxy_mode == ProxyMode::Explicit && field <= 2)
        }
        _ => false,
    }
}

fn modal_cursor_at(app: &App, hitbox: ModalInputHitbox, column: u16) -> Option<usize> {
    let input = match &app.modal {
        Modal::AddCommand { name, command, .. } => {
            if hitbox.field == 0 {
                name
            } else {
                command
            }
        }
        Modal::TargetVersion { version, .. } => version,
        Modal::GithubMonitorForm {
            name,
            repository,
            asset_regex,
            target_directory,
            max_download_bytes,
            max_extracted_bytes,
            max_extracted_files,
            strip_components,
            ..
        } => match hitbox.field {
            0 => name,
            1 => repository,
            2 => asset_regex,
            3 => target_directory,
            7 => max_download_bytes,
            8 => max_extracted_bytes,
            9 => max_extracted_files,
            10 => strip_components,
            _ => return None,
        },
        Modal::GithubPollInterval { seconds } => seconds,
        Modal::NetworkProxy {
            proxy_url,
            no_proxy,
            ..
        } => {
            if hitbox.field == 1 {
                proxy_url
            } else if hitbox.field == 2 {
                no_proxy
            } else {
                return None;
            }
        }
        _ => return None,
    };
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

fn scrolled_index(current: usize, length: usize, delta: isize) -> usize {
    if length == 0 {
        return 0;
    }
    current
        .saturating_add_signed(delta)
        .min(length.saturating_sub(1))
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
            app.doctor_index = previous_index(app.doctor_index, app.visible_doctor_count());
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.doctor_index = next_index(app.doctor_index, app.visible_doctor_count());
        }
        KeyCode::Enter if app.doctor_loading || app.doctor_never_scanned() => {
            request_doctor_refresh(app);
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

fn handle_settings_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.settings_index = previous_index(app.settings_index, SETTINGS_ROW_COUNT);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.settings_index = next_index(app.settings_index, SETTINGS_ROW_COUNT);
        }
        KeyCode::Enter | KeyCode::Char(' ') => app.toggle_setting(app.settings_index),
        _ => {}
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(Color::White).bg(SURFACE)),
        area,
    );
    if let Some(progress) = app.initial_load {
        draw_initial_load(frame, app.language, progress, app.frame);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(4),
        ])
        .split(area);
    let header_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(1)])
        .split(chunks[0]);

    let titles = match app.language {
        Language::English => ["Tools", "Activity", "Jobs", "Doctor", "Settings"],
        Language::Chinese => ["工具", "活动", "任务", "诊断", "设置"],
    }
    .into_iter()
    .map(Line::from)
    .collect::<Vec<_>>();
    app.tab_hitboxes = tab_hitboxes(header_chunks[0], &titles);
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
    frame.render_widget(tabs, header_chunks[0]);
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
    draw_github_rate_limit(frame, app, header_chunks[1]);

    match app.tab {
        Tab::Tools => draw_tools(frame, app, chunks[1]),
        Tab::Activity => draw_activity(frame, app, chunks[1]),
        Tab::Jobs => draw_jobs(frame, app, chunks[1]),
        Tab::Doctor => draw_doctor(frame, app, chunks[1]),
        Tab::Settings => draw_settings(frame, app, chunks[1]),
    }

    let shared_tool_help = match app.language {
        Language::English => {
            "Ctrl+C quit · t TOML · o editor · c add · e edit · d del · r refresh · L 中/EN · ←/→ tab"
        }
        Language::Chinese => {
            "Ctrl+C 退出 · t TOML · o 编辑器 · c 添加 · e 编辑 · d 删除 · r 刷新 · L 中/EN · ←/→ 标签页"
        }
    };
    let help = match (app.tab, app.language) {
        (Tab::Tools, Language::English) => match app.tool_view {
            ToolView::Commands => [
                "↑↓/hover move · click/Space select · a all · Enter latest · v choose version · Tab GitHub",
                shared_tool_help,
            ],
            ToolView::Github => [
                "↑↓/hover move · click/Space select · a all · Enter install · Tab command tools",
                shared_tool_help,
            ],
        },
        (Tab::Tools, Language::Chinese) => match app.tool_view {
            ToolView::Commands => [
                "↑↓/悬停 移动 · 点击/Space 选择 · a 全选 · Enter 更新 · v 指定版本 · Tab GitHub",
                shared_tool_help,
            ],
            ToolView::Github => [
                "↑↓/悬停 移动 · 点击/Space 选择 · a 全选 · Enter 安装 · Tab 命令工具",
                shared_tool_help,
            ],
        },
        (Tab::Activity, Language::English) => [
            "↑↓ scroll · click execution to expand · Home/End · r refresh",
            "Ctrl+C quit · ←/→ or click tab · L 中/EN · Shift+Tab policy",
        ],
        (Tab::Activity, Language::Chinese) => [
            "↑↓ 滚动 · 点击执行展开 · Home/End · r 刷新",
            "Ctrl+C 退出 · ←/→ 或点击标签页 · L 中/EN · Shift+Tab 策略",
        ],
        (Tab::Jobs, Language::English) => [
            "↑↓/hover move · click/Enter expand result · PgUp/PgDn scroll · r refresh",
            "Ctrl+C quit · ←/→ or click tab · L 中/EN · Shift+Tab policy",
        ],
        (Tab::Jobs, Language::Chinese) => [
            "↑↓/悬停 移动 · 点击/Enter 展开结果 · PgUp/PgDn 滚动 · r 刷新",
            "Ctrl+C 退出 · ←/→ 或点击标签页 · L 中/EN · Shift+Tab 策略",
        ],
        (Tab::Doctor, Language::English) => [
            "Enter scan/expand · r rescan · ↑↓/hover move · PgUp/PgDn scroll",
            "Ctrl+C quit · active wins PATH · shadowed is hidden · ←/→ or click tab · L 中/EN",
        ],
        (Tab::Doctor, Language::Chinese) => [
            "Enter 扫描/展开 · r 重新扫描 · ↑↓/悬停 移动 · PgUp/PgDn 滚动",
            "Ctrl+C 退出 · active 为当前生效项 · shadowed 为被遮蔽项 · ←/→ 或点击标签页",
        ],
        (Tab::Settings, Language::English) => [
            "↑↓/hover move · click/Space/Enter toggle setting",
            "Ctrl+C quit · settings save immediately · ←/→ or click tab · L 中/EN",
        ],
        (Tab::Settings, Language::Chinese) => [
            "↑↓/悬停 移动 · 点击/Space/Enter 切换设置",
            "Ctrl+C 退出 · 设置立即保存 · ←/→ 或点击标签页 · L 中/EN",
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

fn draw_github_rate_limit(frame: &mut Frame, app: &App, area: Rect) {
    let (ratio, color, label) = if let Some(rate) = &app.github_rate_limit {
        let suffix = if app.github_rate_limit_loading {
            app.language.text(" · refreshing…", " · 刷新中…")
        } else if app.github_rate_limit_error.is_some() {
            app.language.text(" · refresh failed", " · 刷新失败")
        } else {
            ""
        };
        let label = match app.language {
            Language::English => format!(
                "GitHub API  @{} · used {} / {} · remaining {}{}",
                rate.owner, rate.used, rate.limit, rate.remaining, suffix
            ),
            Language::Chinese => format!(
                "GitHub API  @{} · 已用 {} / {} · 剩余 {}{}",
                rate.owner, rate.used, rate.limit, rate.remaining, suffix
            ),
        };
        (
            rate.used as f64 / rate.limit as f64,
            github_rate_limit_color(rate),
            label,
        )
    } else if app.github_credential_error.is_some() {
        (
            0.0,
            ERROR_COLOR,
            app.language
                .text(
                    "GitHub API · encrypted settings error",
                    "GitHub API · 加密配置错误",
                )
                .to_owned(),
        )
    } else if app.github_rate_limit_loading {
        (
            0.0,
            ACCENT,
            app.language
                .text(
                    "GitHub API · loading @Token owner and quota…",
                    "GitHub API · 正在加载 @Token 主人和配额…",
                )
                .to_owned(),
        )
    } else if app.github_rate_limit_error.is_some() {
        (
            0.0,
            ERROR_COLOR,
            app.language
                .text(
                    "GitHub API · owner and quota unavailable",
                    "GitHub API · 无法获取主人和配额",
                )
                .to_owned(),
        )
    } else {
        (
            0.0,
            SUBTLE,
            app.language
                .text(
                    "GitHub API · Token not configured",
                    "GitHub API · Token 未配置",
                )
                .to_owned(),
        )
    };
    frame.render_widget(
        Gauge::default()
            .ratio(ratio.clamp(0.0, 1.0))
            .gauge_style(Style::default().fg(color).bg(SURFACE))
            .label(Span::styled(
                label,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )),
        area,
    );
}

fn github_rate_limit_color(rate: &version::GithubRateLimit) -> Color {
    let remaining_ratio = rate.remaining as f64 / rate.limit as f64;
    if remaining_ratio <= 0.10 {
        ERROR_COLOR
    } else if remaining_ratio <= 0.25 {
        WARNING_COLOR
    } else {
        SUCCESS
    }
}

fn draw_initial_load(
    frame: &mut Frame,
    language: Language,
    progress: InitialLoadProgress,
    frame_number: u64,
) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BACKDROP_BG)),
        area,
    );
    let panel = centered_rect(64, 9, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(PANEL_BG))
        .title(Span::styled(
            language.text(" Starting dvup ", " 正在启动 dvup "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);
    if inner.is_empty() {
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    let pulse = if frame_number % 2 == 0 { "·" } else { "•" };
    frame.render_widget(
        Paragraph::new(format!("{pulse} {}", progress.label(language)))
            .alignment(Alignment::Center)
            .style(Style::default().fg(DIM)),
        rows[1],
    );
    let percentage = progress.percentage();
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(ACCENT).bg(SURFACE))
            .ratio(f64::from(percentage) / 100.0)
            .label(format!("{percentage}%")),
        rows[3],
    );
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
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);
    let titles = match app.language {
        Language::English => ["Command tools", "GitHub repositories"],
        Language::Chinese => ["命令工具", "GitHub 仓库"],
    }
    .into_iter()
    .map(Line::from)
    .collect::<Vec<_>>();
    app.tool_view_hitboxes = tab_hitboxes(sections[0], &titles);
    frame.render_widget(
        Tabs::new(titles)
            .select(app.tool_view.index())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(BORDER))
                    .title(Span::styled(
                        app.language.text(" Tool views ", " 工具视图 "),
                        Style::default().fg(ACCENT),
                    )),
            )
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        sections[0],
    );
    match app.tool_view {
        ToolView::Commands => draw_command_tools(frame, app, sections[1]),
        ToolView::Github => draw_github_tools(frame, app, sections[1]),
    }
}

fn draw_command_tools(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.visible_tool_indices.iter().filter_map(|&index| {
        let tool = app.tools.get(index)?;
        let checked = if tool.selected { "[x]" } else { "[ ]" };
        let kind = tool.kind.label(app.language);
        Some(Row::new(vec![
            Cell::from(checked),
            Cell::from(tool.name.clone()),
            Cell::from(tool.availability.label(app.language)).style(tool.availability.style()),
            Cell::from(tool.version.label().to_owned()).style(tool.version.style()),
            Cell::from(latest_version_label(&tool.latest_version, app.language))
                .style(latest_version_style(tool)),
            Cell::from(format_run_result(tool, app.frame, app.language))
                .style(tool.run_state.style()),
            Cell::from(kind),
            Cell::from(tool.command.clone()),
        ]))
    });
    let header = Row::new(match app.language {
        Language::English => [
            "",
            "TOOL",
            "AVAILABLE",
            "INSTALLED",
            "LATEST",
            "RESULT",
            "TYPE",
            "COMMAND",
        ],
        Language::Chinese => [
            "",
            "工具",
            "可用性",
            "已安装版本",
            "最新版本",
            "结果",
            "类型",
            "命令",
        ],
    })
    .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD))
    .bottom_margin(1);
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Length(16),
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
    let mut state = TableState::default()
        .with_offset(app.tool_viewport.offset())
        .with_selected((!app.visible_tool_indices.is_empty()).then_some(app.tool_index));
    frame.render_stateful_widget(table, area, &mut state);

    let first_row = area.y.saturating_add(3);
    let visible_rows = area.height.saturating_sub(4) as usize;
    let offset = state.offset();
    app.tool_viewport.update(
        Rect::new(
            area.x.saturating_add(1),
            first_row,
            area.width.saturating_sub(2),
            u16::try_from(visible_rows).unwrap_or(u16::MAX),
        ),
        app.visible_tool_indices.len(),
        offset,
    );
    app.tool_hitboxes = (offset
        ..app
            .visible_tool_indices
            .len()
            .min(offset.saturating_add(visible_rows)))
        .enumerate()
        .map(|(rendered_index, visible_position)| {
            (
                Rect {
                    x: area.x.saturating_add(1),
                    y: first_row.saturating_add(rendered_index as u16),
                    width: area.width.saturating_sub(2),
                    height: 1,
                },
                visible_position,
            )
        })
        .collect();
    render_scrollbar(
        frame,
        area,
        app.visible_tool_indices.len(),
        visible_rows,
        offset,
    );
}

fn draw_github_tools(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.github_monitors.iter().map(|monitor| {
        let checked = if app.selected_github_monitors.contains(&monitor.name) {
            "[x]"
        } else {
            "[ ]"
        };
        let status = app
            .release_monitor_statuses
            .iter()
            .find(|status| status.name == monitor.name);
        let installed = status
            .and_then(|status| status.installed_tag.as_deref())
            .unwrap_or("—");
        let latest = if !monitor.enabled {
            app.language.text("disabled", "已停用")
        } else if app.release_probe_running {
            app.language.text("checking…", "检查中…")
        } else {
            status
                .and_then(|status| status.latest_tag.as_deref())
                .unwrap_or("—")
        };
        let (result, result_style) = github_monitor_result(monitor, status, app.language);
        Row::new([
            Cell::from(checked),
            Cell::from(monitor.name.clone()),
            Cell::from(monitor.repository.clone()),
            Cell::from(installed.to_owned()),
            Cell::from(latest.to_owned()),
            Cell::from(result).style(result_style),
            Cell::from(release_update_policy_label(
                monitor.update_policy,
                app.language,
            )),
            Cell::from(monitor.target_directory.display().to_string()),
        ])
    });
    let header = Row::new(match app.language {
        Language::English => [
            "",
            "TOOL",
            "REPOSITORY",
            "INSTALLED",
            "LATEST",
            "STATUS",
            "POLICY",
            "TARGET",
        ],
        Language::Chinese => [
            "",
            "工具",
            "仓库",
            "已安装版本",
            "最新版本",
            "状态",
            "策略",
            "目标目录",
        ],
    })
    .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD))
    .bottom_margin(1);
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(16),
            Constraint::Length(22),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .title(Span::styled(
                app.language.text(" GitHub repositories ", " GitHub 仓库 "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
    )
    .row_highlight_style(
        Style::default()
            .bg(SELECTION_BG)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ");
    let monitor_count = app.github_monitors.len();
    let mut state = TableState::default()
        .with_offset(app.github_monitor_viewport.offset())
        .with_selected((monitor_count > 0).then_some(app.github_monitor_index));
    frame.render_stateful_widget(table, area, &mut state);

    let first_row = area.y.saturating_add(3);
    let visible_rows = area.height.saturating_sub(4) as usize;
    let offset = state.offset();
    app.github_monitor_viewport.update(
        Rect::new(
            area.x.saturating_add(1),
            first_row,
            area.width.saturating_sub(2),
            u16::try_from(visible_rows).unwrap_or(u16::MAX),
        ),
        monitor_count,
        offset,
    );
    app.github_monitor_hitboxes = (offset..monitor_count.min(offset.saturating_add(visible_rows)))
        .enumerate()
        .map(|(rendered_index, monitor_index)| {
            (
                Rect::new(
                    area.x.saturating_add(1),
                    first_row.saturating_add(rendered_index as u16),
                    area.width.saturating_sub(2),
                    1,
                ),
                monitor_index,
            )
        })
        .collect();
    render_scrollbar(frame, area, monitor_count, visible_rows, offset);
}

fn github_monitor_result(
    monitor: &GithubReleaseMonitor,
    status: Option<&MonitorStatus>,
    language: Language,
) -> (String, Style) {
    if !monitor.enabled {
        return (
            language.text("disabled", "已停用").to_owned(),
            Style::default().fg(SUBTLE),
        );
    }
    let Some(status) = status else {
        return (
            language.text("not checked", "尚未检查").to_owned(),
            Style::default().fg(DIM),
        );
    };
    if status.error.is_some() {
        return (
            language.text("fetch failed", "获取失败").to_owned(),
            Style::default().fg(ERROR_COLOR),
        );
    }
    match (&status.installed_tag, &status.latest_tag) {
        (Some(installed), Some(latest)) if release::release_versions_match(installed, latest) => (
            language.text("up to date", "已是最新").to_owned(),
            Style::default().fg(SUCCESS),
        ),
        (_, Some(_)) => (
            language.text("update available", "可更新").to_owned(),
            Style::default().fg(WARNING_COLOR),
        ),
        _ => (
            language.text("not checked", "尚未检查").to_owned(),
            Style::default().fg(DIM),
        ),
    }
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
    let mut state = TableState::default()
        .with_offset(app.job_viewport.offset())
        .with_selected(Some(app.job_index));
    frame.render_stateful_widget(table, table_area, &mut state);

    let first_row = table_area.y.saturating_add(3);
    let visible_rows = table_area.height.saturating_sub(4) as usize;
    let offset = state.offset();
    app.job_viewport.update(
        Rect::new(
            table_area.x.saturating_add(1),
            first_row,
            table_area.width.saturating_sub(2),
            u16::try_from(visible_rows).unwrap_or(u16::MAX),
        ),
        app.jobs.len(),
        offset,
    );
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

fn diagnosis_is_visible(
    diagnosis: &doctor::ToolDiagnosis,
    hide_unsupported_and_missing: bool,
) -> bool {
    !hide_unsupported_and_missing
        || (diagnosis.supported && !diagnosis.target.candidates.is_empty())
}

fn draw_doctor(frame: &mut Frame, app: &mut App, area: Rect) {
    let never_scanned = app.doctor_never_scanned();
    let visible_doctor_count = app.visible_doctor_count();
    let (table_area, detail_area) =
        if !never_scanned && app.expanded_doctor.is_some() && area.height >= 10 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
                .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };
    let rows = app.visible_doctor_diagnoses().map(|diagnosis| {
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
        .visible_doctor_diagnoses()
        .filter(|diagnosis| diagnosis.has_conflict())
        .count();
    let (scan_status, scan_status_style) = if app.doctor_loading {
        (
            app.language.text("● scanning", "● 扫描中"),
            Style::default().fg(ACCENT),
        )
    } else if never_scanned {
        (
            app.language.text("● not scanned", "● 尚未诊断"),
            Style::default().fg(WARNING_COLOR),
        )
    } else {
        (
            app.language.text("● ready", "● 已完成"),
            Style::default().fg(SUCCESS),
        )
    };
    let result_summary = if never_scanned {
        String::new()
    } else {
        match app.language {
            Language::English => {
                format!("  {visible_doctor_count} tools · {conflicts} warn")
            }
            Language::Chinese => {
                format!("  {visible_doctor_count} 工具 · {conflicts} 冲突")
            }
        }
    };
    let title = Line::from(vec![
        Span::styled(
            app.language.text(" Doctor  ", " 安装诊断  "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(scan_status, scan_status_style),
        Span::styled(
            result_summary,
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
    let mut state = TableState::default()
        .with_offset(app.doctor_viewport.offset())
        .with_selected(if never_scanned || visible_doctor_count == 0 {
            None
        } else {
            Some(app.doctor_index)
        });
    frame.render_stateful_widget(table, table_area, &mut state);

    if never_scanned {
        app.doctor_hitboxes.clear();
        app.doctor_viewport.clear();
        app.doctor_detail_area = None;
        let prompt_height = table_area.height.saturating_sub(4).min(2);
        let prompt_area = Rect {
            x: table_area.x.saturating_add(2),
            y: table_area
                .y
                .saturating_add(3)
                .saturating_add(table_area.height.saturating_sub(4 + prompt_height) / 2),
            width: table_area.width.saturating_sub(4),
            height: prompt_height,
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    app.language.text(
                        "Installation diagnostics have not been run.",
                        "尚未运行安装冲突诊断。",
                    ),
                    Style::default().fg(DIM),
                ),
                Line::styled(
                    app.language.text(
                        "Press Enter to scan; press R to rescan later.",
                        "按 Enter 开始扫描；之后可按 R 重新扫描。",
                    ),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ])
            .alignment(Alignment::Center),
            prompt_area,
        );
        return;
    }

    let first_row = table_area.y.saturating_add(3);
    let visible_rows = table_area.height.saturating_sub(4) as usize;
    let offset = state.offset();
    app.doctor_viewport.update(
        Rect::new(
            table_area.x.saturating_add(1),
            first_row,
            table_area.width.saturating_sub(2),
            u16::try_from(visible_rows).unwrap_or(u16::MAX),
        ),
        visible_doctor_count,
        offset,
    );
    app.doctor_hitboxes = (offset..visible_doctor_count.min(offset.saturating_add(visible_rows)))
        .enumerate()
        .map(|(visible_index, doctor_index)| {
            (
                Rect {
                    x: table_area.x.saturating_add(1),
                    y: first_row.saturating_add(visible_index as u16),
                    width: table_area.width.saturating_sub(2),
                    height: 1,
                },
                doctor_index,
            )
        })
        .collect();
    render_scrollbar(
        frame,
        table_area,
        visible_doctor_count,
        visible_rows,
        offset,
    );

    app.doctor_detail_area = detail_area;
    let Some(detail_area) = detail_area else {
        return;
    };
    let hide_unavailable = app.settings.hide_unsupported_and_missing_tools;
    let diagnosis = app.expanded_doctor.as_deref().and_then(|name| {
        app.doctor_diagnoses.iter().find(|diagnosis| {
            diagnosis_is_visible(diagnosis, hide_unavailable) && diagnosis.name == name
        })
    });
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

fn draw_settings(frame: &mut Frame, app: &mut App, area: Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(12), Constraint::Min(3)])
        .split(area);
    let enabled_label = |enabled| match (enabled, app.language) {
        (true, Language::English) => "enabled",
        (true, Language::Chinese) => "已启用",
        (false, Language::English) => "disabled",
        (false, Language::Chinese) => "已关闭",
    };
    let proxy_url = app
        .settings
        .network
        .proxy_url
        .as_deref()
        .unwrap_or("—")
        .to_owned();
    let no_proxy = if app.settings.network.no_proxy.is_empty() {
        "—".to_owned()
    } else {
        app.settings.network.no_proxy.join(", ")
    };
    let test_state = if app.network_test_loading {
        app.language.text("testing…", "测试中…")
    } else if app.network_test_results.is_empty() {
        app.language.text("press Enter", "按 Enter")
    } else if app
        .network_test_results
        .iter()
        .any(|result| result.error.is_some())
    {
        app.language.text("failed", "有失败")
    } else {
        app.language.text("passed", "已通过")
    };
    let api_key_state = if app.github_credential_error.is_some() {
        app.language.text("credential error", "凭据错误")
    } else if app.github_api_key_configured {
        app.language.text("stored securely", "已安全保存")
    } else {
        app.language.text("not configured", "未配置")
    };
    let rows = vec![
        Row::new([
            format!(
                "{} {}",
                if app.settings.auto_diagnose_on_startup {
                    "[x]"
                } else {
                    "[ ]"
                },
                app.language.text(
                    "Run Doctor diagnostics when TUI starts",
                    "进入 TUI 时自动运行 Doctor 诊断"
                )
            ),
            enabled_label(app.settings.auto_diagnose_on_startup).to_owned(),
        ]),
        Row::new([
            format!(
                "{} {}",
                if app.settings.hide_unsupported_and_missing_tools {
                    "[x]"
                } else {
                    "[ ]"
                },
                app.language.text(
                    "Hide unsupported or uninstalled tools",
                    "隐藏不支持或未安装的工具"
                )
            ),
            enabled_label(app.settings.hide_unsupported_and_missing_tools).to_owned(),
        ]),
        Row::new([
            app.language.text("Proxy mode", "代理模式").to_owned(),
            app.settings.network.proxy_mode.label().to_owned(),
        ]),
        Row::new([
            app.language.text("Proxy URL", "代理地址").to_owned(),
            proxy_url,
        ]),
        Row::new([
            app.language.text("No proxy", "代理绕过").to_owned(),
            no_proxy,
        ]),
        Row::new([
            app.language
                .text("Test registry connections", "测试仓库连接")
                .to_owned(),
            test_state.to_owned(),
        ]),
        Row::new([
            app.language
                .text("GitHub API key", "GitHub API Key")
                .to_owned(),
            api_key_state.to_owned(),
        ]),
        Row::new([
            app.language
                .text("Repository monitor interval", "仓库监控间隔")
                .to_owned(),
            match app.language {
                Language::English => {
                    format!("{} seconds", app.settings.github.poll_interval_secs)
                }
                Language::Chinese => format!("{} 秒", app.settings.github.poll_interval_secs),
            },
        ]),
    ];
    let table = Table::new(rows, [Constraint::Min(28), Constraint::Percentage(42)])
        .header(
            Row::new(match app.language {
                Language::English => ["OPTION", "STATE"],
                Language::Chinese => ["选项", "状态"],
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
                    app.language.text(" Settings ", " 设置 "),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
        )
        .row_highlight_style(
            Style::default()
                .bg(SELECTION_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = TableState::default().with_selected(Some(app.settings_index));
    frame.render_stateful_widget(table, sections[0], &mut state);
    app.settings_hitboxes = (0..SETTINGS_ROW_COUNT)
        .map(|index| {
            (
                Rect {
                    x: sections[0].x.saturating_add(1),
                    y: sections[0].y.saturating_add(3 + index as u16),
                    width: sections[0].width.saturating_sub(2),
                    height: 1,
                },
                index,
            )
        })
        .collect();

    let mut description = vec![
        Line::styled(
            app.language.text(
                "environment inherits standard proxy variables; explicit uses only this URL; direct removes all proxy variables. No mode falls back to another.",
                "environment 继承标准代理变量；explicit 只使用此处地址；direct 会移除全部代理变量。任何模式都不会回退到另一模式。",
            ),
            Style::default().fg(DIM),
        ),
        Line::styled(
            app.language.text(
                "Explicit mode supports HTTP/HTTPS CONNECT only. Proxy credentials and SOCKS are rejected.",
                "显式模式仅支持 HTTP/HTTPS CONNECT；代理凭据和 SOCKS 会被拒绝。",
            ),
            Style::default().fg(SUBTLE),
        ),
    ];
    for result in &app.network_test_results {
        let (status, style) = if let Some(error) = &result.error {
            (format!("FAILED  {error}"), Style::default().fg(ERROR_COLOR))
        } else {
            (
                format!("OK  {} ms", result.elapsed_ms),
                Style::default().fg(SUCCESS),
            )
        };
        description.push(Line::from(vec![
            Span::styled(format!("{:<10}", result.name), Style::default().fg(ACCENT)),
            Span::styled(status, style),
        ]));
    }
    if !app.github_monitors.is_empty() {
        description.push(Line::styled(
            match app.language {
                Language::English => format!(
                    "GitHub Release metadata refreshes every {} seconds; installation requires confirmation in Tools.",
                    app.settings.github.poll_interval_secs
                ),
                Language::Chinese => format!(
                    "GitHub Release 元数据每 {} 秒刷新一次；安装必须在工具页确认。",
                    app.settings.github.poll_interval_secs
                ),
            },
            Style::default().fg(DIM),
        ));
    }
    description.push(Line::styled(
        match app.language {
            Language::English => format!("Stored in {}", app.state.settings_path().display()),
            Language::Chinese => format!("保存位置：{}", app.state.settings_path().display()),
        },
        Style::default().fg(SUBTLE),
    ));
    frame.render_widget(
        Paragraph::new(description)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(BORDER))
                    .title(Span::styled(
                        app.language
                            .text(" Network and behavior ", " 网络与行为说明 "),
                        Style::default().fg(ACCENT),
                    )),
            )
            .wrap(Wrap { trim: false }),
        sections[1],
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
    app.toml_editor_hitbox = None;
    if matches!(app.modal, Modal::None) {
        return;
    }
    render_modal_backdrop(frame, area);

    if matches!(app.modal, Modal::TomlEditor { .. }) {
        draw_toml_editor(frame, app, area);
        return;
    }

    match &app.modal {
        Modal::None => {}
        Modal::ConfirmGithubMonitorUpdate { monitors } => {
            let inner = modal_panel(
                frame,
                area,
                app.language
                    .text("Install GitHub updates", "安装 GitHub 更新"),
                78,
                12,
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        match app.language {
                            Language::English => format!(
                                "Download and install {} selected GitHub repository update(s)?",
                                monitors.len()
                            ),
                            Language::Chinese => {
                                format!("下载并安装所选的 {} 项 GitHub 仓库更新？", monitors.len())
                            }
                        },
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    labeled_value(
                        app.language.text("Selected", "已选择"),
                        &monitors.join(", "),
                        ACCENT,
                    ),
                    Line::raw(""),
                    Line::styled(
                        app.language.text(
                            "Targets are replaced only after download and extraction succeed.",
                            "仅在下载和解压成功后替换目标目录。",
                        ),
                        Style::default().fg(WARNING_COLOR),
                    ),
                    Line::raw(""),
                    modal_actions(
                        app.language,
                        app.language.text("install", "安装"),
                        app.language.text("cancel", "取消"),
                    ),
                ])
                .style(Style::default().bg(PANEL_BG))
                .wrap(Wrap { trim: false }),
                inner,
            );
        }
        Modal::ConfirmUpdate {
            tools,
            target_version,
            current_tools,
        } => {
            let inner = modal_panel(
                frame,
                area,
                app.language.text("Confirm update", "确认更新"),
                74,
                15,
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
                    if current_tools.is_empty() {
                        Line::raw("")
                    } else {
                        labeled_value(
                            app.language.text("Already latest", "已是最新"),
                            &current_tools.join(", "),
                            SUCCESS,
                        )
                    },
                    target_version.as_ref().map_or_else(
                        || Line::raw(""),
                        |version| {
                            labeled_value(
                                app.language.text("Target version", "目标版本"),
                                version,
                                WARNING_COLOR,
                            )
                        },
                    ),
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
        Modal::TargetVersion { name, version } => {
            let inner = modal_panel(
                frame,
                area,
                app.language.text("Choose version", "指定版本"),
                68,
                10,
            );
            let label = app.language.text("Version", "版本");
            let value_width = input_value_width(inner.width, label);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        match app.language {
                            Language::English => format!("Update {name} to an exact version."),
                            Language::Chinese => format!("将 {name} 更新到指定版本。"),
                        },
                        Style::default().fg(DIM),
                    ),
                    Line::raw(""),
                    modal_input_line(true, label, version, "1.2.3", value_width),
                    Line::raw(""),
                    Line::styled(
                        app.language.text(
                            "The configured update_version command will be used.",
                            "将使用配置中的 update_version 命令。",
                        ),
                        Style::default().fg(SUBTLE),
                    ),
                    Line::raw(""),
                    modal_form_actions(app.language, false),
                ])
                .style(Style::default().bg(PANEL_BG)),
                inner,
            );
            if inner.height > 2 && value_width > 0 {
                let label_width = u16::try_from(display_width(label)).unwrap_or(u16::MAX);
                let (visible_start, visible_end) = version.visible_range(value_width);
                app.modal_input_hitboxes.push(ModalInputHitbox {
                    area: Rect::new(
                        inner
                            .x
                            .saturating_add(2)
                            .saturating_add(label_width)
                            .saturating_add(2),
                        inner.y.saturating_add(2),
                        u16::try_from(value_width).unwrap_or(u16::MAX),
                        1,
                    ),
                    field: 0,
                    visible_start,
                    visible_end,
                });
                let cursor_width =
                    u16::try_from(display_width(&version.value[visible_start..version.cursor]))
                        .unwrap_or(u16::MAX);
                let cursor_x = inner
                    .x
                    .saturating_add(2)
                    .saturating_add(label_width)
                    .saturating_add(2)
                    .saturating_add(cursor_width)
                    .min(inner.right().saturating_sub(1));
                frame.set_cursor_position(Position::new(cursor_x, inner.y.saturating_add(2)));
            }
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
                    "claude update",
                    command_width,
                ),
                Line::raw(""),
                Line::styled(
                    app.language.text("Examples", "示例"),
                    Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                ),
                Line::styled("  claude update", Style::default().fg(SUBTLE)),
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
        Modal::GithubMonitorForm {
            mode,
            field,
            name,
            repository,
            asset_regex,
            target_directory,
            format,
            update_policy,
            cleanup_installer,
            max_download_bytes,
            max_extracted_bytes,
            max_extracted_files,
            strip_components,
            enabled,
            ..
        } => {
            let inner = modal_panel(
                frame,
                area,
                match mode {
                    MonitorFormMode::Add => app
                        .language
                        .text("Add GitHub repository monitor", "添加 GitHub 仓库监控"),
                    MonitorFormMode::Edit => app
                        .language
                        .text("Edit GitHub repository monitor", "编辑 GitHub 仓库监控"),
                },
                104,
                22,
            );
            let labels = [
                app.language.text("Name", "名称"),
                app.language.text("Repository", "仓库"),
                app.language.text("Asset regex", "资产正则"),
                app.language.text("Target directory", "目标目录"),
                app.language.text("Format", "格式"),
                app.language.text("Update policy", "更新策略"),
                app.language.text("Clean installer", "清理安装包"),
                app.language.text("Max download bytes", "最大下载字节数"),
                app.language.text("Max extracted bytes", "最大解压字节数"),
                app.language.text("Max extracted files", "最大文件数"),
                app.language.text("Strip components", "剥离路径层级"),
                app.language.text("Enabled", "启用"),
            ];
            let value_width = labels
                .iter()
                .map(|label| input_value_width(inner.width, label))
                .min()
                .unwrap_or(0);
            #[cfg(windows)]
            let target_placeholder = "C:\\Tools\\deno";
            #[cfg(not(windows))]
            let target_placeholder = "/opt/tools/deno";
            let selector = |active: bool, label: &str, value: &str| {
                Line::from(vec![
                    Span::styled(
                        if active { "› " } else { "  " },
                        Style::default().fg(if active { ACCENT } else { SUBTLE }),
                    ),
                    Span::styled(
                        format!("{label}  "),
                        Style::default().fg(if active { ACCENT } else { DIM }),
                    ),
                    Span::styled(
                        format!("[ {value} ]"),
                        Style::default()
                            .fg(if active { SUCCESS } else { SUBTLE })
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            };
            let lines = vec![
                Line::styled(
                    app.language.text(
                        "Fields are strict: owner/repo, a valid Rust regex, and an absolute target path.",
                        "字段严格校验：owner/repo、有效的 Rust 正则表达式，且目标路径必须为绝对路径。",
                    ),
                    Style::default().fg(DIM),
                ),
                Line::raw(""),
                modal_input_line(*field == 0, labels[0], name, "deno", value_width),
                modal_input_line(
                    *field == 1,
                    labels[1],
                    repository,
                    "denoland/deno",
                    value_width,
                ),
                modal_input_line(
                    *field == 2,
                    labels[2],
                    asset_regex,
                    r"^deno-.*-x86_64-pc-windows-msvc\.zip$",
                    value_width,
                ),
                modal_input_line(
                    *field == 3,
                    labels[3],
                    target_directory,
                    target_placeholder,
                    value_width,
                ),
                selector(*field == 4, labels[4], release_format_label(*format)),
                selector(
                    *field == 5,
                    labels[5],
                    release_update_policy_label(*update_policy, app.language),
                ),
                selector(
                    *field == 6,
                    labels[6],
                    app.language.text(
                        if *cleanup_installer { "enabled" } else { "keep" },
                        if *cleanup_installer { "自动清理" } else { "保留" },
                    ),
                ),
                modal_input_line(
                    *field == 7,
                    labels[7],
                    max_download_bytes,
                    "536870912",
                    value_width,
                ),
                modal_input_line(
                    *field == 8,
                    labels[8],
                    max_extracted_bytes,
                    "2147483648 (file: 0)",
                    value_width,
                ),
                modal_input_line(
                    *field == 9,
                    labels[9],
                    max_extracted_files,
                    "10000 (file: 0)",
                    value_width,
                ),
                modal_input_line(
                    *field == 10,
                    labels[10],
                    strip_components,
                    "0",
                    value_width,
                ),
                selector(
                    *field == 11,
                    labels[11],
                    app.language.text(
                        if *enabled { "enabled" } else { "disabled" },
                        if *enabled { "已启用" } else { "已停用" },
                    ),
                ),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("[Tab/↑↓]", Style::default().fg(ACCENT)),
                    Span::raw(app.language.text(" field  ", " 切换字段  ")),
                    Span::styled("[←/→/Space]", Style::default().fg(ACCENT)),
                    Span::raw(app.language.text(" choice  ", " 切换选项  ")),
                    Span::styled("[Enter/Ctrl+S]", Style::default().fg(SUCCESS)),
                    Span::raw(app.language.text(" save  ", " 保存  ")),
                    Span::styled("[Esc]", Style::default().fg(ERROR_COLOR)),
                    Span::raw(app.language.text(" back", " 返回")),
                ]),
            ];
            frame.render_widget(
                Paragraph::new(lines)
                    .style(Style::default().bg(PANEL_BG))
                    .wrap(Wrap { trim: false }),
                inner,
            );

            let input_fields = [
                (0, labels[0], name, 2),
                (1, labels[1], repository, 3),
                (2, labels[2], asset_regex, 4),
                (3, labels[3], target_directory, 5),
                (7, labels[7], max_download_bytes, 9),
                (8, labels[8], max_extracted_bytes, 10),
                (9, labels[9], max_extracted_files, 11),
                (10, labels[10], strip_components, 12),
            ];
            for (input_field, label, input, row) in input_fields {
                if inner.height <= row {
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
                    field: input_field,
                    visible_start,
                    visible_end,
                });
                if *field == input_field {
                    let cursor_width =
                        u16::try_from(display_width(&input.value[visible_start..input.cursor]))
                            .unwrap_or(u16::MAX);
                    frame.set_cursor_position(Position::new(
                        inner
                            .x
                            .saturating_add(2)
                            .saturating_add(label_width)
                            .saturating_add(2)
                            .saturating_add(cursor_width)
                            .min(inner.right().saturating_sub(1)),
                        inner.y.saturating_add(row),
                    ));
                }
            }
            for (selector_field, row) in [(4, 6), (5, 7), (6, 8), (11, 13)] {
                if inner.height > row {
                    app.modal_input_hitboxes.push(ModalInputHitbox {
                        area: Rect::new(inner.x, inner.y.saturating_add(row), inner.width, 1),
                        field: selector_field,
                        visible_start: 0,
                        visible_end: 0,
                    });
                }
            }
        }
        Modal::ConfirmDeleteGithubMonitor { index } => {
            let monitor = app.github_monitors.get(*index);
            let inner = modal_panel(
                frame,
                area,
                app.language
                    .text("Delete repository monitor", "删除仓库监控"),
                72,
                10,
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        app.language.text(
                            "Remove this GitHub repository monitor?",
                            "删除这项 GitHub 仓库监控？",
                        ),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    labeled_value(
                        app.language.text("Name", "名称"),
                        monitor.map(|value| value.name.as_str()).unwrap_or("—"),
                        ERROR_COLOR,
                    ),
                    labeled_value(
                        app.language.text("Repository", "仓库"),
                        monitor
                            .map(|value| value.repository.as_str())
                            .unwrap_or("—"),
                        ACCENT,
                    ),
                    Line::raw(""),
                    modal_actions(
                        app.language,
                        app.language.text("delete", "删除"),
                        app.language.text("cancel", "取消"),
                    ),
                ]),
                inner,
            );
        }
        Modal::GithubPollInterval { seconds } => {
            let inner = modal_panel(
                frame,
                area,
                app.language
                    .text("Repository monitor interval", "仓库监控间隔"),
                72,
                10,
            );
            let label = app.language.text("Seconds", "秒数");
            let value_width = input_value_width(inner.width, label);
            let (visible_start, visible_end) = seconds.visible_range(value_width);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        app.language.text(
                            "How often to check enabled repositories (60–86400 seconds).",
                            "已启用仓库的检查频率（60–86400 秒）。",
                        ),
                        Style::default().fg(DIM),
                    ),
                    Line::raw(""),
                    modal_input_line(true, label, seconds, "1800", value_width),
                    Line::raw(""),
                    modal_form_actions(app.language, false),
                ]),
                inner,
            );
            app.modal_input_hitboxes.push(ModalInputHitbox {
                area: Rect::new(
                    inner
                        .x
                        .saturating_add(2)
                        .saturating_add(u16::try_from(display_width(label)).unwrap_or(u16::MAX))
                        .saturating_add(2),
                    inner.y.saturating_add(2),
                    u16::try_from(value_width).unwrap_or(u16::MAX),
                    1,
                ),
                field: 0,
                visible_start,
                visible_end,
            });
            let cursor_width =
                u16::try_from(display_width(&seconds.value[visible_start..seconds.cursor]))
                    .unwrap_or(u16::MAX);
            frame.set_cursor_position(Position::new(
                inner
                    .x
                    .saturating_add(2)
                    .saturating_add(u16::try_from(display_width(label)).unwrap_or(u16::MAX))
                    .saturating_add(2)
                    .saturating_add(cursor_width)
                    .min(inner.right().saturating_sub(1)),
                inner.y.saturating_add(2),
            ));
        }
        Modal::GithubApiKey { api_key } => {
            let inner = modal_panel(
                frame,
                area,
                app.language.text("GitHub API key", "GitHub API Key"),
                76,
                10,
            );
            let label = app.language.text("API key", "API Key");
            let value_width = input_value_width(inner.width, label);
            let (visible_start, visible_end) = api_key.visible_range(value_width);
            let masked = "•".repeat(api_key.value[visible_start..visible_end].chars().count());
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        app.language.text(
                            "Encrypted into settings.toml; plaintext is never persisted.",
                            "加密保存到 settings.toml；绝不持久化明文。",
                        ),
                        Style::default().fg(DIM),
                    ),
                    Line::raw(""),
                    Line::from(vec![
                        Span::styled("› ", Style::default().fg(ACCENT)),
                        Span::styled(format!("{label}  "), Style::default().fg(ACCENT)),
                        Span::styled(masked, Style::default().fg(SUCCESS).bg(SURFACE)),
                    ]),
                    Line::raw(""),
                    Line::styled(
                        app.language.text(
                            "[Enter] save · empty input removes · [Esc] cancel",
                            "[Enter] 保存 · 留空删除 · [Esc] 取消",
                        ),
                        Style::default().fg(SUBTLE),
                    ),
                ])
                .style(Style::default().bg(PANEL_BG))
                .wrap(Wrap { trim: false }),
                inner,
            );
            if inner.height > 2 && inner.width > 0 {
                let label_width = u16::try_from(display_width(label)).unwrap_or(u16::MAX);
                let cursor_width =
                    u16::try_from(api_key.value[visible_start..api_key.cursor].chars().count())
                        .unwrap_or(u16::MAX);
                let cursor_x = inner
                    .x
                    .saturating_add(2)
                    .saturating_add(label_width)
                    .saturating_add(2)
                    .saturating_add(cursor_width)
                    .min(inner.right().saturating_sub(1));
                frame.set_cursor_position(Position::new(cursor_x, inner.y.saturating_add(2)));
            }
        }
        Modal::NetworkProxy {
            proxy_mode,
            field,
            proxy_url,
            no_proxy,
        } => {
            let inner = modal_panel(
                frame,
                area,
                app.language.text("Network proxy", "网络代理"),
                92,
                15,
            );
            let mode_label = app.language.text("Proxy mode", "代理模式");
            let url_label = app.language.text("Proxy URL", "代理地址");
            let bypass_label = app.language.text("No proxy", "代理绕过");
            let url_width = input_value_width(inner.width, url_label);
            let bypass_width = input_value_width(inner.width, bypass_label);
            let explicit = *proxy_mode == ProxyMode::Explicit;
            let mode_line = Line::from(vec![
                Span::styled(
                    if *field == 0 { "› " } else { "  " },
                    if *field == 0 {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(SUBTLE)
                    },
                ),
                Span::styled(
                    format!("{mode_label}  "),
                    if *field == 0 {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    },
                ),
                Span::styled(
                    if *proxy_mode == ProxyMode::Environment {
                        "[environment]"
                    } else {
                        " environment "
                    },
                    if *proxy_mode == ProxyMode::Environment {
                        Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(SUBTLE)
                    },
                ),
                Span::raw("  "),
                Span::styled(
                    if explicit { "[explicit]" } else { " explicit " },
                    if explicit {
                        Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(SUBTLE)
                    },
                ),
                Span::raw("  "),
                Span::styled(
                    if *proxy_mode == ProxyMode::Direct {
                        "[direct]"
                    } else {
                        " direct "
                    },
                    if *proxy_mode == ProxyMode::Direct {
                        Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(SUBTLE)
                    },
                ),
            ])
            .style(if *field == 0 {
                Style::default().bg(SURFACE)
            } else {
                Style::default().bg(PANEL_BG)
            });
            let disabled_input = |label: &str| {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{label}  "), Style::default().fg(DIM)),
                    Span::styled(
                        app.language
                            .text("— explicit mode only", "— 仅显式模式可用"),
                        Style::default().fg(SUBTLE),
                    ),
                ])
            };
            let mode_description = match (*proxy_mode, app.language) {
                (ProxyMode::Environment, Language::English) => {
                    "Inherit standard proxy variables from the environment."
                }
                (ProxyMode::Environment, Language::Chinese) => "继承环境中的标准代理变量。",
                (ProxyMode::Explicit, Language::English) => {
                    "Use only the HTTP/HTTPS proxy configured here."
                }
                (ProxyMode::Explicit, Language::Chinese) => "只使用此处配置的 HTTP/HTTPS 代理。",
                (ProxyMode::Direct, Language::English) => {
                    "Connect directly and remove proxy variables from update commands."
                }
                (ProxyMode::Direct, Language::Chinese) => "直接连接，并从更新命令中移除代理变量。",
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        app.language.text(
                            "Choose how dvup and update commands connect to the network.",
                            "选择 dvup 和更新命令的联网方式。",
                        ),
                        Style::default().fg(DIM),
                    ),
                    Line::raw(""),
                    mode_line,
                    if explicit {
                        modal_input_line(
                            *field == 1,
                            url_label,
                            proxy_url,
                            "http://127.0.0.1:7890",
                            url_width,
                        )
                    } else {
                        disabled_input(url_label)
                    },
                    if explicit {
                        modal_input_line(
                            *field == 2,
                            bypass_label,
                            no_proxy,
                            "localhost, .example.com",
                            bypass_width,
                        )
                    } else {
                        disabled_input(bypass_label)
                    },
                    Line::raw(""),
                    Line::styled(mode_description, Style::default().fg(SUBTLE)),
                    Line::styled(
                        app.language.text(
                            "No fallback. Explicit mode rejects credentials and SOCKS URLs.",
                            "不会回退；显式模式拒绝凭据和 SOCKS 地址。",
                        ),
                        Style::default().fg(SUBTLE),
                    ),
                    Line::raw(""),
                    Line::from(vec![
                        Span::styled("[←/→/Space]", Style::default().fg(ACCENT)),
                        Span::raw(app.language.text(" mode    ", " 模式    ")),
                        Span::styled("[Tab/↑↓]", Style::default().fg(ACCENT)),
                        Span::raw(app.language.text(" field", " 切换栏")),
                    ]),
                    Line::from(vec![
                        Span::styled("[Ctrl+S]", Style::default().fg(SUCCESS)),
                        Span::raw(app.language.text(" save    ", " 保存    ")),
                        Span::styled("[Enter]", Style::default().fg(SUCCESS)),
                        Span::raw(app.language.text(" next/save    ", " 下一步/保存    ")),
                        Span::styled("[Esc]", Style::default().fg(ERROR_COLOR)),
                        Span::raw(app.language.text(" cancel", " 取消")),
                    ]),
                ])
                .style(Style::default().bg(PANEL_BG)),
                inner,
            );
            app.modal_input_hitboxes.push(ModalInputHitbox {
                area: Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
                field: 0,
                visible_start: 0,
                visible_end: 0,
            });
            if !explicit {
                return;
            }
            for (field, label, input, row, value_width) in [
                (1, url_label, proxy_url, 3, url_width),
                (2, bypass_label, no_proxy, 4, bypass_width),
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
            let Some((label, input, row, value_width)) = (match *field {
                1 => Some((url_label, proxy_url, 3, url_width)),
                2 => Some((bypass_label, no_proxy, 4, bypass_width)),
                _ => None,
            }) else {
                return;
            };
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
        Modal::TomlEditor { .. } => unreachable!("TOML editor is rendered separately"),
    }
}

fn draw_toml_editor(frame: &mut Frame, app: &mut App, area: Rect) {
    let language = app.language;
    let status = app.message.clone();
    let Modal::TomlEditor { editor } = &mut app.modal else {
        return;
    };
    let title = if editor.dirty {
        language.text("TOML editor — modified", "TOML 编辑器 — 已修改")
    } else {
        language.text("TOML editor", "TOML 编辑器")
    };
    let title = format!("{title} [{}]", editor.mode.label());
    let inner = modal_panel(
        frame,
        area,
        &title,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                language.text(" File  ", " 文件  "),
                Style::default().fg(DIM),
            ),
            Span::styled(
                editor.path.display().to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]))
        .style(Style::default().bg(PANEL_BG)),
        chunks[0],
    );

    let editor_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(SURFACE));
    let editor_inner = editor_block.inner(chunks[1]);
    frame.render_widget(editor_block, chunks[1]);

    editor.refresh_highlights();
    let ranges = editor.line_ranges();
    let gutter_width = ranges.len().max(1).to_string().len().saturating_add(2);
    let editor_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(u16::try_from(gutter_width).unwrap_or(u16::MAX)),
            Constraint::Min(1),
        ])
        .split(editor_inner);
    let gutter_area = editor_columns[0];
    let content_area = editor_columns[1];
    if editor.follow_cursor {
        editor.ensure_cursor_visible(
            usize::from(content_area.height),
            usize::from(content_area.width),
        );
        editor.follow_cursor = false;
    }
    app.toml_editor_hitbox = Some(TomlEditorHitbox { area: content_area });

    let gutter_lines = (1..=ranges.len())
        .map(|number| {
            Line::styled(
                format!("{number:>width$} ", width = gutter_width.saturating_sub(1)),
                Style::default().fg(SUBTLE).bg(PANEL_BG),
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(gutter_lines)
            .scroll((u16::try_from(editor.scroll_y).unwrap_or(u16::MAX), 0))
            .style(Style::default().bg(PANEL_BG)),
        gutter_area,
    );

    let content_lines = ranges
        .iter()
        .map(|&(start, end)| toml_editor_line(editor, start, end))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(content_lines)
            .scroll((
                u16::try_from(editor.scroll_y).unwrap_or(u16::MAX),
                u16::try_from(editor.scroll_x).unwrap_or(u16::MAX),
            ))
            .style(Style::default().bg(SURFACE)),
        content_area,
    );

    render_scrollbar(
        frame,
        chunks[1],
        ranges.len(),
        usize::from(content_area.height),
        editor.scroll_y,
    );

    let (cursor_line, cursor_column) = editor.cursor_line_column();
    if cursor_line >= editor.scroll_y
        && cursor_line
            < editor
                .scroll_y
                .saturating_add(usize::from(content_area.height))
        && cursor_column >= editor.scroll_x
        && cursor_column
            < editor
                .scroll_x
                .saturating_add(usize::from(content_area.width))
    {
        frame.set_cursor_position(Position::new(
            content_area
                .x
                .saturating_add(u16::try_from(cursor_column - editor.scroll_x).unwrap_or(u16::MAX)),
            content_area
                .y
                .saturating_add(u16::try_from(cursor_line - editor.scroll_y).unwrap_or(u16::MAX)),
        ));
    }

    let mut help = vec![Line::styled(
        format!(" {status}"),
        Style::default()
            .fg(WARNING_COLOR)
            .add_modifier(Modifier::BOLD),
    )];
    if editor.mode == TomlEditorMode::Standard {
        help.extend([
            Line::from(vec![
                Span::styled("[Ctrl+S]", Style::default().fg(SUCCESS)),
                Span::raw(language.text(" save  ", " 保存  ")),
                Span::styled("[Esc]", Style::default().fg(ERROR_COLOR)),
                Span::raw(language.text(" close  ", " 关闭  ")),
                Span::styled("[F2]", Style::default().fg(ACCENT)),
                Span::raw(language.text(" Vim  ", " Vim  ")),
                Span::styled("[Ctrl+C]", Style::default().fg(ACCENT)),
                Span::raw(language.text(" copy  ", " 复制  ")),
                Span::styled("[Ctrl+V]", Style::default().fg(ACCENT)),
                Span::raw(language.text(" paste", " 粘贴")),
            ]),
            Line::styled(
                language.text(
                    "Ctrl+/ comment · Ctrl+Z/Y undo/redo · arrows move · Shift/mouse drag selects · wheel scrolls",
                    "Ctrl+/ 注释 · Ctrl+Z/Y 撤销/重做 · 方向键移动 · Shift/鼠标拖选 · 滚轮滚动",
                ),
                Style::default().fg(DIM),
            ),
        ]);
    } else {
        let vim_help = match editor.mode {
            TomlEditorMode::VimNormal => language.text(
                "NORMAL: h/j/k/l w/b 0/$ gg/G · i/a/I/A insert · v/V select · x dd yy p u Ctrl+R",
                "NORMAL：h/j/k/l w/b 0/$ gg/G 移动 · i/a/I/A 插入 · v/V 选择 · x dd yy p u Ctrl+R",
            ),
            TomlEditorMode::VimInsert => language.text(
                "INSERT: type to edit · Esc returns to NORMAL · arrows and standard Ctrl shortcuts work",
                "INSERT：直接输入编辑 · Esc 返回 NORMAL · 方向键和标准 Ctrl 快捷键仍可用",
            ),
            TomlEditorMode::VimVisual => language.text(
                "VISUAL: h/j/k/l w/b 0/$ select · y copy · d/x delete · c replace · Esc NORMAL",
                "VISUAL：h/j/k/l w/b 0/$ 选择 · y 复制 · d/x 删除 · c 替换 · Esc 返回 NORMAL",
            ),
            TomlEditorMode::Standard => unreachable!(),
        };
        help.extend([
            Line::from(vec![
                Span::styled("[F2]", Style::default().fg(ACCENT)),
                Span::raw(language.text(" standard  ", " 标准模式  ")),
                Span::styled("[Ctrl+S]", Style::default().fg(SUCCESS)),
                Span::raw(language.text(" save  ", " 保存  ")),
                Span::styled("[Ctrl+Q]", Style::default().fg(ERROR_COLOR)),
                Span::raw(language.text(" close  ", " 关闭  ")),
                Span::styled("[Ctrl+/]", Style::default().fg(ACCENT)),
                Span::raw(language.text(" comment", " 注释")),
            ]),
            Line::styled(vim_help, Style::default().fg(DIM)),
        ]);
    }
    frame.render_widget(
        Paragraph::new(help).style(Style::default().bg(PANEL_BG)),
        chunks[2],
    );
}

fn toml_editor_line(editor: &TomlEditor, start: usize, end: usize) -> Line<'static> {
    let base = Style::default().fg(Color::White).bg(SURFACE);
    if start == end {
        return Line::styled(String::new(), base);
    }

    let selection = editor.selection();
    let mut boundaries = vec![start, end];
    for highlight in &editor.highlights {
        if highlight.start < end && highlight.end > start {
            boundaries.push(highlight.start.max(start));
            boundaries.push(highlight.end.min(end));
        }
    }
    if let Some((selection_start, selection_end)) = selection
        && selection_start < end
        && selection_end > start
    {
        boundaries.push(selection_start.max(start));
        boundaries.push(selection_end.min(end));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let selection_style = Style::default()
        .fg(Color::Black)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD);
    let spans = boundaries
        .windows(2)
        .filter_map(|boundary| {
            let segment_start = boundary[0];
            let segment_end = boundary[1];
            (segment_start < segment_end).then(|| {
                let selected = selection.is_some_and(|(selection_start, selection_end)| {
                    selection_start <= segment_start && segment_end <= selection_end
                });
                let style = if selected {
                    selection_style
                } else {
                    editor
                        .highlights
                        .iter()
                        .find(|highlight| {
                            highlight.start <= segment_start && segment_start < highlight.end
                        })
                        .map_or(base, |highlight| highlight.style)
                };
                Span::styled(editor.text[segment_start..segment_end].to_owned(), style)
            })
        })
        .collect::<Vec<_>>();
    Line::from(spans)
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
    .then(|| numeric.to_owned())
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

fn latest_version_label(state: &VersionState, language: Language) -> String {
    let VersionState::Failed(error) = state else {
        return state.label().to_owned();
    };
    match (error.kind, language) {
        (version::LatestVersionErrorKind::RateLimited, Language::English) => "rate limited",
        (version::LatestVersionErrorKind::RateLimited, Language::Chinese) => "已限流",
        (version::LatestVersionErrorKind::Authentication, Language::English) => "auth failed",
        (version::LatestVersionErrorKind::Authentication, Language::Chinese) => "认证失败",
        (version::LatestVersionErrorKind::NotFound, Language::English) => "not found",
        (version::LatestVersionErrorKind::NotFound, Language::Chinese) => "未找到",
        (version::LatestVersionErrorKind::RequestFailed, Language::English)
        | (version::LatestVersionErrorKind::InvalidResponse, Language::English) => "fetch failed",
        (version::LatestVersionErrorKind::RequestFailed, Language::Chinese)
        | (version::LatestVersionErrorKind::InvalidResponse, Language::Chinese) => "获取失败",
    }
    .to_owned()
}

fn latest_version_error_message(
    name: &str,
    error: &version::LatestVersionError,
    language: Language,
) -> String {
    match (error.kind, language) {
        (version::LatestVersionErrorKind::RateLimited, Language::English) => format!(
            "Latest-version check for {name} was rate-limited; wait for quota reset or verify the GitHub Token ({})",
            error.detail()
        ),
        (version::LatestVersionErrorKind::RateLimited, Language::Chinese) => format!(
            "{name} 的最新版本检查已被限流；请等待配额重置或检查 GitHub Token（{}）",
            error.detail()
        ),
        (version::LatestVersionErrorKind::Authentication, Language::English) => format!(
            "Latest-version authentication failed for {name}; replace the GitHub Token ({})",
            error.detail()
        ),
        (version::LatestVersionErrorKind::Authentication, Language::Chinese) => format!(
            "{name} 的最新版本认证失败；请重新配置 GitHub Token（{}）",
            error.detail()
        ),
        (version::LatestVersionErrorKind::NotFound, Language::English) => format!(
            "Latest version for {name} was not found; check its repository or Release configuration ({})",
            error.detail()
        ),
        (version::LatestVersionErrorKind::NotFound, Language::Chinese) => format!(
            "未找到 {name} 的最新版本；请检查仓库或 Release 配置（{}）",
            error.detail()
        ),
        (
            version::LatestVersionErrorKind::RequestFailed
            | version::LatestVersionErrorKind::InvalidResponse,
            Language::English,
        ) => format!(
            "Could not fetch the latest version for {name}; check network/proxy settings and press r to retry ({})",
            error.detail()
        ),
        (
            version::LatestVersionErrorKind::RequestFailed
            | version::LatestVersionErrorKind::InvalidResponse,
            Language::Chinese,
        ) => format!(
            "无法获取 {name} 的最新版本；请检查网络/代理设置并按 r 重试（{}）",
            error.detail()
        ),
    }
}

fn latest_version_style(tool: &ToolItem) -> Style {
    match (&tool.version, &tool.latest_version) {
        (VersionState::Available(installed), VersionState::Available(latest))
            if installed == latest =>
        {
            Style::default().fg(SUCCESS)
        }
        (VersionState::Available(_), VersionState::Available(_)) => {
            Style::default().fg(WARNING_COLOR)
        }
        _ => tool.latest_version.style(),
    }
}

fn tool_is_up_to_date(tool: &ToolItem) -> bool {
    matches!(
        (&tool.version, &tool.latest_version),
        (VersionState::Available(installed), VersionState::Available(latest))
            if installed == latest
    )
}

fn update_arguments(
    name: &str,
    config_path: Option<&Path>,
    terminate_locking_processes: bool,
    target_version: Option<&str>,
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
    if let Some(version) = target_version {
        arguments.push("--to".to_owned());
        arguments.push(version.to_owned());
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

fn load_initial_data<F>(
    state: StateDirs,
    config_path: Option<PathBuf>,
    mut on_progress: F,
) -> Result<InitialLoadData>
where
    F: FnMut(InitialLoadProgress),
{
    thread::scope(|scope| {
        let job_state = state.clone();
        let jobs = scope.spawn(move || load_job_items(&job_state));
        let (tools, github_monitors) = load_tool_items(&state, config_path, &mut on_progress)?;
        on_progress(InitialLoadProgress {
            phase: InitialLoadPhase::Jobs,
            completed: tools.len(),
            total: tools.len().saturating_add(1),
        });
        let jobs = jobs
            .join()
            .map_err(|_| Error::Message("startup job loader stopped unexpectedly".to_owned()))??;
        Ok(InitialLoadData {
            tools,
            github_monitors,
            jobs,
        })
    })
}

fn load_tool_items<F>(
    state: &StateDirs,
    config_path: Option<PathBuf>,
    mut on_progress: F,
) -> Result<(Vec<ToolItem>, Vec<GithubReleaseMonitor>)>
where
    F: FnMut(InitialLoadProgress),
{
    on_progress(InitialLoadProgress {
        phase: InitialLoadPhase::Configuration,
        completed: 0,
        total: 1,
    });
    let (manifest, working_directory, _) = cli::load_manifest(config_path.clone(), state)?;
    let tool_kinds = load_tool_kinds(state, config_path.as_deref())?;
    let total = manifest.tools.len().saturating_add(1);
    on_progress(InitialLoadProgress {
        phase: InitialLoadPhase::Tools,
        completed: 0,
        total,
    });

    let definitions = manifest.tools.into_iter().collect::<Vec<_>>();
    let readiness =
        command::tool_readiness_many(definitions.iter().map(|(_, tool)| tool), &working_directory);
    let tools = definitions
        .into_iter()
        .zip(readiness)
        .enumerate()
        .map(|(index, ((name, tool), readiness))| {
            let item = build_tool_item(name, tool, readiness, &working_directory, &tool_kinds);
            on_progress(InitialLoadProgress {
                phase: InitialLoadPhase::Tools,
                completed: index.saturating_add(1),
                total,
            });
            item
        })
        .collect();
    Ok((tools, manifest.github.monitors))
}

fn build_tool_item(
    name: String,
    tool: Tool,
    readiness: command::ToolReadiness,
    working_directory: &Path,
    tool_kinds: &HashMap<String, ToolKind>,
) -> ToolItem {
    let probe_command = command::probe_spec(&tool, working_directory);
    let availability = match readiness {
        command::ToolReadiness::Unsupported => Availability::Unsupported,
        command::ToolReadiness::TargetMissing => Availability::Missing,
        command::ToolReadiness::UpdaterMissing => Availability::UpdaterMissing,
        command::ToolReadiness::Installed => Availability::Installed,
    };
    let allows_version_checks = availability.allows_version_checks();
    let actual_command = format_command(&tool.program, &tool.args);
    ToolItem {
        command: cli::display_command(&name, &actual_command),
        kind: tool_kinds.get(&name).copied().unwrap_or(ToolKind::BuiltIn),
        name,
        availability,
        version: if allows_version_checks {
            VersionState::Loading
        } else {
            VersionState::Unavailable
        },
        version_command: probe_command,
        version_probe_id: 0,
        latest_version: if allows_version_checks && tool.latest.is_some() {
            VersionState::Loading
        } else {
            VersionState::Unavailable
        },
        latest_source: tool.latest,
        latest_probe_id: 0,
        supports_target_version: tool.update_version.is_some(),
        selected: false,
        run_state: RunState::Idle,
        elapsed: None,
    }
}

fn load_job_items(state: &StateDirs) -> Result<Vec<JobItem>> {
    Ok(JobStore::new(state.clone())?
        .list()?
        .into_iter()
        .map(|job| JobItem {
            id: job.id,
            name: job.name,
            status: job.status,
            updated_at_unix_ms: job.updated_at_unix_ms,
        })
        .collect())
}

fn load_tool_kinds(
    state: &StateDirs,
    explicit_config: Option<&Path>,
) -> Result<HashMap<String, ToolKind>> {
    let path = explicit_config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state.custom_config_path());
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    Ok(UserConfig::load(&path)?
        .tools
        .into_keys()
        .map(|name| (name, ToolKind::Custom))
        .collect())
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

    #[test]
    fn startup_loading_screen_renders_progress_before_tools_are_ready() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::empty(state, None).expect("empty app");
        app.initial_load = Some(InitialLoadProgress {
            phase: InitialLoadPhase::Tools,
            completed: 3,
            total: 8,
        });

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render startup progress");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(screen.contains("Starting dvup"), "screen: {screen}");
        assert!(
            screen.contains("Checking installed tools"),
            "screen: {screen}"
        );
        assert!(screen.contains("37%"), "screen: {screen}");
        assert!(app.tools.is_empty());
    }

    #[test]
    fn q_does_not_exit_during_initial_loading() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::empty(state, None).expect("empty app");
        app.initial_load = Some(InitialLoadProgress {
            phase: InitialLoadPhase::Configuration,
            completed: 0,
            total: 1,
        });

        dispatch_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        );

        assert!(!app.should_quit);
    }

    #[test]
    fn initial_loading_requires_two_consecutive_ctrl_c_presses_to_exit() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::empty(state, None).expect("empty app");
        app.initial_load = Some(InitialLoadProgress {
            phase: InitialLoadPhase::Configuration,
            completed: 0,
            total: 1,
        });
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        dispatch_event(&mut app, ctrl_c.clone());

        assert!(!app.should_quit);
        assert!(app.ctrl_c_armed);
        assert_eq!(app.message, "Press Ctrl+C again to quit");

        dispatch_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        );
        assert!(!app.ctrl_c_armed);

        dispatch_event(&mut app, ctrl_c.clone());
        assert!(!app.should_quit);
        assert!(app.ctrl_c_armed);

        dispatch_event(&mut app, ctrl_c);

        assert!(app.should_quit);
    }

    #[test]
    fn main_view_paints_a_dark_background_across_the_entire_terminal() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render main view");

        let buffer = terminal.backend().buffer();
        for point in [(0, 4), (1, 12), (50, 18)] {
            assert_eq!(
                buffer[point].bg, SURFACE,
                "main view did not paint its base background at {point:?}"
            );
        }
    }

    #[test]
    fn idle_job_polling_is_slower_until_a_job_becomes_active() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::empty(state, None).expect("empty app");

        assert_eq!(app.job_refresh_interval(), IDLE_JOB_REFRESH_INTERVAL);

        app.jobs.push(JobItem {
            id: "pending".to_owned(),
            name: "example".to_owned(),
            status: JobStatus::Pending,
            updated_at_unix_ms: 0,
        });
        assert_eq!(app.job_refresh_interval(), ACTIVE_JOB_REFRESH_INTERVAL);
    }

    #[test]
    fn tool_kinds_have_one_custom_category() {
        assert_eq!(ToolKind::BuiltIn.label(Language::English), "built-in");
        assert_eq!(ToolKind::BuiltIn.label(Language::Chinese), "内置");
        assert_eq!(ToolKind::Custom.label(Language::English), "custom");
        assert_eq!(ToolKind::Custom.label(Language::Chinese), "自定义");
    }

    #[test]
    fn initial_load_skips_version_checks_for_missing_and_unsupported_tools() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::empty(state, None).expect("empty app");
        let versioned_tool = |name: &str, availability| ToolItem {
            name: name.to_owned(),
            command: format!("{name} update"),
            version: VersionState::Loading,
            version_command: test_version_command(name),
            version_probe_id: 0,
            latest_version: VersionState::Loading,
            latest_source: Some(LatestVersionSource::Npm {
                package: name.to_owned(),
            }),
            latest_probe_id: 0,
            supports_target_version: false,
            availability,
            kind: ToolKind::BuiltIn,
            selected: false,
            run_state: RunState::Idle,
            elapsed: None,
        };

        app.apply_initial_load(InitialLoadData {
            tools: vec![
                versioned_tool("installed", Availability::Installed),
                versioned_tool("updater-missing", Availability::UpdaterMissing),
                versioned_tool("missing", Availability::Missing),
                versioned_tool("unsupported", Availability::Unsupported),
            ],
            github_monitors: Vec::new(),
            jobs: Vec::new(),
        });

        assert_eq!(app.next_version_probe_id, 2);
        assert_eq!(app.next_latest_probe_id, 2);
        for tool in &app.tools[..2] {
            assert!(matches!(tool.version, VersionState::Loading));
            assert!(matches!(tool.latest_version, VersionState::Loading));
            assert_ne!(tool.version_probe_id, 0);
            assert_ne!(tool.latest_probe_id, 0);
        }
        for tool in &app.tools[2..] {
            assert!(matches!(tool.version, VersionState::Unavailable));
            assert!(matches!(tool.latest_version, VersionState::Unavailable));
            assert_eq!(tool.version_probe_id, 0);
            assert_eq!(tool.latest_probe_id, 0);
        }
    }

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
                probe_args: vec!["--version".to_owned()],
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
        assert_eq!(
            version_from_output(
                b"Hermes Agent v0.18.2 (2026.7.7.2) - upstream 7b5ba205\n",
                b"",
            ),
            Some("0.18.2".to_owned())
        );
    }

    #[test]
    fn builds_version_commands_from_explicit_probes() {
        let working_directory = PathBuf::from("workspace");
        let uv = Config::starter().tools.remove("uv").expect("uv preset");
        let uv_version = command::probe_spec(&uv, &working_directory);
        assert_eq!(uv_version.program, "uv");
        assert_eq!(uv_version.args, ["--version"]);

        let ripgrep = Tool::custom(
            "ripgrep",
            "/opt/homebrew/bin/brew".to_owned(),
            vec!["upgrade".to_owned(), "ripgrep".to_owned()],
        );
        let ripgrep_version = command::probe_spec(&ripgrep, &working_directory);
        assert_eq!(ripgrep_version.program, "ripgrep");
        assert_eq!(ripgrep_version.args, ["--version"]);

        let claude = Tool::custom(
            "claude-custom",
            "claude".to_owned(),
            vec!["install".to_owned()],
        );
        let claude_version = command::probe_spec(&claude, &working_directory);
        assert_eq!(claude_version.program, "claude-custom");
        assert_eq!(claude_version.args, ["--version"]);
    }

    #[test]
    fn ignores_stale_version_probe_results() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state.clone(), None).expect("app");
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
    fn ignores_stale_latest_version_results() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let tool = app
            .tools
            .iter_mut()
            .find(|tool| tool.latest_source.is_some())
            .expect("version-aware built-in tool");
        let name = tool.name.clone();
        let current_probe = tool.latest_probe_id;
        tool.latest_version = VersionState::Loading;

        app.tx
            .send(AppEvent::LatestVersionResolved {
                name: name.clone(),
                probe_id: current_probe.wrapping_sub(1),
                result: Ok("0.0.1".to_owned()),
            })
            .expect("stale result");
        app.process_events();
        let tool = app.tools.iter().find(|tool| tool.name == name).unwrap();
        assert!(matches!(tool.latest_version, VersionState::Loading));

        app.tx
            .send(AppEvent::LatestVersionResolved {
                name: name.clone(),
                probe_id: current_probe,
                result: Ok("1.2.3".to_owned()),
            })
            .expect("current result");
        app.process_events();
        let tool = app.tools.iter().find(|tool| tool.name == name).unwrap();
        assert!(matches!(
            &tool.latest_version,
            VersionState::Available(version) if version == "1.2.3"
        ));
    }

    #[test]
    fn github_latest_version_rate_limit_is_visible_in_the_tool_row_and_footer() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.language = Language::Chinese;
        let tool = app
            .tools
            .iter_mut()
            .find(|tool| {
                matches!(
                    tool.latest_source,
                    Some(
                        LatestVersionSource::GithubRelease { .. }
                            | LatestVersionSource::GithubTag { .. }
                    )
                )
            })
            .expect("GitHub latest-version tool");
        let name = tool.name.clone();
        let probe_id = tool.latest_probe_id;
        tool.latest_version = VersionState::Loading;
        app.tx
            .send(AppEvent::LatestVersionResolved {
                name: name.clone(),
                probe_id,
                result: Err(version::LatestVersionError::new(
                    version::LatestVersionErrorKind::RateLimited,
                    "http status: 403",
                )),
            })
            .expect("rate-limited result");

        app.process_events();

        let tool = app.tools.iter().find(|tool| tool.name == name).unwrap();
        assert!(matches!(
            &tool.latest_version,
            VersionState::Failed(error)
                if error.kind == version::LatestVersionErrorKind::RateLimited
        ));
        assert!(app.message.contains("已被限流"), "message: {}", app.message);
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render rate-limited tool");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = screen.replace(' ', "");
        assert!(compact.contains("已限流"), "screen: {screen}");
        assert!(compact.contains("GitHubToken"), "screen: {screen}");
    }

    #[test]
    fn latest_version_network_failure_has_a_retry_prompt() {
        let error = version::LatestVersionError::new(
            version::LatestVersionErrorKind::RequestFailed,
            "timeout",
        );

        assert_eq!(
            latest_version_label(&VersionState::Failed(error.clone()), Language::English),
            "fetch failed"
        );
        let message = latest_version_error_message("example", &error, Language::English);
        assert!(message.contains("network/proxy"));
        assert!(message.contains("press r to retry"));
    }

    #[test]
    fn target_version_shortcut_collects_one_exact_version() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let index = app.focused_tool_index().expect("focused tool");
        let name = app.tools[index].name.clone();
        app.tools[index].availability = Availability::Installed;
        app.tools[index].supports_target_version = true;

        handle_tools_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        );
        assert!(matches!(
            &app.modal,
            Modal::TargetVersion { name: modal_name, .. } if modal_name == &name
        ));

        for character in "1.2.3".chars() {
            handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            );
        }
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &app.modal,
            Modal::ConfirmUpdate {
                tools,
                target_version: Some(version),
                ..
            } if tools == &[name] && version == "1.2.3"
        ));
    }

    #[test]
    fn enter_skips_a_tool_that_is_already_at_the_latest_version() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let index = app.focused_tool_index().expect("focused tool");
        app.tools[index].availability = Availability::Installed;
        app.tools[index].version = VersionState::Available("1.2.3".to_owned());
        app.tools[index].latest_version = VersionState::Available("1.2.3".to_owned());
        app.tools[index].selected = true;

        handle_tools_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.running, 0);
        assert!(app.message.contains("latest"));
    }

    #[test]
    fn enter_confirms_only_tools_that_differ_from_the_latest_version() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let current = app.tools[0].name.clone();
        let outdated = app.tools[1].name.clone();
        for tool in &mut app.tools[0..=1] {
            tool.availability = Availability::Installed;
            tool.selected = true;
        }
        app.tools[0].version = VersionState::Available("1.2.3".to_owned());
        app.tools[0].latest_version = VersionState::Available("1.2.3".to_owned());
        app.tools[1].version = VersionState::Available("1.2.2".to_owned());
        app.tools[1].latest_version = VersionState::Available("1.2.3".to_owned());

        handle_tools_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            &app.modal,
            Modal::ConfirmUpdate {
                tools,
                current_tools,
                target_version: None,
            } if tools == &[outdated] && current_tools == &[current]
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
    fn entering_doctor_waits_for_an_explicit_scan() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");

        app.select_tab(Tab::Doctor);

        assert_eq!(app.tab, Tab::Doctor);
        assert_eq!(app.doctor_probe_id, 0);
        assert!(!app.doctor_loading);
        assert!(app.doctor_checked_at.is_none());
        assert!(app.message.contains("Enter"));
    }

    #[test]
    fn doctor_enter_starts_the_first_scan_and_busy_keys_do_not_duplicate_it() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tab = Tab::Doctor;

        handle_doctor_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.doctor_loading);
        assert_eq!(app.doctor_probe_id, 1);
        assert_eq!(app.next_doctor_probe_id, 1);

        handle_doctor_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_normal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE),
        );
        assert_eq!(app.doctor_probe_id, 1);
        assert_eq!(app.next_doctor_probe_id, 1);
        assert!(app.message.contains("already running"));
    }

    #[test]
    fn doctor_r_starts_the_first_scan_and_rescans_existing_results() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tab = Tab::Doctor;

        handle_normal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        );
        assert!(app.doctor_loading);
        assert_eq!(app.doctor_probe_id, 1);
        assert!(app.message.contains("Scanning installation conflicts"));

        app.doctor_loading = false;
        app.doctor_checked_at = Some("2026-07-12 20:00:00".to_owned());
        app.doctor_diagnoses = vec![test_diagnosis("uv", &[("first/uv", Some("1.0.0"))])];
        handle_normal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE),
        );
        assert!(app.doctor_loading);
        assert_eq!(app.doctor_probe_id, 2);
        assert_eq!(app.doctor_diagnoses.len(), 1);
    }

    #[test]
    fn doctor_view_prompts_before_the_first_scan_in_both_languages() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tab = Tab::Doctor;
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render English Doctor prompt");
        let english = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(english.contains("not scanned"), "screen: {english}");
        assert!(english.contains("Press Enter to scan"), "screen: {english}");

        app.language = Language::Chinese;
        terminal.clear().expect("clear English Doctor prompt");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render Chinese Doctor prompt");
        let chinese = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact_chinese = chinese.replace(' ', "");
        assert!(compact_chinese.contains("尚未诊断"), "screen: {chinese}");
        assert!(
            compact_chinese.contains("按Enter开始扫描"),
            "screen: {chinese}"
        );
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
    fn tool_filter_hides_unavailable_doctor_rows_without_discarding_diagnoses() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let mut unsupported = test_diagnosis("skip", &[("path/skip", Some("1.0.0"))]);
        unsupported.supported = false;
        app.doctor_diagnoses = vec![
            unsupported,
            test_diagnosis("missing", &[]),
            test_diagnosis("ready", &[("path/ready", Some("1.0.0"))]),
        ];
        app.doctor_checked_at = Some("2026-07-13 12:00:00".to_owned());
        app.doctor_index = 1;
        app.expanded_doctor = Some("missing".to_owned());

        app.toggle_setting(1);

        assert!(app.settings.hide_unsupported_and_missing_tools);
        assert_eq!(app.doctor_diagnoses.len(), 3);
        assert_eq!(app.visible_doctor_count(), 1);
        assert_eq!(
            app.focused_doctor_diagnosis()
                .map(|diagnosis| diagnosis.name.as_str()),
            Some("ready")
        );
        assert!(app.expanded_doctor.is_none());

        app.tab = Tab::Doctor;
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render filtered Doctor rows");
        let filtered_screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            !filtered_screen.contains("skip"),
            "screen: {filtered_screen}"
        );
        assert!(
            !filtered_screen.contains("missing"),
            "screen: {filtered_screen}"
        );
        assert!(
            filtered_screen.contains("ready"),
            "screen: {filtered_screen}"
        );
        assert!(
            filtered_screen.contains("1 tools · 0 warn"),
            "screen: {filtered_screen}"
        );
        assert_eq!(app.doctor_hitboxes.len(), 1);

        let ready_area = app.doctor_hitboxes[0].0;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: ready_area.x,
                row: ready_area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.expanded_doctor.as_deref(), Some("ready"));

        app.toggle_setting(1);
        assert!(!app.settings.hide_unsupported_and_missing_tools);
        assert_eq!(app.visible_doctor_count(), 3);
        assert_eq!(app.doctor_diagnoses.len(), 3);
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
    fn github_api_key_submission_ignores_clipboard_edge_whitespace() {
        assert_eq!(
            github_api_key_submission("  github_pat_example_123\r\n"),
            Some("github_pat_example_123")
        );
        assert_eq!(github_api_key_submission(" \r\n\t"), None);
    }

    #[test]
    fn github_api_key_modal_persists_encrypted_settings_across_restart() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let token = "github_pat_tui_restart_test";
        let mut app = App::new(state.clone(), None).expect("app");
        app.modal = Modal::GithubApiKey {
            api_key: TextInput::new(token),
        };

        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(app.modal, Modal::None));
        assert!(app.github_api_key_configured);
        let serialized = std::fs::read_to_string(state.settings_path()).expect("saved settings");
        assert!(serialized.contains("encrypted_api_key"));
        assert!(!serialized.contains(token));

        let restarted = App::new(state, None).expect("restarted app");
        let decrypted =
            credential::github_api_key(restarted.settings.github.encrypted_api_key.as_deref())
                .expect("decrypt restarted GitHub API key")
                .expect("restarted GitHub API key configured");
        assert_eq!(decrypted.as_str(), token);
    }

    #[test]
    fn github_api_key_save_failure_keeps_masked_input_and_reports_settings_path() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let settings_path = state.settings_path();
        let token = "github_pat_tui_retry_test";
        let mut app = App::new(state, None).expect("app");
        app.language = Language::Chinese;
        std::fs::create_dir(&settings_path).expect("block settings destination with a directory");
        app.modal = Modal::GithubApiKey {
            api_key: TextInput::new(token),
        };

        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            &app.modal,
            Modal::GithubApiKey { api_key } if api_key.value == token
        ));
        assert!(app.settings.github.encrypted_api_key.is_none());
        assert!(!app.github_api_key_configured);
        assert!(app.github_credential_error.is_none());
        assert!(
            app.message.contains(&settings_path.display().to_string()),
            "message: {}",
            app.message
        );
        assert!(
            app.message.contains("Enter 重试"),
            "message: {}",
            app.message
        );
        assert!(!app.message.contains(token));
    }

    #[test]
    fn l_switches_languages_but_remains_text_in_the_add_form() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state.clone(), None).expect("app");
        let lower_l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);

        handle_key(&mut app, lower_l);
        assert_eq!(app.language, Language::Chinese);
        assert_eq!(app.settings.language, Language::Chinese);
        assert_eq!(
            AppSettings::load(&state.settings_path())
                .expect("persisted language")
                .language,
            Language::Chinese
        );
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

        let restarted = App::new(state, None).expect("restarted app");
        assert_eq!(restarted.language, Language::Chinese);
        assert_eq!(restarted.activity[0], "欢迎使用 dvup。");
        assert_eq!(
            restarted.activity[1],
            "按 Space 选择工具，然后按 Enter 更新。"
        );
        assert_eq!(restarted.message, "就绪");
    }

    #[test]
    fn modal_keyboard_input_never_reaches_the_underlying_view() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::ConfirmUpdate {
            tools: vec!["example".to_owned()],
            target_version: None,
            current_tools: Vec::new(),
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
        assert!(!app.ctrl_c_armed);
        assert!(!app.should_quit);
    }

    #[test]
    fn queued_self_update_tells_the_user_to_exit() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let dvup = app
            .tools
            .iter_mut()
            .find(|tool| tool.name == "dvup")
            .expect("dvup preset");
        dvup.run_state = RunState::Queued;
        app.update_batch = Some(UpdateBatch {
            started: Instant::now(),
            total: 1,
        });

        app.finish_update_batch();

        assert!(app.message.contains("exit dvup"));
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
            latest_version: VersionState::Unavailable,
            latest_source: None,
            latest_probe_id: 0,
            supports_target_version: false,
            availability: Availability::Installed,
            kind: ToolKind::BuiltIn,
            selected: false,
            run_state: RunState::Idle,
            elapsed: None,
        }];
        app.rebuild_visible_tool_indices(None);
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
        let mut custom = UserConfig::empty();
        custom.tools.insert(
            "example".to_owned(),
            UserTool::custom(
                "example",
                "example-cli".to_owned(),
                vec!["update".to_owned(), "two words".to_owned()],
            ),
        );
        custom
            .save(&state.custom_config_path())
            .expect("save custom command");
        let mut app = App::new(state, None).expect("app");
        app.focus_tool_named("example");
        assert_eq!(
            app.focused_tool().expect("global custom tool").kind,
            ToolKind::Custom
        );

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
                latest_version: VersionState::Unavailable,
                latest_source: None,
                latest_probe_id: 0,
                supports_target_version: false,
                availability: Availability::Installed,
                kind: ToolKind::BuiltIn,
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
                latest_version: VersionState::Unavailable,
                latest_source: None,
                latest_probe_id: 0,
                supports_target_version: false,
                availability: Availability::Missing,
                kind: ToolKind::BuiltIn,
                selected: false,
                run_state: RunState::Idle,
                elapsed: None,
            },
        ];
        app.rebuild_visible_tool_indices(None);
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
        assert!(screen.contains("INSTALLED"), "screen: {screen}");
        assert!(screen.contains("LATEST"), "screen: {screen}");
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
    fn filtered_tool_rows_map_to_the_canonical_tool_without_reprobing() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tools = vec![
            ToolItem {
                name: "missing".to_owned(),
                command: "missing update".to_owned(),
                version: VersionState::Unavailable,
                version_command: test_version_command("missing"),
                version_probe_id: 11,
                latest_version: VersionState::Unavailable,
                latest_source: None,
                latest_probe_id: 0,
                supports_target_version: false,
                availability: Availability::Missing,
                kind: ToolKind::BuiltIn,
                selected: false,
                run_state: RunState::Idle,
                elapsed: None,
            },
            ToolItem {
                name: "installed".to_owned(),
                command: "installed update".to_owned(),
                version: VersionState::Available("1.2.3".to_owned()),
                version_command: test_version_command("installed"),
                version_probe_id: 12,
                latest_version: VersionState::Unavailable,
                latest_source: None,
                latest_probe_id: 0,
                supports_target_version: false,
                availability: Availability::Installed,
                kind: ToolKind::BuiltIn,
                selected: false,
                run_state: RunState::Idle,
                elapsed: None,
            },
        ];
        app.next_version_probe_id = 12;
        app.settings.hide_unsupported_and_missing_tools = true;
        app.rebuild_visible_tool_indices(Some("installed"));
        app.tab = Tab::Tools;
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render filtered tools");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!screen.contains("missing update"), "screen: {screen}");
        assert!(screen.contains("installed update"), "screen: {screen}");
        assert_eq!(app.visible_tool_indices, [1]);
        assert_eq!(app.tool_index, 0);
        assert_eq!(
            app.focused_tool().map(|tool| tool.name.as_str()),
            Some("installed")
        );

        let area = app.tool_hitboxes[0].0;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert!(!app.tools[0].selected);
        assert!(app.tools[1].selected);
        assert_eq!(app.selected_for_update(), ["installed"]);
        assert_eq!(app.next_version_probe_id, 12);
        assert_eq!(app.tools[1].version_probe_id, 12);
        assert!(matches!(
            &app.tools[1].version,
            VersionState::Available(version) if version == "1.2.3"
        ));
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
                latest_version: VersionState::Unavailable,
                latest_source: None,
                latest_probe_id: 0,
                supports_target_version: false,
                availability: Availability::Installed,
                kind: ToolKind::BuiltIn,
                selected: false,
                run_state: RunState::Idle,
                elapsed: None,
            })
            .collect();
        app.rebuild_visible_tool_indices(None);
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
            NetworkSettings::default(),
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
        let mut job = crate::job::Job::from_tool(
            "rustup".to_owned(),
            tool,
            temporary.path().to_path_buf(),
            NetworkSettings::default(),
        );
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
    fn settings_toggle_persists_and_enables_diagnostics_on_the_next_tui_start() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state.clone(), None).expect("app");
        app.tab = Tab::Settings;

        assert!(!app.settings.auto_diagnose_on_startup);
        assert_eq!(app.doctor_probe_id, 0);
        handle_settings_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );

        assert!(app.settings.auto_diagnose_on_startup);
        assert_eq!(app.doctor_probe_id, 0);
        assert!(app.message.contains("next time TUI starts"));
        assert!(
            AppSettings::load(&state.settings_path())
                .expect("saved settings")
                .auto_diagnose_on_startup
        );

        let restarted = App::new(state, None).expect("restarted app");
        assert!(restarted.settings.auto_diagnose_on_startup);
        assert!(restarted.doctor_loading);
        assert_eq!(restarted.doctor_probe_id, 1);
        assert!(
            restarted
                .message
                .contains("Scanning installation conflicts")
        );
    }

    #[test]
    fn settings_view_supports_mouse_focus_and_click_toggle() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state.clone(), None).expect("app");
        let unfiltered_count = app.tools.len();
        let original_names = app
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let original_probe_ids = app
            .tools
            .iter()
            .map(|tool| tool.version_probe_id)
            .collect::<Vec<_>>();
        let original_versions = app
            .tools
            .iter()
            .map(|tool| tool.version.label().to_owned())
            .collect::<Vec<_>>();
        let next_probe_id = app.next_version_probe_id;
        assert!(app.tools.iter().any(|tool| matches!(
            tool.availability,
            Availability::Unsupported | Availability::Missing
        )));
        app.tab = Tab::Settings;
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render settings");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Settings"), "screen: {screen}");
        assert!(
            screen.contains("Run Doctor diagnostics when TUI starts"),
            "screen: {screen}"
        );
        assert!(
            screen.contains("Hide unsupported or uninstalled tools"),
            "screen: {screen}"
        );
        assert_eq!(app.settings_hitboxes.len(), SETTINGS_ROW_COUNT);
        let area = app.settings_hitboxes[0].0;

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.settings_index, 0);
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert!(app.settings.auto_diagnose_on_startup);
        assert!(
            AppSettings::load(&state.settings_path())
                .expect("mouse-saved settings")
                .auto_diagnose_on_startup
        );

        let filter_area = app.settings_hitboxes[1].0;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: filter_area.x,
                row: filter_area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.settings_index, 1);
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: filter_area.x,
                row: filter_area.y,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert!(app.settings.hide_unsupported_and_missing_tools);
        assert_eq!(app.tools.len(), unfiltered_count);
        assert_eq!(
            app.tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>(),
            original_names
        );
        assert_eq!(
            app.tools
                .iter()
                .map(|tool| tool.version_probe_id)
                .collect::<Vec<_>>(),
            original_probe_ids
        );
        assert_eq!(app.next_version_probe_id, next_probe_id);
        assert!(app.visible_tool_indices.len() < unfiltered_count);
        assert!(app.visible_tool_indices.iter().all(|&index| !matches!(
            app.tools[index].availability,
            Availability::Unsupported | Availability::Missing
        )));
        assert_eq!(
            app.tools
                .iter()
                .map(|tool| tool.version.label().to_owned())
                .collect::<Vec<_>>(),
            original_versions
        );
        assert!(
            AppSettings::load(&state.settings_path())
                .expect("mouse-saved tool filter")
                .hide_unsupported_and_missing_tools
        );

        app.toggle_setting(1);
        assert!(!app.settings.hide_unsupported_and_missing_tools);
        assert_eq!(
            app.visible_tool_indices,
            (0..app.tools.len()).collect::<Vec<_>>()
        );
        assert_eq!(app.next_version_probe_id, next_probe_id);
        assert_eq!(
            app.tools
                .iter()
                .map(|tool| tool.version_probe_id)
                .collect::<Vec<_>>(),
            original_probe_ids
        );
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
        assert_eq!(Tab::Tools.previous(), Tab::Settings);
        assert_eq!(Tab::Jobs.next(), Tab::Doctor);
        assert_eq!(Tab::Jobs.previous(), Tab::Activity);
        assert_eq!(Tab::Doctor.next(), Tab::Settings);
        assert_eq!(Tab::Doctor.previous(), Tab::Jobs);
        assert_eq!(Tab::Settings.next(), Tab::Tools);
        assert_eq!(Tab::Settings.previous(), Tab::Doctor);
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
        assert_eq!(app.tab_hitboxes.len(), 5);

        for (index, expected) in [
            Tab::Tools,
            Tab::Activity,
            Tab::Jobs,
            Tab::Doctor,
            Tab::Settings,
        ]
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
    fn clicking_tool_view_titles_switches_command_and_github_tables() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render tool views");
        assert_eq!(app.tool_view_hitboxes.len(), 2);

        for (index, expected) in [ToolView::Commands, ToolView::Github]
            .into_iter()
            .enumerate()
        {
            let area = app.tool_view_hitboxes[index].0;
            handle_mouse(
                &mut app,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                },
            );
            assert_eq!(app.tool_view, expected);
        }
    }

    #[test]
    fn recognizes_ctrl_c_and_navigation_keys() {
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
        assert_eq!(
            navigated_tab(Tab::Tools, &KeyCode::Left),
            Some(Tab::Settings)
        );
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
    fn ctrl_c_requires_two_consecutive_presses_to_quit() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        handle_key(&mut app, ctrl_c);

        assert!(!app.should_quit);
        assert!(app.ctrl_c_armed);
        assert_eq!(app.message, "Press Ctrl+C again to quit");

        handle_key(&mut app, ctrl_c);

        assert!(app.should_quit);
    }

    #[test]
    fn q_is_not_an_exit_shortcut() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");

        for code in [KeyCode::Char('q'), KeyCode::Char('Q')] {
            handle_key(&mut app, KeyEvent::new(code, KeyModifiers::NONE));
            assert!(!app.should_quit);
        }
    }

    #[test]
    fn non_ctrl_c_key_cancels_the_quit_confirmation() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        handle_key(&mut app, ctrl_c);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        handle_key(&mut app, ctrl_c);

        assert!(!app.should_quit);
        assert!(app.ctrl_c_armed);
        assert_eq!(app.message, "Press Ctrl+C again to quit");
    }

    #[test]
    fn ctrl_c_confirmation_is_localized_and_second_press_forces_exit() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.language = Language::Chinese;
        app.running = 1;

        handle_key(&mut app, ctrl_c);

        assert!(!app.should_quit);
        assert_eq!(app.message, "再次按 Ctrl+C 退出");

        handle_key(&mut app, ctrl_c);

        assert!(app.should_quit);
    }

    #[test]
    fn footer_shows_ctrl_c_as_the_only_quit_shortcut_in_both_languages() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render English footer");
        let english = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(english.contains("Ctrl+C quit"), "screen: {english}");
        assert!(!english.contains("q / Ctrl+C"), "screen: {english}");
        assert!(english.contains("t TOML"), "screen: {english}");

        app.language = Language::Chinese;
        terminal.clear().expect("clear English footer");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render Chinese footer");
        let chinese = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(chinese.contains("Ctrl+C"), "screen: {chinese}");
        assert!(!chinese.contains("q / Ctrl+C"), "screen: {chinese}");
        assert!(chinese.contains("t "), "screen: {chinese}");
        assert!(chinese.contains("TOML"), "screen: {chinese}");
        assert!(chinese.contains("退 出"), "screen: {chinese}");
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
            NetworkSettings::default(),
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
            NetworkSettings::default(),
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
            NetworkSettings::default(),
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
            NetworkSettings::default(),
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
            update_arguments("claude", Some(Path::new("dvup_custom.toml")), false, None,),
            [
                "update",
                "--background",
                "auto",
                "--config",
                "dvup_custom.toml",
                "claude"
            ]
        );
        assert_eq!(
            update_arguments("claude", None, true, None),
            [
                "update",
                "--background",
                "auto",
                "--terminate-locking-processes",
                "claude"
            ]
        );
        assert_eq!(
            update_arguments("claude", None, false, Some("2.1.207")),
            [
                "update",
                "--background",
                "auto",
                "--to",
                "2.1.207",
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

    #[test]
    fn toml_editor_moves_across_lines_and_replaces_the_selection() {
        let mut editor = TomlEditor::new(
            PathBuf::from("dvup_custom.toml"),
            "abc\nde\nfghi".to_owned(),
        );

        editor.move_right(false);
        editor.move_right(false);
        editor.move_vertical(1, false);
        assert_eq!(editor.cursor, 6);

        editor.move_vertical(1, true);
        assert_eq!(editor.selected_text(), Some("\nfg"));

        editor.insert_text("中\r\n文");
        assert_eq!(editor.text, "abc\nde中\n文hi");
        assert!(editor.dirty);
        assert!(editor.selection().is_none());
    }

    #[test]
    fn toml_editor_toggles_selected_line_comments_and_undoes_as_one_action() {
        let source = concat!(
            "[tools.example]\n",
            "update = [\"example\", \"update\"]\n",
            "probe = [\"example\", \"--version\"]\n",
        );
        let mut editor = TomlEditor::new(PathBuf::from("dvup_custom.toml"), source.to_owned());
        let original_anchor = source.find("tools").expect("selection start");
        let original_cursor = source.find("probe").expect("selection end");
        editor.selection_anchor = Some(original_anchor);
        editor.cursor = original_cursor;

        assert_eq!(
            editor.toggle_line_comments(),
            Some(TomlCommentAction::Commented)
        );
        assert_eq!(
            editor.text,
            concat!(
                "# [tools.example]\n",
                "# update = [\"example\", \"update\"]\n",
                "probe = [\"example\", \"--version\"]\n",
            )
        );
        assert_eq!(editor.revision, 1);
        assert!(editor.dirty);
        assert_eq!(editor.undo_stack.len(), 1);

        assert!(editor.undo());
        assert_eq!(editor.text, source);
        assert_eq!(editor.selection_anchor, Some(original_anchor));
        assert_eq!(editor.cursor, original_cursor);
        assert!(!editor.dirty);

        assert!(editor.redo());
        assert!(editor.text.contains("# [tools.example]"));
        assert!(editor.text.contains("# update ="));
        assert_eq!(editor.revision, 3);
        assert!(editor.dirty);
    }

    #[test]
    fn toml_editor_uncomments_indented_lines_and_excludes_selection_end_line() {
        let source = "  # first = 1\n  #second = 2\nthird = 3";
        let mut editor = TomlEditor::new(PathBuf::from("dvup_custom.toml"), source.to_owned());
        editor.selection_anchor = Some(2);
        editor.cursor = source.find("third").expect("third line start");

        assert_eq!(
            editor.toggle_line_comments(),
            Some(TomlCommentAction::Uncommented)
        );

        assert_eq!(editor.text, "  first = 1\n  second = 2\nthird = 3");
        assert!(!editor.text.contains("# third"));
    }

    #[test]
    fn toml_editor_undo_tracks_saved_text_and_new_edits_clear_redo() {
        let mut editor = TomlEditor::new(PathBuf::from("dvup_custom.toml"), "a".to_owned());
        editor.move_end(true, false);
        editor.insert_text("b");
        editor.mark_saved();
        assert!(!editor.dirty);

        assert!(editor.undo());
        assert_eq!(editor.text, "a");
        assert!(editor.dirty);
        assert!(editor.redo());
        assert_eq!(editor.text, "ab");
        assert!(!editor.dirty);

        assert!(editor.undo());
        editor.insert_text("c");
        assert_eq!(editor.text, "ac");
        assert!(!editor.redo());
    }

    #[test]
    fn queued_toml_paste_is_undone_in_one_step() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(PathBuf::from("dvup_custom.toml"), String::new()),
        };
        let events =
            (0..1_000).map(|_| Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
        handle_event_batch(&mut app, events);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
        );

        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor }
                if editor.text.is_empty() && editor.revision == 2 && !editor.dirty
        ));
        assert_eq!(app.message, "Undid TOML edit");
    }

    #[test]
    fn toml_editor_comment_and_redo_shortcuts_dispatch_to_the_editor() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(PathBuf::from("dvup_custom.toml"), "value = 1".to_owned()),
        };

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor } if editor.text == "# value = 1"
        ));
        assert_eq!(app.message, "Commented TOML line(s)");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor } if editor.text == "value = 1"
        ));

        handle_key(
            &mut app,
            KeyEvent::new(
                KeyCode::Char('Z'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        );
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor } if editor.text == "# value = 1"
        ));
        assert_eq!(app.message, "Redid TOML edit");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor } if editor.text == "value = 1"
        ));
        assert_eq!(app.message, "Uncommented TOML line(s)");
    }

    #[test]
    fn toml_editor_vim_mode_supports_modal_keyboard_editing() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(
                PathBuf::from("dvup_custom.toml"),
                "alpha beta\nsecond".to_owned(),
            ),
        };

        handle_key(&mut app, KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor }
                if editor.mode == TomlEditorMode::VimNormal && editor.text == "alpha beta\nsecond"
        ));

        for code in [
            KeyCode::Char('l'),
            KeyCode::Char('i'),
            KeyCode::Char('X'),
            KeyCode::Esc,
            KeyCode::Char('0'),
            KeyCode::Char('v'),
            KeyCode::Char('w'),
            KeyCode::Char('y'),
        ] {
            handle_key(&mut app, KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor }
                if editor.mode == TomlEditorMode::VimNormal
                    && editor.text == "aXlpha beta\nsecond"
                    && editor.vim_register == "aXlpha "
        ));

        for code in [KeyCode::Char('0'), KeyCode::Char('d'), KeyCode::Char('d')] {
            handle_key(&mut app, KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor } if editor.text == "second"
        ));

        for code in [KeyCode::Char('y'), KeyCode::Char('y'), KeyCode::Char('p')] {
            handle_key(&mut app, KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor } if editor.text == "second\nsecond"
        ));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        );

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        );
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor } if editor.text == "aXlpha beta\nsecond"
        ));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor } if editor.text == "second"
        ));

        handle_key(&mut app, KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor } if editor.mode == TomlEditorMode::Standard
        ));
    }

    #[test]
    fn system_text_editor_command_uses_the_platform_editor_and_exact_path() {
        let path = PathBuf::from("folder with spaces").join("dvup_custom.toml");
        let (program, arguments) = system_text_editor_command(&path);

        #[cfg(windows)]
        {
            assert_eq!(program, OsString::from("notepad.exe"));
            assert_eq!(arguments, [path.into_os_string()]);
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(program, OsString::from("open"));
            assert_eq!(arguments, [OsString::from("-t"), path.into_os_string()]);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            assert_eq!(program, OsString::from("xdg-open"));
            assert_eq!(arguments, [path.into_os_string()]);
        }
    }

    #[test]
    fn toml_editor_history_is_bounded() {
        let mut editor = TomlEditor::new(PathBuf::from("dvup_custom.toml"), String::new());

        for _ in 0..(TOML_HISTORY_LIMIT + 25) {
            editor.insert_text("x");
        }

        assert_eq!(editor.undo_stack.len(), TOML_HISTORY_LIMIT);
        assert!(
            editor
                .undo_stack
                .iter()
                .map(|snapshot| snapshot.text.len())
                .sum::<usize>()
                <= TOML_HISTORY_BYTE_LIMIT
        );
    }

    #[test]
    fn toml_editor_seeds_a_valid_manifest_for_the_focused_tool() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let app = App::new(state.clone(), None).expect("app");
        let path = state.custom_config_path();

        let text = app.toml_editor_text(&path).expect("seed TOML");
        let parsed = UserConfig::parse(&text).expect("valid seed TOML");

        assert_eq!(parsed.tools.len(), 1);
        assert!(
            parsed
                .tools
                .contains_key(&app.focused_tool().expect("tool").name)
        );
        assert!(text.contains("update = ["));
        assert!(text.contains("probe = ["));
        assert!(!text.contains("program ="));
        assert!(!text.contains("lock_timeout_secs"));
        assert!(!path.exists());

        let ensured = app
            .ensure_toml_editor_file()
            .expect("create TOML for the system editor");
        assert_eq!(ensured, path);
        assert!(path.is_file());
        UserConfig::load(&path).expect("system editor receives a valid TOML file");
    }

    #[test]
    fn tools_t_opens_the_explicit_toml_file() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let path = temporary.path().join("manifest.toml");
        let mut config = UserConfig::empty();
        config.tools.insert(
            "example".to_owned(),
            UserTool::custom("example", "example".to_owned(), vec!["update".to_owned()]),
        );
        config.save(&path).expect("save manifest");
        let expected = std::fs::read_to_string(&path).expect("read manifest");
        let mut app = App::new(state, Some(path.clone())).expect("app");
        assert_eq!(
            app.tools
                .iter()
                .find(|tool| tool.name == "example")
                .expect("explicit custom tool")
                .kind,
            ToolKind::Custom
        );
        assert_eq!(
            app.tools
                .iter()
                .find(|tool| tool.name == "dvup")
                .expect("built-in tool")
                .kind,
            ToolKind::BuiltIn
        );

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );

        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor }
                if editor.path == path && editor.text == expected && !editor.dirty
        ));
    }

    #[test]
    fn direct_toml_file_opens_even_when_the_source_is_invalid() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let path = temporary.path().join("broken.toml");
        std::fs::write(&path, "invalid =").expect("write invalid TOML");
        let mut app = App::new(state, None).expect("app");

        app.open_toml_file(path.clone())
            .expect("open invalid TOML source");

        assert_eq!(app.config_path.as_deref(), Some(path.as_path()));
        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor }
                if editor.path == path && editor.text == "invalid =" && !editor.dirty
        ));
    }

    #[test]
    fn ctrl_c_inside_toml_editor_never_arms_or_exits_the_tui() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(
                PathBuf::from("dvup_custom.toml"),
                "[tools.example]".to_owned(),
            ),
        };

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert!(matches!(app.modal, Modal::TomlEditor { .. }));
        assert!(!app.ctrl_c_armed);
        assert!(!app.should_quit);
        assert_eq!(app.message, "Select TOML text before copying");
    }

    #[test]
    fn bracketed_paste_replaces_the_toml_editor_selection() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let mut editor = TomlEditor::new(PathBuf::from("dvup_custom.toml"), "old value".to_owned());
        editor.selection_anchor = Some(0);
        editor.cursor = 3;
        app.modal = Modal::TomlEditor { editor };

        handle_paste(&mut app, "new\r\nline");

        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor }
                if editor.text == "new\nline value" && editor.dirty
        ));
        assert!(!app.should_quit);
    }

    #[test]
    fn queued_toml_character_events_are_inserted_as_one_batch() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(PathBuf::from("dvup_custom.toml"), String::new()),
        };
        let initial_message = app.message.clone();
        let events =
            (0..1_000).map(|_| Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));

        handle_event_batch(&mut app, events);

        let Modal::TomlEditor { editor } = &app.modal else {
            panic!("TOML editor");
        };
        assert_eq!(editor.text, "x".repeat(1_000));
        assert_eq!(editor.revision, 1);
        assert_eq!(app.message, initial_message);
    }

    #[test]
    fn queued_toml_multiline_input_is_inserted_as_one_batch() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(PathBuf::from("dvup_custom.toml"), String::new()),
        };
        let events = [
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
        ];

        handle_event_batch(&mut app, events);

        let Modal::TomlEditor { editor } = &app.modal else {
            panic!("TOML editor");
        };
        assert_eq!(editor.text, "a\n\tb");
        assert_eq!(editor.revision, 1);
    }

    #[test]
    fn queued_toml_input_flushes_before_navigation() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(PathBuf::from("dvup_custom.toml"), String::new()),
        };
        let events = [
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        ];

        handle_event_batch(&mut app, events);

        let Modal::TomlEditor { editor } = &app.modal else {
            panic!("TOML editor");
        };
        assert_eq!(editor.text, "axb");
        assert_eq!(editor.cursor, 2);
        assert_eq!(editor.revision, 2);
    }

    #[test]
    fn toml_highlights_preserve_source_and_distinguish_token_kinds() {
        let source = concat!(
            "# comment\n",
            "enabled = true\n",
            "count = 42\n",
            "ratio = 1.5\n",
            "name = \"dvup\"\n",
            "released = 2026-07-12T12:30:00Z\n",
            "\n",
            "[tools.codex]\n",
        );
        let mut editor = TomlEditor::new(PathBuf::from("dvup_custom.toml"), source.to_owned());

        editor.refresh_highlights();

        let reconstructed = editor
            .highlights
            .iter()
            .map(|highlight| &editor.text[highlight.start..highlight.end])
            .collect::<String>();
        assert_eq!(reconstructed, source);

        let style_at = |needle: &str| {
            let index = source.find(needle).expect("highlighted token");
            editor
                .highlights
                .iter()
                .find(|highlight| highlight.start <= index && index < highlight.end)
                .expect("style at source offset")
                .style
        };
        assert_eq!(style_at("# comment").fg, Some(TOML_COMMENT));
        assert_eq!(style_at("enabled").fg, Some(TOML_KEY));
        assert_eq!(style_at("true").fg, Some(TOML_BOOLEAN));
        assert_eq!(style_at("42").fg, Some(TOML_NUMBER));
        assert_eq!(style_at("1.5").fg, Some(TOML_NUMBER));
        assert_eq!(style_at("\"dvup\"").fg, Some(TOML_STRING));
        assert_eq!(style_at("2026-07-12T12:30:00Z").fg, Some(TOML_DATE_TIME));
        assert_eq!(style_at("tools").fg, Some(TOML_KEY));
    }

    #[test]
    fn toml_highlighting_handles_incomplete_source_without_losing_text() {
        let source = "[tools.claude]\nprogram =";
        let mut editor = TomlEditor::new(PathBuf::from("dvup_custom.toml"), source.to_owned());

        editor.refresh_highlights();

        assert_eq!(
            editor
                .highlights
                .iter()
                .map(|highlight| &editor.text[highlight.start..highlight.end])
                .collect::<String>(),
            source
        );
        assert_eq!(editor.highlighted_revision, editor.revision);
    }

    #[test]
    fn toml_selection_style_overrides_syntax_highlighting() {
        let source = "enabled = true # note";
        let mut editor = TomlEditor::new(PathBuf::from("dvup_custom.toml"), source.to_owned());
        editor.selection_anchor = Some(source.find("true").expect("boolean"));
        editor.cursor = editor.selection_anchor.expect("anchor") + "true".len();
        editor.refresh_highlights();

        let line = toml_editor_line(&editor, 0, source.len());
        let selected = line
            .spans
            .iter()
            .find(|span| span.content == "true")
            .expect("selected boolean span");
        let key = line
            .spans
            .iter()
            .find(|span| span.content == "enabled")
            .expect("syntax-highlighted key span");

        assert_eq!(selected.style.bg, Some(ACCENT));
        assert_eq!(selected.style.fg, Some(Color::Black));
        assert_eq!(key.style.fg, Some(TOML_KEY));
        assert_eq!(key.style.bg, Some(SURFACE));
    }

    #[test]
    fn toml_editor_renders_syntax_colors() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(
                PathBuf::from("dvup_custom.toml"),
                "# comment\nname = \"dvup\"\ncount = 42".to_owned(),
            ),
        };
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render highlighted TOML editor");

        let area = app.toml_editor_hitbox.expect("TOML content hitbox").area;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(area.x, area.y)].fg, TOML_COMMENT);
        assert_eq!(buffer[(area.x, area.y + 1)].fg, TOML_KEY);
        assert_eq!(buffer[(area.x + 7, area.y + 1)].fg, TOML_STRING);
        assert_eq!(buffer[(area.x + 8, area.y + 2)].fg, TOML_NUMBER);
    }

    #[test]
    fn toml_editor_saves_valid_source_text_and_rejects_invalid_toml() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let path = temporary.path().join("manifest.toml");
        let mut config = UserConfig::empty();
        config.tools.insert(
            "example".to_owned(),
            UserTool::custom("example", "example".to_owned(), vec!["update".to_owned()]),
        );
        config.save(&path).expect("save manifest");
        let mut app = App::new(state, Some(path.clone())).expect("app");
        app.open_toml_editor();
        let Modal::TomlEditor { editor } = &mut app.modal else {
            panic!("TOML editor");
        };
        editor.text.insert_str(0, "# preserved comment\n");
        editor.dirty = true;

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );

        let saved = std::fs::read_to_string(&path).expect("saved TOML");
        assert!(saved.starts_with("# preserved comment\n"));
        assert!(matches!(&app.modal, Modal::TomlEditor { editor } if !editor.dirty));

        let Modal::TomlEditor { editor } = &mut app.modal else {
            panic!("TOML editor");
        };
        editor.text = "invalid =".to_owned();
        editor.cursor = editor.text.len();
        editor.dirty = true;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );

        assert_eq!(
            std::fs::read_to_string(&path).expect("unchanged TOML"),
            saved
        );
        assert!(app.message.starts_with("TOML was not saved:"));
        assert!(matches!(&app.modal, Modal::TomlEditor { editor } if editor.dirty));
    }

    #[test]
    fn toml_editor_mouse_wheel_scrolls_and_drag_selects_source_text() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let text = (0..30)
            .map(|index| format!("line{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(PathBuf::from("dvup_custom.toml"), text),
        };
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render TOML editor");
        let hitbox = app.toml_editor_hitbox.expect("editor hitbox");

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: hitbox.area.x,
                row: hitbox.area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert!(matches!(&app.modal, Modal::TomlEditor { editor } if editor.scroll_y == 1));

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("redraw scrolled TOML editor");
        assert!(matches!(&app.modal, Modal::TomlEditor { editor } if editor.scroll_y == 1));

        for (kind, expected) in [
            (MouseEventKind::ScrollDown, 2),
            (MouseEventKind::ScrollUp, 1),
        ] {
            handle_mouse(
                &mut app,
                MouseEvent {
                    kind,
                    column: hitbox.area.x,
                    row: hitbox.area.y,
                    modifiers: KeyModifiers::NONE,
                },
            );
            terminal
                .draw(|frame| draw(frame, &mut app))
                .expect("redraw repeatedly scrolled TOML editor");
            assert!(matches!(
                &app.modal,
                Modal::TomlEditor { editor } if editor.scroll_y == expected
            ));
        }

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hitbox.area.x,
                row: hitbox.area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: hitbox.area.x.saturating_add(4),
                row: hitbox.area.y.saturating_add(1),
                modifiers: KeyModifiers::NONE,
            },
        );

        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor }
                if editor.selected_text().is_some_and(|text| text.contains("line01\nline"))
        ));
    }

    #[test]
    fn toml_editor_renders_copy_paste_and_mouse_help() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(
                PathBuf::from("dvup_custom.toml"),
                "[tools.example]".to_owned(),
            ),
        };
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render TOML editor");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(screen.contains("TOML editor"), "screen: {screen}");
        assert!(screen.contains("Ctrl+C"), "screen: {screen}");
        assert!(screen.contains("Ctrl+V"), "screen: {screen}");
        assert!(screen.contains("Ctrl+/"), "screen: {screen}");
        assert!(screen.contains("Ctrl+Z/Y"), "screen: {screen}");
        assert!(screen.contains("STANDARD"), "screen: {screen}");
        assert!(screen.contains("F2"), "screen: {screen}");
        assert!(screen.contains("mouse drag selects"), "screen: {screen}");

        handle_key(&mut app, KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        terminal.clear().expect("clear standard editor");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render Vim TOML editor");
        let vim_screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(vim_screen.contains("VIM NORMAL"), "screen: {vim_screen}");
        assert!(vim_screen.contains("h/j/k/l"), "screen: {vim_screen}");
        assert!(vim_screen.contains("Ctrl+Q"), "screen: {vim_screen}");
    }

    #[test]
    fn toml_editor_scrollbar_reaches_the_bottom_at_the_last_viewport() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        let text = (0..40)
            .map(|index| format!("line{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.modal = Modal::TomlEditor {
            editor: TomlEditor::new(PathBuf::from("dvup_custom.toml"), text),
        };
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render TOML editor");
        let hitbox = app.toml_editor_hitbox.expect("editor hitbox");
        let Modal::TomlEditor { editor } = &mut app.modal else {
            panic!("TOML editor");
        };
        editor.scroll_y = editor
            .line_ranges()
            .len()
            .saturating_sub(usize::from(hitbox.area.height));
        editor.follow_cursor = false;

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render final TOML viewport");

        let bottom = terminal
            .backend()
            .buffer()
            .cell((hitbox.area.right(), hitbox.area.bottom().saturating_sub(1)))
            .expect("scrollbar bottom cell");
        assert_eq!(bottom.symbol(), "█");
    }

    #[test]
    fn doctor_table_wheel_scrolls_after_the_terminal_height_shrinks() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.tab = Tab::Doctor;
        app.doctor_checked_at = Some("2026-07-12 22:00:00".to_owned());
        app.doctor_diagnoses = (0..20)
            .map(|index| {
                test_diagnosis(&format!("tool{index:02}"), &[("path/tool", Some("1.0.0"))])
            })
            .collect();

        let large_backend = TestBackend::new(100, 30);
        let mut large_terminal = Terminal::new(large_backend).expect("large terminal");
        large_terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render large Doctor view");
        drop(large_terminal);

        let small_backend = TestBackend::new(100, 16);
        let mut small_terminal = Terminal::new(small_backend).expect("small terminal");
        small_terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render resized Doctor view");
        let table_row = app.doctor_hitboxes.get(1).expect("second Doctor row").0;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: table_row.x,
                row: table_row.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.doctor_index, 1);

        for expected in [2, 3] {
            handle_mouse(
                &mut app,
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: table_row.x,
                    row: table_row.y,
                    modifiers: KeyModifiers::NONE,
                },
            );
            small_terminal
                .draw(|frame| draw(frame, &mut app))
                .expect("redraw scrolled Doctor view");
            assert_eq!(app.doctor_index, expected);
            assert_eq!(
                hitbox_target(&app.doctor_hitboxes, table_row.x, table_row.y),
                Some(expected)
            );
        }
    }

    #[test]
    fn list_scroll_keeps_focus_on_the_mouse_screen_row() {
        let mut viewport = ListViewport::default();
        viewport.update(Rect::new(10, 20, 30, 4), 10, 3);

        assert_eq!(viewport.scroll_at(12, 21, 1), Some(5));
        assert_eq!(viewport.offset(), 4);
        assert_eq!(viewport.scroll_at(12, 21, -1), Some(4));
        assert_eq!(viewport.offset(), 3);
    }

    #[test]
    fn proxy_editor_saves_strict_explicit_settings() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state.clone(), None).expect("app");

        app.toggle_setting(2);
        assert!(matches!(app.modal, Modal::NetworkProxy { .. }));
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_paste(&mut app, "http://127.0.0.1:7890");
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_paste(&mut app, "localhost, .example.com");
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.modal, Modal::None));

        let saved = AppSettings::load(&state.settings_path()).expect("saved proxy settings");
        assert_eq!(saved.network.proxy_mode, ProxyMode::Explicit);
        assert_eq!(
            saved.network.proxy_url.as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(saved.network.no_proxy, ["localhost", ".example.com"]);

        app.modal = Modal::None;
        app.toggle_setting(2);
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let saved = AppSettings::load(&state.settings_path()).expect("saved direct settings");
        assert_eq!(saved.network.proxy_mode, ProxyMode::Direct);
        assert!(saved.network.proxy_url.is_none());
        assert!(saved.network.no_proxy.is_empty());
    }

    #[test]
    fn stale_network_test_results_are_ignored() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.network_test_probe_id = 2;
        app.network_test_loading = true;
        app.tx
            .send(AppEvent::NetworkTestResolved {
                probe_id: 1,
                results: Ok(vec![version::NetworkTestResult {
                    name: "stale",
                    elapsed_ms: 1,
                    error: None,
                }]),
            })
            .expect("stale result");
        app.tx
            .send(AppEvent::NetworkTestResolved {
                probe_id: 2,
                results: Ok(vec![version::NetworkTestResult {
                    name: "current",
                    elapsed_ms: 2,
                    error: None,
                }]),
            })
            .expect("current result");

        app.process_events();

        assert!(!app.network_test_loading);
        assert_eq!(app.network_test_results.len(), 1);
        assert_eq!(app.network_test_results[0].name, "current");
    }

    #[test]
    fn stale_github_credential_status_cannot_override_a_newer_result() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.github_credential_probe_id = 2;
        app.tx
            .send(AppEvent::GithubCredentialResolved {
                probe_id: 1,
                result: Ok(false),
            })
            .expect("stale credential state");
        app.tx
            .send(AppEvent::GithubCredentialResolved {
                probe_id: 2,
                result: Ok(true),
            })
            .expect("current credential state");

        app.process_events();

        assert!(app.github_api_key_configured);
        assert!(app.github_credential_error.is_none());
    }

    #[test]
    fn github_monitor_form_preserves_exact_byte_limits() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let monitor = github_monitor_from_form(
            &TextInput::new("example".to_owned()),
            &TextInput::new("owner/repository".to_owned()),
            &TextInput::new(r"^example-.*\.zip$".to_owned()),
            &TextInput::new(temporary.path().join("example").display().to_string()),
            ReleaseAssetFormat::Zip,
            ReleaseUpdatePolicy::Automatic,
            true,
            &TextInput::new("104857601".to_owned()),
            &TextInput::new("314572803".to_owned()),
            &TextInput::new("1001".to_owned()),
            &TextInput::new("1".to_owned()),
            true,
        )
        .expect("valid monitor form");

        assert_eq!(monitor.max_download_bytes, 104_857_601);
        assert_eq!(monitor.max_extracted_bytes, 314_572_803);
        assert_eq!(monitor.max_extracted_files, 1_001);
        assert_eq!(monitor.strip_components, 1);
        assert_eq!(monitor.update_policy, ReleaseUpdatePolicy::Automatic);
    }

    #[test]
    fn tools_tab_switches_between_command_tools_and_github_repositories() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");

        handle_tools_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.tool_view, ToolView::Github);
        handle_tools_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.tool_view, ToolView::Commands);
    }

    #[test]
    fn github_refresh_reloads_repository_configuration_from_disk() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state.clone(), None).expect("app");
        app.tool_view = ToolView::Github;
        app.release_monitor_running = true;
        assert!(app.github_monitors.is_empty());

        let mut custom = UserConfig::empty();
        custom.github.monitors.push(GithubReleaseMonitor {
            name: "reloaded".to_owned(),
            repository: "owner/reloaded".to_owned(),
            asset_regex: r"^reloaded\.zip$".to_owned(),
            target_directory: temporary.path().join("reloaded"),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 1024,
            max_extracted_bytes: 2048,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: false,
        });
        custom
            .save(&state.custom_config_path())
            .expect("save external config change");

        handle_normal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        );

        assert_eq!(app.github_monitors.len(), 1);
        assert_eq!(app.github_monitors[0].name, "reloaded");
    }

    #[test]
    fn doctor_refresh_reloads_tool_configuration_from_disk() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state.clone(), None).expect("app");
        app.tab = Tab::Doctor;
        app.doctor_loading = true;
        assert!(!app.tools.iter().any(|tool| tool.name == "reloaded"));

        let mut custom = UserConfig::empty();
        custom.tools.insert(
            "reloaded".to_owned(),
            UserTool::custom(
                "reloaded",
                "missing-reloaded-command".to_owned(),
                vec!["update".to_owned()],
            ),
        );
        custom
            .save(&state.custom_config_path())
            .expect("save external config change");

        handle_normal_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        );

        assert!(app.tools.iter().any(|tool| tool.name == "reloaded"));
    }

    #[test]
    fn github_tool_a_toggles_all_enabled_repository_selections() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state, None).expect("app");
        let monitor = |name: &str, enabled| GithubReleaseMonitor {
            name: name.to_owned(),
            repository: format!("owner/{name}"),
            asset_regex: format!(r"^{name}\.zip$"),
            target_directory: temporary.path().join(name),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 1024,
            max_extracted_bytes: 2048,
            max_extracted_files: 10,
            strip_components: 0,
            enabled,
        };
        app.github_monitors = vec![
            monitor("first", true),
            monitor("disabled", false),
            monitor("second", true),
        ];

        handle_github_tools_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );

        assert_eq!(
            app.selected_github_monitors,
            HashSet::from(["first".to_owned(), "second".to_owned()])
        );

        handle_github_tools_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE),
        );

        assert!(app.selected_github_monitors.is_empty());
    }

    #[test]
    fn github_tool_c_opens_the_add_repository_form() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state, None).expect("app");

        handle_github_tools_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );

        assert!(matches!(
            app.modal,
            Modal::GithubMonitorForm {
                mode: MonitorFormMode::Add,
                ..
            }
        ));
    }

    #[test]
    fn github_tool_t_opens_the_toml_editor() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state.clone(), None).expect("app");

        handle_github_tools_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );

        assert!(matches!(
            &app.modal,
            Modal::TomlEditor { editor }
                if editor.path == state.custom_config_path()
                    && UserConfig::parse(&editor.text).is_ok()
        ));
    }

    #[test]
    fn github_tool_o_uses_the_system_toml_editor_action() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state, None).expect("app");
        app.running = 1;
        app.message.clear();

        handle_github_tools_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        );

        assert_eq!(
            app.message,
            "Wait for the current operation before editing TOML"
        );
    }

    #[test]
    fn github_tool_enter_requires_an_explicit_repository_selection() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state, None).expect("app");
        app.github_monitors.push(GithubReleaseMonitor {
            name: "example".to_owned(),
            repository: "owner/repository".to_owned(),
            asset_regex: r"^example\.zip$".to_owned(),
            target_directory: temporary.path().join("installed"),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 1024,
            max_extracted_bytes: 2048,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: true,
        });

        handle_github_tools_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.message, "Select an enabled GitHub repository first");
    }

    #[test]
    fn github_tool_enter_confirms_available_update_and_skips_current_release() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state, None).expect("app");
        app.tool_view = ToolView::Github;
        app.github_monitors.push(GithubReleaseMonitor {
            name: "example".to_owned(),
            repository: "owner/repository".to_owned(),
            asset_regex: r"^example\.zip$".to_owned(),
            target_directory: temporary.path().join("installed"),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: false,
            max_download_bytes: 1024,
            max_extracted_bytes: 2048,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: true,
        });
        app.release_monitor_statuses.push(MonitorStatus {
            name: "example".to_owned(),
            installed_tag: Some("v1.0.0".to_owned()),
            latest_tag: Some("v1.1.0".to_owned()),
            asset: Some("example.zip".to_owned()),
            error: None,
        });

        handle_github_tools_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        handle_github_tools_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &app.modal,
            Modal::ConfirmGithubMonitorUpdate { monitors }
                if monitors == &["example".to_owned()]
        ));

        app.modal = Modal::None;
        app.release_monitor_statuses[0].installed_tag = Some("v1.1.0".to_owned());
        handle_github_tools_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.modal, Modal::None));
        assert!(app.message.contains("Already at the latest"));
    }

    #[test]
    fn github_probe_event_updates_versions_without_creating_install_state() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let state_path = state.release_state_path();
        let mut app = App::new(state, None).expect("app");
        app.release_probe_running = true;
        app.tx
            .send(AppEvent::ReleaseMonitorsProbed(Ok(vec![MonitorStatus {
                name: "example".to_owned(),
                installed_tag: Some("v1.0.0".to_owned()),
                latest_tag: Some("v1.1.0".to_owned()),
                asset: Some("example.zip".to_owned()),
                error: None,
            }])))
            .expect("probe result");

        app.process_events();

        assert!(!app.release_probe_running);
        assert_eq!(
            app.release_monitor_statuses[0].latest_tag.as_deref(),
            Some("v1.1.0")
        );
        assert!(app.message.contains("1 update"));
        assert!(!state_path.exists());
    }

    #[test]
    fn only_available_automatic_monitors_are_selected_after_a_probe() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let monitor = |name: &str, update_policy, enabled| GithubReleaseMonitor {
            name: name.to_owned(),
            repository: "owner/repository".to_owned(),
            asset_regex: r"^example\.zip$".to_owned(),
            target_directory: temporary.path().join(name),
            format: ReleaseAssetFormat::Zip,
            update_policy,
            cleanup_installer: true,
            max_download_bytes: 1024,
            max_extracted_bytes: 2048,
            max_extracted_files: 10,
            strip_components: 0,
            enabled,
        };
        let status =
            |name: &str, installed: Option<&str>, latest: Option<&str>, error| MonitorStatus {
                name: name.to_owned(),
                installed_tag: installed.map(str::to_owned),
                latest_tag: latest.map(str::to_owned),
                asset: Some("example.zip".to_owned()),
                error,
            };
        let monitors = vec![
            monitor("automatic", ReleaseUpdatePolicy::Automatic, true),
            monitor("manual", ReleaseUpdatePolicy::Manual, true),
            monitor("current", ReleaseUpdatePolicy::Automatic, true),
            monitor("failed", ReleaseUpdatePolicy::Automatic, true),
            monitor("disabled", ReleaseUpdatePolicy::Automatic, false),
        ];
        let statuses = vec![
            status("automatic", Some("v1"), Some("v2"), None),
            status("manual", Some("v1"), Some("v2"), None),
            status("current", Some("2"), Some("v2"), None),
            status(
                "failed",
                Some("v1"),
                None,
                Some("release failed".to_owned()),
            ),
            status("disabled", Some("v1"), Some("v2"), None),
        ];

        assert_eq!(
            automatic_release_update_names(&monitors, &statuses),
            ["automatic".to_owned()]
        );
    }

    #[test]
    fn github_monitor_form_enter_saves_from_any_field_and_keeps_strict_fields() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state.clone(), None).expect("app");
        app.modal = Modal::GithubMonitorForm {
            mode: MonitorFormMode::Add,
            original_index: None,
            field: 2,
            name: TextInput::new("example".to_owned()),
            repository: TextInput::new("owner/repository".to_owned()),
            asset_regex: TextInput::new(r"^example-.*\.zip$".to_owned()),
            target_directory: TextInput::new(
                temporary.path().join("installed").display().to_string(),
            ),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Automatic,
            cleanup_installer: false,
            max_download_bytes: TextInput::new("104857601".to_owned()),
            max_extracted_bytes: TextInput::new("314572803".to_owned()),
            max_extracted_files: TextInput::new("1001".to_owned()),
            strip_components: TextInput::new("1".to_owned()),
            enabled: true,
        };

        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.tool_view, ToolView::Github);
        assert_eq!(app.github_monitor_index, 0);
        let saved = UserConfig::load(&state.custom_config_path()).expect("saved custom config");
        let monitor = saved.github.monitors.first().expect("saved monitor");
        assert_eq!(monitor.repository, "owner/repository");
        assert_eq!(monitor.asset_regex, r"^example-.*\.zip$");
        assert_eq!(monitor.max_download_bytes, 104_857_601);
        assert_eq!(monitor.max_extracted_bytes, 314_572_803);
        assert_eq!(monitor.update_policy, ReleaseUpdatePolicy::Automatic);
        assert!(!monitor.cleanup_installer);
        assert!(monitor.enabled);
    }

    #[test]
    fn invalid_github_monitor_form_keeps_all_input_for_correction() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state.clone(), None).expect("app");
        app.modal = Modal::GithubMonitorForm {
            mode: MonitorFormMode::Add,
            original_index: None,
            field: GITHUB_MONITOR_FORM_FIELD_COUNT - 1,
            name: TextInput::new("example".to_owned()),
            repository: TextInput::new("owner/repository".to_owned()),
            asset_regex: TextInput::new(r"^example-.*\.zip$".to_owned()),
            target_directory: TextInput::new("relative/path".to_owned()),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: TextInput::new("104857600".to_owned()),
            max_extracted_bytes: TextInput::new("314572800".to_owned()),
            max_extracted_files: TextInput::new("1000".to_owned()),
            strip_components: TextInput::new("1".to_owned()),
            enabled: true,
        };

        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            &app.modal,
            Modal::GithubMonitorForm {
                repository,
                target_directory,
                asset_regex,
                ..
            } if repository.value == "owner/repository"
                && target_directory.value == "relative/path"
                && asset_regex.value == r"^example-.*\.zip$"
        ));
        assert!(app.message.contains("absolute path"), "{}", app.message);
        assert!(app.github_monitors.is_empty());
        assert!(!state.settings_path().exists());
    }

    #[test]
    fn github_monitor_edit_and_delete_actions_are_persisted() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state.clone(), None).expect("app");
        let monitor = GithubReleaseMonitor {
            name: "example".to_owned(),
            repository: "owner/repository".to_owned(),
            asset_regex: r"^example\.zip$".to_owned(),
            target_directory: temporary.path().join("installed"),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 100,
            max_extracted_bytes: 200,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: true,
        };
        app.save_github_monitor(None, monitor)
            .expect("save monitor");

        let mut disabled = app.github_monitors[0].clone();
        disabled.enabled = false;
        app.save_github_monitor(Some(0), disabled)
            .expect("disable monitor");
        assert!(
            !UserConfig::load(&state.custom_config_path())
                .expect("disabled custom config")
                .github
                .monitors[0]
                .enabled
        );

        assert_eq!(app.delete_github_monitor(0), 0);
        assert!(
            !state.custom_config_path().exists(),
            "deleting the final custom entry should remove dvup_custom.toml"
        );
        assert!(!state.settings_path().exists());
    }

    #[test]
    fn github_monitors_load_from_custom_config_and_preserve_custom_tools() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut custom = UserConfig::empty();
        custom.tools.insert(
            "example-command".to_owned(),
            UserTool::custom(
                "example-command",
                "example".to_owned(),
                vec!["update".to_owned()],
            ),
        );
        custom.github.monitors.push(GithubReleaseMonitor {
            name: "existing-repository".to_owned(),
            repository: "owner/existing".to_owned(),
            asset_regex: r"^existing\.zip$".to_owned(),
            target_directory: temporary.path().join("existing"),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 100,
            max_extracted_bytes: 200,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: true,
        });
        custom
            .save(&state.custom_config_path())
            .expect("seed dvup_custom.toml");

        let mut app = App::new(state.clone(), None).expect("load app custom config");
        assert_eq!(app.github_monitors.len(), 1);
        assert_eq!(app.github_monitors[0].name, "existing-repository");

        app.save_github_monitor(
            None,
            GithubReleaseMonitor {
                name: "second-repository".to_owned(),
                repository: "owner/second".to_owned(),
                asset_regex: r"^second\.zip$".to_owned(),
                target_directory: temporary.path().join("second"),
                format: ReleaseAssetFormat::Zip,
                update_policy: ReleaseUpdatePolicy::Manual,
                cleanup_installer: true,
                max_download_bytes: 100,
                max_extracted_bytes: 200,
                max_extracted_files: 10,
                strip_components: 0,
                enabled: true,
            },
        )
        .expect("add second monitor");
        app.delete_github_monitor(0);

        let reloaded = UserConfig::load(&state.custom_config_path()).expect("reload custom config");
        assert!(reloaded.tools.contains_key("example-command"));
        assert_eq!(reloaded.github.monitors.len(), 1);
        assert_eq!(reloaded.github.monitors[0].name, "second-repository");
        assert!(!state.settings_path().exists());
    }

    #[test]
    fn explicit_config_loads_github_monitors_but_disables_form_writes() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let explicit_path = temporary.path().join("explicit.toml");
        let mut explicit = UserConfig::empty();
        explicit.github.monitors.push(GithubReleaseMonitor {
            name: "explicit".to_owned(),
            repository: "owner/explicit".to_owned(),
            asset_regex: r"^explicit\.zip$".to_owned(),
            target_directory: temporary.path().join("explicit"),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 100,
            max_extracted_bytes: 200,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: true,
        });
        explicit.save(&explicit_path).expect("save explicit config");
        let original = std::fs::read_to_string(&explicit_path).expect("read explicit config");

        let mut app = App::new(state, Some(explicit_path.clone())).expect("load explicit config");
        app.tool_view = ToolView::Github;
        assert_eq!(app.github_monitors.len(), 1);
        handle_github_tools_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );

        assert!(matches!(app.modal, Modal::None));
        assert!(app.message.contains("--config"));
        assert_eq!(
            std::fs::read_to_string(explicit_path).expect("explicit config remains readable"),
            original
        );
    }

    #[test]
    fn github_tool_view_renders_repository_versions_status_and_target() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().join("state"));
        let mut app = App::new(state, None).expect("app");
        app.github_monitors.push(GithubReleaseMonitor {
            name: "example".to_owned(),
            repository: "owner/repository".to_owned(),
            asset_regex: r"^example-.*\.zip$".to_owned(),
            target_directory: temporary.path().join("installed"),
            format: ReleaseAssetFormat::Zip,
            update_policy: ReleaseUpdatePolicy::Manual,
            cleanup_installer: true,
            max_download_bytes: 100,
            max_extracted_bytes: 200,
            max_extracted_files: 10,
            strip_components: 0,
            enabled: true,
        });
        app.release_monitor_statuses.push(MonitorStatus {
            name: "example".to_owned(),
            installed_tag: Some("v1.2.3".to_owned()),
            latest_tag: Some("v1.2.3".to_owned()),
            asset: Some("example-v1.2.3.zip".to_owned()),
            error: None,
        });
        app.tool_view = ToolView::Github;
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render GitHub tool view");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(screen.contains("owner/repository"), "screen: {screen}");
        assert!(screen.contains("v1.2.3"), "screen: {screen}");
        assert!(screen.contains("up to date"), "screen: {screen}");
        assert!(screen.contains("INSTALLED"), "screen: {screen}");
        assert!(screen.contains("manual"), "screen: {screen}");
        assert!(screen.contains("a all"), "screen: {screen}");
        assert!(screen.contains("c add"), "screen: {screen}");
        assert!(screen.contains("t TOML"), "screen: {screen}");
        assert!(screen.contains("o editor"), "screen: {screen}");
        assert!(screen.contains("Enter install"), "screen: {screen}");
    }

    #[test]
    fn github_rate_limit_bar_renders_token_owner_used_and_remaining() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.github_api_key_configured = true;
        app.github_rate_limit = Some(version::GithubRateLimit {
            owner: "octocat".to_owned(),
            limit: 5_000,
            used: 125,
            remaining: 4_875,
            reset_unix: 1_800_000_000,
        });
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render GitHub API quota");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(screen.contains("@octocat"), "screen: {screen}");
        assert!(screen.contains("used 125 / 5000"), "screen: {screen}");
        assert!(screen.contains("remaining 4875"), "screen: {screen}");
    }

    #[test]
    fn github_rate_limit_color_warns_before_quota_is_exhausted() {
        let status = |remaining| version::GithubRateLimit {
            owner: "octocat".to_owned(),
            limit: 100,
            used: 100 - remaining,
            remaining,
            reset_unix: 1,
        };

        assert_eq!(github_rate_limit_color(&status(26)), SUCCESS);
        assert_eq!(github_rate_limit_color(&status(25)), WARNING_COLOR);
        assert_eq!(github_rate_limit_color(&status(10)), ERROR_COLOR);
    }

    #[test]
    fn stale_github_rate_limit_status_cannot_override_a_newer_result() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.github_rate_limit_probe_id = 2;
        app.github_rate_limit_loading = true;
        let status = |owner: &str| version::GithubRateLimit {
            owner: owner.to_owned(),
            limit: 5_000,
            used: 1,
            remaining: 4_999,
            reset_unix: 1,
        };
        app.tx
            .send(AppEvent::GithubRateLimitResolved {
                probe_id: 1,
                result: Ok(status("stale")),
            })
            .expect("stale GitHub API status");
        app.tx
            .send(AppEvent::GithubRateLimitResolved {
                probe_id: 2,
                result: Ok(status("current")),
            })
            .expect("current GitHub API status");

        app.process_events();

        assert!(!app.github_rate_limit_loading);
        assert_eq!(
            app.github_rate_limit
                .as_ref()
                .map(|status| status.owner.as_str()),
            Some("current")
        );
    }

    #[test]
    fn proxy_modal_can_select_and_save_direct_mode() {
        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state.clone(), None).expect("app");
        app.settings.network = NetworkSettings::default();

        app.toggle_setting(2);
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        handle_modal_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.settings.network.proxy_mode, ProxyMode::Direct);
        assert_eq!(
            AppSettings::load(&state.settings_path())
                .expect("saved settings")
                .network
                .proxy_mode,
            ProxyMode::Direct
        );
    }

    #[test]
    fn proxy_modal_renders_complete_chinese_controls_at_common_width() {
        use ratatui::backend::TestBackend;

        let temporary = tempfile::TempDir::new().expect("temp dir");
        let state = StateDirs::at(temporary.path().to_path_buf());
        let mut app = App::new(state, None).expect("app");
        app.language = Language::Chinese;
        app.toggle_setting(2);
        let backend = TestBackend::new(92, 26);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("render proxy modal");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = screen.replace(' ', "");

        assert!(compact.contains("代理模式"), "screen: {screen}");
        assert!(compact.contains("[Esc]取消"), "screen: {screen}");
    }
}
