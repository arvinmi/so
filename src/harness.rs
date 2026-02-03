use std::{path::Path, process::Stdio};

use tokio::process::Command;

use crate::{Error, Harness, RunMode, TaskMode, sandbox};

pub(crate) fn build_prompt(base: &str, mode: RunMode, cwd: &Path, iter: u32, max_iter: u32) -> String {
  match mode {
    RunMode::Step => {
      format!("{base}\n\nDo not commit, human will handle that.\n\n---\nIteration {iter}/{max_iter}.")
    }
    RunMode::Run => {
      let commits = sandbox::git_recent(cwd, sandbox::BASE_TAG, 10);
      format!("{base}\n\nRecent commits:\n{commits}\n\n---\nIteration {iter}/{max_iter}.")
    }
  }
}

pub(crate) async fn run_harness(harness: Harness, prompt: &str, task: TaskMode, cwd: &Path) -> Result<(), Error> {
  match harness {
    Harness::Claude => run_claude(prompt, cwd).await,
    Harness::Opencode => run_opencode(prompt, task, cwd).await,
    Harness::Codex => run_codex(prompt, task, cwd).await,
  }
}

fn harness_cmd(cwd: &Path, program: &str) -> Command {
  let mut c = Command::new(program);
  c.current_dir(cwd);
  c
}

async fn run_claude(prompt: &str, cwd: &Path) -> Result<(), Error> {
  let mut cmd = harness_cmd(cwd, "claude");
  if let Ok(m) = std::env::var("MODEL") {
    cmd.arg("--model").arg(m);
  }
  cmd.stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
  let mut child = cmd.spawn().map_err(|e| harness_err("claude", e))?;
  if let Some(mut stdin) = child.stdin.take() {
    use tokio::io::AsyncWriteExt;
    stdin.write_all(prompt.as_bytes()).await?;
  }
  wait_child(child, "claude").await
}

async fn run_opencode(prompt: &str, task: TaskMode, cwd: &Path) -> Result<(), Error> {
  let (model, _effort) = resolve_model_effort(
    task,
    ("openai/gpt-5.2", "high"),
    ("openai/gpt-5.2", "medium"),
    ("openai/gpt-5.2-codex", "medium"),
  );

  let mut cmd = harness_cmd(cwd, "opencode");
  cmd.args(["--prompt", prompt, "-m", &model]);
  cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
  let child = cmd.spawn().map_err(|e| harness_err("opencode", e))?;
  wait_child(child, "opencode").await
}

async fn run_codex(prompt: &str, task: TaskMode, cwd: &Path) -> Result<(), Error> {
  let (model, effort) =
    resolve_model_effort(task, ("gpt-5.2", "high"), ("gpt-5.2", "medium"), ("gpt-5.2-codex", "medium"));

  let mut cmd = harness_cmd(cwd, "codex");
  let cfg = format!("model_reasoning_effort={effort}");
  let bypass = "--dangerously-bypass-approvals-and-sandbox";
  cmd.args([prompt, "--model", &model, bypass, "--config", &cfg]);
  cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
  let child = cmd.spawn().map_err(|e| harness_err("codex", e))?;
  wait_child(child, "codex").await
}

fn harness_err(name: &str, e: std::io::Error) -> Error {
  if e.kind() == std::io::ErrorKind::NotFound {
    Error::Harness(format!("`{name}` not found"))
  } else {
    Error::Harness(format!("failed to run `{name}`: {e}"))
  }
}

fn check_status_code(status: std::process::ExitStatus, name: &str) -> Result<(), Error> {
  // use sigint (ctrl+c) and sigterm for graceful shutdown, not failure
  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    if status.signal().is_some_and(|sig| sig == libc::SIGINT || sig == libc::SIGTERM) {
      return Ok(());
    }
    if status.code().is_some_and(|c| c == 130 || c == 143) {
      return Ok(());
    }
  }
  if status.success() { Ok(()) } else { Err(Error::Harness(format!("`{name}` failed"))) }
}

// wait for child process, treating ctrl+c as normal exit
async fn wait_child(mut child: tokio::process::Child, name: &str) -> Result<(), Error> {
  use tokio::signal;
  tokio::select! {
    biased;
    res = child.wait() => check_status_code(res.map_err(|e| harness_err(name, e))?, name),
    _ = signal::ctrl_c() => {
      println!();
      let _ = child.kill().await;
      let _ = child.wait().await;
      Ok(())
    }
  }
}

fn resolve_model_effort(
  task: TaskMode,
  plan: (&'static str, &'static str),
  learn: (&'static str, &'static str),
  code: (&'static str, &'static str),
) -> (String, String) {
  let (def_model, def_effort) = match task {
    TaskMode::Plan => plan,
    TaskMode::Learn => learn,
    TaskMode::Code => code,
  };
  let model = std::env::var("MODEL").unwrap_or_else(|_| def_model.into());
  let effort = std::env::var("EFFORT").unwrap_or_else(|_| def_effort.into());
  (model, effort)
}
