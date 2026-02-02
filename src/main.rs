mod sandbox;
mod tui;

use std::{
  cmp::min,
  io::{self, Write},
  path::Path,
  process::Stdio,
  time::{Duration, Instant},
};

use clap::{
  Parser, Subcommand, ValueEnum,
  builder::{Styles, styling::AnsiColor},
};
use colored::Colorize;
use git2::Repository;
use sandbox::SandboxType;
use thiserror::Error;
use tokio::process::Command;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Harness {
  #[default]
  Claude,
  Opencode,
  Codex,
}

impl Harness {
  pub fn as_str(self) -> &'static str {
    match self {
      Harness::Claude => "claude",
      Harness::Opencode => "opencode",
      Harness::Codex => "codex",
    }
  }
}

// =============================================================================
// Error
// =============================================================================

#[derive(Error, Debug)]
pub enum Error {
  #[error("git: {0}")]
  Git(#[from] git2::Error),
  #[error("io: {0}")]
  Io(#[from] std::io::Error),
  #[error("docker: {0}")]
  Docker(String),
  #[error("harness: {0}")]
  Harness(String),
  #[error("uncommitted changes, commit or stash first")]
  UncommittedChanges,
  #[error("specs/prompt.md not found, run `so plan` first")]
  NoPrompt,
  #[error("Dockerfile.sandbox not found")]
  NoDockerfile,
  #[error("cannot determine home directory")]
  NoHome,
  #[error("run requires a terminal")]
  RunRequiresTerminal,
  #[error("sandbox run requires a terminal")]
  SandboxRequiresTerminal,
  #[error("bubblewrap not available on macOS, use --sandbox docker")]
  BwrapUnavailableMacos,
  #[error("bubblewrap not installed, run `sudo apt install bubblewrap`")]
  BwrapNotInstalled,
  #[error("bwrap blocked by apparmor restriction")]
  BwrapBlocked,
  #[error("docker not installed")]
  DockerNotInstalled,
  #[error("docker not running")]
  DockerNotRunning,
  #[error("cannot determine original repo")]
  OriginalRepoNotFound,
  #[error("sandbox metadata missing, start a new run")]
  SandboxMetadataMissing,
  #[error("menu requires a terminal")]
  MenuRequiresTerminal,
  #[error("failed to enable raw mode: {0}")]
  RawModeEnable(String),
  #[error("failed to enter alternate screen: {0}")]
  AlternateScreenEnter(String),
  #[error("failed to create terminal: {0}")]
  TerminalCreate(String),
  #[error("bwrap spawn failed: {0}")]
  BwrapSpawn(String),
  #[error("bwrap exited with code {0}")]
  BwrapExit(i32),
  #[error("interrupted")]
  Interrupted,
  #[error("{0}")]
  Other(String),
}

// =============================================================================
// So
// =============================================================================

const STYLES: Styles = Styles::plain().header(AnsiColor::White.on_default().underline());

#[derive(Parser)]
#[command(
  name = "so",
  about = "Sandbox orchestrator for your agents",
  version,
  styles = STYLES,
  after_help = "Run 'so <command> --help' for more information on a specific command."
)]
struct Cli {
  /// Agent harness to use
  #[arg(short = 'H', long, global = true, default_value = "claude", value_enum)]
  harness: Harness,

  /// Number of iterations
  #[arg(short = 'i', long, global = true, default_value = "10")]
  iterations: u32,

  /// Model override
  #[arg(short, long, global = true)]
  model: Option<String>,

  /// Effort level for reasoning
  #[arg(short, long, global = true)]
  effort: Option<String>,

  /// Sandbox type
  #[arg(short, long, global = true, env = "SANDBOX", default_value = "docker", value_enum)]
  sandbox: SandboxType,

  #[command(subcommand)]
  command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
  /// Unattended, continuous execution
  Run,
  /// Attended, human-in-the-loop
  Step,
  /// Generate implementation plan and specs
  Plan,
  /// Fix code smells
  Clean,
  /// Remove duplicate code
  Dup,
  /// Guided learning
  Learn,
  /// Manage existing sandboxes
  Menu,
}

