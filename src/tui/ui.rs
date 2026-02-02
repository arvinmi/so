use std::time::Duration;

use ratatui::{
  Frame,
  layout::{Constraint, Layout, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{List, ListItem, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::{ActivityKind, AppState, FocusPane, IterStatus, RunPopup, RunStatus};

// set colors
const BORDER_GRAY: Color = Color::Rgb(70, 70, 70);
const DIM_GRAY: Color = Color::Rgb(110, 110, 110);
const MEDIUM_GRAY: Color = Color::Rgb(160, 160, 160);
const TEXT_WHITE: Color = Color::Rgb(220, 220, 220);
const GREEN: Color = Color::Rgb(80, 200, 120);
const YELLOW: Color = Color::Rgb(220, 180, 50);
const RED: Color = Color::Rgb(220, 80, 80);
const CYAN: Color = Color::Rgb(80, 180, 200);

pub fn render(frame: &mut Frame, state: &mut AppState) {
  let area = frame.area();
  let width = area.width as usize;

  // layout: header box (3 lines) + iterations + activity (includes footer)
  let [header_box, iterations_area, activity_area] = Layout::vertical([
    // header box with top + running + bottom
    Constraint::Length(3),
    // iterations box
    Constraint::Percentage(35),
    // activity box
    Constraint::Fill(1),
  ])
  .areas(area);

  // update visible heights for scroll bounds (subtract 2 for header/footer borders)
  let iter_visible = iterations_area.height.saturating_sub(2) as usize;
  let activity_visible = activity_area.height.saturating_sub(2) as usize;
  state.set_visible_heights(activity_visible, iter_visible);

  // header box (rectangle)
  render_header_box(frame, header_box, state, width);

  // iterations section (rectangle with stats in header)
  render_iterations_section(frame, iterations_area, state);

  // activity section (rectangle with footer)
  render_activity_section(frame, activity_area, state);

  // popup overlay
  match &state.popup {
    RunPopup::None => {}
    RunPopup::Options => render_options_popup(frame, area, state),
    RunPopup::Reset { commits, selected } => super::popup::render_reset_popup(frame, area, commits, *selected),
    RunPopup::Continue { input } => super::popup::render_continue_popup(frame, area, input),
    RunPopup::Error { message } => super::popup::render_error_popup(frame, area, message),
  }
}

fn render_header_box(frame: &mut Frame, area: Rect, state: &AppState, width: usize) {
  // split into 3 lines as top border, status, bottom border
  let [top_line, status_line, bottom_line] =
    Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)]).areas(area);

  // top border: ┌ repo-name ─ 00:00:00 ─────────────────────────────────┐
  let name = format!(" {} ", state.sandbox_name);
  let time = format!(" {} ", fmt_duration(state.elapsed()));
  let name_len = name.chars().count();
  let time_len = time.chars().count();
  // total: ┌ + name + ─ + time + dashes + ┐ = width
  let dashes = width.saturating_sub(name_len + time_len + 3); // 3 = ┌ + separator ─ + ┐

  let top = Line::from(vec![
    Span::styled("┌", Style::default().fg(BORDER_GRAY)),
    Span::styled(name, Style::default().fg(DIM_GRAY)),
    Span::styled("─", Style::default().fg(BORDER_GRAY)),
    Span::styled(time, Style::default().fg(DIM_GRAY)),
    Span::styled("─".repeat(dashes), Style::default().fg(BORDER_GRAY)),
    Span::styled("┐", Style::default().fg(BORDER_GRAY)),
  ]);
  frame.render_widget(Paragraph::new(top), top_line);

  // status: │                    ▶ RUNNING                    │
  let (icon, label, color) = match state.status {
    RunStatus::Running => ("▶", "RUNNING", GREEN),
    RunStatus::Done => ("■", "DONE", RED),
  };

  let status_text = format!("{} {}", icon, label);
  let text_len = status_text.chars().count();
  let inner_width = width.saturating_sub(2);
  let left_pad = inner_width.saturating_sub(text_len) / 2;
  let right_pad = inner_width.saturating_sub(text_len + left_pad);

  let status = Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::raw(" ".repeat(left_pad)),
    Span::styled(status_text, Style::default().fg(color).add_modifier(Modifier::BOLD)),
    Span::raw(" ".repeat(right_pad)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ]);
  frame.render_widget(Paragraph::new(status), status_line);

  // bottom border: └─────────────────────────────────────────────────────┘
  frame.render_widget(Paragraph::new(box_footer(width, BORDER_GRAY)), bottom_line);
}

