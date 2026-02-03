use std::{
  collections::HashSet,
  ffi::{OsStr, OsString},
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum SandboxType {
  #[default]
  Docker,
  Bwrap,
}

impl SandboxType {
  pub fn as_str(self) -> &'static str {
    match self {
      SandboxType::Docker => "docker",
      SandboxType::Bwrap => "bwrap",
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub enum Mode {
  Run,
  Clean,
  Dup,
}

fn project_name(p: &Path) -> &OsStr {
  p.file_name().unwrap_or_else(|| OsStr::new("project"))
}

// =============================================================================
// Sandbox
// =============================================================================

pub struct Sandbox {
  pub path: PathBuf,
  pub original: PathBuf,
  pub branch: String,
  pub task_id: String,
}

impl Sandbox {
  pub fn new(original: &Path, mode: Mode, prompt: Option<&str>) -> Result<Self, Error> {
    let project = project_name(original).to_string_lossy();
    let ts = chrono::Utc::now().timestamp();
    let path = PathBuf::from(format!("/tmp/sandbox-{}-{}", project, ts));
    let task_id = format!("so-{}-{}", project, ts);

    copy_dir(original, &path)?;
    let branch = setup_git(&path, original)?;
    setup_specs(&path, prompt, mode)?;

    Ok(Self { path, original: original.to_path_buf(), branch, task_id })
  }
}

pub struct Info {
  pub name: String,
  pub path: PathBuf,
  pub original: PathBuf,
  pub created: std::time::SystemTime,
  pub status: String,
}

fn parse_sandbox_timestamp(name: &str) -> Option<u64> {
  let rest = name.strip_prefix("sandbox-")?;
  let (project, ts) = rest.rsplit_once('-')?;
  if project.is_empty() || ts.is_empty() {
    return None;
  }
  if !ts.as_bytes().iter().all(|b| b.is_ascii_digit()) {
    return None;
  }
  ts.parse::<u64>().ok()
}

pub fn list() -> Result<Vec<Info>, Error> {
  let mut out = Vec::new();
  for e in std::fs::read_dir("/tmp")? {
    let e = e?;
    let name = e.file_name().to_string_lossy().to_string();
    if name.starts_with("sandbox-") {
      let path = e.path();
      let fallback = e.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
      let created = parse_sandbox_timestamp(&name)
        .and_then(|ts| std::time::SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(ts)))
        .unwrap_or(fallback);
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
      let original = Repository::open(&path)
        .ok()
        .and_then(|r| r.config().ok())
        .and_then(|c| c.get_string("so.original").ok())
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|| PathBuf::from("."));
      out.push(Info { name, path, original, created, status: status.into() });
    }
  }
  out.sort_by(|a, b| b.created.cmp(&a.created));
  Ok(out)
}

// =============================================================================
// Docker
// =============================================================================

pub struct DockerContainer {
  pub id: String,
  pub workdir: String,
  exec_user: Option<String>,
  creds: PathBuf,
}

impl DockerContainer {
  pub fn exec_cmd_tty(&self, program: &str, tty: bool) -> Command {
    let mut cmd = Command::new("docker");
    if tty {
      cmd.args(["exec", "-it"]);
    } else {
      cmd.args(["exec", "-i"]);
    }
    if let Some(user) = &self.exec_user {
      cmd.args(["-u", user]);
    }
    cmd.args(["-w", &self.workdir, &self.id, program]);
    cmd
  }

  pub async fn stop(&self) {
    let _ = Command::new("docker").args(["stop", "-t", "2", &self.id]).output().await;
    let _ = Command::new("docker").args(["rm", "-f", &self.id]).output().await;
    let _ = std::fs::remove_dir_all(&self.creds);
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuStatus {
  Available,
  MissingToolkit,
  NoGpu,
}

pub fn check_gpu() -> GpuStatus {
  if !has_nvidia_driver() {
    return GpuStatus::NoGpu;
  }
  if has_nvidia_runtime() || has_cdi_nvidia() { GpuStatus::Available } else { GpuStatus::MissingToolkit }
}

fn has_gpu() -> bool {
  check_gpu() == GpuStatus::Available
}

fn has_nvidia_driver() -> bool {
  std::process::Command::new("nvidia-smi")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

fn has_nvidia_runtime() -> bool {
  std::process::Command::new("docker")
    .args(["info", "--format", "{{json .Runtimes}}"])
    .output()
    .ok()
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .is_some_and(|s| s.contains("nvidia"))
}

fn has_cdi_nvidia() -> bool {
  std::fs::read_dir("/var/run/cdi")
    .into_iter()
    .flatten()
    .flatten()
    .any(|e| e.file_name().to_str().is_some_and(|n| n.contains("nvidia")))
}

fn get_uid() -> u32 {
  unsafe { libc::getuid() }
}

fn get_gid() -> u32 {
  unsafe { libc::getgid() }
}

async fn image_is_fresh(image: &str, dockerfile: &Path) -> bool {
  let created = Command::new("docker")
    .args(["inspect", "-f", "{{.Created}}", image])
    .output()
    .await
    .ok()
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
    .map(|dt| dt.with_timezone(&chrono::Utc));
  let modified =
    std::fs::metadata(dockerfile).ok().and_then(|m| m.modified().ok()).map(chrono::DateTime::<chrono::Utc>::from);
  matches!((created, modified), (Some(c), Some(m)) if c >= m)
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
    (".local/state/opencode", ".local/state/opencode"),
  ] {
    let p = creds.join(src);
    if p.exists() {
      cmd.arg("-v").arg(format!("{}:{}/{}", p.display(), home, dst));
    }
  }
}

fn add_env(cmd: &mut Command, key: &str, value: &str) {
  cmd.args(["-e", &format!("{}={}", key, value)]);
}

fn add_env_if_set(cmd: &mut Command, key: &str) {
  if let Ok(value) = std::env::var(key) {
    add_env(cmd, key, &value);
  }
}

fn add_user(cmd: &mut Command) {
  if cfg!(target_os = "linux") {
    cmd.arg("--user").arg(format!("{}:{}", get_uid(), get_gid()));
  }
}

pub async fn start_docker(sb: &Sandbox) -> Result<DockerContainer, Error> {
  let project = project_name(&sb.original).to_string_lossy();
  let dockerfile = sb.original.join("Dockerfile.sandbox");
  if !dockerfile.exists() {
    return Err(Error::NoDockerfile);
  }

  let home = "/home/ubuntu".to_string();
  let code = format!("{}/{}", home, project);
  let image = format!("sandbox-{}", project.to_lowercase());

  // rebuild image if dockerfile changed
  if image_is_fresh(&image, &dockerfile).await {
    println!("{}", "▶ Using cached image".yellow().bold());
  } else {
    println!("{}", "▶ Building image".yellow().bold());
    let mut cmd = Command::new("docker");
    cmd.args(["build", "-q", "-t", &image, "-f"]).arg(&dockerfile).arg(&sb.original);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    if !cmd.status().await?.success() {
      return Err(Error::Docker("build failed".into()));
    }
  }

  let creds = setup_creds()?;
  let mut cmd = Command::new("docker");
  cmd.args(["run", "-d", "--network", "host"]);

  if cfg!(target_os = "linux") && has_gpu() {
    cmd.args(["--gpus", "all"]);
  }

  add_user(&mut cmd);
  cmd.args(["-v", "/etc/localtime:/etc/localtime:ro"]);
  cmd.arg("-v").arg(format!("{}:{}", sb.path.display(), code));

  let gc = sb.path.join(".git/config");
  if gc.exists() {
    cmd.arg("-v").arg(format!("{}:{}/.git/config:ro", gc.display(), code));
  }

  mount_creds(&mut cmd, &creds, &home);

  add_env(&mut cmd, "CLAUDE_CODE_TASK_LIST_ID", &sb.task_id);
  add_env(&mut cmd, "HOME", &home);
  add_env(&mut cmd, "XDG_CONFIG_HOME", &format!("{}/.config", home));
  add_env(&mut cmd, "XDG_DATA_HOME", &format!("{}/.local/share", home));
  add_env(&mut cmd, "OPENCODE_PERMISSION", r#"{"*":"allow"}"#);
  add_env_if_set(&mut cmd, "MODEL");
  add_env_if_set(&mut cmd, "EFFORT");

  cmd.args(["-w", &code]).arg(&image);
  cmd.args(["tail", "-f", "/dev/null"]);
  cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());

  let output = match cmd.output().await {
    Ok(o) => o,
    Err(e) => {
      let _ = std::fs::remove_dir_all(&creds);
      return Err(Error::Docker(format!("spawn failed: {}", e)));
    }
  };

  if !output.status.success() {
    let _ = std::fs::remove_dir_all(&creds);
    return Err(Error::Docker("container start failed".into()));
  }

  let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
  if id.is_empty() {
    let _ = std::fs::remove_dir_all(&creds);
    return Err(Error::Docker("no container id returned".into()));
  }

  let exec_user = if cfg!(target_os = "linux") { None } else { Some("1000:1000".into()) };

  Ok(DockerContainer { id, workdir: code, exec_user, creds })
}

// =============================================================================
// Bwrap (linux only)
// =============================================================================

pub async fn run_bwrap(sb: &Sandbox, harness: &str, iterations: u32) -> Result<(), Error> {
  let home = dirs::home_dir().ok_or(Error::NoHome)?;
  let code = home.join(project_name(&sb.original));
  let creds = setup_creds()?;

  let mut a: Vec<OsString> = Vec::new();
  let mut created_dirs: HashSet<PathBuf> = HashSet::new();

  // system (ro)
  ro(&mut a, Path::new("/usr"));
  ro(&mut a, Path::new("/lib"));
  ro(&mut a, Path::new("/bin"));
  if_exists_ro(&mut a, Path::new("/lib64"));
  if_exists_ro(&mut a, Path::new("/sbin"));
  if_exists_ro(&mut a, Path::new("/snap"));

  // network and ssl (ro)
  ro(&mut a, Path::new("/etc/resolv.conf"));
  ro(&mut a, Path::new("/etc/hosts"));
  ro(&mut a, Path::new("/etc/passwd"));
  ro(&mut a, Path::new("/etc/group"));
  if_exists_ro(&mut a, Path::new("/etc/ssl"));
  if_exists_ro(&mut a, Path::new("/etc/ca-certificates"));

  // home tmpfs
  check_dir(&mut a, &mut created_dirs, &home);
  push_arg(&mut a, "--tmpfs");
  push_path(&mut a, &home);

  // tools (ro)
  for d in [".nvm", ".cargo", ".rustup", ".pyenv", ".deno", "go", ".sdkman", ".ssh"] {
    if_exists_ro(&mut a, &home.join(d));
  }

  // conda (ro)
  let conda = ["miniforge3", "anaconda3", "miniconda3"];
  for d in &conda {
    if_exists_ro(&mut a, &home.join(d));
  }

  // writable tool dirs (tmpfs with optional ro overlays)
  for (dir, ro_subs) in [(".bun", &[] as &[&str]), (".local", &["bin", "share"])] {
    let p = home.join(dir);
    check_dir(&mut a, &mut created_dirs, &p);
    push_arg(&mut a, "--tmpfs");
    push_path(&mut a, &p);
    for sub in ro_subs {
      if_exists_ro(&mut a, &p.join(sub));
    }
  }

  // caches (tmpfs)
  for d in [".cache", ".npm", ".conda"] {
    let p = home.join(d);
    check_dir(&mut a, &mut created_dirs, &p);
    push_arg(&mut a, "--tmpfs");
    push_path(&mut a, &p);
  }
  push_arg(&mut a, "--tmpfs");
  push_path(&mut a, Path::new("/tmp"));
  push_arg(&mut a, "--tmpfs");
  push_path(&mut a, Path::new("/var"));

  // agent configs (rw)
  check_dir(&mut a, &mut created_dirs, &home.join(".config"));
  check_dir(&mut a, &mut created_dirs, &home.join(".local"));
  check_dir(&mut a, &mut created_dirs, &home.join(".local/share"));
  if_exists_bind(&mut a, &creds.join(".claude"), &home.join(".claude"), &mut created_dirs);
  if_exists_bind(&mut a, &creds.join(".claude.json"), &home.join(".claude.json"), &mut created_dirs);
  if_exists_bind(&mut a, &creds.join(".codex"), &home.join(".codex"), &mut created_dirs);
  if_exists_bind(&mut a, &creds.join(".config/opencode"), &home.join(".config/opencode"), &mut created_dirs);
  if_exists_bind(&mut a, &creds.join(".local/share/opencode"), &home.join(".local/share/opencode"), &mut created_dirs);
  push_arg(&mut a, "--ro-bind");
  push_path(&mut a, &creds.join(".gitconfig"));
  push_path(&mut a, &home.join(".gitconfig"));

  // workspace
  check_dir(&mut a, &mut created_dirs, &code);
  push_arg(&mut a, "--bind");
  push_path(&mut a, &sb.path);
  push_path(&mut a, &code);

  // so binary
  let so_dir = std::env::current_exe()
    .ok()
    .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    .unwrap_or_else(|| PathBuf::from("/usr/local/bin"));
  push_arg(&mut a, "--ro-bind");
  push_path(&mut a, &so_dir);
  push_path(&mut a, Path::new("/opt/so"));

  // docker socket
  if Path::new("/var/run/docker.sock").exists() {
    push_arg(&mut a, "--bind");
    push_path(&mut a, Path::new("/var/run/docker.sock"));
    push_path(&mut a, Path::new("/var/run/docker.sock"));
  }
  if_exists_ro(&mut a, &home.join(".docker"));

  push_arg(&mut a, "--dev");
  push_path(&mut a, Path::new("/dev"));
  push_arg(&mut a, "--proc");
  push_path(&mut a, Path::new("/proc"));
  push_arg(&mut a, "--unshare-pid");

  // PATH with conda
  let mut path = std::env::var("PATH").unwrap_or_default();
  for d in &conda {
    let bin = home.join(d).join("bin");
    if bin.exists() {
      path = format!("{}:{}", bin.display(), path);
    }
  }

  // env
  let h = home.display().to_string();
  for (k, v) in [
    ("HOME", h.as_str()),
    ("PATH", &path),
    ("CLAUDE_CODE_TASK_LIST_ID", &sb.task_id),
    ("SO_UNATTENDED", "1"),
    ("TMPDIR", "/tmp"),
    ("UV_CACHE_DIR", &format!("{}/.cache/uv", h)),
  ] {
    push_env(&mut a, k, v);
  }
  push_env_if_set(&mut a, "MODEL");
  push_env_if_set(&mut a, "EFFORT");

  push_arg(&mut a, "--chdir");
  push_path(&mut a, &code);
  push_arg(&mut a, "--");
  push_arg(&mut a, "/opt/so/so");
  push_arg(&mut a, "-H");
  push_arg(&mut a, harness);
  push_arg(&mut a, "step");
  push_arg(&mut a, "-i");
  push_arg(&mut a, &iterations.to_string());

  let mut cmd = Command::new("bwrap");
  cmd.args(a.iter().map(|p| p.as_os_str())).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());

  let child = match cmd.spawn() {
    Ok(c) => c,
    Err(e) => {
      let _ = std::fs::remove_dir_all(&creds);
      return Err(Error::Other(format!("bwrap spawn failed: {}", e)));
    }
  };

  let result = wait_with_ctrl_c(child).await;
  let _ = std::fs::remove_dir_all(&creds);

  match result {
    Ok(s) if s.success() => Ok(()),
    Ok(s) => Err(Error::Other(format!("bwrap exited with code {}", s.code().unwrap_or(-1)))),
    Err(e) => Err(e),
  }
}

fn push_arg(a: &mut Vec<OsString>, s: &str) {
  a.push(OsString::from(s));
}

fn push_path(a: &mut Vec<OsString>, p: &Path) {
  a.push(p.as_os_str().to_os_string());
}

fn push_env(a: &mut Vec<OsString>, key: &str, value: &str) {
  push_arg(a, "--setenv");
  push_arg(a, key);
  push_arg(a, value);
}

fn push_env_if_set(a: &mut Vec<OsString>, key: &str) {
  if let Ok(value) = std::env::var(key) {
    push_env(a, key, &value);
  }
}

fn ro(a: &mut Vec<OsString>, p: &Path) {
  push_arg(a, "--ro-bind");
  push_path(a, p);
  push_path(a, p);
}

fn if_exists_ro(a: &mut Vec<OsString>, p: &Path) {
  if p.exists() {
    ro(a, p);
  }
}

fn if_exists_bind(a: &mut Vec<OsString>, src: &Path, dst: &Path, created_dirs: &mut HashSet<PathBuf>) {
  if src.exists() {
    if let Some(parent) = dst.parent() {
      check_dir(a, created_dirs, parent);
    }
    if src.is_dir() {
      check_dir(a, created_dirs, dst);
    }
    push_arg(a, "--bind");
    push_path(a, src);
    push_path(a, dst);
  }
}

fn check_dir(a: &mut Vec<OsString>, created: &mut HashSet<PathBuf>, path: &Path) {
  if created.insert(path.to_path_buf()) {
    push_arg(a, "--dir");
    push_path(a, path);
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
  std::fs::create_dir_all(creds.join(".local/state/opencode"))?;

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

  // for macos, claude stores credentials in keychain, extract them for docker
  #[cfg(target_os = "macos")]
  if let Some(output) = std::process::Command::new("security")
    .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
    .output()
    .ok()
    .filter(|o| o.status.success())
    .filter(|_| !creds.join(".claude/.credentials.json").exists())
  {
    let claude_dir = creds.join(".claude");
    let _ = std::fs::create_dir_all(&claude_dir);
    let _ = std::fs::write(claude_dir.join(".credentials.json"), &output.stdout);
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

fn setup_git(sandbox: &Path, original: &Path) -> Result<String, Error> {
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
  let mut cfg = repo.config()?;
  cfg.set_str("receive.denyCurrentBranch", "updateInstead")?;
  cfg.set_str("so.original", &original.display().to_string())?;

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