// =============================================================================
// Modes
// =============================================================================

#[derive(Clone, Copy)]
enum TaskMode {
  Code,
  Plan,
  Learn,
}

#[derive(Clone, Copy, PartialEq)]
pub enum RunMode {
  Step,
  Run,
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() {
  if let Err(e) = run().await {
    eprintln!("{} {}", "error:".red().bold(), e);
    std::process::exit(1);
  }
}

async fn run() -> Result<(), Error> {
  let result = run_inner().await;

  if matches!(result, Err(Error::Interrupted)) {
    std::process::exit(130);
  }

  result
}

async fn run_inner() -> Result<(), Error> {
  let cli = Cli::parse();
  let h = cli.harness;
  let model = cli.model;
  let effort = cli.effort;
  let st = cli.sandbox;

  // set env vars for harness runners
  if let Some(m) = &model {
    unsafe { std::env::set_var("MODEL", m) };
  }
  if let Some(e) = &effort {
    unsafe { std::env::set_var("EFFORT", e) };
  }

  match cli.command.unwrap_or(Cmd::Menu) {
    Cmd::Run => {
      if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(Error::RunRequiresTerminal);
      }
      do_run(h, cli.iterations, st).await
    }
    Cmd::Step => do_step(h, cli.iterations).await,
    Cmd::Plan => do_plan(h).await,
    Cmd::Clean => do_clean(h, cli.iterations, st).await,
    Cmd::Dup => do_dup(h, cli.iterations, st).await,
    Cmd::Learn => do_learn(h).await,
    Cmd::Menu => do_menu().await,
  }
}

// =============================================================================
// Validation
// =============================================================================

fn validate_sandbox(st: SandboxType) -> Result<(), Error> {
  if st == SandboxType::Bwrap {
    if cfg!(target_os = "macos") {
      return Err(Error::BwrapUnavailableMacos);
    }
    if std::process::Command::new("bwrap").arg("--version").output().is_err() {
      return Err(Error::BwrapNotInstalled);
    }
    if needs_bwrap_apparmor() {
      return Err(Error::BwrapBlocked);
    }
  } else {
    if std::process::Command::new("docker").arg("--version").output().is_err() {
      return Err(Error::DockerNotInstalled);
    }
    if std::process::Command::new("docker").args(["info"]).output().map(|o| !o.status.success()).unwrap_or(true) {
      return Err(Error::DockerNotRunning);
    }
  }
  Ok(())
}

// check for apparmor restriction on bwrap (ubuntu 24.04+)
fn needs_bwrap_apparmor() -> bool {
  if !cfg!(target_os = "linux") {
    return false;
  }
  let restricted = std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
    .map(|v| v.trim() == "1")
    .unwrap_or(false);
  if !restricted {
    return false;
  }
  let profile_loaded = std::fs::read_to_string("/sys/kernel/security/apparmor/profiles")
    .map(|profiles| profiles.lines().any(|l| l.starts_with("bwrap ")))
    .unwrap_or(false);
  !profile_loaded
}

// =============================================================================
// Commands
// =============================================================================

async fn do_step(harness: Harness, iterations: u32) -> Result<(), Error> {
  let cwd = std::env::current_dir()?;
  let unattended = std::env::var("SO_UNATTENDED").is_ok();
  let mode = if unattended { RunMode::Run } else { RunMode::Step };
  let use_tui = unattended && std::env::var("SO_TUI").is_ok() && std::io::IsTerminal::is_terminal(&std::io::stdin());
  let start_head = sandbox::git_head(&cwd).ok();
  let start = Instant::now();

  if !cwd.join("specs/prompt.md").exists() {
    return Err(Error::NoPrompt);
  }
  if !cwd.join("specs/status.md").exists() {
    std::fs::write(cwd.join("specs/status.md"), "Status: pending\n")?;
  }

  let effective_max = effective_max(&cwd, iterations);
  if use_tui {
    let name = cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "sandbox".into());
    tui::run(name, harness, mode, effective_max, cwd.clone()).await?;
  } else {
    run_loop(mode, harness, effective_max, &cwd).await?;
  }

  if !unattended && let Some(base) = start_head {
    println!();
    print_summary(&sandbox::git_stat(&cwd, &base), Some(&fmt_time(start.elapsed())), None);
  }
  Ok(())
}

