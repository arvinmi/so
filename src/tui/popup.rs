use std::path::Path;

use ratatui::{
  Frame,
  layout::{Constraint, Layout, Rect},
  style::{Color, Style},
  text::{Line, Span},
  widgets::{Clear, List, ListItem, Paragraph},
};

use crate::sandbox;

const BORDER_GRAY: Color = Color::Rgb(70, 70, 70);
const DIM_GRAY: Color = Color::Rgb(110, 110, 110);
const TEXT_WHITE: Color = Color::Rgb(220, 220, 220);
const GREEN: Color = Color::Rgb(80, 200, 120);
const RED: Color = Color::Rgb(220, 80, 80);
const YELLOW: Color = Color::Rgb(220, 180, 50);

pub struct DetailPopupData<'a> {
  pub path: &'a Path,
  pub insertions: u32,
  pub deletions: u32,
  pub files_changed: u32,
  pub commit_count: u32,
  pub time_str: String,
}

pub fn render_detail_popup(frame: &mut Frame, area: Rect, data: &DetailPopupData) {
  let popup_area = centered_fixed(55, 8, area);
  let width = popup_area.width as usize;

  let mut lines: Vec<Line> = Vec::new();

  lines.push(popup_header("", width, BORDER_GRAY));
  lines.push(popup_empty(width));

  // path (yellow)
  let path_str = data.path.display().to_string();
  let max_path = width.saturating_sub(4);
  let path_display = if path_str.chars().count() > max_path {
    format!("{}...", path_str.chars().take(max_path.saturating_sub(3)).collect::<String>())
  } else {
    path_str
  };
  let path_content = format!(" {}", path_display);
  lines.push(popup_line(&path_content, width, YELLOW));

  // stats with colored +/-
  let plus = format!("+{}", data.insertions);
  let minus = format!("-{}", data.deletions);
  let files_word = if data.files_changed == 1 { "file" } else { "files" };
  let commits_word = if data.commit_count == 1 { "commit" } else { "commits" };
  let rest =
    format!(" ({} {})  {} {}  {}", data.files_changed, files_word, data.commit_count, commits_word, data.time_str);
  let stats_len = 1 + plus.chars().count() + 1 + minus.chars().count() + rest.chars().count();
  let stats_pad = width.saturating_sub(stats_len + 2);
  lines.push(Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::styled(format!(" {}", plus), Style::default().fg(GREEN)),
    Span::styled("/", Style::default().fg(TEXT_WHITE)),
    Span::styled(minus, Style::default().fg(RED)),
    Span::styled(rest, Style::default().fg(DIM_GRAY)),
    Span::raw(" ".repeat(stats_pad)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ]));

  lines.push(popup_empty(width));

  lines.push(popup_separator(width, BORDER_GRAY));

  let actions = " d diff  s shell  r reset  m merge  c continue";
  lines.push(popup_line(actions, width, TEXT_WHITE));

  lines.push(popup_footer(" Esc back ", width, BORDER_GRAY));

  render_lines(frame, popup_area, lines);
}

pub fn render_error_popup(frame: &mut Frame, area: Rect, message: &str) {
  let popup_area = centered_fixed(55, 7, area);
  let width = popup_area.width as usize;

  let mut lines: Vec<Line> = Vec::new();
  lines.push(popup_header(" error ", width, RED));
  lines.push(error_empty(width));
  lines.push(error_empty(width));

  let max_msg = width.saturating_sub(4);
  let msg_display = if message.chars().count() > max_msg {
    format!("{}...", message.chars().take(max_msg.saturating_sub(3)).collect::<String>())
  } else {
    message.to_string()
  };
  lines.push(error_line(&format!(" {}", msg_display), width, TEXT_WHITE));

  // empty line
  lines.push(error_empty(width));
  lines.push(error_empty(width));

  lines.push(popup_footer(" Enter ok ", width, RED));

  render_lines(frame, popup_area, lines);
}

