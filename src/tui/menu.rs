use std::io;

use crossterm::{
  event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
  execute,
  terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{
  Frame, Terminal,
  backend::CrosstermBackend,
  layout::{Constraint, Layout, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Clear, List, ListItem, Paragraph},
};

use crate::{
  Error, sandbox,
  tui::{
    BORDER_GRAY, ConfirmChoice, DIM_GRAY, GREEN, MAGENTA, RED, TEXT_WHITE, YELLOW, actions, confirm_choice,
    popup::{centered_fixed, detail_empty, error_empty, popup_footer, popup_header},
  },
};

// =============================================================================
// State
// =============================================================================

#[derive(Clone, Debug)]
enum Popup {
  None,
  Detail,
  Reset { commits: Vec<(String, String)>, selected: usize },
  Continue { input: String },
  Merge,
  Error { message: String },
  Clean { projects: Vec<(String, usize)>, selected: usize },
  ConfirmClean { project: Option<String>, count: usize },
  ConfirmDelete { name: String, path: std::path::PathBuf },
}

pub enum MenuAction {
  Quit,
  Run { sandbox_path: std::path::PathBuf, iterations: u32 },
  Merged,
}

struct MenuState {
  popup: Popup,
  sandboxes: Vec<sandbox::Info>,
  projects: Vec<(String, Vec<usize>)>,
  flat: Vec<FlatItem>,
  selected: usize,
  scroll_offset: usize,
  visible_height: usize,
  input_buffer: String,
}

impl MenuState {
  fn new(sandboxes: Vec<sandbox::Info>) -> Self {
    let mut project_map: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    for (idx, sb) in sandboxes.iter().enumerate() {
      let project = project_name_from(&sb.name);
      project_map.entry(project).or_default().push(idx);
    }

    let mut projects: Vec<(String, Vec<usize>)> = project_map.into_iter().collect();
    projects.sort_by(|a, b| {
      let newest_a = a.1.iter().filter_map(|&i| sandboxes.get(i)).map(|s| s.created).max();
      let newest_b = b.1.iter().filter_map(|&i| sandboxes.get(i)).map(|s| s.created).max();
      newest_b.cmp(&newest_a)
    });

    let flat = build_flat(&projects);

    Self {
      popup: Popup::None,
      sandboxes,
      projects,
      flat,
      selected: 0,
      scroll_offset: 0,
      visible_height: 10,
      input_buffer: String::new(),
    }
  }

  fn selected_sandbox(&self) -> Option<&sandbox::Info> {
    if let Some(FlatItem::Sandbox(idx)) = self.flat.get(self.selected) { self.sandboxes.get(*idx) } else { None }
  }

  fn move_up(&mut self) {
    if self.flat.is_empty() {
      return;
    }
    loop {
      if self.selected == 0 {
        self.selected = self.flat.len().saturating_sub(1);
      } else {
        self.selected -= 1;
      }
      if matches!(self.flat.get(self.selected), Some(FlatItem::Sandbox(_))) {
        break;
      }
      if self.flat.iter().all(|i| matches!(i, FlatItem::Project(_))) {
        break;
      }
    }
    self.adjust_scroll();
  }

  fn move_down(&mut self) {
    if self.flat.is_empty() {
      return;
    }
    loop {
      self.selected = (self.selected + 1) % self.flat.len();
      if matches!(self.flat.get(self.selected), Some(FlatItem::Sandbox(_))) {
        break;
      }
      if self.flat.iter().all(|i| matches!(i, FlatItem::Project(_))) {
        break;
      }
    }
    self.adjust_scroll();
  }

  fn adjust_scroll(&mut self) {
    if self.selected < self.scroll_offset {
      // keep project header visible when scrolling up
      let mut target = self.selected;
      if target > 0 && matches!(self.flat.get(target - 1), Some(FlatItem::Project(_))) {
        target -= 1;
      }
      self.scroll_offset = target;
    } else if self.selected >= self.scroll_offset + self.visible_height {
      self.scroll_offset = self.selected.saturating_sub(self.visible_height) + 1;
    }

    if self.scroll_offset > 0
      && matches!(self.flat.get(self.scroll_offset - 1), Some(FlatItem::Project(_)))
      && matches!(self.flat.get(self.scroll_offset), Some(FlatItem::Sandbox(_)))
    {
      // include header when the first visible item is a sandbox
      self.scroll_offset -= 1;
    }
  }

  fn sandbox_number(&self, sb_idx: &usize) -> usize {
    let mut num = 0;
    for (_, indices) in &self.projects {
      for &idx in indices {
        num += 1;
        if idx == *sb_idx {
          return num;
        }
      }
    }
    num
  }

  fn select_by_number(&mut self, num: usize) -> bool {
    let mut sandbox_num = 0;
    for (i, item) in self.flat.iter().enumerate() {
      if matches!(item, FlatItem::Sandbox(_)) {
        sandbox_num += 1;
        if sandbox_num == num {
          self.selected = i;
          self.adjust_scroll();
          return true;
        }
      }
    }
    false
  }

  fn sandbox_count(&self) -> usize {
    self.sandboxes.len()
  }

  fn project_summaries(&self) -> Vec<(String, usize)> {
    self.projects.iter().map(|(name, indices)| (name.clone(), indices.len())).collect()
  }

  fn select_first_sandbox(&mut self) {
    if let Some((idx, _)) = self.flat.iter().enumerate().find(|(_, item)| matches!(item, FlatItem::Sandbox(_))) {
      self.selected = idx;
    }
  }
}

#[derive(Clone, Debug)]
enum FlatItem {
  Project(String),
  Sandbox(usize),
}

// =============================================================================
// Entry point
// =============================================================================

pub async fn run() -> Result<MenuAction, Error> {
  let sandboxes = sandbox::list()?;
  if sandboxes.is_empty() {
    println!("No active sandboxes.");
    println!("Run 'so run' to start a new sandbox.");
    return Ok(MenuAction::Quit);
  }

  if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
    return Err(Error::MenuRequiresTerminal);
  }

  enable_raw_mode().map_err(|e| Error::RawModeEnable(e.to_string()))?;
  let mut stdout = io::stdout();
  execute!(stdout, EnterAlternateScreen).map_err(|e| Error::AlternateScreenEnter(e.to_string()))?;
  let backend = CrosstermBackend::new(stdout);
  let mut terminal = Terminal::new(backend).map_err(|e| Error::TerminalCreate(e.to_string()))?;

  let result = run_loop(&mut terminal, sandboxes).await;

  disable_raw_mode().ok();
  execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
  terminal.show_cursor().ok();

  result
}