async fn do_run(harness: Harness, iterations: u32, st: SandboxType) -> Result<(), Error> {
  validate_sandbox(st)?;
  let cwd = std::env::current_dir()?;

  if sandbox::git_dirty(&cwd)? {
    return Err(Error::UncommittedChanges);
  }
  if !cwd.join("specs/prompt.md").exists() {
    return Err(Error::NoPrompt);
  }

  let sb = sandbox::Sandbox::new(&cwd, sandbox::Mode::Run, None)?;
  set_mdata(&sb.path, harness, st, &sb.task_id)?;

  let start = Instant::now();
  let effective_max = effective_max(&sb.path, iterations);

  finalize_sandbox(&sb, harness, effective_max, &cwd, start, st).await
}

async fn do_clean(harness: Harness, iterations: u32, st: SandboxType) -> Result<(), Error> {
  let prompt = r#"Ignore any existing specs/ files.

Review codebase super carefully with fresh eyes. Look for:
- Bugs, errors, or incorrect logic
- Code smells: unused exports, dead code, inconsistent patterns
- Confusing or unclear code

Fix one issue per iteration.
Commit: "clean: <description>"
When nothing left, set specs/status.md to "Status: done""#;
  run_with_prompt(harness, iterations, prompt, sandbox::Mode::Clean, st).await
}

async fn do_dup(harness: Harness, iterations: u32, st: SandboxType) -> Result<(), Error> {
  let prompt = r#"Ignore any existing specs/ files.
Run jscpd.
Pick one duplicate from the report.
Refactor into shared utility.
Commit: "dry: <utility name>"
When nothing left, set specs/status.md to "Status: done""#;
  run_with_prompt(harness, iterations, prompt, sandbox::Mode::Dup, st).await
}

async fn run_with_prompt(
  harness: Harness,
  iterations: u32,
  prompt: &str,
  mode: sandbox::Mode,
  st: SandboxType,
) -> Result<(), Error> {
  if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
    return Err(Error::SandboxRequiresTerminal);
  }
  validate_sandbox(st)?;
  let cwd = std::env::current_dir()?;

  if sandbox::git_dirty(&cwd)? {
    return Err(Error::UncommittedChanges);
  }

  let sb = sandbox::Sandbox::new(&cwd, mode, Some(prompt))?;
  set_mdata(&sb.path, harness, st, &sb.task_id)?;

  let start = Instant::now();
  finalize_sandbox(&sb, harness, iterations, &cwd, start, st).await
}

async fn do_plan(harness: Harness) -> Result<(), Error> {
  let cwd = std::env::current_dir()?;
  let specs = cwd.join("specs");
  std::fs::create_dir_all(&specs)?;

  write_if_missing(&specs.join("readme.md"), include_str!("templates/readme.md"))?;
  write_if_missing(&specs.join("implementation-plan.md"), include_str!("templates/implementation-plan.md"))?;
  write_if_missing(&specs.join("prompt.md"), include_str!("templates/prompt.md"))?;
  write_if_missing(&specs.join("status.md"), "Status: pending\n")?;
  write_if_missing(&cwd.join("Dockerfile.sandbox"), include_str!("templates/Dockerfile.sandbox"))?;

  let prompt = r#"PLANNING ONLY. DO NOT IMPLEMENT.

1. Ask what I want to build
2. Search codebase for existing patterns first
3. Interview with detailed questions (1-4 at a time)
4. Generate: specs/<feature>.md, update implementation-plan.md and readme.md
5. Keep tasks atomic. Add 10+ keywords to readme.md for search.
6. When done: "Planning complete. Run so run to implement."

Never write code, only specs."#;

  println!("{}", format!("▶ Planning [{}]", harness.as_str()).cyan().bold());
  run_harness(harness, prompt, RunMode::Step, TaskMode::Plan, &cwd).await
}