pub fn render_reset_popup(frame: &mut Frame, area: Rect, commits: &[(String, String)], selected: usize) {
  let height = (commits.len() + 5).clamp(12, 15) as u16;
  let popup_area = centered_fixed(55, height, area);
  let width = popup_area.width as usize;

  frame.render_widget(Clear, popup_area);

  let [header_area, content_area, footer_area] =
    Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(popup_area);

  frame.render_widget(Paragraph::new(popup_header(" reset to commit ", width, BORDER_GRAY)), header_area);

  let visible = content_area.height as usize;
  let items: Vec<ListItem> = commits
    .iter()
    .enumerate()
    .take(visible)
    .map(|(idx, (hash, msg))| {
      let short_hash = if hash.len() >= 7 { &hash[..7] } else { hash };
      let max_msg = width.saturating_sub(14);
      let msg_display = if msg.chars().count() > max_msg {
        format!("{}...", msg.chars().take(max_msg.saturating_sub(3)).collect::<String>())
      } else {
        msg.clone()
      };

      let bg = if idx == selected { Color::Rgb(40, 40, 50) } else { Color::Reset };
      let hash_part = format!(" {} ", short_hash);
      let content_len = hash_part.chars().count() + msg_display.chars().count();
      let padding = width.saturating_sub(content_len + 2);

      ListItem::new(Line::from(vec![
        Span::styled("│", Style::default().fg(BORDER_GRAY)),
        Span::styled(hash_part, Style::default().fg(YELLOW).bg(bg)),
        Span::styled(msg_display, Style::default().fg(TEXT_WHITE).bg(bg)),
        Span::styled(" ".repeat(padding), Style::default().bg(bg)),
        Span::styled("│", Style::default().fg(BORDER_GRAY)),
      ]))
    })
    .collect();

  let mut all = items;
  while all.len() < visible {
    all.push(list_empty(width));
  }
  frame.render_widget(List::new(all), content_area);

  frame.render_widget(Paragraph::new(popup_footer(" Enter reset │ Esc back ", width, BORDER_GRAY)), footer_area);
}

pub fn render_continue_popup(frame: &mut Frame, area: Rect, input: &str) {
  let popup_area = centered_fixed(55, 5, area);
  let width = popup_area.width as usize;

  let mut lines: Vec<Line> = Vec::new();

  lines.push(popup_header(" continue ", width, BORDER_GRAY));
  lines.push(detail_empty(width));

  let (display, num_color) = if input.is_empty() { ("10", DIM_GRAY) } else { (input, TEXT_WHITE) };
  let prompt_text = "  Iterations? [";
  let padding = width.saturating_sub(prompt_text.chars().count() + display.chars().count() + 3);
  lines.push(Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::styled(prompt_text, Style::default().fg(TEXT_WHITE)),
    Span::styled(display, Style::default().fg(num_color)),
    Span::styled("]", Style::default().fg(TEXT_WHITE)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ]));

  lines.push(detail_empty(width));

  lines.push(popup_footer(" Enter start │ Esc back ", width, BORDER_GRAY));

  render_lines(frame, popup_area, lines);
}

pub fn render_merge_popup(
  frame: &mut Frame,
  area: Rect,
  orig: &Path,
  files_changed: u32,
  insertions: u32,
  deletions: u32,
  commit_count: u32,
) {
  let popup_area = centered_fixed(55, 7, area);
  let width = popup_area.width as usize;

  let mut lines: Vec<Line> = Vec::new();
  lines.push(popup_header(" merge ", width, BORDER_GRAY));
  lines.push(detail_empty(width));

  let orig_str = orig.display().to_string();
  let orig_str = if let Ok(home) = std::env::var("HOME") {
    if orig_str.starts_with(&home) { orig_str.replacen(&home, "~", 1) } else { orig_str }
  } else {
    orig_str
  };
  let max_orig = width.saturating_sub(4);
  let orig_display = if orig_str.chars().count() > max_orig {
    format!("{}...", orig_str.chars().take(max_orig.saturating_sub(3)).collect::<String>())
  } else {
    orig_str
  };
  lines.push(popup_line(&format!(" {}", orig_display), width, YELLOW));

  let branch = sandbox::git_branch(orig).unwrap_or_else(|_| "main".into());
  lines.push(popup_line(&format!(" {}", branch), width, DIM_GRAY));

  let plus = format!("+{}", insertions);
  let minus = format!("-{}", deletions);
  let files_word = if files_changed == 1 { "file" } else { "files" };
  let commits_word = if commit_count == 1 { "commit" } else { "commits" };
  let rest = format!(" ({} {})  {} {}", files_changed, files_word, commit_count, commits_word);
  let stats_len = 1 + plus.chars().count() + 1 + minus.chars().count() + rest.chars().count();
  let stats_pad = width.saturating_sub(stats_len + 2);
  lines.push(Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::styled(format!(" {}", plus), Style::default().fg(GREEN)),
    Span::styled("/", Style::default().fg(TEXT_WHITE)),
    Span::styled(minus, Style::default().fg(RED)),
    Span::styled(rest, Style::default().fg(DIM_GRAY)),
    Span::raw(" ".repeat(stats_pad)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ]));

  lines.push(detail_empty(width));
  lines.push(popup_footer(" y yes │ n no ", width, BORDER_GRAY));

  render_lines(frame, popup_area, lines);
}

