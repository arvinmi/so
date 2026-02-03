pub mod actions;
pub mod menu;
pub mod popup;
mod ui;

use std::{
  collections::VecDeque,
  io,
  path::{Path, PathBuf},
  process::Stdio,
  sync::Arc,
  time::{Duration, Instant},
};

use crossterm::{
  event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind},
  execute,
  terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend, style::Color};
use serde_json::Value;
use tokio::{
  io::{AsyncBufReadExt, BufReader},
  process::Command,
  sync::mpsc,
};

use crate::{
  Error, Harness, RunMode,
  harness::build_prompt,
  sandbox::{BwrapContext, DockerContainer},
};

// shared color palette
pub(crate) const BORDER_GRAY: Color = Color::Rgb(70, 70, 70);
pub(crate) const DIM_GRAY: Color = Color::Rgb(85, 85, 85);
pub(crate) const MEDIUM_GRAY: Color = Color::Rgb(160, 160, 160);
pub(crate) const TEXT_WHITE: Color = Color::Rgb(220, 220, 220);
pub(crate) const GREEN: Color = Color::Rgb(80, 200, 120);
pub(crate) const YELLOW: Color = Color::Rgb(220, 180, 50);
pub(crate) const RED: Color = Color::Rgb(220, 80, 80);
pub(crate) const CYAN: Color = Color::Rgb(80, 180, 200);
pub(crate) const MAGENTA: Color = Color::Rgb(180, 100, 180);

const SPEC_PLAN: &str = "specs/implementation-plan.md";
const SPEC_PROMPT: &str = "specs/prompt.md";
const SPEC_STATUS: &str = "specs/status.md";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfirmChoice {
  Yes,
  No,
}

pub(crate) fn confirm_choice(code: KeyCode) -> Option<ConfirmChoice> {
  match code {
    KeyCode::Char('y' | 'Y') => Some(ConfirmChoice::Yes),
    KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => Some(ConfirmChoice::No),
    _ => None,
  }
}

const ACTIVITY_CAPACITY: usize = 1000;