async fn do_learn(harness: Harness) -> Result<(), Error> {
  let cwd = std::env::current_dir()?;

  let prompt = r#"Teaching mode: help me learn, don't solve for me.

Rules:
- Explain concepts, errors, and "why", not just "how"
- Ask clarifying questions about what I've tried
- Point to relevant lectures, docs, or codebase patterns
- Debug by asking guiding questions, not providing fixes
- Suggest approaches, don't implement them
- Code examples: max 2-5 lines, single concept, different names, explain each line
- Encourage adapting examples, not copying
- Never write full functions, TODO completions, assignment solutions, quiz/exam answers, or large refactors

Approach:
- First principles, build understanding from fundamentals
- Go slow, one step at a time
- Review my code and point out improvements
- Simple explanations, no unnecessary complexity

When in doubt: explain more, code less.

Start by asking what I want to learn."#;

  println!("{}", format!("▶ Learn [{}]", harness.as_str()).cyan().bold());
  run_harness(harness, prompt, RunMode::Step, TaskMode::Learn, &cwd).await
}

async fn do_menu() -> Result<(), Error> {
  use tui::menu::{MenuAction, run as run_menu_tui};

  loop {
    match run_menu_tui().await? {
      MenuAction::Quit => return Ok(()),
      MenuAction::Merged => {
        println!("{}", "Merged successfully. Files staged.".green());
        return Ok(());
      }
      MenuAction::Run { sandbox_path, iterations } => {
        continue_sandbox(&sandbox_path, iterations).await?;
      }
    }
  }
}

// =============================================================================
// Run loop
// =============================================================================

async fn run_loop(mode: RunMode, harness: Harness, max: u32, cwd: &Path) -> Result<(), Error> {
  let prompt_path = cwd.join("specs/prompt.md");
  let status_path = cwd.join("specs/status.md");
  let unattended = std::env::var("SO_UNATTENDED").is_ok();
  let effective_max = effective_max(cwd, max);

  for i in 1..=effective_max {
    if check_status(cwd)? {
      if let Ok(c) = std::fs::read_to_string(&status_path) {
        let status = c.trim();
        if !status.is_empty() {
          println!("{}", status);
        }
        if status.to_lowercase().contains("status: done") {
          println!("All tasks complete.");
        }
      }
      break;
    }

    print_header(harness, i, effective_max);
    let iter_start = Instant::now();

    let base = std::fs::read_to_string(&prompt_path)?;
    let prompt = build_prompt(&base, mode, cwd, i, effective_max);

    run_harness(harness, &prompt, mode, TaskMode::Code, cwd).await?;

    if unattended {
      enforce_commit(harness, cwd).await;
    }

    print_header_time(harness, i, effective_max, &fmt_time(iter_start.elapsed()));

    if i < effective_max {
      if mode == RunMode::Step {
        print!("Continue? [y/n] ");
        io::stdout().flush()?;
        let yn = read_line_trim()?;
        if !yn.to_lowercase().starts_with('y') {
          println!("\nStopped.");
          break;
        }
        println!();
      } else {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
      }
    }
  }
  Ok(())
}

fn task_count(cwd: &Path) -> Option<u32> {
  let plan = cwd.join("specs/implementation-plan.md");
  let content = std::fs::read_to_string(plan).ok()?;
  let count = content.lines().filter(|l| l.trim_start().starts_with("- [ ]")).count();
  if count == 0 { None } else { Some(count as u32) }
}

pub(crate) fn build_prompt(base: &str, mode: RunMode, cwd: &Path, iter: u32, max_iter: u32) -> String {
  match mode {
    RunMode::Step => {
      format!("{}\n\nDo not commit, human will handle that.\n\n---\nIteration {}/{}.", base, iter, max_iter)
    }
    RunMode::Run => {
      let commits = sandbox::git_recent(cwd, sandbox::BASE_TAG, 10);
      format!("{}\n\nRecent commits:\n{}\n\n---\nIteration {}/{}.", base, commits, iter, max_iter)
    }
  }
}

// =============================================================================
// Harness
// =============================================================================

