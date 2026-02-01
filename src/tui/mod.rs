mod ui;

use std::{
  collections::VecDeque,
  io::{self, Write},
  path::{Path, PathBuf},
  process::Stdio,
  time::{Duration, Instant},
};

use crossterm::{
  event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseEvent, MouseEventKind,
  },
  execute,
  terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use serde_json::Value;
use tokio::{
  io::{AsyncBufReadExt, BufReader},
  process::Command,
  sync::mpsc,
};

use crate::{Error, Harness, RunMode, build_prompt};

// =============================================================================
// Types
// =============================================================================

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunStatus {
  #[default]
  Running,
  Done,
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
  pub harness: String,
  pub start_time: Instant,
  pub end_time: Option<Instant>,
  pub status: RunStatus,
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
  pub plan_tasks: Vec<String>,
  plan_raw: String,
}

impl AppState {
  pub fn new(sandbox_name: String, harness: String, max_iter: u32) -> Self {
    let iterations = (1..=max_iter).map(Iteration::new).collect();
    Self {
      sandbox_name,
      harness,
      start_time: Instant::now(),
      end_time: None,
      status: RunStatus::Running,
      current_iter: 0,
      max_iter,
      iterations,
      files_changed: 0,
      insertions: 0,
      deletions: 0,
      commit_count: 0,
      activity: VecDeque::with_capacity(1000),
      focus: FocusPane::Activity,
      scroll_offset: 0,
      iter_scroll_offset: 0,
      activity_visible: 10,
      iter_visible: 5,
      should_quit: false,
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
    if self.activity.len() >= 1000 {
      self.activity.pop_front();
    }
    self.activity.push_back(ActivityEntry { timestamp: Instant::now(), kind, content });
  }

  pub fn refresh_plan(&mut self, cwd: &Path) {
    let plan_path = cwd.join("specs/implementation-plan.md");
    let content = match std::fs::read_to_string(&plan_path) {
      Ok(c) => c,
      Err(_) => return,
    };
    if content == self.plan_raw {
      return;
    }
    self.plan_raw = content.clone();
    self.plan_tasks = content
      .lines()
      .filter_map(|line| {
        line.trim_start().strip_prefix("- [ ]").map(|rest| rest.trim()).filter(|t| !t.is_empty()).map(String::from)
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

pub async fn run(
  sandbox_name: String,
  harness: Harness,
  mode: RunMode,
  max_iter: u32,
  cwd: PathBuf,
) -> Result<(), Error> {
  enable_raw_mode().map_err(|e| Error::Other(format!("failed to enable raw mode: {}", e)))?;
  let mut stdout = io::stdout();
  execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
    .map_err(|e| Error::Other(format!("failed to enter alternate screen: {}", e)))?;
  let backend = CrosstermBackend::new(stdout);
  let mut terminal = Terminal::new(backend).map_err(|e| Error::Other(format!("failed to create terminal: {}", e)))?;

  let result = run_loop(&mut terminal, sandbox_name, harness, mode, max_iter, cwd).await;

  disable_raw_mode().ok();
  execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture).ok();
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
) -> Result<(), Error> {
  let mut state = AppState::new(sandbox_name, harness.as_str().into(), max_iter);
  state.refresh_plan(&cwd);

  let (harness_tx, mut harness_rx) = mpsc::unbounded_channel::<HarnessEvent>();
  let harness_handle = tokio::spawn(run_harness_loop(harness, mode, max_iter, cwd.clone(), harness_tx));

  // use EventStream for reliable async event reading (works in tmux)
  let mut event_stream = EventStream::new();
  let mut last_refresh = Instant::now();
  let tick_rate = Duration::from_millis(50);

  loop {
    terminal.draw(|f| ui::render(f, &mut state))?;

    // use tokio::select! to wait on multiple async sources
    tokio::select! {
      // terminal events via EventStream
      maybe_event = event_stream.next() => {
        match maybe_event {
          Some(Ok(Event::Key(key))) => {
            // only handle key press events
            if key.kind == KeyEventKind::Press
              && handle_term_event(&mut state, TermEvent::Key(key), terminal, &cwd)?
            {
              harness_handle.abort();
              return Ok(());
            }
          }
          Some(Ok(Event::Resize(_, _))) => {
            let _ = handle_term_event(&mut state, TermEvent::Resize, terminal, &cwd);
          }
          Some(Ok(Event::Mouse(mouse))) => {
            let _ = handle_term_event(&mut state, TermEvent::Mouse(mouse), terminal, &cwd);
          }
          Some(Err(_)) | None => {}
          _ => {}
        }
      }
      // harness events
      Some(ev) = harness_rx.recv() => {
        handle_harness_event(&mut state, ev);
      }
      // tick for refresh
      _ = tokio::time::sleep(tick_rate) => {
        if last_refresh.elapsed() > Duration::from_millis(500) {
          state.refresh_plan(&cwd);
          last_refresh = Instant::now();
        }
      }
    }

    if state.should_quit {
      break;
    }
  }

  harness_handle.abort();
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
    HarnessEvent::Finished => {
      state.status = RunStatus::Done;
      state.end_time = Some(Instant::now());
      state.add_activity(ActivityKind::Text, "-- run complete, press ^C to exit --".into());
    }
  }
}

fn handle_term_event(
  state: &mut AppState,
  event: TermEvent,
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  cwd: &Path,
) -> Result<bool, Error> {
  match event {
    TermEvent::Key(key) => {
      if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.should_quit = true;
        return Ok(true);
      }
      if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
        suspend_and_run(terminal, "git diff", cwd)?;
      }
      if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let shell = std::env::var("SHELL").expect("SHELL env not set");
        suspend_and_run(terminal, &shell, cwd)?;
      }
      if key.code == KeyCode::Tab {
        state.toggle_focus();
      }
      match key.code {
        KeyCode::Char('k') | KeyCode::Up | KeyCode::PageUp => state.scroll_up(),
        KeyCode::Char('j') | KeyCode::Down | KeyCode::PageDown => state.scroll_down(),
        KeyCode::Home => {
          // scroll to top
          match state.focus {
            FocusPane::Activity => state.scroll_offset = state.activity.len().saturating_sub(1),
            FocusPane::Iterations => state.iter_scroll_offset = 0,
          }
        }
        KeyCode::End => {
          // scroll to bottom
          match state.focus {
            FocusPane::Activity => state.scroll_offset = 0,
            FocusPane::Iterations => state.iter_scroll_offset = state.iterations.len().saturating_sub(1),
          }
        }
        _ => {}
      }
    }
    TermEvent::Mouse(mouse) => match mouse.kind {
      MouseEventKind::ScrollUp => state.scroll_up(),
      MouseEventKind::ScrollDown => state.scroll_down(),
      _ => {}
    },
    TermEvent::Resize => {}
  }
  Ok(false)
}

