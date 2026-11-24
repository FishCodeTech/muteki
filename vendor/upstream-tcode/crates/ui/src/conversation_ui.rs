//! Client-owned UI state that follows a conversation across surface switches.

use tcode_core::ui::RightTab;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffFocus {
    pub session: String,
    pub turn: usize,
    pub path: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ConversationUiState {
    pub right_panel_open: bool,
    pub right_panel_expanded: bool,
    pub right_tab: RightTab,
    pub diff_selected_turn: Option<usize>,
    pub diff_split: bool,
    pub diff_wrap: bool,
    pub pending_diff_focus: Option<DiffFocus>,
    pub diff_refresh_generation: u64,
    pub terminal_open: bool,
    pub terminal_height: f32,
    pub auto_open_task_suppressed: bool,
    pub preview_url: Option<String>,
    pub preview_canvas: Option<(u32, u32)>,
}

impl ConversationUiState {
    pub fn new(diff_wrap: bool, terminal_open: bool, terminal_height: f32) -> Self {
        Self {
            right_panel_open: false,
            right_panel_expanded: false,
            right_tab: RightTab::default(),
            diff_selected_turn: None,
            diff_split: false,
            diff_wrap,
            pending_diff_focus: None,
            diff_refresh_generation: 0,
            terminal_open,
            terminal_height,
            auto_open_task_suppressed: false,
            preview_url: None,
            preview_canvas: None,
        }
    }

    pub fn refresh_diff(&mut self) {
        self.diff_refresh_generation = self.diff_refresh_generation.wrapping_add(1);
    }

    pub fn take_diff_focus(&mut self, session: &str, turn: usize) -> Option<DiffFocus> {
        let matches = self
            .pending_diff_focus
            .as_ref()
            .is_some_and(|request| request.session == session && request.turn == turn);
        matches.then(|| self.pending_diff_focus.take()).flatten()
    }

    pub fn discard_diff_focus(&mut self) {
        self.pending_diff_focus = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_file_focus_request_is_consumed_once_and_discarded_on_scope_change() {
        let mut ui = ConversationUiState::new(false, false, 240.);
        ui.pending_diff_focus = Some(DiffFocus {
            session: "session-a".into(),
            turn: 3,
            path: "src/second.rs".into(),
        });
        assert_eq!(ui.take_diff_focus("session-a", 2), None);
        assert!(ui.pending_diff_focus.is_some());
        assert_eq!(
            ui.take_diff_focus("session-a", 3),
            Some(DiffFocus {
                session: "session-a".into(),
                turn: 3,
                path: "src/second.rs".into(),
            })
        );
        assert_eq!(ui.take_diff_focus("session-a", 3), None);
        ui.pending_diff_focus = Some(DiffFocus {
            session: "session-a".into(),
            turn: 3,
            path: "src/second.rs".into(),
        });
        ui.discard_diff_focus();
        assert!(ui.pending_diff_focus.is_none());
    }
}