async fn run_harness(harness: Harness, prompt: &str, mode: RunMode, task: TaskMode, cwd: &Path) -> Result<(), Error> {
  match harness {
    Harness::Claude => run_claude(prompt, mode, cwd).await,
    Harness::Opencode => run_opencode(prompt, mode, task, cwd).await,
    Harness::Codex => run_codex(prompt, mode, task, cwd).await,
  }
}

async fn run_claude(prompt: &str, mode: RunMode, cwd: &Path) -> Result<(), Error> {
  let mut cmd = Command::new("claude");
  if let Ok(m) = std::env::var("MODEL") {
    cmd.arg("--model").arg(m);
  }

  if mode == RunMode::Step {
    cmd.stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).current_dir(cwd);
    let mut child = cmd.spawn().map_err(|e| harness_err("claude", e))?;
    if let Some(mut stdin) = child.stdin.take() {
      use tokio::io::AsyncWriteExt;
      stdin.write_all(prompt.as_bytes()).await?;
    }
    check_status_code(child.wait().await?, "claude")
  } else {
    cmd.args(["--dangerously-skip-permissions", "--setting-sources", "project,local"]);
    cmd.arg("--settings").arg(r#"{"outputStyle":"Explanatory","alwaysThinkingEnabled":true}"#);
    cmd.args(["-p", "--verbose", "--output-format", "stream-json"]).arg(prompt);
    cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());

    // parse streaming JSON output
    let mut child = cmd.spawn().map_err(|e| harness_err("claude", e))?;
    if let Some(stdout) = child.stdout.take() {
      use tokio::io::{AsyncBufReadExt, BufReader};
      let mut lines = BufReader::new(stdout).lines();
      while let Some(line) = lines.next_line().await? {
        if !line.starts_with('{') {
          continue;
        }
        let j = match serde_json::from_str::<serde_json::Value>(&line) {
          Ok(v) => v,
          Err(_) => continue,
        };
        if j.get("type").and_then(|t| t.as_str()) != Some("assistant") {
          continue;
        }
        let content = match j.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
          Some(c) => c,
          None => continue,
        };
        for item in content {
          if item.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
          }
          let text = match item.get("text").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
          };
          println!("{}\n", text);
        }
      }
    }
    check_status_code(child.wait().await?, "claude")
  }
}

