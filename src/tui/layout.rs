// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/layout.rs
use ratatui::layout::{Constraint, Direction, Layout, Rect};

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutMode {
    Simple,    // chat + input only
    Standard,  // chat + input + status bar
    RightFull, // sidebar right  + chat + input + status bar
    LeftFull,  // sidebar left   + chat + input + status bar
    RightOnly, // sidebar right  + chat + input (no footer)
    LeftOnly,  // sidebar left   + chat + input (no footer)
}

impl LayoutMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "simple"                       => LayoutMode::Simple,
            "full" | "right-full"
            | "right full"                 => LayoutMode::RightFull,
            "left-full"  | "left full"     => LayoutMode::LeftFull,
            "right-only" | "right only"    => LayoutMode::RightOnly,
            "left-only"  | "left only"     => LayoutMode::LeftOnly,
            _                              => LayoutMode::Standard,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            LayoutMode::Simple    => "simple",
            LayoutMode::Standard  => "standard",
            LayoutMode::RightFull => "right-full",
            LayoutMode::LeftFull  => "left-full",
            LayoutMode::RightOnly => "right-only",
            LayoutMode::LeftOnly  => "left-only",
        }
    }
    /// Cycle order: simple → standard → right-full → left-full → right-only → left-only → simple
    pub fn next(&self) -> Self {
        match self {
            LayoutMode::Simple    => LayoutMode::Standard,
            LayoutMode::Standard  => LayoutMode::RightFull,
            LayoutMode::RightFull => LayoutMode::LeftFull,
            LayoutMode::LeftFull  => LayoutMode::RightOnly,
            LayoutMode::RightOnly => LayoutMode::LeftOnly,
            LayoutMode::LeftOnly  => LayoutMode::Simple,
        }
    }
}

pub struct TuiLayout {
    pub chat_area:    Rect,
    pub input_area:   Rect,
    pub status_area:  Option<Rect>,
    pub sidebar_area: Option<Rect>,
}

pub const INPUT_HEIGHT:  u16 = 3;
pub const SIDEBAR_WIDTH: u16 = 28;

/// Returns 1 if `available_width` fits the entire status bar on one row,
/// or 2 when primary info + hints need to be stacked.
fn status_rows(available_width: u16, status_content_width: u16) -> u16 {
    if available_width >= status_content_width { 1 } else { 2 }
}