fn centered_fixed(percent_x: u16, height: u16, r: Rect) -> Rect {
  let top_pad = r.height.saturating_sub(height) / 2;
  let popup_width = (r.width as u32 * percent_x as u32 / 100) as u16;
  let left_pad = r.width.saturating_sub(popup_width) / 2;

  Rect { x: r.x + left_pad, y: r.y + top_pad, width: popup_width.min(r.width), height: height.min(r.height) }
}

fn popup_empty(width: usize) -> Line<'static> {
  let padding = width.saturating_sub(2);
  Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ])
}

fn popup_line(text: &str, width: usize, color: Color) -> Line<'static> {
  let padding = width.saturating_sub(text.chars().count() + 2);
  Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::styled(text.to_string(), Style::default().fg(color)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ])
}

fn popup_header(label: &str, width: usize, border_color: Color) -> Line<'static> {
  let dashes = width.saturating_sub(label.chars().count() + 2);
  let label_span = if label.is_empty() {
    Span::styled("─".repeat(dashes), Style::default().fg(border_color))
  } else {
    Span::styled(label.to_string(), Style::default().fg(TEXT_WHITE))
  };
  let fill = if label.is_empty() { String::new() } else { "─".repeat(dashes) };
  Line::from(vec![
    Span::styled("┌", Style::default().fg(border_color)),
    label_span,
    Span::styled(fill, Style::default().fg(border_color)),
    Span::styled("┐", Style::default().fg(border_color)),
  ])
}

fn popup_footer(keys: &str, width: usize, border_color: Color) -> Line<'static> {
  let dashes = width.saturating_sub(keys.chars().count() + 2);
  Line::from(vec![
    Span::styled("└", Style::default().fg(border_color)),
    Span::styled(keys.to_string(), Style::default().fg(DIM_GRAY)),
    Span::styled("─".repeat(dashes), Style::default().fg(border_color)),
    Span::styled("┘", Style::default().fg(border_color)),
  ])
}

fn popup_separator(width: usize, border_color: Color) -> Line<'static> {
  Line::from(vec![
    Span::styled("├", Style::default().fg(border_color)),
    Span::styled("─".repeat(width.saturating_sub(2)), Style::default().fg(border_color)),
    Span::styled("┤", Style::default().fg(border_color)),
  ])
}

fn list_empty(width: usize) -> ListItem<'static> {
  let padding = width.saturating_sub(2);
  ListItem::new(Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ]))
}

fn render_lines(frame: &mut Frame, area: Rect, lines: Vec<Line>) {
  frame.render_widget(Clear, area);
  for (i, line) in lines.into_iter().enumerate() {
    let line_area = Rect { x: area.x, y: area.y + i as u16, width: area.width, height: 1 };
    frame.render_widget(Paragraph::new(line), line_area);
  }
}

fn detail_empty(width: usize) -> Line<'static> {
  let padding = width.saturating_sub(2);
  Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ])
}

fn error_empty(width: usize) -> Line<'static> {
  let padding = width.saturating_sub(2);
  Line::from(vec![
    Span::styled("│", Style::default().fg(RED)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(RED)),
  ])
}

fn error_line(text: &str, width: usize, color: Color) -> Line<'static> {
  let padding = width.saturating_sub(text.chars().count() + 2);
  Line::from(vec![
    Span::styled("│", Style::default().fg(RED)),
    Span::styled(text.to_string(), Style::default().fg(color)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(RED)),
  ])
}