async fn run_loop(
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  sandboxes: Vec<sandbox::Info>,
) -> Result<MenuAction, Error> {
  let mut state = MenuState::new(sandboxes);
  state.select_first_sandbox();

  let mut event_stream = EventStream::new();

  loop {
    terminal.draw(|f| render(f, &mut state))?;

    if let Some(maybe_event) = event_stream.next().await {
      match maybe_event {
        Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
          if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(MenuAction::Quit);
          }
          if let Some(action) = handle_key(&mut state, key.code, terminal)? {
            return Ok(action);
          }
        }
        _ => {}
      }
    }
  }
}

fn handle_key(
  state: &mut MenuState,
  key: KeyCode,
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<Option<MenuAction>, Error> {
  let sandbox_info = state.selected_sandbox().map(|sb| (sb.path.clone(), sb.original.clone()));
  let mut popup = std::mem::replace(&mut state.popup, Popup::None);

  let (action, replace) = match &mut popup {
    Popup::None => handle_base_key(state, key),
    Popup::Detail => handle_popup_detail(state, key, terminal, sandbox_info),
    Popup::Reset { commits, selected } => handle_popup_reset(state, key, sandbox_info, commits.as_slice(), selected),
    Popup::Continue { input } => handle_popup_continue(state, key, sandbox_info, input),
    Popup::Error { .. } => handle_popup_error(state, key),
    Popup::Merge => handle_popup_merge(state, key, sandbox_info),
    Popup::Clean { projects, selected } => handle_popup_clean(state, key, projects.as_slice(), selected),
    Popup::ConfirmClean { project, .. } => handle_popup_confirm_clean(state, key, project),
    Popup::ConfirmDelete { path, .. } => handle_popup_confirm_delete(state, key, path),
  }?;

  if let Some(next_popup) = replace {
    popup = next_popup;
  }
  state.popup = popup;
  Ok(action)
}

fn handle_base_key(state: &mut MenuState, key: KeyCode) -> Result<(Option<MenuAction>, Option<Popup>), Error> {
  let mut replace = None;
  match key {
    KeyCode::Up | KeyCode::Char('k') => {
      state.input_buffer.clear();
      state.move_up();
    }
    KeyCode::Down | KeyCode::Char('j') => {
      state.input_buffer.clear();
      state.move_down();
    }
    KeyCode::Enter => {
      if !state.input_buffer.is_empty() {
        if let Ok(num) = state.input_buffer.parse::<usize>()
          && num > 0
          && state.select_by_number(num)
        {
          replace = Some(Popup::Detail);
        }
        state.input_buffer.clear();
      } else if state.selected_sandbox().is_some() {
        replace = Some(Popup::Detail);
      }
    }
    KeyCode::Backspace => {
      state.input_buffer.pop();
    }
    KeyCode::Esc => {
      state.input_buffer.clear();
    }
    KeyCode::Char(c) if c.is_ascii_digit() => {
      state.input_buffer.push(c);
    }
    KeyCode::Char('c') if state.input_buffer.is_empty() => {
      let projects = state.project_summaries();
      replace = Some(Popup::Clean { projects, selected: 0 });
    }
    KeyCode::Char('x') if state.input_buffer.is_empty() => {
      if let Some(sb) = state.selected_sandbox() {
        replace = Some(Popup::ConfirmDelete { name: sb.name.clone(), path: sb.path.clone() });
      }
    }
    _ => {}
  }

  Ok((None, replace))
}

fn handle_popup_detail(
  _state: &mut MenuState,
  key: KeyCode,
  terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
  sandbox_info: Option<(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<(Option<MenuAction>, Option<Popup>), Error> {
  let mut replace = None;
  match key {
    KeyCode::Esc | KeyCode::Char('q') => {
      replace = Some(Popup::None);
    }
    KeyCode::Char('d') => {
      if let Some((path, _)) = &sandbox_info {
        let range = actions::diff_range(path);
        actions::suspend_and_run_git(terminal, &["diff", &range], path, false)?;
      }
    }
    KeyCode::Char('s') => {
      if let Some((path, _)) = &sandbox_info {
        actions::suspend_and_run_shell(terminal, path, false)?;
      }
    }
    KeyCode::Char('r') => {
      if let Some((path, _)) = &sandbox_info {
        let commits = sandbox::git_commits(path, sandbox::BASE_TAG).unwrap_or_default();
        if commits.is_empty() {
          replace = Some(Popup::Error { message: "No commits to reset".into() });
        } else {
          replace = Some(Popup::Reset { commits, selected: 0 });
        }
      }
    }
    KeyCode::Char('m') => {
      replace = Some(Popup::Merge);
    }
    KeyCode::Char('c') => {
      replace = Some(Popup::Continue { input: String::new() });
    }
    _ => {}
  }
  Ok((None, replace))
}

fn handle_popup_reset(
  _state: &mut MenuState,
  key: KeyCode,
  sandbox_info: Option<(std::path::PathBuf, std::path::PathBuf)>,
  commits: &[(String, String)],
  selected: &mut usize,
) -> Result<(Option<MenuAction>, Option<Popup>), Error> {
  let mut replace = None;
  match key {
    KeyCode::Up | KeyCode::Char('k') => {
      if *selected > 0 {
        *selected -= 1;
      }
    }
    KeyCode::Down | KeyCode::Char('j') => {
      if *selected < commits.len().saturating_sub(1) {
        *selected += 1;
      }
    }
    KeyCode::Enter => {
      if let Some((hash, _)) = commits.get(*selected)
        && let Some((path, _)) = &sandbox_info
      {
        let hash = hash.clone();
        let path = path.clone();
        let ok = actions::git_reset_hard(&path, &hash);
        if ok {
          replace = Some(Popup::Detail);
        } else {
          replace = Some(Popup::Error { message: "Reset failed".into() });
        }
      }
    }
    KeyCode::Esc | KeyCode::Char('q') => {
      replace = Some(Popup::Detail);
    }
    _ => {}
  }
  Ok((None, replace))
}

fn handle_popup_continue(
  _state: &mut MenuState,
  key: KeyCode,
  sandbox_info: Option<(std::path::PathBuf, std::path::PathBuf)>,
  input: &mut String,
) -> Result<(Option<MenuAction>, Option<Popup>), Error> {
  let mut replace = None;
  match key {
    KeyCode::Char(c) if c.is_ascii_digit() => {
      if input.len() < 3 {
        input.push(c);
      }
    }
    KeyCode::Backspace => {
      input.pop();
    }
    KeyCode::Enter => {
      if let Some((path, _)) = sandbox_info {
        let iter = input.parse::<u32>().unwrap_or(10).max(1);
        replace = Some(Popup::None);
        return Ok((Some(MenuAction::Run { sandbox_path: path, iterations: iter }), replace));
      }
    }
    KeyCode::Esc | KeyCode::Char('q') => {
      replace = Some(Popup::Detail);
    }
    _ => {}
  }
  Ok((None, replace))
}

fn handle_popup_error(_state: &mut MenuState, key: KeyCode) -> Result<(Option<MenuAction>, Option<Popup>), Error> {
  let mut replace = None;
  match key {
    KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
      replace = Some(Popup::Detail);
    }
    _ => {}
  }
  Ok((None, replace))
}

fn handle_popup_merge(
  _state: &mut MenuState,
  key: KeyCode,
  sandbox_info: Option<(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<(Option<MenuAction>, Option<Popup>), Error> {
  let mut replace = None;
  match confirm_choice(key) {
    Some(ConfirmChoice::Yes) => {
      if let Some((sandbox_path, orig)) = sandbox_info {
        match actions::merge_sandbox(&sandbox_path, &orig) {
          Ok(()) => {
            replace = Some(Popup::None);
            return Ok((Some(MenuAction::Merged), replace));
          }
          Err(err) => {
            replace = Some(Popup::Error { message: err.to_string() });
            return Ok((None, replace));
          }
        }
      }
    }
    Some(ConfirmChoice::No) => {
      replace = Some(Popup::Detail);
    }
    None => {}
  }
  Ok((None, replace))
}

fn handle_popup_clean(
  _state: &mut MenuState,
  key: KeyCode,
  projects: &[(String, usize)],
  selected: &mut usize,
) -> Result<(Option<MenuAction>, Option<Popup>), Error> {
  let mut replace = None;
  match key {
    KeyCode::Up | KeyCode::Char('k') => {
      if *selected > 0 {
        *selected -= 1;
      }
    }
    KeyCode::Down | KeyCode::Char('j') => {
      if *selected < projects.len().saturating_sub(1) {
        *selected += 1;
      }
    }
    KeyCode::Enter => {
      if let Some((project, count)) = projects.get(*selected) {
        replace = Some(Popup::ConfirmClean { project: Some(project.clone()), count: *count });
      }
    }
    KeyCode::Char('a' | 'A') => {
      let total: usize = projects.iter().map(|(_, c)| c).sum();
      replace = Some(Popup::ConfirmClean { project: None, count: total });
    }
    KeyCode::Esc | KeyCode::Char('q') => {
      replace = Some(Popup::None);
    }
    _ => {}
  }
  Ok((None, replace))
}

fn handle_popup_confirm_clean(
  state: &mut MenuState,
  key: KeyCode,
  project: &mut Option<String>,
) -> Result<(Option<MenuAction>, Option<Popup>), Error> {
  let mut replace = None;
  match key {
    KeyCode::Char('y' | 'Y') => {
      match project {
        Some(proj) => {
          for sb in &state.sandboxes {
            let sb_project = project_name_from(&sb.name);
            if &sb_project == proj {
              let _ = std::fs::remove_dir_all(&sb.path);
            }
          }
        }
        None => {
          for sb in &state.sandboxes {
            let _ = std::fs::remove_dir_all(&sb.path);
          }
        }
      }
      if let Some(action) = refresh_sandboxes_after_delete(state) {
        return Ok((Some(action), Some(Popup::None)));
      }
      replace = Some(Popup::None);
    }
    KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => {
      let projects = state.project_summaries();
      replace = Some(Popup::Clean { projects, selected: 0 });
    }
    _ => {}
  }
  Ok((None, replace))
}

fn handle_popup_confirm_delete(
  state: &mut MenuState,
  key: KeyCode,
  path: &mut std::path::PathBuf,
) -> Result<(Option<MenuAction>, Option<Popup>), Error> {
  let mut replace = None;
  match key {
    KeyCode::Char('y' | 'Y') => {
      let _ = std::fs::remove_dir_all(path);
      if let Some(action) = refresh_sandboxes_after_delete(state) {
        return Ok((Some(action), Some(Popup::None)));
      }
      replace = Some(Popup::None);
    }
    KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => {
      replace = Some(Popup::None);
    }
    _ => {}
  }
  Ok((None, replace))
}

// =============================================================================
// Rendering
// =============================================================================

fn render(frame: &mut Frame, state: &mut MenuState) {
  let area = frame.area();
  let width = area.width as usize;

  render_list(frame, area, state, width);

  if !matches!(state.popup, Popup::None) {
    render_popup(frame, area, state);
  }
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut MenuState, width: usize) {
  let [header_area, content_area, footer_area] =
    Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(area);

  state.visible_height = content_area.height as usize;
  // re-adjust after resize
  state.adjust_scroll();

  // header
  let label = " sandboxes ";
  let dashes = width.saturating_sub(label.chars().count() + 2);
  let header = Line::from(vec![
    Span::styled("┌", Style::default().fg(BORDER_GRAY)),
    Span::styled(label, Style::default().fg(TEXT_WHITE)),
    Span::styled("─".repeat(dashes), Style::default().fg(BORDER_GRAY)),
    Span::styled("┐", Style::default().fg(BORDER_GRAY)),
  ]);
  frame.render_widget(Paragraph::new(header), header_area);

  // content
  let visible_height = content_area.height as usize;

  let items: Vec<ListItem> = state
    .flat
    .iter()
    .enumerate()
    .skip(state.scroll_offset)
    .take(visible_height)
    .map(|(idx, item)| match item {
      FlatItem::Project(name) => project_line(name, width),
      FlatItem::Sandbox(sb_idx) => {
        let sb = &state.sandboxes[*sb_idx];
        let selected = idx == state.selected;
        let flat_idx = state.sandbox_number(sb_idx);
        sandbox_line(sb, width, flat_idx, selected)
      }
    })
    .collect();

  let mut all_items = items;
  while all_items.len() < visible_height {
    all_items.push(empty_line(width));
  }

  frame.render_widget(List::new(all_items), content_area);

  // footer
  let count = state.sandbox_count();
  let keys = if state.input_buffer.is_empty() {
    format!(" [1-{count}] select │ x delete │ c clean │ ^C quit ")
  } else {
    format!(" [{}] │ Enter select │ Esc back ", state.input_buffer)
  };
  let keys_len = keys.chars().count();
  let dashes = width.saturating_sub(keys_len + 2);
  let footer = Line::from(vec![
    Span::styled("└", Style::default().fg(BORDER_GRAY)),
    Span::styled(keys, Style::default().fg(if state.input_buffer.is_empty() { DIM_GRAY } else { TEXT_WHITE })),
    Span::styled("─".repeat(dashes), Style::default().fg(BORDER_GRAY)),
    Span::styled("┘", Style::default().fg(BORDER_GRAY)),
  ]);
  frame.render_widget(Paragraph::new(footer), footer_area);
}

fn render_popup(frame: &mut Frame, area: Rect, state: &MenuState) {
  match &state.popup {
    Popup::None => {}
    Popup::Detail => render_detail_popup(frame, area, state),
    Popup::Reset { commits, selected } => super::popup::render_reset_popup(frame, area, commits, *selected),
    Popup::Continue { input } => super::popup::render_continue_popup(frame, area, input),
    Popup::Merge => render_merge_popup(frame, area, state),
    Popup::Error { message } => super::popup::render_error_popup(frame, area, message),
    Popup::Clean { projects, selected } => render_clean_popup(frame, area, projects, *selected),
    Popup::ConfirmClean { project, count } => render_confirm_popup(frame, area, project.as_deref(), *count),
    Popup::ConfirmDelete { name, .. } => render_confirm_delete_popup(frame, area, name),
  }
}

fn render_detail_popup(frame: &mut Frame, area: Rect, state: &MenuState) {
  let Some(sb) = state.selected_sandbox() else { return };

  let time_str = fmt_age(sb.created.elapsed().map(|d| d.as_secs() / 60).unwrap_or(0));
  let data = super::popup::DetailPopupData {
    path: &sb.path,
    insertions: sb.insertions,
    deletions: sb.deletions,
    files_changed: sb.files_changed,
    commit_count: sb.commit_count,
    time_str,
  };
  super::popup::render_detail_popup(frame, area, &data);
}

fn render_merge_popup(frame: &mut Frame, area: Rect, state: &MenuState) {
  let Some(sb) = state.selected_sandbox() else { return };
  super::popup::render_merge_popup(
    frame,
    area,
    &sb.original,
    sb.files_changed,
    sb.insertions,
    sb.deletions,
    sb.commit_count,
  );
}

fn render_clean_popup(frame: &mut Frame, area: Rect, projects: &[(String, usize)], selected: usize) {
  // clean popup
  let height = (projects.len() + 4).clamp(14, 20) as u16;
  let popup_area = centered_fixed(55, height, area);
  let width = popup_area.width as usize;

  frame.render_widget(Clear, popup_area);

  let mut lines: Vec<Line> = Vec::new();

  lines.push(popup_header(" clean ", width, BORDER_GRAY));

  lines.push(detail_empty(width));

  for (idx, (name, count)) in projects.iter().enumerate() {
    let bg = if idx == selected { Color::Rgb(40, 40, 50) } else { Color::Reset };
    let count_word = if *count == 1 { "sandbox" } else { "sandboxes" };
    let info = format!("({count} {count_word})");
    let name_display = if name.chars().count() > 20 {
      format!("{}...", name.chars().take(17).collect::<String>())
    } else {
      name.clone()
    };
    let content_len = 1 + name_display.chars().count() + 2 + info.chars().count();
    let padding = width.saturating_sub(content_len + 2);

    lines.push(Line::from(vec![
      Span::styled("│", Style::default().fg(BORDER_GRAY)),
      Span::styled(format!(" {name_display}"), Style::default().fg(TEXT_WHITE).bg(bg)),
      Span::styled("  ", Style::default().bg(bg)),
      Span::styled(info, Style::default().fg(DIM_GRAY).bg(bg)),
      Span::styled(" ".repeat(padding), Style::default().bg(bg)),
      Span::styled("│", Style::default().fg(BORDER_GRAY)),
    ]));
  }

  let target_lines = (height as usize).saturating_sub(1);
  while lines.len() < target_lines {
    lines.push(detail_empty(width));
  }

  lines.push(popup_footer(" Enter delete │ a all │ Esc back ", width, BORDER_GRAY));

  for (i, line) in lines.into_iter().enumerate() {
    let line_area = Rect { x: popup_area.x, y: popup_area.y + i as u16, width: popup_area.width, height: 1 };
    frame.render_widget(Paragraph::new(line), line_area);
  }
}

fn render_confirm_popup(frame: &mut Frame, area: Rect, project: Option<&str>, count: usize) {
  // confirm popup
  let popup_area = centered_fixed(55, 5, area);
  let width = popup_area.width as usize;

  frame.render_widget(Clear, popup_area);

  let mut lines: Vec<Line> = Vec::new();

  lines.push(popup_header(" confirm ", width, RED));

  lines.push(error_empty(width));

  let sandbox_word = if count == 1 { "sandbox" } else { "sandboxes" };
  let count_part = format!(" ({count} {sandbox_word})");
  let (name_part, name_len) = match project {
    Some(p) => (p.to_string(), p.chars().count()),
    None => ("all".to_string(), 3),
  };
  let content_len = 9 + name_len + 1 + count_part.chars().count();
  let padding = width.saturating_sub(content_len + 2);

  lines.push(Line::from(vec![
    Span::styled("│", Style::default().fg(RED)),
    Span::styled("  Delete ", Style::default().fg(TEXT_WHITE)),
    Span::styled(name_part, Style::default().fg(TEXT_WHITE).add_modifier(Modifier::BOLD)),
    Span::styled("?", Style::default().fg(TEXT_WHITE)),
    Span::styled(count_part, Style::default().fg(DIM_GRAY)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(RED)),
  ]));

  lines.push(error_empty(width));

  lines.push(popup_footer(" y yes │ n no ", width, RED));

  for (i, line) in lines.into_iter().enumerate() {
    let line_area = Rect { x: popup_area.x, y: popup_area.y + i as u16, width: popup_area.width, height: 1 };
    frame.render_widget(Paragraph::new(line), line_area);
  }
}

fn render_confirm_delete_popup(frame: &mut Frame, area: Rect, name: &str) {
  // confirm delete popup
  let popup_area = centered_fixed(55, 5, area);
  let width = popup_area.width as usize;

  frame.render_widget(Clear, popup_area);

  let mut lines: Vec<Line> = Vec::new();

  lines.push(popup_header(" confirm ", width, RED));

  lines.push(error_empty(width));

  let name_len = name.chars().count();
  let content_len = 9 + name_len + 1;
  let padding = width.saturating_sub(content_len + 2);

  lines.push(Line::from(vec![
    Span::styled("│", Style::default().fg(RED)),
    Span::styled("  Delete ", Style::default().fg(TEXT_WHITE)),
    Span::styled(name.to_string(), Style::default().fg(TEXT_WHITE).add_modifier(Modifier::BOLD)),
    Span::styled("?", Style::default().fg(TEXT_WHITE)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(RED)),
  ]));

  lines.push(error_empty(width));

  lines.push(popup_footer(" y yes │ n no ", width, RED));

  for (i, line) in lines.into_iter().enumerate() {
    let line_area = Rect { x: popup_area.x, y: popup_area.y + i as u16, width: popup_area.width, height: 1 };
    frame.render_widget(Paragraph::new(line), line_area);
  }
}

// =============================================================================
// Helpers
// =============================================================================

fn empty_line(width: usize) -> ListItem<'static> {
  let padding = width.saturating_sub(2);
  ListItem::new(Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ]))
}

fn project_line(name: &str, width: usize) -> ListItem<'static> {
  let inner = format!("  {name} ");
  let padding = width.saturating_sub(inner.chars().count() + 2);
  ListItem::new(Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::styled(inner, Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ]))
}