// =============================================================================
// Types
// =============================================================================

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunStatus {
  #[default]
  Running,
  Done,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RunPopup {
  #[default]
  None,
  Options,
  Reset {
    commits: Vec<(String, String)>,
    selected: usize,
  },
  Continue {
    input: String,
  },
  MergeConfirm {
    orig: PathBuf,
  },
  Error {
    message: String,
  },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IterStatus {
  #[default]
  Pending,
  Running,
  Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
  Reading,
  Writing,
  Text,
  Code,
  ToolCall,
  Thinking,
}

#[derive(Clone, Debug)]
pub struct Iteration {
  pub number: u32,
  pub status: IterStatus,
  pub start_time: Option<Instant>,
  pub duration: Option<Duration>,
  pub diff_stats: Option<(u32, u32)>,
  pub commit_msg: Option<String>,
}

impl Iteration {
  pub fn new(number: u32) -> Self {
    Self { number, status: IterStatus::Pending, start_time: None, duration: None, diff_stats: None, commit_msg: None }
  }

  pub fn elapsed(&self) -> Option<Duration> {
    match self.status {
      IterStatus::Pending => None,
      IterStatus::Running => self.start_time.map(|s| s.elapsed()),
      IterStatus::Completed => self.duration,
    }
  }
}

#[derive(Clone, Debug)]
pub struct ActivityEntry {
  pub timestamp: Instant,
  pub kind: ActivityKind,
  pub content: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FocusPane {
  #[default]
  Activity,
  Iterations,
}

// =============================================================================
// Events
// =============================================================================

#[derive(Clone, Debug)]
pub enum HarnessEvent {
  IterationStart { n: u32 },
  IterationComplete { n: u32, diff: Option<(u32, u32)>, msg: Option<String> },
  Activity { kind: ActivityKind, content: String },
  GitStats { files: u32, ins: u32, del: u32, commits: u32 },
  Error { message: String },
  Finished,
}

enum TermEvent {
  Key(KeyEvent),
  Mouse(MouseEvent),
  Resize,
}

// =============================================================================
// App state
// =============================================================================

pub struct AppState {
  pub sandbox_name: String,
  pub sandbox_path: PathBuf,
  pub harness: String,
  pub start_time: Instant,
  pub end_time: Option<Instant>,
  pub status: RunStatus,
  pub popup: RunPopup,
  pub current_iter: u32,
  pub max_iter: u32,
  pub iterations: Vec<Iteration>,
  pub files_changed: u32,
  pub insertions: u32,
  pub deletions: u32,
  pub commit_count: u32,
  pub activity: VecDeque<ActivityEntry>,
  pub focus: FocusPane,
  pub scroll_offset: usize,
  pub iter_scroll_offset: usize,
  pub activity_visible: usize,
  pub iter_visible: usize,
  pub should_quit: bool,
  pub interrupted: bool,
  pub error_fatal: bool,
  pub restart_harness: bool,
  pub restart_from: Option<u32>,
  pub plan_tasks: Vec<String>,
  plan_raw: String,
}

impl AppState {
  pub fn new(sandbox_name: String, sandbox_path: PathBuf, harness: String, max_iter: u32) -> Self {
    let iterations = (1..=max_iter).map(Iteration::new).collect();
    Self {
      sandbox_name,
      sandbox_path,
      harness,
      start_time: Instant::now(),
      end_time: None,
      status: RunStatus::Running,
      popup: RunPopup::None,
      current_iter: 0,
      max_iter,
      iterations,
      files_changed: 0,
      insertions: 0,
      deletions: 0,
      commit_count: 0,
      activity: VecDeque::with_capacity(ACTIVITY_CAPACITY),
      focus: FocusPane::Activity,
      scroll_offset: 0,
      iter_scroll_offset: 0,
      activity_visible: 10,
      iter_visible: 5,
      should_quit: false,
      interrupted: false,
      error_fatal: false,
      restart_harness: false,
      restart_from: None,
      plan_tasks: Vec::new(),
      plan_raw: String::new(),
    }
  }

  pub fn elapsed(&self) -> Duration {
    match self.end_time {
      Some(end) => end.duration_since(self.start_time),
      None => self.start_time.elapsed(),
    }
  }

  pub fn start_iteration(&mut self, n: u32) {
    self.current_iter = n;
    if let Some(iter) = self.iterations.get_mut((n - 1) as usize) {
      iter.status = IterStatus::Running;
      iter.start_time = Some(Instant::now());
    }
  }

  pub fn complete_iteration(&mut self, n: u32, diff: Option<(u32, u32)>, msg: Option<String>) {
    if let Some(iter) = self.iterations.get_mut((n - 1) as usize) {
      iter.status = IterStatus::Completed;
      iter.duration = iter.start_time.map(|s| s.elapsed());
      iter.diff_stats = diff;
      iter.commit_msg = msg;
    }
  }

  pub fn add_activity(&mut self, kind: ActivityKind, content: String) {
    if self.activity.len() >= ACTIVITY_CAPACITY {
      self.activity.pop_front();
    }
    self.activity.push_back(ActivityEntry { timestamp: Instant::now(), kind, content });
  }

  pub fn refresh_plan(&mut self, cwd: &Path) {
    let plan_path = cwd.join(SPEC_PLAN);
    let Ok(content) = std::fs::read_to_string(&plan_path) else { return };
    if content == self.plan_raw {
      return;
    }
    self.plan_raw = content;
    self.plan_tasks = self
      .plan_raw
      .lines()
      .filter_map(|line| {
        line.trim_start().strip_prefix("- [ ]").map(str::trim).filter(|t| !t.is_empty()).map(String::from)
      })
      .collect();
  }

  pub fn update_git_stats(&mut self, files: u32, ins: u32, del: u32, commits: u32) {
    self.files_changed = files;
    self.insertions = ins;
    self.deletions = del;
    self.commit_count = commits;
  }

  pub fn toggle_focus(&mut self) {
    self.focus = match self.focus {
      FocusPane::Activity => FocusPane::Iterations,
      FocusPane::Iterations => FocusPane::Activity,
    };
  }

  pub fn scroll_up(&mut self) {
    match self.focus {
      FocusPane::Activity => {
        // max scroll = entries that are off-screen above
        let max_scroll = self.activity.len().saturating_sub(self.activity_visible);
        if self.scroll_offset < max_scroll {
          self.scroll_offset += 1;
        }
      }
      FocusPane::Iterations => {
        // can't scroll past the last item
        let max_scroll = self.iterations.len().saturating_sub(self.iter_visible);
        if self.iter_scroll_offset < max_scroll {
          self.iter_scroll_offset += 1;
        }
      }
    }
  }

  pub fn scroll_down(&mut self) {
    match self.focus {
      FocusPane::Activity => self.scroll_offset = self.scroll_offset.saturating_sub(1),
      FocusPane::Iterations => self.iter_scroll_offset = self.iter_scroll_offset.saturating_sub(1),
    }
  }

  pub fn set_visible_heights(&mut self, activity: usize, iterations: usize) {
    self.activity_visible = activity;
    self.iter_visible = iterations;
  }
}

// =============================================================================
// Tui entry point
// =============================================================================

#[derive(Clone)]
pub(crate) enum TuiBackend {
  Bwrap(Arc<BwrapContext>),
  Docker(Arc<DockerContainer>),
}

pub async fn run(
  sandbox_name: String,
  harness: Harness,
  mode: RunMode,
  max_iter: u32,
  cwd: PathBuf,
  backend: Option<TuiBackend>,
) -> Result<(), Error> {
  enable_raw_mode().map_err(|e| Error::RawModeEnable(e.to_string()))?;
  let mut stdout = io::stdout();
  execute!(stdout, EnterAlternateScreen).map_err(|e| Error::AlternateScreenEnter(e.to_string()))?;
  let term_backend = CrosstermBackend::new(stdout);
  let mut terminal = Terminal::new(term_backend).map_err(|e| Error::TerminalCreate(e.to_string()))?;

  let result = run_loop(&mut terminal, sandbox_name, harness, mode, max_iter, cwd, backend).await;

  disable_raw_mode().ok();
  execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
  terminal.show_cursor().ok();

  result
}

async fn run_loop(
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  sandbox_name: String,
  harness: Harness,
  mode: RunMode,
  max_iter: u32,
  cwd: PathBuf,
  backend: Option<TuiBackend>,
) -> Result<(), Error> {
  let mut state = AppState::new(sandbox_name, cwd.clone(), harness.as_str().into(), max_iter);
  state.refresh_plan(&cwd);
  load_activity_log(&mut state, &cwd);

  let (harness_tx, mut harness_rx) = mpsc::unbounded_channel::<HarnessEvent>();
  let mut harness_handle =
    tokio::spawn(run_harness_loop(harness, mode, 1, max_iter, cwd.clone(), harness_tx.clone(), backend.clone()));

  // use EventStream for reliable async event reading
  let mut event_stream = EventStream::new();
  let mut last_refresh = Instant::now();
  let tick_rate = Duration::from_millis(50);

  loop {
    terminal.draw(|f| ui::render(f, &mut state))?;

    tokio::select! {
      // terminal events via EventStream
      maybe_event = event_stream.next() => {
        match maybe_event {
          Some(Ok(Event::Key(key))) => {
            // only handle key press events
            if key.kind == KeyEventKind::Press {
              handle_term_event(&mut state, TermEvent::Key(key), terminal, &cwd)?;
            }
          }
          Some(Ok(Event::Resize(_, _))) => {
            let _ = handle_term_event(&mut state, TermEvent::Resize, terminal, &cwd);
          }
          Some(Ok(Event::Mouse(mouse))) => {
            let _ = handle_term_event(&mut state, TermEvent::Mouse(mouse), terminal, &cwd);
          }
          _ => {}
        }
      }
      // harness events
      Some(ev) = harness_rx.recv() => {
        handle_harness_event(&mut state, ev);
      }
      // tick for refresh
      () = tokio::time::sleep(tick_rate) => {
        if last_refresh.elapsed() > Duration::from_millis(500) {
          state.refresh_plan(&cwd);
          last_refresh = Instant::now();
        }
      }
    }

    if state.restart_harness {
      if !harness_handle.is_finished() {
        harness_handle.abort();
      }
      let start_iter = state.restart_from.take().unwrap_or(state.current_iter.saturating_add(1));
      state.restart_harness = false;
      state.status = RunStatus::Running;
      state.end_time = None;
      harness_handle = tokio::spawn(run_harness_loop(
        harness,
        mode,
        start_iter,
        state.max_iter,
        cwd.clone(),
        harness_tx.clone(),
        backend.clone(),
      ));
    }

    if state.should_quit {
      break;
    }
  }

  harness_handle.abort();

  if state.interrupted {
    return Err(crate::Error::Interrupted);
  }

  Ok(())
}

fn handle_harness_event(state: &mut AppState, event: HarnessEvent) {
  match event {
    HarnessEvent::IterationStart { n } => state.start_iteration(n),
    HarnessEvent::IterationComplete { n, diff, msg } => {
      state.complete_iteration(n, diff, msg);
    }
    HarnessEvent::Activity { kind, content } => state.add_activity(kind, content),
    HarnessEvent::GitStats { files, ins, del, commits } => state.update_git_stats(files, ins, del, commits),
    HarnessEvent::Error { message } => {
      state.add_activity(ActivityKind::Text, format!("[error] {message}"));
      state.popup = RunPopup::Error { message };
      state.error_fatal = true;
    }
    HarnessEvent::Finished => {
      state.status = RunStatus::Done;
      state.end_time = Some(Instant::now());
      state.popup = RunPopup::Options;
    }
  }
}

fn load_activity_log(state: &mut AppState, cwd: &Path) {
  let Ok(repo) = git2::Repository::open(cwd) else { return };
  let Ok(mut cfg) = repo.config() else { return };
  {
    let Ok(mut entries) = cfg.entries(Some("so.activity")) else { return };
    while let Some(entry) = entries.next() {
      if let Ok(e) = entry
        && let Some(v) = e.value()
      {
        let v = v.trim();
        if !v.is_empty() {
          state.add_activity(ActivityKind::Text, v.to_string());
        }
      }
    }
  }
  let _ = cfg.remove_multivar("so.activity", ".*");
}

fn handle_term_event(
  state: &mut AppState,
  event: TermEvent,
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  cwd: &Path,
) -> Result<(), Error> {
  match event {
    TermEvent::Key(key) => {
      if handle_ctrl_c(state, key) {
        return Ok(());
      }

      let mut popup = std::mem::replace(&mut state.popup, RunPopup::None);
      let handled_popup = match &mut popup {
        RunPopup::Options => {
          if let Some(next) = handle_popup_options(state, key, terminal, cwd)? {
            popup = next;
          }
          true
        }
        RunPopup::Reset { commits, selected } => {
          if let Some(next) = handle_popup_reset(state, key, cwd, commits.as_slice(), selected)? {
            popup = next;
          }
          true
        }
        RunPopup::Continue { input } => {
          if let Some(next) = handle_popup_continue(state, key, input)? {
            popup = next;
          }
          true
        }
        RunPopup::MergeConfirm { orig } => {
          if let Some(next) = handle_popup_merge(state, key, cwd, orig)? {
            popup = next;
          }
          true
        }
        RunPopup::Error { .. } => {
          if let Some(next) = handle_popup_error(state, key) {
            popup = next;
          }
          true
        }
        RunPopup::None => false,
      };

      state.popup = popup;
      if handled_popup {
        return Ok(());
      }

      handle_nav_keys(state, key);
    }
    TermEvent::Mouse(mouse) => match mouse.kind {
      MouseEventKind::ScrollUp => state.scroll_up(),
      MouseEventKind::ScrollDown => state.scroll_down(),
      _ => {}
    },
    TermEvent::Resize => {}
  }
  Ok(())
}

fn handle_ctrl_c(state: &mut AppState, key: KeyEvent) -> bool {
  if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
    state.should_quit = true;
    state.interrupted = true;
    return true;
  }
  false
}

fn handle_popup_options(
  _state: &mut AppState,
  key: KeyEvent,
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  cwd: &Path,
) -> Result<Option<RunPopup>, Error> {
  let mut replace = None;
  match key.code {
    KeyCode::Char('d') => {
      let range = actions::diff_range(cwd);
      actions::suspend_and_run_git(terminal, &["diff", &range], cwd, false)?;
    }
    KeyCode::Char('s') => {
      actions::suspend_and_run_shell(terminal, cwd, false)?;
    }
    KeyCode::Char('r') => {
      let commits = crate::sandbox::git_commits(cwd, crate::sandbox::BASE_TAG).unwrap_or_default();
      if commits.is_empty() {
        replace = Some(RunPopup::Error { message: "No commits to reset".into() });
      } else {
        replace = Some(RunPopup::Reset { commits, selected: 0 });
      }
    }
    KeyCode::Char('m') => {
      if let Some(orig) = actions::original_repo(cwd) {
        replace = Some(RunPopup::MergeConfirm { orig });
      } else {
        replace = Some(RunPopup::Error { message: Error::OriginalRepoNotFound.to_string() });
      }
    }
    KeyCode::Char('c') => {
      replace = Some(RunPopup::Continue { input: String::new() });
    }
    KeyCode::Char('q') | KeyCode::Esc => {
      replace = Some(RunPopup::None);
    }
    _ => {}
  }
  Ok(replace)
}

fn handle_popup_reset(
  state: &mut AppState,
  key: KeyEvent,
  cwd: &Path,
  commits: &[(String, String)],
  selected: &mut usize,
) -> Result<Option<RunPopup>, Error> {
  let mut replace = None;
  match key.code {
    KeyCode::Up | KeyCode::Char('k') => {
      if *selected > 0 {
        *selected -= 1;
      }
    }
    KeyCode::Down | KeyCode::Char('j') => {
      if *selected + 1 < commits.len() {
        *selected += 1;
      }
    }
    KeyCode::Enter => {
      if let Some((hash, _)) = commits.get(*selected) {
        let ok = actions::git_reset_hard(cwd, hash);
        if !ok {
          replace = Some(RunPopup::Error { message: "Reset failed".into() });
          return Ok(replace);
        }
        refresh_iterations_from_commits(state, cwd);
        update_git_stats_state(state, cwd);
      }
      replace = Some(RunPopup::Options);
    }
    KeyCode::Esc => {
      replace = Some(RunPopup::Options);
    }
    _ => {}
  }
  Ok(replace)
}

fn handle_popup_continue(state: &mut AppState, key: KeyEvent, input: &mut String) -> Result<Option<RunPopup>, Error> {
  let mut replace = None;
  match key.code {
    KeyCode::Char(c) if c.is_ascii_digit() => {
      if input.len() < 3 {
        input.push(c);
      }
    }
    KeyCode::Backspace => {
      input.pop();
    }
    KeyCode::Enter => {
      let extra: u32 = input.parse().unwrap_or(10);
      state.max_iter += extra;
      for n in (state.current_iter + 1)..=(state.max_iter) {
        state.iterations.push(Iteration::new(n));
      }
      state.status = RunStatus::Running;
      state.restart_from = Some(state.current_iter.saturating_add(1));
      state.restart_harness = true;
      replace = Some(RunPopup::None);
    }
    KeyCode::Esc => {
      replace = Some(RunPopup::Options);
    }
    _ => {}
  }
  Ok(replace)
}

fn handle_popup_error(state: &mut AppState, key: KeyEvent) -> Option<RunPopup> {
  match key.code {
    KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
      if state.error_fatal {
        state.should_quit = true;
        return None;
      }
      Some(RunPopup::Options)
    }
    _ => None,
  }
}

fn handle_popup_merge(state: &mut AppState, key: KeyEvent, cwd: &Path, orig: &Path) -> Result<Option<RunPopup>, Error> {
  let mut replace = None;
  match confirm_choice(key.code) {
    Some(ConfirmChoice::Yes) => match actions::merge_sandbox(cwd, orig) {
      Ok(()) => {
        replace = Some(RunPopup::None);
        state.should_quit = true;
      }
      Err(err) => {
        replace = Some(RunPopup::Error { message: err.to_string() });
      }
    },
    Some(ConfirmChoice::No) => {
      replace = Some(RunPopup::Options);
    }
    None => {}
  }
  Ok(replace)
}

fn handle_nav_keys(state: &mut AppState, key: KeyEvent) {
  if key.code == KeyCode::Tab {
    state.toggle_focus();
  }

  match key.code {
    KeyCode::Char('k') | KeyCode::Up | KeyCode::PageUp => state.scroll_up(),
    KeyCode::Char('j') | KeyCode::Down | KeyCode::PageDown => state.scroll_down(),
    KeyCode::Home => match state.focus {
      FocusPane::Activity => state.scroll_offset = state.activity.len().saturating_sub(1),
      FocusPane::Iterations => state.iter_scroll_offset = 0,
    },
    KeyCode::End => match state.focus {
      FocusPane::Activity => state.scroll_offset = 0,
      FocusPane::Iterations => state.iter_scroll_offset = state.iterations.len().saturating_sub(1),
    },
    _ => {}
  }
}

// =============================================================================
// Harness runners
// =============================================================================

async fn run_harness_loop(
  harness: Harness,
  mode: RunMode,
  start_iter: u32,
  max_iter: u32,
  cwd: PathBuf,
  tx: mpsc::UnboundedSender<HarnessEvent>,
  backend: Option<TuiBackend>,
) {
  let _ =
    tx.send(HarnessEvent::Activity { kind: ActivityKind::Text, content: format!("starting {}", harness.as_str()) });

  if harness == Harness::Claude
    && let Ok(task_id) = get_task_id(&cwd)
  {
    let _ = tx.send(HarnessEvent::Activity { kind: ActivityKind::Text, content: format!("tasks: {task_id}") });
  }

  for i in start_iter..=max_iter {
    let _ = tx.send(HarnessEvent::IterationStart { n: i });

    if let Err(e) = run_single_iteration(harness, mode, i, max_iter, &cwd, &tx, backend.as_ref()).await {
      let _ = tx.send(HarnessEvent::Error { message: e.to_string() });
      return;
    }

    if let Err(e) = enforce_commit_tui(harness, &cwd, &tx, backend.as_ref()).await {
      let _ = tx.send(HarnessEvent::Error { message: e.to_string() });
      return;
    }

    if check_status_done(&cwd) {
      let _ = tx.send(HarnessEvent::Activity { kind: ActivityKind::Text, content: "status: done".into() });
    }

    tokio::time::sleep(Duration::from_secs(1)).await;
  }
  let _ = tx.send(HarnessEvent::Finished);
}

async fn enforce_commit_tui(
  harness: Harness,
  cwd: &Path,
  tx: &mpsc::UnboundedSender<HarnessEvent>,
  backend: Option<&TuiBackend>,
) -> Result<(), Error> {
  let msg = "Commit now. Do not ask questions. Always commit no matter what.\nIf there are no changes, make an empty commit with --allow-empty.\nMessage format:\n- What: <what was done>\n- Why: <reasoning>\n- Alternatives: <what else was considered>";
  for i in 1..=3 {
    if !crate::sandbox::git_dirty(cwd).unwrap_or(false) {
      return Ok(());
    }
    let _ = tx.send(HarnessEvent::Activity {
      kind: ActivityKind::Text,
      content: format!("warning: uncommitted changes ({i}/3)"),
    });
    if run_commit_prompt(harness, msg, cwd, tx, backend).await.is_err() {
      break;
    }
  }
  if crate::sandbox::git_dirty(cwd).unwrap_or(false) {
    let _ = tx.send(HarnessEvent::Activity {
      kind: ActivityKind::Text,
      content: "error: failed to commit after 3 attempts".into(),
    });
  }
  Ok(())
}

async fn run_commit_prompt(
  harness: Harness,
  prompt: &str,
  cwd: &Path,
  tx: &mpsc::UnboundedSender<HarnessEvent>,
  backend: Option<&TuiBackend>,
) -> Result<(), Error> {
  match harness {
    Harness::Claude => run_claude(prompt, cwd, tx, 0, backend).await,
    Harness::Opencode => run_opencode(prompt, cwd, tx, 0, backend).await,
    Harness::Codex => run_codex(prompt, cwd, tx, 0, backend).await,
  }
}

async fn run_single_iteration(
  harness: Harness,
  mode: RunMode,
  iter: u32,
  max_iter: u32,
  cwd: &Path,
  tx: &mpsc::UnboundedSender<HarnessEvent>,
  backend: Option<&TuiBackend>,
) -> Result<(), Error> {
  let prompt_path = cwd.join(SPEC_PROMPT);
  let base = std::fs::read_to_string(&prompt_path)?;
  let prompt = build_prompt(&base, mode, cwd, iter, max_iter);

  match harness {
    Harness::Claude => run_claude(&prompt, cwd, tx, iter, backend).await,
    Harness::Opencode => run_opencode(&prompt, cwd, tx, iter, backend).await,
    Harness::Codex => run_codex(&prompt, cwd, tx, iter, backend).await,
  }
}

async fn run_claude(
  prompt: &str,
  cwd: &Path,
  tx: &mpsc::UnboundedSender<HarnessEvent>,
  current_iter: u32,
  backend: Option<&TuiBackend>,
) -> Result<(), Error> {
  let mut cmd = harness_cmd(backend, cwd, "claude");
  if let Ok(m) = std::env::var("MODEL") {
    cmd.arg("--model").arg(m);
  }
  cmd.args(["--dangerously-skip-permissions", "--setting-sources", "project,local"]);
  cmd.arg("--settings").arg(r#"{"outputStyle":"Explanatory","alwaysThinkingEnabled":true}"#);
  cmd.args(["-p", "--verbose", "--output-format", "stream-json"]).arg(prompt);
  cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());

  let mut child = cmd.spawn().map_err(|e| Error::Harness(format!("claude spawn failed: {e}")))?;

  if let Some(stdout) = child.stdout.take() {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
      parse_claude(&line, tx, current_iter);
    }
  }

  let status = child.wait().await?;
  if !status.success() {
    return Err(Error::Harness("claude failed".into()));
  }
  update_git_stats(cwd, tx);
  Ok(())
}