async fn run_opencode(prompt: &str, mode: RunMode, task: TaskMode, cwd: &Path) -> Result<(), Error> {
  let (def_model, def_effort) = match task {
    TaskMode::Plan => ("openai/gpt-5.2", "high"),
    TaskMode::Learn => ("openai/gpt-5.2", "medium"),
    TaskMode::Code => ("openai/gpt-5.2-codex", "medium"),
  };
  let model = std::env::var("MODEL").unwrap_or_else(|_| def_model.into());
  let effort = std::env::var("EFFORT").unwrap_or_else(|_| def_effort.into());

  let mut cmd = Command::new("opencode");
  if mode == RunMode::Step {
    cmd.args(["--prompt", prompt, "-m", &model]);
  } else {
    cmd.env("OPENCODE_PERMISSION", r#"{"*":"allow"}"#);
    cmd.args(["run", "--log-level", "ERROR", "-m", &model, "--variant", &effort, prompt]);
  }
  cmd.current_dir(cwd).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
  check_status_code(cmd.status().await.map_err(|e| harness_err("opencode", e))?, "opencode")
}

async fn run_codex(prompt: &str, mode: RunMode, task: TaskMode, cwd: &Path) -> Result<(), Error> {
  let (def_model, def_effort) = match task {
    TaskMode::Plan => ("gpt-5.2", "high"),
    TaskMode::Learn => ("gpt-5.2", "medium"),
    TaskMode::Code => ("gpt-5.2-codex", "medium"),
  };
  let model = std::env::var("MODEL").unwrap_or_else(|_| def_model.into());
  let effort = std::env::var("EFFORT").unwrap_or_else(|_| def_effort.into());

  let mut cmd = Command::new("codex");
  let cfg = format!("model_reasoning_effort={}", effort);
  if mode == RunMode::Step {
    cmd.args([prompt, "--model", &model, "--full-auto", "--config", &cfg]);
  } else {
    cmd.args(["exec", "--full-auto", prompt, "--model", &model, "--config", &cfg]);
  }
  cmd.current_dir(cwd).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
  check_status_code(cmd.status().await.map_err(|e| harness_err("codex", e))?, "codex")
}

fn harness_err(name: &str, e: std::io::Error) -> Error {
  if e.kind() == std::io::ErrorKind::NotFound {
    Error::Harness(format!("`{}` not found", name))
  } else {
    Error::Harness(format!("failed to run `{}`: {}", name, e))
  }
}

fn check_status_code(status: std::process::ExitStatus, name: &str) -> Result<(), Error> {
  if status.success() { Ok(()) } else { Err(Error::Harness(format!("`{}` failed", name))) }
}

// =============================================================================
// Helpers
// =============================================================================

async fn enforce_commit(harness: Harness, cwd: &Path) {
  let msg = "Commit now. Message format:\n- What: <what was done>\n- Why: <reasoning>\n- Alternatives: <what else was considered>";
  for i in 1..=3 {
    if !sandbox::git_dirty(cwd).unwrap_or(false) {
      return;
    }
    eprintln!("\n{} uncommitted changes ({}/3)\n", "warning:".yellow().bold(), i);
    if run_harness(harness, msg, RunMode::Run, TaskMode::Code, cwd).await.is_err() {
      break;
    }
  }
  if sandbox::git_dirty(cwd).unwrap_or(false) {
    eprintln!("\n{} failed to commit after 3 attempts\n", "error:".red().bold());
  }
}

fn check_status(cwd: &Path) -> Result<bool, Error> {
  let p = cwd.join("specs/status.md");
  if !p.exists() {
    return Ok(false);
  }
  let c = std::fs::read_to_string(&p)?.to_lowercase();
  Ok(c.contains("status: done"))
}

fn read_status(cwd: &Path) -> Option<String> {
  let p = cwd.join("specs/status.md");
  std::fs::read_to_string(&p).ok().map(|s| s.trim().to_string())
}

fn is_done(status: &str) -> bool {
  let s = status.to_lowercase();
  s.contains("status: done")
}

fn set_status_pending(cwd: &Path) -> Result<(), Error> {
  std::fs::write(cwd.join("specs/status.md"), "Status: pending\n")?;
  Ok(())
}

fn write_if_missing(path: &Path, content: &str) -> Result<(), Error> {
  if !path.exists() {
    std::fs::write(path, content)?;
  }
  Ok(())
}

fn effective_max(cwd: &Path, max: u32) -> u32 {
  task_count(cwd).map(|n| min(max, n)).unwrap_or(max)
}

async fn finalize_sandbox(
  sb: &sandbox::Sandbox,
  harness: Harness,
  iterations: u32,
  _cwd: &Path,
  _start: Instant,
  st: SandboxType,
) -> Result<(), Error> {
  match run_sandbox_iterations(sb, harness, iterations, st).await {
    Ok(RunOutcome::Completed) => {
      use tui::menu::{MenuAction, run as run_menu_tui};
      loop {
        match run_menu_tui().await? {
          MenuAction::Quit | MenuAction::Merged => break,
          MenuAction::Run { sandbox_path, iterations: iter_count } => {
            continue_sandbox(&sandbox_path, iter_count).await?;
          }
        }
      }
      Ok(())
    }
    Ok(RunOutcome::Interrupted) => {
      println!("{}", format!("Interrupted. Sandbox kept at: {}", sb.path.display()).yellow());
      Ok(())
    }
    Err(e) => {
      let _ = std::fs::remove_dir_all(&sb.path);
      Err(e)
    }
  }
}

fn read_line_trim() -> io::Result<String> {
  let mut buf = String::new();
  io::stdin().read_line(&mut buf)?;
  Ok(buf.trim().to_string())
}

enum RunOutcome {
  Completed,
  Interrupted,
}

async fn run_sandbox_iterations(
  sb: &sandbox::Sandbox,
  harness: Harness,
  iterations: u32,
  st: SandboxType,
) -> Result<RunOutcome, Error> {
  match sandbox::run(sb, harness.as_str(), iterations, st).await {
    Ok(()) => Ok(RunOutcome::Completed),
    Err(Error::Interrupted) => Ok(RunOutcome::Interrupted),
    Err(e) => Err(e),
  }
}

async fn continue_sandbox(sandbox_path: &Path, iterations: u32) -> Result<(), Error> {
  let mdata = read_mdata(sandbox_path)?;
  validate_sandbox(mdata.sandbox)?;

  let original = Repository::open(sandbox_path)
    .ok()
    .and_then(|r| r.config().ok())
    .and_then(|c| c.get_string("so.original").ok())
    .map(|s| std::path::PathBuf::from(s.trim()))
    .ok_or(Error::OriginalRepoNotFound)?;

  let sb = sandbox::Sandbox { path: sandbox_path.to_path_buf(), original, task_id: mdata.task_id.clone() };

  // reset status if done
  if let Some(status) = read_status(sandbox_path)
    && is_done(&status)
  {
    set_status_pending(sandbox_path)?;
  }

  match run_sandbox_iterations(&sb, mdata.harness, iterations, mdata.sandbox).await {
    Ok(RunOutcome::Completed) => Ok(()),
    Ok(RunOutcome::Interrupted) => {
      println!("{}", format!("Interrupted. Sandbox kept at: {}", sb.path.display()).yellow());
      Ok(())
    }
    Err(e) => Err(e),
  }
}

struct Mdata {
  harness: Harness,
  sandbox: SandboxType,
  task_id: String,
}

fn set_mdata(path: &Path, harness: Harness, sandbox: SandboxType, task_id: &str) -> Result<(), Error> {
  let repo = Repository::open(path)?;
  let mut cfg = repo.config()?;
  cfg.set_str("so.mdata.harness", harness.as_str())?;
  cfg.set_str("so.mdata.sandbox", sandbox.as_str())?;
  cfg.set_str("so.mdata.task-id", task_id)?;
  Ok(())
}

fn read_mdata(path: &Path) -> Result<Mdata, Error> {
  let repo = Repository::open(path)?;
  let cfg = repo.config()?;
  let harness = cfg.get_string("so.mdata.harness").ok().and_then(|v| parse_harness(&v));
  let sandbox = cfg.get_string("so.mdata.sandbox").ok().and_then(|v| parse_sandbox_type(&v));
  let task_id = cfg.get_string("so.mdata.task-id").ok();
  match (harness, sandbox, task_id) {
    (Some(h), Some(s), Some(t)) => Ok(Mdata { harness: h, sandbox: s, task_id: t }),
    _ => Err(Error::SandboxMetadataMissing),
  }
}

fn parse_harness(s: &str) -> Option<Harness> {
  match s.to_lowercase().as_str() {
    "claude" => Some(Harness::Claude),
    "opencode" => Some(Harness::Opencode),
    "codex" => Some(Harness::Codex),
    _ => None,
  }
}

fn parse_sandbox_type(s: &str) -> Option<SandboxType> {
  match s.to_lowercase().as_str() {
    "docker" => Some(SandboxType::Docker),
    "bwrap" => Some(SandboxType::Bwrap),
    _ => None,
  }
}

// =============================================================================
// Formatting
// =============================================================================

fn fmt_time(d: Duration) -> String {
  let s = d.as_secs();
  if s >= 60 { format!("{}m {:02}s", s / 60, s % 60) } else { format!("{}s", s) }
}

fn print_header(harness: Harness, cur: u32, total: u32) {
  println!("{}", format!("▶ [{}] {}/{}", harness.as_str(), cur, total).cyan().bold());
}

fn print_header_time(harness: Harness, cur: u32, total: u32, time: &str) {
  println!("{}", format!("▶ [{}] {}/{} | {}", harness.as_str(), cur, total, time).cyan().bold());
}

fn print_summary(stats: &str, time: Option<&str>, path: Option<&str>) {
  let line = "━".repeat(48).white().bold();
  println!("{}", line);
  if let Some(p) = path {
    println!(" {}", p.yellow());
  }
  if let Some(t) = time {
    println!(" {} | {}", stats, t);
  } else {
    println!(" {}", stats);
  }
  println!("{}", line);
}