fn render_iterations_section(frame: &mut Frame, area: Rect, state: &AppState) {
  let width = area.width as usize;
  let focused = state.focus == FocusPane::Iterations;
  let header_color = if focused { MEDIUM_GRAY } else { DIM_GRAY };

  // split into header + content + footer
  let [header_area, content_area, footer_area] =
    Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(area);

  // header: ┌ iterations: 1/1 │ +0/-0 (0 files) │ commits: 0 ───────────┐
  let label = format!(
    " iterations: {}/{} │ +{}/-{} ({} files) │ commits: {} ",
    state.current_iter, state.max_iter, state.insertions, state.deletions, state.files_changed, state.commit_count
  );

  frame.render_widget(Paragraph::new(box_header(&label, width, header_color)), header_area);

  // content with side borders
  let visible_height = content_area.height as usize;
  let items: Vec<ListItem> = state
    .iterations
    .iter()
    .skip(state.iter_scroll_offset)
    .take(visible_height)
    .enumerate()
    .map(|(idx, iter)| {
      let (icon, icon_color) = match iter.status {
        IterStatus::Pending => ("○", DIM_GRAY),
        IterStatus::Running => ("◐", YELLOW),
        IterStatus::Completed => ("●", GREEN),
      };

      // get task from plan_tasks, fall back to status desc
      let task = state.plan_tasks.get(state.iter_scroll_offset + idx).cloned().unwrap_or_else(|| match iter.status {
        IterStatus::Pending => "pending".into(),
        IterStatus::Running => "running...".into(),
        IterStatus::Completed => iter.commit_msg.clone().unwrap_or_else(|| "completed".into()),
      });

      let duration = iter.elapsed().map(|d| format!("[{}]", fmt_duration_short(d))).unwrap_or_else(|| "[--:--]".into());

      // ● 1   Task from implementation plan [00:45]
      // leave room for icon, num, duration, borders
      let max_task_len = width.saturating_sub(20);
      let task_display = truncate_str(&task, max_task_len);

      let inner = format!(" {} {:<2}  {} {}", icon, iter.number, task_display, duration);
      let inner_len = inner.chars().count();
      // 2 for side borders
      let padding = width.saturating_sub(inner_len + 2);

      let line = Line::from(vec![
        Span::styled("│", Style::default().fg(BORDER_GRAY)),
        Span::styled(format!(" {} ", icon), Style::default().fg(icon_color)),
        Span::styled(format!("{:<2}  ", iter.number), Style::default().fg(TEXT_WHITE)),
        Span::styled(task_display, Style::default().fg(TEXT_WHITE)),
        Span::raw(" "),
        Span::styled(duration, Style::default().fg(DIM_GRAY)),
        Span::raw(" ".repeat(padding)),
        Span::styled("│", Style::default().fg(BORDER_GRAY)),
      ]);

      ListItem::new(line)
    })
    .collect();

  // fill remaining with empty bordered lines
  let list = List::new(fill_list(items, visible_height, width));
  frame.render_widget(list, content_area);

  frame.render_widget(Paragraph::new(box_footer(width, BORDER_GRAY)), footer_area);
}

fn render_activity_section(frame: &mut Frame, area: Rect, state: &AppState) {
  let width = area.width as usize;
  let focused = state.focus == FocusPane::Activity;
  let header_color = if focused { MEDIUM_GRAY } else { DIM_GRAY };

  // split into header + content + footer
  let [header_area, content_area, footer_area] =
    Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(area);

  // header: ┌ activity ─────────────────────────────────────────────────┐
  let label = " activity ";

  frame.render_widget(Paragraph::new(box_header(label, width, header_color)), header_area);

  // content with side borders
  let visible_height = content_area.height as usize;
  let total = state.activity.len();
  let start =
    if total > visible_height + state.scroll_offset { total - visible_height - state.scroll_offset } else { 0 };

  let harness = &state.harness;

  let items: Vec<ListItem> = state
    .activity
    .iter()
    .skip(start)
    .take(visible_height)
    .map(|entry| {
      // time = elapsed from run start
      let elapsed = entry.timestamp.saturating_duration_since(state.start_time);
      let mins = elapsed.as_secs() / 60;
      let secs = elapsed.as_secs() % 60;
      let time_str = format!("{:>2} {:02}", mins, secs);

      let (prefix, is_tool) = match entry.kind {
        ActivityKind::Reading => ("reading ", true),
        ActivityKind::Writing => ("writing ", true),
        ActivityKind::ToolCall => ("shell: ", true),
        ActivityKind::Thinking | ActivityKind::Text | ActivityKind::Code => ("", false),
      };

      // fixed parts: │ + time (8) + harness (9) + │ = 19 chars
      let time_part = format!(" {}  ", time_str);
      let harness_part = format!("{:<8} ", harness);
      let content = format!("{}{}", prefix, entry.content);
      let max_content = width.saturating_sub(20);
      let display = truncate_str(&content, max_content);
      let padding = width.saturating_sub(19 + display.width());
      let text_color = if is_tool { DIM_GRAY } else { TEXT_WHITE };

      let line = Line::from(vec![
        Span::styled("│", Style::default().fg(BORDER_GRAY)),
        Span::styled(time_part, Style::default().fg(DIM_GRAY)),
        Span::styled(harness_part, Style::default().fg(CYAN)),
        Span::styled(display, Style::default().fg(text_color)),
        Span::raw(" ".repeat(padding)),
        Span::styled("│", Style::default().fg(BORDER_GRAY)),
      ]);

      ListItem::new(line)
    })
    .collect();

  // fill remaining with empty bordered lines
  let list = List::new(fill_list(items, visible_height, width));
  frame.render_widget(list, content_area);

  // footer: └ ^C quit │ ^D diff │ ^S shell │ ↑↓ scroll ─────────┘
  let keys = " ^C quit │ ^D diff │ ^S shell │ ↑↓ scroll ";

  frame.render_widget(Paragraph::new(box_footer_keys(keys, width, BORDER_GRAY)), footer_area);
}