async fn run_opencode(
  prompt: &str,
  cwd: &Path,
  tx: &mpsc::UnboundedSender<HarnessEvent>,
  current_iter: u32,
  backend: Option<&TuiBackend>,
) -> Result<(), Error> {
  let model = env_or_default("MODEL", "openai/gpt-5.2-codex");
  let effort = env_or_default("EFFORT", "medium");

  let mut cmd = harness_cmd(backend, cwd, "opencode");
  cmd.env("OPENCODE_PERMISSION", r#"{"*":"allow"}"#);
  cmd.args(["run", "--log-level", "INFO", "-m", &model, "--variant", &effort, prompt]);
  cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

  let mut child = cmd.spawn().map_err(|e| Error::Harness(format!("opencode spawn failed: {e}")))?;
  stream_output(&mut child, tx, parse_opencode).await;

  let status = child.wait().await?;
  if !status.success() {
    return Err(Error::Harness("opencode failed".into()));
  }
  update_git_stats(cwd, tx);
  let _ = tx.send(HarnessEvent::IterationComplete { n: current_iter, diff: None, msg: Some("complete".into()) });
  Ok(())
}

async fn run_codex(
  prompt: &str,
  cwd: &Path,
  tx: &mpsc::UnboundedSender<HarnessEvent>,
  current_iter: u32,
  backend: Option<&TuiBackend>,
) -> Result<(), Error> {
  let model = env_or_default("MODEL", "gpt-5.2-codex");
  let effort = env_or_default("EFFORT", "medium");

  let mut cmd = harness_cmd(backend, cwd, "codex");
  let cfg = format!("model_reasoning_effort={effort}");
  cmd.args([
    "exec",
    "--full-auto",
    "--dangerously-bypass-approvals-and-sandbox",
    prompt,
    "--model",
    &model,
    "--config",
    &cfg,
  ]);
  cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

  let mut child = cmd.spawn().map_err(|e| Error::Harness(format!("codex spawn failed: {e}")))?;
  stream_output(&mut child, tx, parse_codex).await;

  let status = child.wait().await?;
  if !status.success() {
    return Err(Error::Harness("codex failed".into()));
  }
  update_git_stats(cwd, tx);
  let _ = tx.send(HarnessEvent::IterationComplete { n: current_iter, diff: None, msg: Some("complete".into()) });
  Ok(())
}

async fn stream_output<F>(child: &mut tokio::process::Child, tx: &mpsc::UnboundedSender<HarnessEvent>, parser: F)
where
  F: Fn(&str, &mpsc::UnboundedSender<HarnessEvent>) + Send + Sync + Copy + 'static,
{
  let stdout = child.stdout.take();
  let stderr = child.stderr.take();

  let tx1 = tx.clone();
  let stdout_task = tokio::spawn(async move {
    if let Some(out) = stdout {
      let mut lines = BufReader::new(out).lines();
      while let Ok(Some(line)) = lines.next_line().await {
        parser(&line, &tx1);
      }
    }
  });

  let tx2 = tx.clone();
  let stderr_task = tokio::spawn(async move {
    if let Some(err) = stderr {
      let mut lines = BufReader::new(err).lines();
      while let Ok(Some(line)) = lines.next_line().await {
        parser(&line, &tx2);
      }
    }
  });

  let _ = tokio::join!(stdout_task, stderr_task);
}

fn update_git_stats(cwd: &Path, tx: &mpsc::UnboundedSender<HarnessEvent>) {
  if let Some((files, ins, del, commits)) = collect_git_stats(cwd) {
    let _ = tx.send(HarnessEvent::GitStats { files, ins, del, commits });
  }
}

fn harness_cmd(backend: Option<&TuiBackend>, cwd: &Path, program: &str) -> Command {
  match backend {
    Some(TuiBackend::Bwrap(ctx)) => ctx.cmd(program),
    Some(TuiBackend::Docker(container)) => container.exec_cmd_tty(program, false),
    None => {
      let mut cmd = Command::new(program);
      cmd.current_dir(cwd);
      cmd
    }
  }
}

fn update_git_stats_state(state: &mut AppState, cwd: &Path) {
  if let Some((files, ins, del, commits)) = collect_git_stats(cwd) {
    state.files_changed = files;
    state.insertions = ins;
    state.deletions = del;
    state.commit_count = commits;
  }
}

fn refresh_iterations_from_commits(state: &mut AppState, cwd: &Path) {
  let mut commits = crate::sandbox::git_commits(cwd, crate::sandbox::BASE_TAG).unwrap_or_default();
  commits.reverse();
  let done = commits.len();

  for (idx, iter) in state.iterations.iter_mut().enumerate() {
    if idx < done {
      iter.status = IterStatus::Completed;
      iter.start_time = None;
      iter.duration = None;
      iter.diff_stats = None;
      iter.commit_msg = commits.get(idx).map(|(_, msg)| msg.clone());
    } else {
      iter.status = IterStatus::Pending;
      iter.start_time = None;
      iter.duration = None;
      iter.diff_stats = None;
      iter.commit_msg = None;
    }
  }

  state.current_iter = done as u32;
  state.status = RunStatus::Done;
  state.iter_scroll_offset = 0;
}

fn collect_git_stats(cwd: &Path) -> Option<(u32, u32, u32, u32)> {
  let repo = git2::Repository::open(cwd).ok()?;
  let base = crate::sandbox::git_base(cwd, crate::sandbox::BASE_TAG);
  let tree = repo.revparse_single(&base).ok()?.peel_to_tree().ok()?;
  let diff = repo.diff_tree_to_workdir_with_index(Some(&tree), None).ok()?;
  let stats = diff.stats().ok()?;
  let commits = count_commits(&repo, cwd).unwrap_or(0);
  Some((stats.files_changed() as u32, stats.insertions() as u32, stats.deletions() as u32, commits))
}

fn count_commits(repo: &git2::Repository, cwd: &Path) -> Option<u32> {
  let base = crate::sandbox::git_base(cwd, crate::sandbox::BASE_TAG);
  let base_oid = repo.revparse_single(&base).ok()?.id();
  let head_oid = repo.head().ok()?.target()?;
  let mut revwalk = repo.revwalk().ok()?;
  revwalk.push(head_oid).ok()?;
  revwalk.hide(base_oid).ok()?;
  let count = revwalk.count() as u32;
  Some(count)
}

fn check_status_done(cwd: &Path) -> bool {
  let status_path = cwd.join(SPEC_STATUS);
  std::fs::read_to_string(status_path).map(|c| c.to_lowercase().contains("status: done")).unwrap_or(false)
}

// =============================================================================
// Output parsers
// =============================================================================

fn strip_ansi(s: &str) -> String {
  let mut result = String::with_capacity(s.len());
  let mut chars = s.chars().peekable();
  while let Some(c) = chars.next() {
    if c == '\x1b' {
      while let Some(&next) = chars.peek() {
        chars.next();
        if next == 'm' {
          break;
        }
      }
    } else {
      result.push(c);
    }
  }
  result
}

fn shorten_path(path: &str) -> String {
  let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
  if parts.len() <= 2 {
    return path.to_string();
  }
  let tail = parts.split_off(parts.len() - 2);
  tail.join("/")
}

fn truncate_chars(s: &str, max: usize) -> String {
  if max == 0 {
    return String::new();
  }
  let count = s.chars().count();
  if count <= max { s.to_string() } else { s.chars().take(max).collect() }
}

fn env_or_default(name: &str, default: &str) -> String {
  std::env::var(name).unwrap_or_else(|_| default.to_string())
}

// claude stream-json parser
fn parse_claude(line: &str, tx: &mpsc::UnboundedSender<HarnessEvent>, current_iter: u32) {
  if !line.starts_with('{') {
    return;
  }
  let j: Value = match serde_json::from_str(line) {
    Ok(v) => v,
    Err(_) => return,
  };

  match j.get("type").and_then(|t| t.as_str()) {
    Some("assistant") => parse_claude_assistant(&j, tx),
    Some("result") => parse_claude_result(&j, tx, current_iter),
    Some("system") => parse_claude_system(&j, tx),
    _ => {}
  }
}

fn parse_claude_assistant(j: &Value, tx: &mpsc::UnboundedSender<HarnessEvent>) {
  let Some(content) = j.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) else { return };

  for item in content {
    match item.get("type").and_then(|t| t.as_str()) {
      Some("text") => {
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
          for line in text.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
              let kind = if trimmed.starts_with("```") { ActivityKind::Code } else { ActivityKind::Text };
              let _ = tx.send(HarnessEvent::Activity { kind, content: trimmed.to_string() });
            }
          }
        }
      }
      Some("thinking") => {
        if let Some(text) = item.get("thinking").and_then(|t| t.as_str()) {
          let first_line = text.lines().next().unwrap_or("thinking...");
          let summary = truncate_chars(first_line, 80);
          let _ =
            tx.send(HarnessEvent::Activity { kind: ActivityKind::Thinking, content: format!("[thinking] {summary}") });
        }
      }
      Some("tool_use") => parse_claude_tool(item, tx),
      _ => {}
    }
  }
}

