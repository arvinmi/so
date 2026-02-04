use std::{
  io,
  path::{Path, PathBuf},
};

use crossterm::{
  event::{DisableMouseCapture, EnableMouseCapture},
  execute,
  terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{Error, sandbox};

pub fn diff_range(cwd: &Path) -> String {
  let base = sandbox::git_base(cwd, sandbox::BASE_TAG);
  format!("{base}..HEAD")
}

pub fn suspend_and_run_git(
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  args: &[&str],
  cwd: &Path,
  use_mouse_capture: bool,
) -> Result<(), Error> {
  // temporarily restore terminal to run interactive commands
  let _signals = SignalGuard::ignore();
  suspend_terminal(terminal);

  if args.first().copied() == Some("diff") {
    let _ = run_diff_pager(args, cwd);
  } else {
    let _ = std::process::Command::new("git")
      .args(args)
      .current_dir(cwd)
      .stdin(std::process::Stdio::inherit())
      .stdout(std::process::Stdio::inherit())
      .stderr(std::process::Stdio::inherit())
      .status();
  }

  resume_terminal(terminal, use_mouse_capture);
  Ok(())
}

fn run_diff_pager(args: &[&str], cwd: &Path) -> Result<(), Error> {
  let (cols, _) = crossterm::terminal::size().unwrap_or((0, 0));
  let mut git = std::process::Command::new("git");
  git.arg("-c").arg("color.ui=always");
  git.args(args);
  git.current_dir(cwd);
  git.stdin(std::process::Stdio::inherit());
  git.stdout(std::process::Stdio::piped());
  git.stderr(std::process::Stdio::inherit());

  let mut git_child = git.spawn()?;
  let Some(git_out) = git_child.stdout.take() else {
    let _ = git_child.wait();
    return Ok(());
  };

  let mut delta_child = None;
  let pager_input: std::process::Stdio = if let Some(delta) = find_delta() {
    let mut cmd = std::process::Command::new(delta);
    cmd.arg("--paging=never");
    if cols > 0 {
      cmd.arg("--width").arg(cols.to_string());
    }
    let mut child =
      cmd.stdin(git_out).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit()).spawn()?;
    let out = child.stdout.take().map(std::process::Stdio::from);
    delta_child = Some(child);
    out.unwrap_or_else(std::process::Stdio::null)
  } else {
    git_out.into()
  };

  let mut pager = std::process::Command::new("less");
  pager.arg("-R");
  pager.env("LESS", "SR");
  pager.stdin(pager_input);
  pager.stdout(std::process::Stdio::inherit());
  pager.stderr(std::process::Stdio::inherit());
  let mut pager_child = pager.spawn()?;

  let _ = pager_child.wait();
  if let Some(mut child) = delta_child {
    let _ = child.wait();
  }
  let _ = git_child.wait();
  Ok(())
}

pub fn suspend_and_run_shell(
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  cwd: &Path,
  use_mouse_capture: bool,
) -> Result<(), Error> {
  // temporarily restore terminal to run interactive commands
  let _signals = SignalGuard::ignore();
  let parent_pgrp = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
  suspend_terminal(terminal);

  let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
  let _ = std::process::Command::new(&shell)
    .current_dir(cwd)
    .stdin(std::process::Stdio::inherit())
    .stdout(std::process::Stdio::inherit())
    .stderr(std::process::Stdio::inherit())
    .status();

  // restore foreground process group before re-entering tui
  if parent_pgrp > 0 {
    unsafe {
      libc::tcsetpgrp(libc::STDIN_FILENO, parent_pgrp);
    }
  }

  resume_terminal(terminal, use_mouse_capture);
  Ok(())
}

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
  disable_raw_mode().ok();
  execute!(terminal.backend_mut(), DisableMouseCapture).ok();
  execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
  terminal.show_cursor().ok();
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, use_mouse_capture: bool) {
  execute!(terminal.backend_mut(), EnterAlternateScreen).ok();
  if use_mouse_capture {
    execute!(terminal.backend_mut(), EnableMouseCapture).ok();
  }
  enable_raw_mode().ok();
  terminal.clear().ok();
}

pub fn git_reset_hard(cwd: &Path, hash: &str) -> bool {
  std::process::Command::new("git")
    .args(["reset", "--hard", hash])
    .current_dir(cwd)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

pub fn merge_sandbox(sandbox_path: &Path, orig: &Path) -> Result<(), Error> {
  let branch = sandbox::git_branch(sandbox_path).unwrap_or_else(|_| "sandbox/main".into());

  if sandbox::git_dirty(orig).unwrap_or(false) {
    return Err(Error::UncommittedChanges);
  }

  let fetch = std::process::Command::new("git")
    .arg("-C")
    .arg(orig)
    .args(["fetch"])
    .arg(sandbox_path)
    .arg(&branch)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status();

  if !fetch.map(|s| s.success()).unwrap_or(false) {
    return Err(Error::Other("fetch failed".into()));
  }

  let merge = std::process::Command::new("git")
    .arg("-C")
    .arg(orig)
    .args(["merge", "--squash", "FETCH_HEAD"])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status();

  if !merge.map(|s| s.success()).unwrap_or(false) {
    return Err(Error::Other("merge conflict".into()));
  }

  let _ = std::process::Command::new("git")
    .arg("-C")
    .arg(orig)
    .args(["checkout", "HEAD", "--", ".gitignore"])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status();

  if sandbox_path.join("specs").exists() {
    let _ = sandbox::copy_dir(&sandbox_path.join("specs"), &orig.join("specs"));
  }

  let _ = std::fs::remove_dir_all(sandbox_path);
  Ok(())
}

pub fn original_repo(cwd: &Path) -> Option<PathBuf> {
  let name = cwd.file_name()?.to_string_lossy().to_string();
  crate::config::read_meta(&name).map(|m| PathBuf::from(m.original))
}

fn find_delta() -> Option<PathBuf> {
  // prefer PATH, fall back to common install locations
  if std::process::Command::new("delta")
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok()
  {
    return Some(PathBuf::from("delta"));
  }
  for p in ["/usr/local/bin/delta", "/usr/bin/delta"] {
    let path = PathBuf::from(p);
    if path.exists() {
      return Some(path);
    }
  }
  None
}

struct SignalGuard {
  ttou: libc::sighandler_t,
  ttin: libc::sighandler_t,
  tstp: libc::sighandler_t,
}

impl SignalGuard {
  fn ignore() -> Self {
    unsafe {
      let ttou = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
      let ttin = libc::signal(libc::SIGTTIN, libc::SIG_IGN);
      let tstp = libc::signal(libc::SIGTSTP, libc::SIG_IGN);
      Self { ttou, ttin, tstp }
    }
  }
}

impl Drop for SignalGuard {
  fn drop(&mut self) {
    unsafe {
      libc::signal(libc::SIGTTOU, self.ttou);
      libc::signal(libc::SIGTTIN, self.ttin);
      libc::signal(libc::SIGTSTP, self.tstp);
    }
  }
}
