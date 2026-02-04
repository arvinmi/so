mod config;
mod harness;
mod sandbox;
mod tui;

use std::{
  cmp::min,
  io::{self, Write},
  path::Path,
  sync::Arc,
  time::{Duration, Instant},
};

use clap::{
  Parser, Subcommand, ValueEnum,
  builder::{Styles, styling::AnsiColor},
};
use colored::Colorize;
use sandbox::{GpuStatus, SandboxType};
use thiserror::Error;

use crate::harness::{build_prompt, run_harness};

// =============================================================================
// Constants
// =============================================================================

const STATUS_PENDING: &str = "status: pending\n";
const STATUS_DONE: &str = "status: done";

const SPECS_DIR: &str = "specs";
const FILE_PROMPT: &str = "prompt.md";
const FILE_STATUS: &str = "status.md";
const FILE_PLAN: &str = "implementation-plan.md";

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
  #[error("no active sandboxes. Run `so run` to start a new sandbox")]
  NoActiveSandboxes,
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
  #[arg(short = 'H', long, global = true, value_enum)]
  harness: Option<Harness>,

  /// Number of iterations
  #[arg(short = 'i', long, global = true)]
  iterations: Option<u32>,

  /// Model override
  #[arg(short, long, global = true)]
  model: Option<String>,

  /// Effort level for reasoning
  #[arg(short, long, global = true)]
  effort: Option<String>,

  /// Sandbox type
  #[arg(short, long, global = true, env = "SANDBOX", value_enum)]
  sandbox: Option<SandboxType>,

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
pub(crate) enum TaskMode {
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
  sandbox::cleanup_stale_creds();
  config::prune_stale();

  let cli = Cli::parse();
  let cfg = config::load();

  // resolve CLI arg > config.toml > default
  let h = cli.harness.or_else(|| cfg.harness.as_deref().and_then(parse_harness)).unwrap_or(Harness::Claude);
  let iterations = cli.iterations.or(cfg.iterations).unwrap_or(10);
  let st = cli.sandbox.or_else(|| cfg.sandbox.as_deref().and_then(parse_sandbox_type)).unwrap_or(SandboxType::Docker);
  let model = cli.model.or(cfg.model);
  let effort = cli.effort.or(cfg.effort);

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
      do_run(h, iterations, st).await
    }
    Cmd::Step => do_step(h, iterations).await,
    Cmd::Plan => do_plan(h).await,
    Cmd::Clean => do_clean(h, iterations, st).await,
    Cmd::Dup => do_dup(h, iterations, st).await,
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
  let start_head = sandbox::git_head(&cwd).ok();
  let start = Instant::now();

  let specs_dir = cwd.join(SPECS_DIR);
  let prompt_path = specs_dir.join(FILE_PROMPT);
  let status_path = specs_dir.join(FILE_STATUS);

  if !prompt_path.exists() {
    return Err(Error::NoPrompt);
  }
  write_if_missing(&status_path, STATUS_PENDING)?;

  let effective_max = effective_max(&cwd, iterations);

  for i in 1..=effective_max {
    if let Some(status) = read_status(&cwd) {
      if !status.is_empty() {
        println!("{status}");
      }
      if is_done(&status) {
        println!("All tasks complete.");
      }
      break;
    }

    print_header(harness, i, effective_max);
    let iter_start = Instant::now();

    let base = std::fs::read_to_string(&prompt_path)?;
    let prompt = build_prompt(&base, RunMode::Step, &cwd, i, effective_max);

    run_harness(harness, &prompt, TaskMode::Code, &cwd).await?;

    print_header_time(harness, i, effective_max, &fmt_time(iter_start.elapsed()));

    if i < effective_max {
      if !confirm("Continue?")? {
        println!("\nStopped.");
        break;
      }
      println!();
    }
  }

  if let Some(base) = start_head {
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
  if !cwd.join(SPECS_DIR).join(FILE_PROMPT).exists() {
    return Err(Error::NoPrompt);
  }

  let sb = sandbox::Sandbox::new(&cwd, sandbox::Mode::Run, None)?;
  write_sandbox_meta(&sb, harness, st);

  let effective_max = effective_max(&sb.path, iterations);
  finalize_sandbox(&sb, harness, effective_max, st).await
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
  write_sandbox_meta(&sb, harness, st);

  finalize_sandbox(&sb, harness, iterations, st).await
}