fn parse_claude_tool(item: &Value, tx: &mpsc::UnboundedSender<HarnessEvent>) {
  let tool_name = item.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
  let input = item.get("input");

  let (kind, content) = match tool_name {
    "Read" => {
      let path = input.and_then(|i| i.get("file_path")).and_then(|p| p.as_str()).unwrap_or("?");
      (ActivityKind::Reading, shorten_path(path))
    }
    "Write" | "Edit" => {
      let path = input.and_then(|i| i.get("file_path")).and_then(|p| p.as_str()).unwrap_or("?");
      (ActivityKind::Writing, shorten_path(path))
    }
    "Bash" => {
      let cmd = input.and_then(|i| i.get("command")).and_then(|c| c.as_str()).unwrap_or("?");
      (ActivityKind::ToolCall, truncate_chars(cmd, 60))
    }
    "Glob" | "Grep" => {
      let pattern = input.and_then(|i| i.get("pattern")).and_then(|p| p.as_str()).unwrap_or("?");
      (ActivityKind::ToolCall, format!("{} {}", tool_name.to_lowercase(), pattern))
    }
    "Task" => {
      let desc = input.and_then(|i| i.get("description")).and_then(|d| d.as_str()).unwrap_or("agent");
      (ActivityKind::ToolCall, format!("spawning {desc}"))
    }
    _ => (ActivityKind::ToolCall, tool_name.to_lowercase()),
  };
  let _ = tx.send(HarnessEvent::Activity { kind, content });
}

