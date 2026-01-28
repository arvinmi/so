use std::{
  collections::HashSet,
  path::{Path, PathBuf},
  process::Stdio,
};

use colored::Colorize;
use git2::{Repository, StatusOptions};
use tokio::{
  process::{Child, Command},
  signal,
};

use crate::Error;

pub const BASE_TAG: &str = "so-base";

fn is_bwrap() -> bool {
  std::env::var("SANDBOX").unwrap_or_else(|_| "docker".into()).to_lowercase() == "bwrap"
}

fn project_name(p: &Path) -> &str {
  p.file_name().and_then(|n| n.to_str()).unwrap_or("project")
}

// =============================================================================
// Sandbox
// =============================================================================

#[derive(Debug, Clone, Copy)]
pub enum Mode {
  Run,
  Clean,
  Dup,
}

pub struct Sandbox {
  pub path: PathBuf,
  pub original: PathBuf,
  pub branch: String,
  pub task_id: String,
}

impl Sandbox {
  pub fn new(original: &Path, mode: Mode, prompt: Option<&str>) -> Result<Self, Error> {
    let project = project_name(original);
    let ts = chrono::Utc::now().timestamp();
    let path = PathBuf::from(format!("/tmp/sandbox-{}-{}", project, ts));
    let task_id = format!("so-{}-{}", project, ts);

    copy_dir(original, &path)?;
    let branch = setup_git(&path)?;
    setup_specs(&path, prompt, mode)?;

    Ok(Self { path, original: original.to_path_buf(), branch, task_id })
  }
}

pub struct Info {
  pub name: String,
  pub path: PathBuf,
  pub created: std::time::SystemTime,
  pub status: String,
}

pub fn list() -> Result<Vec<Info>, Error> {
  let mut out = Vec::new();
  for e in std::fs::read_dir("/tmp")? {
    let e = e?;
    let name = e.file_name().to_string_lossy().to_string();
    if name.starts_with("sandbox-") {
      let path = e.path();
      let created = e.metadata()?.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
      let status = path.join("specs/status.md");
      let status = if status.exists() {
        let c = std::fs::read_to_string(&status).unwrap_or_default().to_lowercase();
        if c.contains("done") {
          "done"
        } else if c.contains("blocked") {
          "blocked"
        } else {
          "pending"
        }
      } else {
        "pending"
      };
      out.push(Info { name, path, created, status: status.into() });
    }
  }
  out.sort_by(|a, b| b.created.cmp(&a.created));
  Ok(out)
}

// =============================================================================
// Run
// =============================================================================

pub async fn run(sb: &Sandbox, harness: &str, iterations: u32) -> Result<(), Error> {
  if is_bwrap() { run_bwrap(sb, harness, iterations).await } else { run_docker(sb, harness, iterations).await }
}