async fn do_plan(harness: Harness) -> Result<(), Error> {
  let cwd = std::env::current_dir()?;
  let specs = cwd.join(SPECS_DIR);
  std::fs::create_dir_all(&specs)?;

  write_if_missing(&specs.join("readme.md"), include_str!("templates/readme.md"))?;
  write_if_missing(&specs.join(FILE_PLAN), include_str!("templates/implementation-plan.md"))?;
  write_if_missing(&specs.join(FILE_PROMPT), include_str!("templates/prompt.md"))?;
  write_if_missing(&specs.join(FILE_STATUS), STATUS_PENDING)?;
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
  run_harness(harness, prompt, TaskMode::Plan, &cwd).await
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
  run_harness(harness, prompt, TaskMode::Learn, &cwd).await
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

fn task_count(cwd: &Path) -> Option<u32> {
  let plan = cwd.join(SPECS_DIR).join(FILE_PLAN);
  let content = std::fs::read_to_string(plan).ok()?;
  let count = content.lines().filter(|l| l.trim_start().starts_with("- [ ]")).count();
  if count == 0 { None } else { Some(count as u32) }
}

// =============================================================================
// Helpers
// =============================================================================

fn read_status(cwd: &Path) -> Option<String> {
  let p = cwd.join(SPECS_DIR).join(FILE_STATUS);
  std::fs::read_to_string(&p).ok().map(|s| s.trim().to_string())
}

fn is_done(status: &str) -> bool {
  status.to_lowercase().contains(STATUS_DONE)
}

fn set_status_pending(cwd: &Path) -> Result<(), Error> {
  std::fs::write(cwd.join(SPECS_DIR).join(FILE_STATUS), STATUS_PENDING)?;
  Ok(())
}

fn write_if_missing(path: &Path, content: &str) -> Result<(), Error> {
  if !path.exists() {
    std::fs::write(path, content)?;
  }
  Ok(())
}

fn effective_max(cwd: &Path, max: u32) -> u32 {
  task_count(cwd).map_or(max, |n| min(max, n))
}

async fn finalize_sandbox(
  sb: &sandbox::Sandbox,
  harness: Harness,
  iterations: u32,
  st: SandboxType,
) -> Result<(), Error> {
  warn_gpu_if_missing(st);

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
    Ok(RunOutcome::Interrupted) => Ok(()),
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

fn confirm(msg: &str) -> io::Result<bool> {
  print!("{msg} [y/n] ");
  io::stdout().flush()?;
  Ok(read_line_trim()?.to_lowercase().starts_with('y'))
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
  let outcome = |result: Result<(), Error>| match result {
    Ok(()) => Ok(RunOutcome::Completed),
    Err(Error::Interrupted) => Ok(RunOutcome::Interrupted),
    Err(e) => Err(e),
  };
  match st {
    SandboxType::Docker => {
      let (container, build_messages) = sandbox::start_docker(sb).await?;
      let container = Arc::new(container);
      let name = sandbox_name(&sb.original);
      let result = tui::run(
        name,
        harness,
        RunMode::Run,
        iterations,
        sb.path.clone(),
        Some(tui::TuiBackend::Docker(container.clone())),
        build_messages,
      )
      .await;
      container.stop().await;
      outcome(result)
    }
    SandboxType::Bwrap => {
      let bwrap = Arc::new(sandbox::BwrapContext::new(sb)?);
      let name = sandbox_name(&sb.original);
      outcome(
        tui::run(
          name,
          harness,
          RunMode::Run,
          iterations,
          sb.path.clone(),
          Some(tui::TuiBackend::Bwrap(bwrap)),
          Vec::new(),
        )
        .await,
      )
    }
  }
}

async fn continue_sandbox(sandbox_path: &Path, iterations: u32) -> Result<(), Error> {
  let sb_name = sandbox_name(sandbox_path);
  let meta = config::read_meta(&sb_name).ok_or(Error::SandboxMetadataMissing)?;
  let harness = parse_harness(&meta.harness).ok_or(Error::SandboxMetadataMissing)?;
  let sandbox_type = parse_sandbox_type(&meta.sandbox).ok_or(Error::SandboxMetadataMissing)?;
  validate_sandbox(sandbox_type)?;

  let original = std::path::PathBuf::from(&meta.original);
  let sb = sandbox::Sandbox { path: sandbox_path.to_path_buf(), original, task_id: meta.task_id };

  // reset status if done
  if let Some(status) = read_status(sandbox_path)
    && is_done(&status)
  {
    set_status_pending(sandbox_path)?;
  }

  // warn if gpu driver present but docker toolkit missing (linux + docker only)
  warn_gpu_if_missing(sandbox_type);

  let iterations = effective_max(sandbox_path, iterations);
  match run_sandbox_iterations(&sb, harness, iterations, sandbox_type).await {
    Ok(RunOutcome::Completed | RunOutcome::Interrupted) => Ok(()),
    Err(e) => Err(e),
  }
}

fn warn_gpu_if_missing(st: SandboxType) {
  if cfg!(target_os = "linux") && st == SandboxType::Docker && sandbox::check_gpu() == GpuStatus::MissingToolkit {
    eprintln!("{} docker gpu support not configured, running without gpu", "warning:".yellow().bold());
  }
}

fn sandbox_name(p: &Path) -> String {
  p.file_name().map_or_else(|| "sandbox".into(), |n| n.to_string_lossy().to_string())
}

fn write_sandbox_meta(sb: &sandbox::Sandbox, harness: Harness, sandbox_type: SandboxType) {
  let sb_name = sandbox_name(&sb.path);
  config::write_meta(
    &sb_name,
    &config::SandboxMeta {
      original: sb.original.display().to_string(),
      harness: harness.as_str().into(),
      sandbox: sandbox_type.as_str().into(),
      task_id: sb.task_id.clone(),
    },
  );
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
  if s >= 60 { format!("{}m {:02}s", s / 60, s % 60) } else { format!("{s}s") }
}

fn print_header(harness: Harness, cur: u32, total: u32) {
  println!("{}", format!("▶ [{}] {}/{}", harness.as_str(), cur, total).cyan().bold());
}

fn print_header_time(harness: Harness, cur: u32, total: u32, time: &str) {
  println!("{}", format!("▶ [{}] {}/{} | {}", harness.as_str(), cur, total, time).cyan().bold());
}

fn print_summary(stats: &str, time: Option<&str>, path: Option<&str>) {
  let line = "━".repeat(48).white().bold();
  println!("{line}");
  if let Some(p) = path {
    println!(" {}", p.yellow());
  }
  if let Some(t) = time {
    println!(" {stats} | {t}");
  } else {
    println!(" {stats}");
  }
  println!("{line}");
}