fn parse_claude_result(j: &Value, tx: &mpsc::UnboundedSender<HarnessEvent>, current_iter: u32) {
  if current_iter > 0 {
    let _ = tx.send(HarnessEvent::IterationComplete { n: current_iter, diff: None, msg: None });
  }
  if let Some(result) = j.get("result")
    && let Some(cost) = result.get("cost_usd").and_then(serde_json::Value::as_f64)
  {
    let _ = tx.send(HarnessEvent::Activity { kind: ActivityKind::Text, content: format!("cost: ${cost:.4}") });
  }
}

fn parse_claude_system(j: &Value, tx: &mpsc::UnboundedSender<HarnessEvent>) {
  if let Some(msg) = j.get("message").and_then(|m| m.as_str())
    && (msg.contains("error") || msg.contains("Error"))
  {
    let _ = tx.send(HarnessEvent::Activity { kind: ActivityKind::Text, content: format!("[error] {msg}") });
  }
}

// opencode text parser
fn parse_opencode(line: &str, tx: &mpsc::UnboundedSender<HarnessEvent>) {
  let stripped = strip_ansi(line);
  let trimmed = stripped.trim();
  if trimmed.is_empty() {
    return;
  }

  if trimmed.starts_with("Resolving") || trimmed.starts_with("Resolved") || trimmed.starts_with("Saved") {
    return;
  }

  let (kind, content) = if trimmed.contains("Read") && trimmed.contains('|') {
    let path = trimmed.split("Read").last().unwrap_or("").trim();
    (ActivityKind::Reading, path.to_string())
  } else if (trimmed.contains("Write") || trimmed.contains("apply_patch")) && trimmed.contains('|') {
    let rest = if trimmed.contains("apply_patch") {
      "patch applied".to_string()
    } else {
      trimmed.split("Write").last().unwrap_or("").trim().to_string()
    };
    (ActivityKind::Writing, rest)
  } else if trimmed.contains("Edit") && trimmed.contains('|') {
    let path = trimmed.split("Edit").last().unwrap_or("").trim();
    (ActivityKind::Writing, path.to_string())
  } else if (trimmed.contains("Bash") || trimmed.contains("Shell")) && trimmed.contains('|') {
    let cmd = trimmed.split('|').next_back().unwrap_or("").trim();
    let cmd_clean = cmd.replace("Bash", "").replace("Shell", "").trim().to_string();
    (ActivityKind::ToolCall, cmd_clean)
  } else if trimmed.contains("Glob") && trimmed.contains('|') {
    let rest = trimmed.split("Glob").last().unwrap_or("").trim();
    (ActivityKind::ToolCall, format!("glob {rest}"))
  } else if trimmed.contains("Grep") && trimmed.contains('|') {
    let rest = trimmed.split("Grep").last().unwrap_or("").trim();
    (ActivityKind::ToolCall, format!("grep {rest}"))
  } else {
    (ActivityKind::Text, trimmed.to_string())
  };

  if !content.is_empty() {
    let _ = tx.send(HarnessEvent::Activity { kind, content });
  }
}

