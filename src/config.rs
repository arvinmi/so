use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn dir() -> PathBuf {
  dirs::home_dir().unwrap_or_else(|| PathBuf::from("~")).join(".config/so")
}

// global config options
#[derive(Deserialize, Default)]
pub struct Config {
  pub harness: Option<String>,
  pub iterations: Option<u32>,
  pub sandbox: Option<String>,
  pub model: Option<String>,
  pub effort: Option<String>,
}

// loads config from ~/.config/so/config.toml, returns defaults if missing or invalid
pub fn load() -> Config {
  std::fs::read_to_string(dir().join("config.toml")).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

// per sandbox metadata
#[derive(Serialize, Deserialize)]
pub struct SandboxMeta {
  pub original: String,
  pub harness: String,
  pub sandbox: String,
  pub task_id: String,
}

fn sandbox_dir(name: &str) -> PathBuf {
  dir().join("sandboxes").join(name)
}

pub fn write_meta(name: &str, meta: &SandboxMeta) {
  let d = sandbox_dir(name);
  let _ = std::fs::create_dir_all(&d);
  let _ = std::fs::write(d.join("metadata.toml"), toml::to_string(meta).unwrap_or_default());
}

pub fn read_meta(name: &str) -> Option<SandboxMeta> {
  std::fs::read_to_string(sandbox_dir(name).join("metadata.toml")).ok().and_then(|s| toml::from_str(&s).ok())
}

// removes sandbox metadata whose /tmp dir no longer exists.
pub fn prune_stale() {
  let sb_dir = dir().join("sandboxes");
  let Ok(entries) = std::fs::read_dir(&sb_dir) else { return };
  for entry in entries.flatten() {
    let name = entry.file_name();
    if !PathBuf::from("/tmp").join(&name).exists() {
      let _ = std::fs::remove_dir_all(entry.path());
    }
  }
}