fn suspend_and_run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, cmd: &str, cwd: &Path) -> Result<(), Error> {
  disable_raw_mode().ok();
  execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture).ok();
  terminal.show_cursor().ok();

  let _ = std::process::Command::new("sh").arg("-c").arg(cmd).current_dir(cwd).status();

  print!("\nPress Enter to continue...");
  io::stdout().flush().ok();
  let _ = io::stdin().read_line(&mut String::new());

  execute!(terminal.backend_mut(), EnterAlternateScreen, EnableMouseCapture).ok();
  enable_raw_mode().ok();
  terminal.clear().ok();
  Ok(())
}

// =============================================================================
// Harness runners
// =============================================================================

async fn run_harness_loop(
  harness: Harness,
  mode: RunMode,
  max_iter: u32,
  cwd: PathBuf,
  tx: mpsc::UnboundedSender<HarnessEvent>,
) {
  for i in 1..=max_iter {
    let _ = tx.send(HarnessEvent::IterationStart { n: i });

    if let Err(e) = run_single_iteration(harness, mode, i, max_iter, &cwd, &tx).await {
      let _ = tx.send(HarnessEvent::Activity { kind: ActivityKind::Text, content: format!("[error] {}", e) });
      break;
    }

    if check_status_done(&cwd) {
      let _ = tx.send(HarnessEvent::Activity { kind: ActivityKind::Text, content: "status: done".into() });
      break;
    }

    tokio::time::sleep(Duration::from_secs(1)).await;
  }
  let _ = tx.send(HarnessEvent::Finished);
}

async fn run_single_iteration(
  harness: Harness,
  mode: RunMode,
  iter: u32,
  max_iter: u32,
  cwd: &Path,
  tx: &mpsc::UnboundedSender<HarnessEvent>,
) -> Result<(), Error> {
  let prompt_path = cwd.join("specs/prompt.md");
  let base = std::fs::read_to_string(&prompt_path)?;
  let prompt = build_prompt(&base, mode, cwd, iter, max_iter);

  match harness {
    Harness::Claude => run_claude(&prompt, cwd, tx, iter).await,
    Harness::Opencode => run_opencode(&prompt, cwd, tx, iter).await,
    Harness::Codex => run_codex(&prompt, cwd, tx, iter).await,
  }
}

