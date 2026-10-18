//! Rendering and input hit testing for the client-local machine bar.
use super::Endpoint;
use crate::protocol::FrameData;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

pub(super) fn content_rows(rows: u16, bar: bool) -> u16 {
    if bar {
        rows.saturating_sub(1).max(1)
    } else {
        rows.max(1)
    }
}

/// Half-open column ranges are shared by rendering and mouse hit testing.
pub(super) fn tabs(
    endpoints: &[Endpoint],
    selected: &str,
    cols: u16,
) -> Vec<(usize, Rect, String)> {
    let widths: Vec<u16> = endpoints
        .iter()
        .map(|e| (e.label.width() + 7).min(30) as u16)
        .collect();
    let selected_index = endpoints.iter().position(|e| e.id == selected).unwrap_or(0);
    let total: usize = widths
        .iter()
        .take(selected_index + 1)
        .map(|&w| w as usize)
        .sum();
    let start = if total > cols as usize {
        selected_index
    } else {
        0
    };
    let mut x = 0u16;
    let mut result = Vec::new();
    for (index, endpoint) in endpoints.iter().enumerate().skip(start) {
        if x >= cols {
            break;
        }
        let width = widths[index].min(cols - x);
        let mark = if endpoint.error.is_some() {
            "!"
        } else if endpoint.connection.is_none() {
            "~"
        } else {
            "●"
        };
        let text = format!(
            " {} {}{} ",
            mark,
            endpoint.label,
            if endpoint.unread { " +" } else { "" }
        );
        result.push((index, Rect::new(x, 0, width, 1), text));
        x += width;
    }
    result
}

pub(super) fn compose(
    endpoints: &[Endpoint],
    selected: &str,
    cols: u16,
    rows: u16,
    bar: bool,
) -> FrameData {
    let height = content_rows(rows, bar);
    let endpoint = endpoints
        .iter()
        .find(|e| e.id == selected)
        .unwrap_or(&endpoints[0]);
    let mut frame = if endpoint.ready(cols, height) {
        // Graphics aren't requested in machine mode, avoiding cross-host image ID collisions.
        endpoint
            .frame
            .clone()
            .unwrap_or_else(|| empty_frame(cols, height))
    } else {
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, height));
        let text = if let Some(error) = &endpoint.error {
            format!("{}: {}\n\nReconnecting automatically. Select Local to keep working.\nFor SSH authentication, run machine reconnect with this machine's label in another terminal.", endpoint.label, clean(error))
        } else {
            format!("Connecting to {}…", endpoint.label)
        };
        Paragraph::new(text)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .render(buffer.area, &mut buffer);
        FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[])
    };
    if bar && rows >= 2 {
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, 1));
        buffer.set_style(
            buffer.area,
            Style::default().fg(Color::Gray).bg(Color::Rgb(30, 30, 46)),
        );
        for (index, area, text) in tabs(endpoints, selected, cols) {
            let style = if endpoints[index].id == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(137, 180, 250))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Paragraph::new(Line::from(Span::styled(text, style))).render(area, &mut buffer);
        }
        frame
            .cells
            .extend(FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]).cells);
        frame.height += 1;
    }
    frame
}

pub(super) fn empty_frame(cols: u16, rows: u16) -> FrameData {
    FrameData::from_ratatui_buffer_with_hyperlinks(
        &Buffer::empty(Rect::new(0, 0, cols, rows)),
        None,
        &[],
    )
}

pub(super) fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(1024)
        .collect()
}

pub(super) enum InputAction {
    Forward,
    Select(String),
    Consume,
}

#[derive(Default)]
pub(super) struct InputRouter {
    bar_gesture: bool,
}

impl InputRouter {
    pub(super) fn action(
        &mut self,
        data: &[u8],
        endpoints: &[Endpoint],
        selected: &str,
        size: (u16, u16),
        bar: bool,
    ) -> InputAction {
        let events = crate::raw_input::parse_raw_input_bytes_sync(data);
        if let [crate::raw_input::RawInputEvent::Mouse(mouse)] = events.as_slice() {
            if self.bar_gesture {
                if matches!(mouse.kind, MouseEventKind::Up(_)) {
                    self.bar_gesture = false;
                }
                return InputAction::Consume;
            }
            if bar
                && mouse.row >= size.1.saturating_sub(1)
                && matches!(mouse.kind, MouseEventKind::Down(_))
            {
                self.bar_gesture = true;
            }
        }
        input_action(data, endpoints, selected, size.0, size.1, bar)
    }
}

pub(super) fn input_action(
    data: &[u8],
    endpoints: &[Endpoint],
    selected: &str,
    cols: u16,
    rows: u16,
    bar: bool,
) -> InputAction {
    let events = crate::raw_input::parse_raw_input_bytes_sync(data);
    if events.len() != 1 {
        return InputAction::Forward;
    }
    let next = |backwards: bool| {
        let index = endpoints.iter().position(|e| e.id == selected).unwrap_or(0);
        let n = endpoints.len();
        endpoints[(index + if backwards { n - 1 } else { 1 }) % n]
            .id
            .clone()
    };
    match &events[0] {
        crate::raw_input::RawInputEvent::Key(key)
            if key.code == KeyCode::Char('m')
                && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if key.kind == KeyEventKind::Release {
                InputAction::Consume
            } else {
                InputAction::Select(next(false))
            }
        }
        crate::raw_input::RawInputEvent::Mouse(mouse)
            if bar && mouse.row >= rows.saturating_sub(1) =>
        {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => tabs(endpoints, selected, cols)
                    .iter()
                    .find(|(_, r, _)| mouse.column >= r.x && mouse.column < r.right())
                    .map(|(i, _, _)| InputAction::Select(endpoints[*i].id.clone()))
                    .unwrap_or(InputAction::Consume),
                MouseEventKind::ScrollDown => InputAction::Select(next(false)),
                MouseEventKind::ScrollUp => InputAction::Select(next(true)),
                _ => InputAction::Consume,
            }
        }
        _ => InputAction::Forward,
    }
}