// codex text parser
fn parse_codex(line: &str, tx: &mpsc::UnboundedSender<HarnessEvent>) {
  let stripped = strip_ansi(line);
  let trimmed = stripped.trim();
  if trimmed.is_empty() {
    return;
  }

  if trimmed == "exec" || trimmed == "thinking" || trimmed == "Thinking..." {
    return;
  }

  if trimmed.starts_with("index ")
    || trimmed.starts_with("diff --git")
    || trimmed.starts_with("@@")
    || trimmed.starts_with("Binary files")
    || trimmed.contains("No newline at end of file")
  {
    return;
  }

  if (trimmed.starts_with('+') || trimmed.starts_with('-')) && trimmed.len() > 1 {
    return;
  }

  let (kind, content) = if trimmed.starts_with("/bin/bash") || trimmed.starts_with("$ ") {
    let cmd = if trimmed.contains('\'') {
      trimmed.split('\'').nth(1).unwrap_or(trimmed)
    } else if let Some(rest) = trimmed.strip_prefix("$ ") {
      rest
    } else {
      trimmed.trim_start_matches("/bin/bash -lc ").trim_start_matches("/bin/bash -c ")
    };
    (ActivityKind::ToolCall, truncate_chars(cmd, 60))
  } else if let Some(path) = trimmed.strip_prefix("Reading ") {
    (ActivityKind::Reading, shorten_path(path.trim()))
  } else if let Some(path) = trimmed.strip_prefix("Writing ") {
    (ActivityKind::Writing, shorten_path(path.trim()))
  } else if let Some(path) = trimmed.strip_prefix("Wrote ") {
    (ActivityKind::Writing, shorten_path(path.split_whitespace().next().unwrap_or("file")))
  } else if trimmed.contains("succeeded in") || trimmed.contains("failed in") {
    (ActivityKind::ToolCall, trimmed.to_string())
  } else {
    (ActivityKind::Text, trimmed.to_string())
  };

  if !content.is_empty() {
    let _ = tx.send(HarnessEvent::Activity { kind, content });
  }
}

// =============================================================================
// Helpers
// =============================================================================

fn get_task_id(cwd: &Path) -> Result<String, Error> {
  let repo = git2::Repository::open(cwd)?;
  let cfg = repo.config()?;
  cfg.get_string("so.mdata.task-id").map_err(Error::Git)
}
