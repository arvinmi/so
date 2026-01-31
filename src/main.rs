mod sandbox;

use std::{
  cmp::min,
  collections::HashMap,
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
use sandbox::SandboxType;
use thiserror::Error;
use tokio::process::Command;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum Harness {
  #[default]
  Claude,
  Opencode,
  Codex,
}

impl Harness {
  fn as_str(self) -> &'static str {
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
  #[arg(short = 'n', long, global = true, default_value = "10")]
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
enum RunMode {
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
    Cmd::Run => do_run(h, cli.iterations, st).await,
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
      return Err(Error::Other("bubblewrap not available on macOS, use --sandbox docker".into()));
    }
    if std::process::Command::new("bwrap").arg("--version").output().is_err() {
      return Err(Error::Other("bubblewrap not installed, run `sudo apt install bubblewrap`".into()));
    }
  } else {
    if std::process::Command::new("docker").arg("--version").output().is_err() {
      return Err(Error::Other("docker not installed".into()));
    }
    if std::process::Command::new("docker").args(["info"]).output().map(|o| !o.status.success()).unwrap_or(true) {
      return Err(Error::Other("docker not running".into()));
    }
  }
  Ok(())
}

// =============================================================================
// Commands
// =============================================================================

async fn do_step(harness: Harness, iterations: u32) -> Result<(), Error> {
  let cwd = std::env::current_dir()?;
  let unattended = std::env::var("SO_UNATTENDED").is_ok();
  let mode = if unattended { RunMode::Run } else { RunMode::Step };

  let start_head = sandbox::git_head(&cwd).ok();
  let start = Instant::now();

  if !cwd.join("specs/prompt.md").exists() {
    return Err(Error::NoPrompt);
  }
  if !cwd.join("specs/status.md").exists() {
    std::fs::write(cwd.join("specs/status.md"), "Status: pending\n")?;
  }

  run_loop(mode, harness, iterations, &cwd).await?;

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

  print_info(&format!("Creating sandbox (run) [{}]", st.as_str()));
  let sb = sandbox::Sandbox::new(&cwd, sandbox::Mode::Run, None)?;
  println!("  {}", sb.path.display().to_string().dimmed());

  let start = Instant::now();
  let effective_max = effective_max(&sb.path, iterations);
  print_sandbox_start(harness, effective_max, st, &sb.task_id);
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
  validate_sandbox(st)?;
  let cwd = std::env::current_dir()?;

  if sandbox::git_dirty(&cwd)? {
    return Err(Error::UncommittedChanges);
  }

  let mode_name = match mode {
    sandbox::Mode::Run => "run",
    sandbox::Mode::Clean => "clean",
    sandbox::Mode::Dup => "dup",
  };
  print_info(&format!("Creating sandbox ({}) [{}]", mode_name, st.as_str()));
  let sb = sandbox::Sandbox::new(&cwd, mode, Some(prompt))?;
  println!("  {}", sb.path.display().to_string().dimmed());

  let start = Instant::now();
  print_sandbox_start(harness, iterations, st, &sb.task_id);
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
  loop {
    let sandboxes = sandbox::list()?;
    if sandboxes.is_empty() {
      println!("No active sandboxes.");
      println!("Run 'so run' to start a new sandbox.");
      return Ok(());
    }

    // group by project
    let mut projects: HashMap<String, Vec<&sandbox::Info>> = HashMap::new();
    for sb in &sandboxes {
      let project = sb
        .name
        .strip_prefix("sandbox-")
        .and_then(|s| s.rsplit_once('-'))
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| sb.name.clone());
      projects.entry(project).or_default().push(sb);
    }

    let mut project_order: Vec<(String, std::time::SystemTime)> = projects
      .iter()
      .map(|(name, sbs)| {
        let newest = sbs.iter().map(|sb| sb.created).max().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        (name.clone(), newest)
      })
      .collect();
    project_order.sort_by(|a, b| b.1.cmp(&a.1));

    println!("{}", "Sandboxes:".white().bold());
    let mut all: Vec<&sandbox::Info> = Vec::new();
    let mut idx = 1;

    for (project, _) in &project_order {
      if let Some(sbs) = projects.get(project) {
        println!("\n  {}", project.magenta().bold());
        for sb in sbs {
          all.push(sb);
          let age = sb.created.elapsed().map(|d| d.as_secs() / 60).unwrap_or(0);
          let status_pad = format!("{:<8}", sb.status);
          let status_col = match sb.status.as_str() {
            "done" => status_pad.green(),
            "blocked" => status_pad.red(),
            "pending" => status_pad.yellow(),
            _ => status_pad.dimmed(),
          };
          println!(
            "    {}. {}  {} {}",
            format!("{:>2}", idx).cyan().bold(),
            sb.name,
            status_col,
            fmt_age(age).dimmed()
          );
          idx += 1;
        }
      }
    }
    println!();

    println!(
      " {}  {}lean  {}uit",
      format!("[1-{}]", all.len()).cyan().bold(),
      "[c]".cyan().bold(),
      "[q]".cyan().bold()
    );
    print!("> ");
    io::stdout().flush()?;
    let input = read_line_trim()?;
    if input.is_empty() {
      continue;
    }
    let lower = input.to_lowercase();

    if lower == "q" {
      return Ok(());
    }
    if lower == "c" {
      let mut project_names: Vec<_> = projects.keys().cloned().collect();
      project_names.sort();

      println!("Clean:");
      for (i, name) in project_names.iter().enumerate() {
        let count = projects.get(name).map(|v| v.len()).unwrap_or(0);
        println!("  {}. {:<16} ({} sandboxes)", format!("{:>2}", i + 1).cyan().bold(), name, count);
      }
      println!();
      println!(" {}ll    {}uit", "[a]".cyan().bold(), "[q]".cyan().bold());
      print!("Select project to clean: ");
      io::stdout().flush()?;
      let choice = read_line_trim()?;
      let choice_lower = choice.to_lowercase();

      if choice_lower == "q" {
        continue;
      }
      if choice_lower == "a" {
        print!("Delete all {} sandboxes? [y/n] ", all.len());
        io::stdout().flush()?;
        let yn = read_line_trim()?;
        if yn.to_lowercase().starts_with('y') {
          for sb in &all {
            let _ = std::fs::remove_dir_all(&sb.path);
          }
          println!("\nAll sandboxes cleaned.");
          return Ok(());
        }
        println!();
        continue;
      }
      if let Ok(n) = choice.parse::<usize>()
        && n >= 1
        && n <= project_names.len()
      {
        let project = &project_names[n - 1];
        let group = projects.get(project).cloned().unwrap_or_default();
        print!("Delete {} sandboxes for \"{}\"? [y/n] ", group.len(), project);
        io::stdout().flush()?;
        let yn = read_line_trim()?;
        if yn.to_lowercase().starts_with('y') {
          for sb in group {
            let _ = std::fs::remove_dir_all(&sb.path);
          }
          println!("\nSandboxes cleaned for {}.", project);
          return Ok(());
        }
        println!();
        continue;
      }
      println!();
      continue;
    }
    if let Ok(n) = input.parse::<usize>()
      && n >= 1
      && n <= all.len()
    {
      let sb = all[n - 1];
      let branch = sandbox::git_branch(&sb.path).unwrap_or_else(|_| "sandbox/main".into());
      let base = sandbox::git_base(&sb.path, sandbox::BASE_TAG);
      println!();
      print_summary(&sandbox::git_stat(&sb.path, &base), None, Some(&sb.path.display().to_string()));
      run_menu(&sb.path, &base, &sb.original, &branch).await?;
      return Ok(());
    }
    eprintln!("{} invalid choice", "error:".red().bold());
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
    let prompt = if mode == RunMode::Step {
      format!("{}\n\nDo not commit, human will handle that.\n\n---\nIteration {}/{}.", base, i, effective_max)
    } else {
      let commits = sandbox::git_recent(cwd, sandbox::BASE_TAG, 10);
      format!("{}\n\nRecent commits:\n{}\n\n---\nIteration {}/{}.", base, commits, i, effective_max)
    };

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
    eprintln!("\n{} uncommitted changes, you must commit ({}/3)\n", "warning:".yellow().bold(), i);
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
  Ok(c.contains("status: done") || c.contains("status: blocked"))
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
  cwd: &Path,
  start: Instant,
  st: SandboxType,
) -> Result<(), Error> {
  let result = sandbox::run(sb, harness.as_str(), iterations, st).await;
  match result {
    Ok(()) => {
      println!();
      print_summary(
        &sandbox::git_stat(&sb.path, sandbox::BASE_TAG),
        Some(&fmt_time(start.elapsed())),
        Some(&sb.path.display().to_string()),
      );
      run_menu(&sb.path, sandbox::BASE_TAG, cwd, &sb.branch).await?;
      Ok(())
    }
    Err(Error::Interrupted) => {
      println!("\n{}", format!("Interrupted. Sandbox kept at: {}", sb.path.display()).yellow());
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

fn short_hash(h: &str) -> &str {
  if h.len() >= 7 { &h[..7] } else { h }
}

fn run_git(args: &[&str], cwd: &Path) -> bool {
  match std::process::Command::new("git")
    .args(args)
    .current_dir(cwd)
    .stdin(Stdio::inherit())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .status()
  {
    Ok(s) => s.success(),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
      eprintln!("{} git not installed", "error:".red().bold());
      false
    }
    Err(_) => false,
  }
}

// =============================================================================
// Sandbox menu
// =============================================================================

async fn run_menu(sandbox: &Path, base: &str, orig: &Path, branch: &str) -> Result<(), Error> {
  loop {
    println!(
      " {}iff  {}hell  {}eset  {}erge  {}uit",
      "[d]".cyan().bold(),
      "[s]".cyan().bold(),
      "[r]".cyan().bold(),
      "[m]".cyan().bold(),
      "[q]".cyan().bold()
    );
    print!("> ");
    io::stdout().flush()?;
    let input = read_line_trim()?;
    if input.is_empty() {
      continue;
    }
    let ch = input.to_lowercase().chars().next().unwrap_or(' ');

    match ch {
      'd' => {
        let range = format!("{}..HEAD", base);
        run_git(&["diff", &range], sandbox);
        println!();
      }
      's' => {
        let shell = match std::env::var("SHELL") {
          Ok(s) => s,
          Err(_) => {
            println!(" {}", "SHELL not set".red());
            continue;
          }
        };
        println!("{}", "Entering sandbox. Type 'exit' to return".yellow());
        match std::process::Command::new(&shell).current_dir(sandbox).status() {
          Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(" {}", format!("Shell not found: {}", shell).red());
          }
          _ => {}
        }
        println!();
      }
      'r' => {
        println!("{}", "Commits since base:".white().bold());
        let commits = sandbox::git_commits(sandbox, base)?;
        if commits.is_empty() {
          println!("  No commits to reset.\n");
          continue;
        }
        for (i, (h, m)) in commits.iter().enumerate() {
          println!("  {}. {} {}", format!("{:>2}", i + 1).cyan().bold(), short_hash(h), m);
        }
        println!();
        print!("Reset to [1-{}] [q]uit ", commits.len());
        io::stdout().flush()?;
        let input = read_line_trim()?;
        if input.to_lowercase() != "q"
          && let Ok(n) = input.parse::<usize>()
          && n >= 1
          && n <= commits.len()
        {
          let (h, m) = &commits[n - 1];
          if run_git(&["reset", "--hard", h], sandbox) {
            println!("{}", format!("Reset to: {} {}", short_hash(h), m).green());
          }
        }
        println!();
      }
      'm' => {
        let cur = sandbox::git_branch(orig).unwrap_or_else(|_| "main".into());
        let dirty = sandbox::git_dirty(orig).unwrap_or(false);
        println!();
        println!(" Branch: {}", cur.yellow().bold());
        if dirty {
          println!(" {}", "Uncommitted changes. Commit or stash first".red());
          println!();
          continue;
        }
        print!(" Merge? [y/n] ");
        io::stdout().flush()?;
        let yn = read_line_trim()?;
        if !yn.to_lowercase().starts_with('y') {
          println!("\n");
          continue;
        }
        println!();

        let fetch =
          std::process::Command::new("git").args(["-C"]).arg(orig).args(["fetch"]).arg(sandbox).arg(branch).status();
        match fetch {
          Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(" {}", "Git not installed".red());
            continue;
          }
          Ok(s) if !s.success() => {
            println!(" {}", "Fetch failed".red());
            continue;
          }
          Err(_) => {
            println!(" {}", "Fetch failed".red());
            continue;
          }
          _ => {}
        }

        let merge =
          std::process::Command::new("git").args(["-C"]).arg(orig).args(["merge", "--squash", "FETCH_HEAD"]).status();
        let merge_ok = match &merge {
          Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(" {}", "Git not installed".red());
            false
          }
          Ok(s) => s.success(),
          Err(_) => false,
        };
        if merge_ok {
          run_git(&["checkout", "HEAD", "--", ".gitignore"], orig);
          if sandbox.join("specs").exists() {
            let _ = sandbox::copy_dir(&sandbox.join("specs"), &orig.join("specs"));
          }
          let _ = std::fs::remove_dir_all(sandbox);
          let staged = std::process::Command::new("git")
            .args(["-C"])
            .arg(orig)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
            .unwrap_or(0);
          println!(" {}", format!("{} files staged. Sandbox cleaned", staged).green());
        } else {
          println!(" {}", format!("Conflict. Sandbox kept at: {}", sandbox.display()).red());
        }
        break;
      }
      'q' => {
        println!("{}", format!("Sandbox kept at: {}", sandbox.display()).yellow());
        break;
      }
      _ => {}
    }
  }
  Ok(())
}