async fn run_claude(
  prompt: &str,
  cwd: &Path,
  tx: &mpsc::UnboundedSender<HarnessEvent>,
  current_iter: u32,
) -> Result<(), Error> {
  let mut cmd = Command::new("claude");
  if let Ok(m) = std::env::var("MODEL") {
    cmd.arg("--model").arg(m);
  }
  cmd.args(["--dangerously-skip-permissions", "--setting-sources", "project,local"]);
  cmd.arg("--settings").arg(r#"{"outputStyle":"Explanatory","alwaysThinkingEnabled":true}"#);
  cmd.args(["-p", "--verbose", "--output-format", "stream-json"]).arg(prompt);
  cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());

  let mut child = cmd.spawn().map_err(|e| Error::Harness(format!("claude spawn failed: {}", e)))?;

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
) -> Result<(), Error> {
  let model = std::env::var("MODEL").unwrap_or_else(|_| "openai/gpt-5.2-codex".into());
  let effort = std::env::var("EFFORT").unwrap_or_else(|_| "medium".into());

  let mut cmd = Command::new("opencode");
  cmd.env("OPENCODE_PERMISSION", r#"{"*":"allow"}"#);
  cmd.args(["run", "--log-level", "INFO", "-m", &model, "--variant", &effort, prompt]);
  cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

  let mut child = cmd.spawn().map_err(|e| Error::Harness(format!("opencode spawn failed: {}", e)))?;
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
) -> Result<(), Error> {
  let model = std::env::var("MODEL").unwrap_or_else(|_| "gpt-5.2-codex".into());
  let effort = std::env::var("EFFORT").unwrap_or_else(|_| "medium".into());

  let mut cmd = Command::new("codex");
  let cfg = format!("model_reasoning_effort={}", effort);
  cmd.args(["exec", "--full-auto", prompt, "--model", &model, "--config", &cfg]);
  cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

  let mut child = cmd.spawn().map_err(|e| Error::Harness(format!("codex spawn failed: {}", e)))?;
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
  if let Ok(repo) = git2::Repository::open(cwd)
    && let Ok(tree) =
      repo.revparse_single(&crate::sandbox::git_base(cwd, crate::sandbox::BASE_TAG)).and_then(|o| o.peel_to_tree())
    && let Ok(diff) = repo.diff_tree_to_workdir_with_index(Some(&tree), None)
    && let Ok(stats) = diff.stats()
  {
    let commits = count_commits(&repo, cwd).unwrap_or(0);
    let _ = tx.send(HarnessEvent::GitStats {
      files: stats.files_changed() as u32,
      ins: stats.insertions() as u32,
      del: stats.deletions() as u32,
      commits,
    });
  }
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
  let status_path = cwd.join("specs/status.md");
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
  let content = match j.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
    Some(c) => c,
    None => return,
  };

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
          let _ = tx
            .send(HarnessEvent::Activity { kind: ActivityKind::Thinking, content: format!("[thinking] {}", summary) });
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
      let short = if cmd.chars().count() > 60 { cmd.chars().take(60).collect() } else { cmd.to_string() };
      (ActivityKind::ToolCall, short)
    }
    "Glob" | "Grep" => {
      let pattern = input.and_then(|i| i.get("pattern")).and_then(|p| p.as_str()).unwrap_or("?");
      (ActivityKind::ToolCall, format!("{} {}", tool_name.to_lowercase(), pattern))
    }
    "Task" => {
      let desc = input.and_then(|i| i.get("description")).and_then(|d| d.as_str()).unwrap_or("agent");
      (ActivityKind::ToolCall, format!("spawning {}", desc))
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
    && let Some(cost) = result.get("cost_usd").and_then(|c| c.as_f64())
  {
    let _ = tx.send(HarnessEvent::Activity { kind: ActivityKind::Text, content: format!("cost: ${:.4}", cost) });
  }
}

fn parse_claude_system(j: &Value, tx: &mpsc::UnboundedSender<HarnessEvent>) {
  if let Some(msg) = j.get("message").and_then(|m| m.as_str())
    && (msg.contains("error") || msg.contains("Error"))
  {
    let _ = tx.send(HarnessEvent::Activity { kind: ActivityKind::Text, content: format!("[error] {}", msg) });
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

  let (kind, content) = if trimmed.contains("Read") && trimmed.contains("|") {
    let path = trimmed.split("Read").last().unwrap_or("").trim();
    (ActivityKind::Reading, path.to_string())
  } else if (trimmed.contains("Write") || trimmed.contains("apply_patch")) && trimmed.contains("|") {
    let rest = if trimmed.contains("apply_patch") {
      "patch applied".to_string()
    } else {
      trimmed.split("Write").last().unwrap_or("").trim().to_string()
    };
    (ActivityKind::Writing, rest)
  } else if trimmed.contains("Edit") && trimmed.contains("|") {
    let path = trimmed.split("Edit").last().unwrap_or("").trim();
    (ActivityKind::Writing, path.to_string())
  } else if (trimmed.contains("Bash") || trimmed.contains("Shell")) && trimmed.contains("|") {
    let cmd = trimmed.split('|').next_back().unwrap_or("").trim();
    let cmd_clean = cmd.replace("Bash", "").replace("Shell", "").trim().to_string();
    (ActivityKind::ToolCall, cmd_clean)
  } else if trimmed.contains("Glob") && trimmed.contains("|") {
    let rest = trimmed.split("Glob").last().unwrap_or("").trim();
    (ActivityKind::ToolCall, format!("glob {}", rest))
  } else if trimmed.contains("Grep") && trimmed.contains("|") {
    let rest = trimmed.split("Grep").last().unwrap_or("").trim();
    (ActivityKind::ToolCall, format!("grep {}", rest))
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
    let cmd = if trimmed.contains("'") {
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
