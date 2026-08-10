
use crate::RendererCore;

impl RendererCore {
    /// Scroll so that the current match is visible at the top of the viewport.
    pub fn navigate_to_current_match(&mut self) {
        let Some(start) = self.highlight.current_local_match_start() else {
            return;
        };
        let Some(line_idx) = self.doc().find_line_for_glyph(start) else {
            return;
        };
        // Only scroll if the match is not already on screen
        let is_visible = self.page.view.frame.page().line_positions().contains(line_idx);
        if is_visible {
            self.request_overlay_repaint();
            return;
        }
        let line_y = self.doc().text().line(line_idx).map(|line| line.point().y).unwrap_or_default();
        self.viewport.begin_forward_at((line_y - 40.0).max(0.0));
        self.request_overlay_repaint();
    }

}

#[cfg(test)]
mod tests {
    use html_view_doc::find_text_matches;
    use crate::{SearchOptions, SearchScope};

    fn options() -> SearchOptions {
        SearchOptions { match_case: false, whole_word: false, match_diacritics: false, scope: SearchScope::WholeBook }
    }

    #[test]
    fn matching_options_cover_case_words_and_diacritics() {
        assert_eq!(find_text_matches("Café CAFE cafeteria", "cafe", options()).len(), 3);
        assert_eq!(find_text_matches("Café CAFE cafeteria", "cafe", SearchOptions { whole_word: true, ..options() }).len(), 2);
        assert_eq!(find_text_matches("Café CAFE", "cafe", SearchOptions { match_case: true, ..options() }).len(), 0);
        assert_eq!(find_text_matches("Café CAFE", "cafe", SearchOptions { match_diacritics: true, ..options() }).len(), 1);
    }
}
