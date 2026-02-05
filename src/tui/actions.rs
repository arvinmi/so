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
  format!("{}..HEAD", base)
}

pub fn suspend_and_run_git(
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  args: &[&str],
  cwd: &Path,
  use_mouse_capture: bool,
) -> Result<(), Error> {
  // temporarily restore terminal to run interactive commands
  let _signals = SignalGuard::ignore();
  disable_raw_mode().ok();
  execute!(terminal.backend_mut(), DisableMouseCapture).ok();
  execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
  terminal.show_cursor().ok();

  let mut cmd = std::process::Command::new("git");
  if args.first().copied() == Some("diff")
    && let Some(delta) = find_delta()
  {
    // force delta when available, regardless of pager env
    let delta = delta.to_string_lossy();
    cmd.arg("-c").arg(format!("core.pager={}", delta));
    cmd.arg("-c").arg(format!("pager.diff={}", delta));
    cmd.env_remove("GIT_PAGER");
    cmd.env_remove("PAGER");
  }
  let _ = cmd
    .args(args)
    .current_dir(cwd)
    .stdin(std::process::Stdio::inherit())
    .stdout(std::process::Stdio::inherit())
    .stderr(std::process::Stdio::inherit())
    .status();

  execute!(terminal.backend_mut(), EnterAlternateScreen).ok();
  if use_mouse_capture {
    execute!(terminal.backend_mut(), EnableMouseCapture).ok();
  }
  enable_raw_mode().ok();
  terminal.clear().ok();
  Ok(())
}

pub fn suspend_and_run_shell(
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  cwd: &Path,
  use_mouse_capture: bool,
) -> Result<(), Error> {
  // temporarily restore terminal to run interactive commands
  let _signals = SignalGuard::ignore();
  disable_raw_mode().ok();
  execute!(terminal.backend_mut(), DisableMouseCapture).ok();
  execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
  terminal.show_cursor().ok();

  let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
  let _ = std::process::Command::new(&shell)
    .current_dir(cwd)
    .stdin(std::process::Stdio::inherit())
    .stdout(std::process::Stdio::inherit())
    .stderr(std::process::Stdio::inherit())
    .status();

  execute!(terminal.backend_mut(), EnterAlternateScreen).ok();
  if use_mouse_capture {
    execute!(terminal.backend_mut(), EnableMouseCapture).ok();
  }
  enable_raw_mode().ok();
  terminal.clear().ok();
  Ok(())
}

pub fn git_reset_hard(cwd: &Path, hash: &str) -> bool {
  std::process::Command::new("git")
    .args(["reset", "--hard", hash])
    .current_dir(cwd)
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

pub fn merge_sandbox(sandbox_path: &Path, orig: &Path) -> Result<(), Error> {
  let branch = sandbox::git_branch(sandbox_path).unwrap_or_else(|_| "sandbox/main".into());

  if sandbox::git_dirty(orig).unwrap_or(false) {
    return Err(Error::UncommittedChanges);
  }

  let fetch =
    std::process::Command::new("git").arg("-C").arg(orig).args(["fetch"]).arg(sandbox_path).arg(&branch).status();

  if !fetch.map(|s| s.success()).unwrap_or(false) {
    return Err(Error::Other("fetch failed".into()));
  }

  let merge = std::process::Command::new("git").arg("-C").arg(orig).args(["merge", "--squash", "FETCH_HEAD"]).status();

  if !merge.map(|s| s.success()).unwrap_or(false) {
    return Err(Error::Other("merge conflict".into()));
  }

  let _ = std::process::Command::new("git").arg("-C").arg(orig).args(["checkout", "HEAD", "--", ".gitignore"]).status();

  if sandbox_path.join("specs").exists() {
    let _ = sandbox::copy_dir(&sandbox_path.join("specs"), &orig.join("specs"));
  }

  let _ = std::fs::remove_dir_all(sandbox_path);
  Ok(())
}

pub fn original_repo(cwd: &Path) -> Option<PathBuf> {
  git2::Repository::open(cwd)
    .ok()
    .and_then(|r| r.config().ok())
    .and_then(|c| c.get_string("so.original").ok())
    .map(|s| PathBuf::from(s.trim()))
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