async fn run_docker(sb: &Sandbox, harness: &str, iterations: u32) -> Result<(), Error> {
  let project = project_name(&sb.original);
  let dockerfile = sb.original.join("Dockerfile.sandbox");
  if !dockerfile.exists() {
    return Err(Error::NoDockerfile);
  }

  let user = dockerfile_user(&dockerfile).unwrap_or_else(|| "so".into());
  let home = format!("/home/{}", user);
  let code = format!("{}/{}", home, project);
  let image = format!("sandbox-{}", project.to_lowercase());

  // rebuild if dockerfile newer than image
  let rebuild = needs_rebuild(&image, &dockerfile).await;
  println!("{}", "▶ Building image".yellow().bold());
  let mut cmd = Command::new("docker");
  cmd.args(["build", "-q"]);
  if rebuild {
    cmd.arg("--no-cache");
  }
  cmd.args(["-t", &image, "-f"]).arg(&dockerfile).arg(&sb.original);
  cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
  if !cmd.status().await?.success() {
    return Err(Error::Docker("build failed".into()));
  }

  let creds = setup_creds()?;

  let mut cmd = Command::new("docker");
  cmd.args(["run", "-it", "--rm", "--network", "host"]);

  // pass gpu if available (linux only)
  if cfg!(target_os = "linux") {
    if has_gpu() {
      cmd.args(["--gpus", "all"]);
    } else {
      eprintln!("{} no GPU detected, running without GPU support", "note:".cyan().bold());
    }
  }

  // mounts
  cmd.args(["-v", "/etc/localtime:/etc/localtime:ro"]);
  cmd.arg("-v").arg(format!("{}:{}", sb.path.display(), code));
  let so_dir = std::env::current_exe()
    .ok()
    .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    .unwrap_or_else(|| PathBuf::from("/usr/local/bin"));
  cmd.arg("-v").arg(format!("{}:/opt/so:ro", so_dir.display()));
  let gc = sb.path.join(".git/config");
  if gc.exists() {
    cmd.arg("-v").arg(format!("{}:{}/.git/config:ro", gc.display(), code));
  }

  // credentials
  mount_creds(&mut cmd, &creds, &home);

  // env
  cmd.args(["-e", &format!("CLAUDE_CODE_TASK_LIST_ID={}", sb.task_id)]);
  cmd.args(["-e", "SO_UNATTENDED=1"]);
  if let Ok(m) = std::env::var("MODEL") {
    cmd.args(["-e", &format!("MODEL={}", m)]);
  }
  if let Ok(e) = std::env::var("EFFORT") {
    cmd.args(["-e", &format!("EFFORT={}", e)]);
  }

  cmd.args(["-w", &code]).arg(&image);
  cmd.args(["/opt/so/so", "step", harness, &iterations.to_string()]);
  cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());

  let child = match cmd.spawn() {
    Ok(c) => c,
    Err(e) => {
      let _ = std::fs::remove_dir_all(&creds);
      return Err(Error::Docker(format!("spawn failed: {}", e)));
    }
  };
  let result = wait_with_ctrl_c(child).await;
  let _ = std::fs::remove_dir_all(&creds);
  let status = match result {
    Ok(s) => s,
    Err(e) => return Err(e),
  };
  if !status.success() {
    return Err(Error::Docker("container failed".into()));
  }
  Ok(())
}