// =============================================================================
// Formatting
// =============================================================================

fn fmt_time(d: Duration) -> String {
  let s = d.as_secs();
  if s >= 60 { format!("{}m {:02}s", s / 60, s % 60) } else { format!("{}s", s) }
}

fn fmt_age(mins: u64) -> String {
  if mins < 60 {
    format!("{}m", mins)
  } else if mins < 1440 {
    format!("{}h", mins / 60)
  } else {
    format!("{}d", mins / 1440)
  }
}

fn print_info(msg: &str) {
  println!("{}", format!("▶ {}", msg).yellow().bold());
}

fn print_header(harness: Harness, cur: u32, total: u32) {
  println!("{}", format!("▶ [{}] {}/{}", harness.as_str(), cur, total).cyan().bold());
}

fn print_header_time(harness: Harness, cur: u32, total: u32, time: &str) {
  println!("{}", format!("▶ [{}] {}/{} | {}", harness.as_str(), cur, total, time).cyan().bold());
}

fn print_sandbox_start(harness: Harness, n: u32, st: SandboxType, task_id: &str) {
  println!("{}", format!("▶ Starting [{}] max={} [{}]", harness.as_str(), n, st.as_str()).green().bold());
  if harness == Harness::Claude {
    println!("{}", format!("  Tasks: {}", task_id).magenta());
  }
  println!();
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