/// `status_content_width` is the display width (in columns) that the status
/// bar would need to fit everything on a single row.  Pass the value returned
/// by `render::status_content_width`.
pub fn compute_layout(mode: LayoutMode, area: Rect, status_content_width: u16) -> TuiLayout {
    match mode {
        LayoutMode::Simple => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),
                    Constraint::Length(INPUT_HEIGHT),
                ])
                .split(area);
            TuiLayout {
                chat_area:    chunks[0],
                input_area:   chunks[1],
                status_area:  None,
                sidebar_area: None,
            }
        }

        LayoutMode::Standard => {
            let status_h = status_rows(area.width, status_content_width);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),
                    Constraint::Length(INPUT_HEIGHT),
                    Constraint::Length(status_h),
                ])
                .split(area);
            TuiLayout {
                chat_area:    chunks[0],
                input_area:   chunks[1],
                status_area:  Some(chunks[2]),
                sidebar_area: None,
            }
        }

        LayoutMode::RightFull => {
            // main area (left) | sidebar (right)
            let h_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(60),
                    Constraint::Length(SIDEBAR_WIDTH),
                ])
                .split(area);
            let main_area = h_chunks[0];
            let sidebar   = h_chunks[1];
            let status_h  = status_rows(main_area.width, status_content_width);
            let v_chunks  = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),
                    Constraint::Length(INPUT_HEIGHT),
                    Constraint::Length(status_h),
                ])
                .split(main_area);
            TuiLayout {
                chat_area:    v_chunks[0],
                input_area:   v_chunks[1],
                status_area:  Some(v_chunks[2]),
                sidebar_area: Some(sidebar),
            }
        }

        LayoutMode::LeftFull => {
            // sidebar (left) | main area (right)
            let h_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(SIDEBAR_WIDTH),
                    Constraint::Min(60),
                ])
                .split(area);
            let sidebar   = h_chunks[0];
            let main_area = h_chunks[1];
            let status_h  = status_rows(main_area.width, status_content_width);
            let v_chunks  = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),
                    Constraint::Length(INPUT_HEIGHT),
                    Constraint::Length(status_h),
                ])
                .split(main_area);
            TuiLayout {
                chat_area:    v_chunks[0],
                input_area:   v_chunks[1],
                status_area:  Some(v_chunks[2]),
                sidebar_area: Some(sidebar),
            }
        }

        LayoutMode::RightOnly => {
            // main area (left) | sidebar (right) — no footer
            let h_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(60),
                    Constraint::Length(SIDEBAR_WIDTH),
                ])
                .split(area);
            let main_area = h_chunks[0];
            let sidebar   = h_chunks[1];
            let v_chunks  = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),
                    Constraint::Length(INPUT_HEIGHT),
                ])
                .split(main_area);
            TuiLayout {
                chat_area:    v_chunks[0],
                input_area:   v_chunks[1],
                status_area:  None,
                sidebar_area: Some(sidebar),
            }
        }

        LayoutMode::LeftOnly => {
            // sidebar (left) | main area (right) — no footer
            let h_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(SIDEBAR_WIDTH),
                    Constraint::Min(60),
                ])
                .split(area);
            let sidebar   = h_chunks[0];
            let main_area = h_chunks[1];
            let v_chunks  = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),
                    Constraint::Length(INPUT_HEIGHT),
                ])
                .split(main_area);
            TuiLayout {
                chat_area:    v_chunks[0],
                input_area:   v_chunks[1],
                status_area:  None,
                sidebar_area: Some(sidebar),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    // Use a realistic status_content_width for tests: 160 chars
    const STATUS_W: u16 = 160;

    #[test]
    fn test_simple_layout_no_sidebar() {
        let area = Rect::new(0, 0, 120, 40);
        let l = compute_layout(LayoutMode::Simple, area, STATUS_W);
        assert!(l.status_area.is_none());
        assert!(l.sidebar_area.is_none());
        assert!(l.chat_area.height > l.input_area.height);
    }
    #[test]
    fn test_standard_layout_has_status_no_sidebar() {
        let area = Rect::new(0, 0, 120, 40);
        let l = compute_layout(LayoutMode::Standard, area, STATUS_W);
        assert!(l.status_area.is_some());
        assert!(l.sidebar_area.is_none());
    }
    #[test]
    fn test_right_full_layout_has_all_areas() {
        let area = Rect::new(0, 0, 200, 50);
        let l = compute_layout(LayoutMode::RightFull, area, STATUS_W);
        assert!(l.status_area.is_some());
        assert!(l.sidebar_area.is_some());
    }
    #[test]
    fn test_left_full_layout_sidebar_on_left() {
        let area = Rect::new(0, 0, 200, 50);
        let l = compute_layout(LayoutMode::LeftFull, area, STATUS_W);
        assert!(l.status_area.is_some());
        assert!(l.sidebar_area.is_some());
        // sidebar starts at x=0, chat area is to the right
        assert_eq!(l.sidebar_area.unwrap().x, 0);
        assert!(l.chat_area.x > 0);
    }
    #[test]
    fn test_right_only_no_footer() {
        let area = Rect::new(0, 0, 200, 50);
        let l = compute_layout(LayoutMode::RightOnly, area, STATUS_W);
        assert!(l.status_area.is_none());
        assert!(l.sidebar_area.is_some());
    }
    #[test]
    fn test_left_only_no_footer() {
        let area = Rect::new(0, 0, 200, 50);
        let l = compute_layout(LayoutMode::LeftOnly, area, STATUS_W);
        assert!(l.status_area.is_none());
        assert!(l.sidebar_area.is_some());
        assert_eq!(l.sidebar_area.unwrap().x, 0);
    }
    #[test]
    fn test_status_bar_height_content_aware() {
        // content needs 160 cols; terminal is 100 wide → 2 rows
        let narrow = Rect::new(0, 0, 100, 40);
        let l = compute_layout(LayoutMode::Standard, narrow, 160);
        assert_eq!(l.status_area.unwrap().height, 2);
        // terminal is 200 wide → 1 row
        let wide = Rect::new(0, 0, 200, 40);
        let l = compute_layout(LayoutMode::Standard, wide, 160);
        assert_eq!(l.status_area.unwrap().height, 1);
        // RightFull: sidebar (28) eats space; main_area.width = 200-28 = 172 ≥ 160 → 1 row
        let l = compute_layout(LayoutMode::RightFull, Rect::new(0, 0, 200, 40), 160);
        assert_eq!(l.status_area.unwrap().height, 1);
    }
}