async fn run_bwrap(sb: &Sandbox, harness: &str, iterations: u32) -> Result<(), Error> {
  let home = dirs::home_dir().ok_or(Error::NoHome)?;
  let code = home.join(project_name(&sb.original));
  let creds = setup_creds()?;

  let mut a: Vec<String> = Vec::new();
  let mut created_dirs: HashSet<String> = HashSet::new();

  // system (ro)
  ro(&mut a, "/usr");
  ro(&mut a, "/lib");
  ro(&mut a, "/bin");
  if_exists_ro(&mut a, "/lib64");
  if_exists_ro(&mut a, "/sbin");
  if_exists_ro(&mut a, "/snap");

  // network and ssl (ro)
  ro(&mut a, "/etc/resolv.conf");
  ro(&mut a, "/etc/hosts");
  ro(&mut a, "/etc/passwd");
  ro(&mut a, "/etc/group");
  if_exists_ro(&mut a, "/etc/ssl");
  if_exists_ro(&mut a, "/etc/ca-certificates");

  // home tmpfs
  check_dir(&mut a, &mut created_dirs, &home.display().to_string());
  a.extend(["--tmpfs".into(), home.display().to_string()]);

  // tools (ro)
  for d in [".nvm", ".cargo", ".rustup", ".pyenv", ".deno", "go", ".sdkman", ".ssh"] {
    if_exists_ro(&mut a, &home.join(d).display().to_string());
  }

  // conda (ro)
  let conda = ["miniforge3", "anaconda3", "miniconda3"];
  for d in &conda {
    if_exists_ro(&mut a, &home.join(d).display().to_string());
  }

  // writable tool dirs (tmpfs)
  for d in [".local", ".bun"] {
    let p = home.join(d);
    check_dir(&mut a, &mut created_dirs, &p.display().to_string());
    a.extend(["--tmpfs".into(), p.display().to_string()]);
  }

  // caches (tmpfs)
  for d in [".cache", ".npm", ".conda"] {
    let p = home.join(d);
    check_dir(&mut a, &mut created_dirs, &p.display().to_string());
    a.extend(["--tmpfs".into(), p.display().to_string()]);
  }
  a.extend(["--tmpfs".into(), "/tmp".into(), "--tmpfs".into(), "/var".into()]);

  // agent configs (rw)
  let h = home.display().to_string();
  check_dir(&mut a, &mut created_dirs, &format!("{}/.config", h));
  check_dir(&mut a, &mut created_dirs, &format!("{}/.local", h));
  check_dir(&mut a, &mut created_dirs, &format!("{}/.local/share", h));
  if_exists_bind(&mut a, &creds.join(".claude"), &format!("{}/.claude", h), &mut created_dirs);
  if_exists_bind(&mut a, &creds.join(".claude.json"), &format!("{}/.claude.json", h), &mut created_dirs);
  if_exists_bind(&mut a, &creds.join(".codex"), &format!("{}/.codex", h), &mut created_dirs);
  if_exists_bind(&mut a, &creds.join(".config/opencode"), &format!("{}/.config/opencode", h), &mut created_dirs);
  if_exists_bind(
    &mut a,
    &creds.join(".local/share/opencode"),
    &format!("{}/.local/share/opencode", h),
    &mut created_dirs,
  );
  let gitconfig_dst = format!("{}/.gitconfig", h);
  a.extend(["--ro-bind".into(), creds.join(".gitconfig").display().to_string(), gitconfig_dst]);

  // workspace
  check_dir(&mut a, &mut created_dirs, &code.display().to_string());
  a.extend(["--bind".into(), sb.path.display().to_string(), code.display().to_string()]);

  // set so binary
  let so_dir = std::env::current_exe()
    .ok()
    .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    .unwrap_or_else(|| PathBuf::from("/usr/local/bin"));
  a.extend(["--ro-bind".into(), so_dir.display().to_string(), "/opt/so".into()]);

  // docker socket
  if Path::new("/var/run/docker.sock").exists() {
    a.extend(["--bind".into(), "/var/run/docker.sock".into(), "/var/run/docker.sock".into()]);
  }
  if_exists_ro(&mut a, &home.join(".docker").display().to_string());

  a.extend(["--dev".into(), "/dev".into(), "--proc".into(), "/proc".into(), "--unshare-pid".into()]);

  // PATH with conda
  let mut path = std::env::var("PATH").unwrap_or_default();
  for d in &conda {
    let bin = home.join(d).join("bin");
    if bin.exists() {
      path = format!("{}:{}", bin.display(), path);
    }
  }

  // env
  a.extend(["--setenv".into(), "HOME".into(), h.clone()]);
  a.extend(["--setenv".into(), "PATH".into(), path]);
  a.extend(["--setenv".into(), "CLAUDE_CODE_TASK_LIST_ID".into(), sb.task_id.clone()]);
  a.extend(["--setenv".into(), "SO_UNATTENDED".into(), "1".into()]);
  a.extend(["--setenv".into(), "TMPDIR".into(), "/tmp".into()]);
  a.extend(["--setenv".into(), "UV_CACHE_DIR".into(), format!("{}/.cache/uv", h)]);
  if let Ok(m) = std::env::var("MODEL") {
    a.extend(["--setenv".into(), "MODEL".into(), m]);
  }
  if let Ok(e) = std::env::var("EFFORT") {
    a.extend(["--setenv".into(), "EFFORT".into(), e]);
  }

  a.extend(["--chdir".into(), code.display().to_string()]);
  a.extend(["--".into(), "/opt/so/so".into(), "step".into(), harness.into(), iterations.to_string()]);

  let mut cmd = Command::new("bwrap");
  cmd.args(&a).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
  let child = match cmd.spawn() {
    Ok(c) => c,
    Err(e) => {
      let _ = std::fs::remove_dir_all(&creds);
      return Err(Error::Other(format!("bwrap spawn failed: {}", e)));
    }
  };
  let result = wait_with_ctrl_c(child).await;
  let _ = std::fs::remove_dir_all(&creds);
  let status = match result {
    Ok(s) => s,
    Err(e) => return Err(e),
  };
  if !status.success() {
    return Err(Error::Other(format!("bwrap exited with code {}", status.code().unwrap_or(-1))));
  }
  Ok(())
}

// =============================================================================
// Bwrap helpers
// =============================================================================

fn ro(a: &mut Vec<String>, p: &str) {
  a.extend(["--ro-bind".into(), p.into(), p.into()]);
}

