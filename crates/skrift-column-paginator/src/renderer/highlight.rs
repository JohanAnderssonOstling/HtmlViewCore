use html::targets::{DocumentTarget, ResolvedDestination};
use crate::RendererCore;

impl RendererCore {
    /// Scroll so that the current match is visible at the top of the viewport.
    pub fn navigate_to_current_match(&mut self) {
        let Some(start) = self.highlight.current_local_match_start() else {
            return;
        };
        let Some(ResolvedDestination::Line { line, point, .. }) = self
            .doc()
            .addressing()
            .destination(DocumentTarget::Glyph(start))
        else {
            return;
        };
        if self.page.frame.page().line_positions().contains(line) {
            self.request_overlay_repaint();
            return;
        }
        let line_y = point.y;
        let (nav, mut cx) = self.nav_cx();
        nav.advance_to_forward_offset(&mut cx, (line_y - 40.0).max(0.0));
        self.request_overlay_repaint();
    }
}

#[cfg(test)]
mod tests {
    use crate::{SearchOptions, SearchScope};
    use html_pipeline::engine::find_text_matches;

    fn options() -> SearchOptions {
        SearchOptions {
            match_case: false,
            whole_word: false,
            match_diacritics: false,
            scope: SearchScope::WholeBook,
        }
    }

    #[test]
    fn matching_options_cover_case_words_and_diacritics() {
        assert_eq!(
            find_text_matches("Café CAFE cafeteria", "cafe", options().into()).len(),
            3
        );
        assert_eq!(
            find_text_matches(
                "Café CAFE cafeteria",
                "cafe",
                (SearchOptions {
                    whole_word: true,
                    ..options()
                })
                .into()
            )
            .len(),
            2
        );
        assert_eq!(
            find_text_matches(
                "Café CAFE",
                "cafe",
                (SearchOptions {
                    match_case: true,
                    ..options()
                })
                .into()
            )
            .len(),
            0
        );
        assert_eq!(
            find_text_matches(
                "Café CAFE",
                "cafe",
                (SearchOptions {
                    match_diacritics: true,
                    ..options()
                })
                .into()
            )
            .len(),
            1
        );
    }
}