fn sandbox_line(sb: &sandbox::Info, width: usize, flat_idx: usize, selected: bool) -> ListItem<'static> {
  let status_color = match sb.status.as_str() {
    "done" => GREEN,
    "blocked" => RED,
    "pending" => YELLOW,
    _ => DIM_GRAY,
  };

  let age = sb.created.elapsed().map(|d| d.as_secs() / 60).unwrap_or(0);
  let age_str = format!("{:<6}", fmt_age(age));

  // fixed-width columns: indent, num, name, status, age
  let name_width = width.saturating_sub(34).max(20);
  let num_part = format!("{flat_idx:>2}.");

  let name_display = if sb.name.chars().count() > name_width {
    let truncated: String = sb.name.chars().take(name_width.saturating_sub(3)).collect();
    format!("{truncated}...")
  } else {
    format!("{:<width$}", sb.name, width = name_width)
  };

  let bg = if selected { Color::Rgb(40, 40, 50) } else { Color::Reset };
  let name_style = Style::default().fg(TEXT_WHITE).bg(bg);
  let num_style = Style::default().fg(DIM_GRAY).bg(bg);
  let status_style = Style::default().fg(status_color).bg(bg);
  let dim_style = Style::default().fg(DIM_GRAY).bg(bg);

  let used = 2 + 4 + 6 + name_width + 2 + 10 + 6;
  let padding = width.saturating_sub(used);

  ListItem::new(Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::styled("    ", Style::default().bg(bg)),
    Span::styled(format!("{num_part}   "), num_style),
    Span::styled(name_display, name_style),
    Span::styled("  ", Style::default().bg(bg)),
    Span::styled(format!("{:<10}", sb.status), status_style),
    Span::styled(age_str, dim_style),
    Span::styled(" ".repeat(padding), Style::default().bg(bg)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ]))
}

fn build_flat(projects: &[(String, Vec<usize>)]) -> Vec<FlatItem> {
  let mut items = Vec::new();
  for (project, indices) in projects {
    items.push(FlatItem::Project(project.clone()));
    for &idx in indices {
      items.push(FlatItem::Sandbox(idx));
    }
  }
  items
}

fn project_name_from(name: &str) -> String {
  name
    .strip_prefix("sandbox-")
    .and_then(|s| s.rsplit_once('-'))
    .map_or_else(|| name.to_string(), |(p, _)| p.to_string())
}

fn refresh_sandboxes_after_delete(state: &mut MenuState) -> Option<MenuAction> {
  state.sandboxes = sandbox::list().unwrap_or_default();
  if state.sandboxes.is_empty() {
    return Some(MenuAction::Quit);
  }
  *state = MenuState::new(state.sandboxes.clone());
  state.select_first_sandbox();
  None
}

fn fmt_age(mins: u64) -> String {
  if mins < 60 {
    format!("{mins}m")
  } else if mins < 1440 {
    format!("{}h", mins / 60)
  } else {
    format!("{}d", mins / 1440)
  }
}