fn if_exists_ro(a: &mut Vec<String>, p: &str) {
  if Path::new(p).exists() {
    ro(a, p);
  }
}

fn if_exists_bind(a: &mut Vec<String>, src: &Path, dst: &str, created_dirs: &mut HashSet<String>) {
  if src.exists() {
    if let Some(parent) = Path::new(dst).parent() {
      check_dir(a, created_dirs, &parent.display().to_string());
    }
    if src.is_dir() {
      check_dir(a, created_dirs, dst);
    }
    a.extend(["--bind".into(), src.display().to_string(), dst.into()]);
  }
}

fn check_dir(a: &mut Vec<String>, created: &mut HashSet<String>, path: &str) {
  if created.insert(path.to_string()) {
    a.extend(["--dir".into(), path.into()]);
  }
}

// =============================================================================
// Docker helpers
// =============================================================================

fn has_gpu() -> bool {
  std::process::Command::new("nvidia-smi")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

async fn needs_rebuild(image: &str, dockerfile: &Path) -> bool {
  let img = Command::new("docker")
    .args(["inspect", "-f", "{{.Created}}", image])
    .output()
    .await
    .ok()
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .and_then(|s| s.split('T').next().map(|d| d.replace('-', "")))
    .and_then(|s| s.trim().parse::<u32>().ok());
  let file = std::fs::metadata(dockerfile)
    .ok()
    .and_then(|m| m.modified().ok())
    .map(|t| chrono::DateTime::<chrono::Utc>::from(t).format("%Y%m%d").to_string().parse::<u32>().unwrap_or(0));
  img.is_none() || file.unwrap_or(0) >= img.unwrap_or(0)
}

fn dockerfile_user(p: &Path) -> Option<String> {
  std::fs::read_to_string(p)
    .ok()?
    .lines()
    .rev()
    .find(|l| l.trim().starts_with("USER "))
    .and_then(|l| l.split_whitespace().nth(1))
    .map(|s| s.into())
}

fn mount_creds(cmd: &mut Command, creds: &Path, home: &str) {
  let gc = creds.join(".gitconfig");
  if gc.exists() {
    cmd.arg("-v").arg(format!("{}:{}/.gitconfig:ro", gc.display(), home));
  }
  for (src, dst) in [
    (".claude", ".claude"),
    (".claude.json", ".claude.json"),
    (".codex", ".codex"),
    (".config/opencode", ".config/opencode"),
    (".local/share/opencode", ".local/share/opencode"),
  ] {
    let p = creds.join(src);
    if p.exists() {
      cmd.arg("-v").arg(format!("{}:{}/{}", p.display(), home, dst));
    }
  }
}

// =============================================================================
// Credentials
// =============================================================================

fn setup_creds() -> Result<PathBuf, Error> {
  let home = dirs::home_dir().ok_or(Error::NoHome)?;
  let creds = PathBuf::from(format!("/tmp/so-creds-{}", std::process::id()));
  std::fs::create_dir_all(&creds)?;
  std::fs::create_dir_all(creds.join(".config"))?;
  std::fs::create_dir_all(creds.join(".local/share"))?;

  std::fs::write(
    creds.join(".gitconfig"),
    r#"[user]
  name = so-sandbox
  email = so-sandbox@local
[filter "lfs"]
  required = true
  clean = git-lfs clean -- %f
  smudge = git-lfs smudge -- %f
  process = git-lfs filter-process
[submodule]
  recurse = true
[url "ssh://git@github.com/"]
  insteadOf = https://github.com/
"#,
  )?;

  // copy configs excluding global agent files
  copy_filtered(&home.join(".claude"), &creds.join(".claude"), &["CLAUDE.md"])?;
  copy_filtered(&home.join(".codex"), &creds.join(".codex"), &["AGENTS.md"])?;
  copy_filtered(&home.join(".config/opencode"), &creds.join(".config/opencode"), &["opencode.json"])?;
  copy_filtered(&home.join(".local/share/opencode"), &creds.join(".local/share/opencode"), &[])?;
  if home.join(".claude.json").exists() {
    std::fs::copy(home.join(".claude.json"), creds.join(".claude.json"))?;
  }

  Ok(creds)
}

fn copy_filtered(src: &Path, dst: &Path, exclude: &[&str]) -> Result<(), Error> {
  if !src.exists() {
    return Ok(());
  }
  std::fs::create_dir_all(dst)?;
  for e in std::fs::read_dir(src)? {
    let e = e?;
    let ft = e.file_type()?;
    let name = e.file_name();
    if exclude.iter().any(|x| *x == name.to_string_lossy()) {
      continue;
    }
    let s = e.path();
    let d = dst.join(&name);
    if ft.is_symlink() {
      copy_symlink(&s, &d)?;
    } else if ft.is_dir() {
      copy_filtered(&s, &d, exclude)?;
    } else if ft.is_file() {
      std::fs::copy(&s, &d)?;
    }
  }
  Ok(())
}

// =============================================================================
// Git setup
// =============================================================================

fn setup_git(sandbox: &Path) -> Result<String, Error> {
  let repo = Repository::open(sandbox)?;
  let branch = repo.head()?.shorthand().unwrap_or("main").to_string();
  let sb_branch = format!("sandbox/{}", branch);
  let commit = repo.head()?.peel_to_commit()?;

  repo.branch(&sb_branch, &commit, true)?;
  let obj = repo.revparse_single(&format!("refs/heads/{}", sb_branch))?;
  repo.checkout_tree(&obj, None)?;
  repo.set_head(&format!("refs/heads/{}", sb_branch))?;

  // remove remotes
  for name in repo.remotes()?.iter().flatten() {
    let _ = repo.remote_delete(name);
  }

  // delete other branches
  for (mut b, _) in repo.branches(Some(git2::BranchType::Local))?.flatten() {
    if b.name()?.unwrap_or("") != sb_branch {
      let _ = b.delete();
    }
  }

  repo.tag_lightweight(BASE_TAG, &commit.into_object(), true)?;
  repo.config()?.set_str("receive.denyCurrentBranch", "updateInstead")?;

  Ok(sb_branch)
}

fn setup_specs(sandbox: &Path, prompt: Option<&str>, mode: Mode) -> Result<(), Error> {
  let specs = sandbox.join("specs");
  std::fs::create_dir_all(&specs)?;
  std::fs::write(specs.join("status.md"), "Status: pending\n")?;
  if let Some(p) = prompt {
    std::fs::write(specs.join("prompt.md"), p)?;
  }

  // update gitignore
  let gi = sandbox.join(".gitignore");
  if gi.exists() {
    let mut lines: Vec<_> =
      std::fs::read_to_string(&gi)?.lines().filter(|l| !l.starts_with("specs")).map(|s| s.to_string()).collect();
    if matches!(mode, Mode::Dup) {
      lines.push(".jscpd/".into());
    }
    std::fs::write(&gi, lines.join("\n") + "\n")?;
  } else if matches!(mode, Mode::Dup) {
    std::fs::write(&gi, ".jscpd/\n")?;
  }

  // commit setup
  let repo = Repository::open(sandbox)?;
  let mut idx = repo.index()?;
  if gi.exists() {
    idx.add_path(Path::new(".gitignore"))?;
  }
  idx.add_all(["specs/*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
  idx.write()?;

  let tree = repo.find_tree(idx.write_tree()?)?;
  let sig = repo.signature()?;
  let parent = repo.head()?.peel_to_commit()?;
  repo.commit(Some("HEAD"), &sig, &sig, "Setup sandbox", &tree, &[&parent])?;

  // update base tag
  let head = repo.head()?.peel_to_commit()?;
  repo.tag_lightweight(BASE_TAG, &head.into_object(), true)?;

  Ok(())
}

// =============================================================================
// Git queries (public)
// =============================================================================

pub fn git_dirty(p: &Path) -> Result<bool, Error> {
  let repo = Repository::open(p)?;
  let mut opts = StatusOptions::new();
  opts.include_untracked(true).include_ignored(false);
  let statuses = repo.statuses(Some(&mut opts))?;
  Ok(!statuses.is_empty())
}

pub fn git_head(p: &Path) -> Result<String, Error> {
  Ok(Repository::open(p)?.head()?.peel_to_commit()?.id().to_string())
}

pub fn git_branch(p: &Path) -> Result<String, Error> {
  Ok(Repository::open(p)?.head()?.shorthand().unwrap_or("main").into())
}

pub fn git_base(p: &Path, tag: &str) -> String {
  let repo = match Repository::open(p) {
    Ok(r) => r,
    Err(_) => return tag.into(),
  };
  if repo.revparse_single(tag).is_ok() {
    return tag.into();
  }
  let mut rw = match repo.revwalk() {
    Ok(r) => r,
    Err(_) => return tag.into(),
  };
  rw.push_head().ok();
  rw.last().and_then(|r| r.ok()).map(|o| o.to_string()).unwrap_or_else(|| tag.into())
}

pub fn git_stat(p: &Path, base: &str) -> String {
  let repo = match Repository::open(p) {
    Ok(r) => r,
    Err(_) => return "0 files".into(),
  };
  let tree = match repo.revparse_single(base).and_then(|o| o.peel_to_tree()) {
    Ok(t) => t,
    Err(_) => return "0 files".into(),
  };
  let diff = match repo.diff_tree_to_workdir_with_index(Some(&tree), None) {
    Ok(d) => d,
    Err(_) => return "0 files".into(),
  };
  let s = match diff.stats() {
    Ok(s) => s,
    Err(_) => return "0 files".into(),
  };
  format!(
    "{} files, {} {}",
    s.files_changed(),
    format!("+{}", s.insertions()).green(),
    format!("-{}", s.deletions()).red()
  )
}

pub fn git_recent(p: &Path, base: &str, max: usize) -> String {
  let repo = match Repository::open(p) {
    Ok(r) => r,
    Err(_) => return String::new(),
  };
  let base_oid = match repo.revparse_single(base) {
    Ok(o) => o.id(),
    Err(_) => return String::new(),
  };
  let head = match repo.head().and_then(|h| h.peel_to_commit()) {
    Ok(c) => c,
    Err(_) => return String::new(),
  };
  let mut rw = match repo.revwalk() {
    Ok(r) => r,
    Err(_) => return String::new(),
  };
  rw.push(head.id()).ok();
  rw.hide(base_oid).ok();
  rw.filter_map(|o| o.ok())
    .filter_map(|o| repo.find_commit(o).ok())
    .take(max)
    .map(|c| {
      let id = c.id().to_string();
      let short = if id.len() >= 7 { &id[..7] } else { &id };
      format!("{} {}", short, c.message().unwrap_or("").lines().next().unwrap_or(""))
    })
    .collect::<Vec<_>>()
    .join("\n")
}

pub fn git_commits(p: &Path, base: &str) -> Result<Vec<(String, String)>, Error> {
  let repo = Repository::open(p)?;
  let base_oid = repo.revparse_single(base)?.id();
  let head = repo.head()?.peel_to_commit()?;
  let mut rw = repo.revwalk()?;
  rw.push(head.id())?;
  rw.hide(base_oid)?;
  Ok(
    rw.filter_map(|o| o.ok())
      .filter_map(|o| repo.find_commit(o).ok())
      .map(|c| (c.id().to_string(), c.message().unwrap_or("").lines().next().unwrap_or("").into()))
      .collect(),
  )
}

// =============================================================================
// Helpers
// =============================================================================

pub fn copy_dir(src: &Path, dst: &Path) -> Result<(), Error> {
  std::fs::create_dir_all(dst)?;
  for e in std::fs::read_dir(src)? {
    let e = e?;
    let ft = e.file_type()?;
    let s = e.path();
    let d = dst.join(e.file_name());
    if ft.is_symlink() {
      copy_symlink(&s, &d)?;
    } else if ft.is_dir() {
      copy_dir(&s, &d)?;
    } else if ft.is_file() {
      std::fs::copy(&s, &d)?;
    }
  }
  Ok(())
}

async fn wait_with_ctrl_c(mut child: Child) -> Result<std::process::ExitStatus, Error> {
  tokio::select! {
    res = child.wait() => Ok(res?),
    _ = signal::ctrl_c() => {
      let _ = child.kill().await;
      let _ = child.wait().await;
      Err(Error::Interrupted)
    }
  }
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> Result<(), Error> {
  let target = std::fs::read_link(src)?;
  std::os::unix::fs::symlink(target, dst)?;
  Ok(())
}