fn empty_bordered_line(width: usize) -> ListItem<'static> {
  let padding = width.saturating_sub(2);
  ListItem::new(Line::from(vec![
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
    Span::raw(" ".repeat(padding)),
    Span::styled("│", Style::default().fg(BORDER_GRAY)),
  ]))
}

fn fill_list(mut items: Vec<ListItem>, visible_height: usize, width: usize) -> Vec<ListItem> {
  while items.len() < visible_height {
    items.push(empty_bordered_line(width));
  }
  items
}

fn box_header(label: &str, width: usize, label_color: Color) -> Line<'static> {
  let label_len = label.chars().count();
  let dashes = width.saturating_sub(label_len + 2);
  Line::from(vec![
    Span::styled("┌", Style::default().fg(BORDER_GRAY)),
    Span::styled(label.to_string(), Style::default().fg(label_color)),
    Span::styled("─".repeat(dashes), Style::default().fg(BORDER_GRAY)),
    Span::styled("┐", Style::default().fg(BORDER_GRAY)),
  ])
}

fn box_footer(width: usize, border_color: Color) -> Line<'static> {
  Line::from(vec![
    Span::styled("└", Style::default().fg(border_color)),
    Span::styled("─".repeat(width.saturating_sub(2)), Style::default().fg(border_color)),
    Span::styled("┘", Style::default().fg(border_color)),
  ])
}

fn box_footer_keys(keys: &str, width: usize, border_color: Color) -> Line<'static> {
  let keys_len = keys.chars().count();
  let dashes = width.saturating_sub(keys_len + 2);
  Line::from(vec![
    Span::styled("└", Style::default().fg(border_color)),
    Span::styled(keys.to_string(), Style::default().fg(DIM_GRAY)),
    Span::styled("─".repeat(dashes), Style::default().fg(border_color)),
    Span::styled("┘", Style::default().fg(border_color)),
  ])
}

fn truncate_str(s: &str, max_width: usize) -> String {
  if max_width == 0 {
    return String::new();
  }
  let display_width = s.width();
  if display_width <= max_width {
    s.to_string()
  } else if max_width <= 3 {
    // not enough room for "...", just take what fits
    let mut result = String::new();
    let mut width = 0;
    for c in s.chars() {
      let cw = c.to_string().width();
      if width + cw > max_width {
        break;
      }
      result.push(c);
      width += cw;
    }
    result
  } else {
    // truncate to fit "..." at end
    let mut result = String::new();
    let mut width = 0;
    let target = max_width - 3;
    for c in s.chars() {
      let cw = c.to_string().width();
      if width + cw > target {
        break;
      }
      result.push(c);
      width += cw;
    }
    format!("{}...", result)
  }
}

fn fmt_duration(d: Duration) -> String {
  let secs = d.as_secs();
  let hours = secs / 3600;
  let mins = (secs % 3600) / 60;
  let s = secs % 60;
  format!("{:02}:{:02}:{:02}", hours, mins, s)
}

fn fmt_duration_short(d: Duration) -> String {
  let secs = d.as_secs();
  let mins = secs / 60;
  let s = secs % 60;
  format!("{:02}:{:02}", mins, s)
}

fn render_options_popup(frame: &mut Frame, area: Rect, state: &AppState) {
  // format run duration as MM:SS
  let secs = state.elapsed().as_secs();
  let time_str = format!("{:02}:{:02}", secs / 60, secs % 60);

  let data = super::popup::DetailPopupData {
    path: &state.sandbox_path,
    insertions: state.insertions,
    deletions: state.deletions,
    files_changed: state.files_changed,
    commit_count: state.commit_count,
    time_str,
  };
  super::popup::render_detail_popup(frame, area, &data);
}
