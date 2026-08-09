use kurbo::{Point, Rect};

use crate::doc_query::DocQuery;
use crate::{BookSearchMatch, HighlightState, PaintDirection, Painter, RendererCore, RendererEvent, RendererHost};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

fn normalized_chars(chars: impl IntoIterator<Item = (usize, char)>, options: crate::SearchOptions) -> Vec<(char, usize)> {
    let mut normalized = Vec::new();
    for (source_index, character) in chars {
        let decomposed: Vec<char> = if options.match_diacritics { vec![character] } else { character.to_string().nfd().filter(|part| !is_combining_mark(*part)).collect() };
        for part in decomposed {
            if options.match_case {
                normalized.push((part, source_index));
            } else {
                normalized.extend(part.to_lowercase().map(|folded| (folded, source_index)));
            }
        }
    }
    normalized
}

pub(crate) fn find_text_matches(text: &str, query: &str, options: crate::SearchOptions) -> Vec<(usize, usize)> {
    let haystack = normalized_chars(text.chars().enumerate(), options);
    let needle: Vec<char> = normalized_chars(query.chars().enumerate(), options).into_iter().map(|(character, _)| character).collect();
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    (0..=haystack.len() - needle.len())
        .filter_map(|start| {
            if !haystack[start..start + needle.len()].iter().map(|(character, _)| *character).eq(needle.iter().copied()) {
                return None;
            }
            if options.whole_word {
                let before = start.checked_sub(1).and_then(|index| haystack.get(index)).map(|(character, _)| *character);
                let after = haystack.get(start + needle.len()).map(|(character, _)| *character);
                if before.is_some_and(char::is_alphanumeric) || after.is_some_and(char::is_alphanumeric) {
                    return None;
                }
            }
            Some((haystack[start].1, haystack[start + needle.len() - 1].1 + 1))
        })
        .collect()
}

impl HighlightState {
    /// Options for restricting a search to the document on screen.
    pub(crate) fn current_document_options(&self) -> crate::SearchOptions {
        crate::SearchOptions { scope: crate::SearchScope::CurrentDocument, ..self.options }
    }

    pub(crate) fn set_active(&mut self, active: bool) -> bool {
        let changed = self.search_active != active;
        self.search_active = active;
        changed
    }

    /// Adopts a new query, dropping every result and cursor from the old one.
    pub(crate) fn restart(&mut self, query: String, options: crate::SearchOptions) {
        self.query = query;
        self.options = options;
        self.book_matches.clear();
        self.matches.clear();
        self.current_match = 0;
        self.current_local_match = 0;
    }

    /// Records whole-document matches and mirrors them as book matches, which
    /// is how a current-document search reports results.
    pub(crate) fn set_local_matches(&mut self, doc: usize, matches: Vec<(u32, u32)>) {
        self.book_matches = matches.iter().enumerate().map(|(occurrence, _)| BookSearchMatch { doc, occurrence, excerpt: self.query.clone() }).collect();
        self.matches = matches;
    }

    pub(crate) fn set_results(&mut self, query: String, options: crate::SearchOptions, results: Vec<BookSearchMatch>) {
        self.restart(query, options);
        self.book_matches = results;
    }

    /// Appends a streamed batch. Returns whether this batch made the result set
    /// non-empty, which is when the caller should jump to the first match.
    pub(crate) fn append_results(&mut self, query: String, options: crate::SearchOptions, results: Vec<BookSearchMatch>) -> bool {
        if self.query != query || self.options != options {
            self.restart(query, options);
        }
        let was_empty = self.book_matches.is_empty();
        self.book_matches.extend(results);
        was_empty && !self.book_matches.is_empty()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.book_matches.is_empty()
    }

    /// Moves the cursor one match forward or backward, wrapping. Returns
    /// whether there was anything to move to.
    pub(crate) fn step(&mut self, forward: bool) -> bool {
        let count = self.book_matches.len();
        if count == 0 {
            return false;
        }
        self.current_match = if forward { (self.current_match + 1) % count } else { (self.current_match + count - 1) % count };
        true
    }

    pub(crate) fn current_book_match(&self) -> Option<BookSearchMatch> {
        self.book_matches.get(self.current_match).cloned()
    }

    /// Points the in-document cursor at `occurrence`, clamped to the matches
    /// actually found in the document now on screen.
    pub(crate) fn focus_local_match(&mut self, occurrence: usize) {
        self.current_local_match = occurrence.min(self.matches.len().saturating_sub(1));
    }

    pub(crate) fn current_local_match_start(&self) -> Option<u32> {
        self.matches.get(self.current_local_match).map(|&(start, _)| start)
    }

    pub(crate) fn emit_match_signal(&self, host: &dyn RendererHost) {
        let total = self.book_matches.len();
        let current = if total == 0 { 0 } else { self.current_match + 1 };
        host.emit(RendererEvent::MatchInfo { current, total });
    }

    pub(crate) fn emit_results(&self, host: &dyn RendererHost) {
        host.emit(RendererEvent::SearchResults(self.book_matches.clone()));
        self.emit_match_signal(host);
    }
}

impl RendererCore {
    /// Scan document glyphs for all occurrences of `query` (case-insensitive).
    /// Returns sorted (start_glyph, end_glyph) pairs.
    pub fn find_matches(&self, query: &str) -> Vec<(u32, u32)> {
        self.doc().find_matches_with_options(query, self.highlight.current_document_options())
    }

    /// Scroll so that the current match is visible at the top of the viewport.
    pub fn navigate_to_current_match(&mut self) {
        let Some(start) = self.highlight.current_local_match_start() else {
            return;
        };
        let Some(line_idx) = self.doc().find_line_for_glyph(start) else {
            return;
        };
        // Only scroll if the match is not already on screen
        let is_visible = self.frame.last_line_positions.contains(line_idx);
        if is_visible {
            self.request_overlay_repaint();
            return;
        }
        let line_y = self.doc().text().line(line_idx).map(|line| line.point().y).unwrap_or_default();
        self.viewport.start_offset_y = (line_y - 40.0).max(0.0);
        self.viewport.direction = PaintDirection::Forward;
        self.request_overlay_repaint();
    }

    pub fn update_match_signal(&self) {
        self.highlight.emit_match_signal(self.host.as_ref());
    }

    /// Paint highlight rects for all matches that overlap `line`.
    /// Called for every visible line during `paint_lines_from_positions`.
    pub fn paint_highlights_for_line(&self, cx: &mut impl Painter, line: &html::layout::RenderLine, screen_point: Point) {
        if self.highlight.matches.is_empty() {
            return;
        }

        // Binary-search for the first match whose end is past the line's start glyph
        let first = self.highlight.matches.partition_point(|&(_, end)| end <= line.start());

        for (i, &(match_start, match_end)) in self.highlight.matches[first..].iter().enumerate() {
            if match_start >= line.end() {
                break;
            }
            let is_current = first + i == self.highlight.current_local_match;
            let color = if is_current { self.interaction_palette.active_search_match } else { self.interaction_palette.search_match };

            for (start, end) in self.doc().line_text_intersections(line.index(), match_start, match_end) {
                let (range_x0, range_x1) = self.glyph_x_span_in_line(line, start, end);
                let x0 = screen_point.x + range_x0;
                let x1 = screen_point.x + range_x1;
                let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };

                if x1 > x0 {
                    let rect = Rect::new(x0, screen_point.y, x1, screen_point.y + line.height());
                    cx.fill_rect(rect, color);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_text_matches;
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

impl<'a> DocQuery<'a> {
    pub(crate) fn find_matches_with_options(self, query: &str, options: crate::SearchOptions) -> Vec<(u32, u32)> {
        if query.is_empty() {
            return Vec::new();
        }

        // Generated marker glyphs are not part of the document's searchable text
        // (matching how browsers exclude ::marker content), so mask them out with
        // a sentinel that cannot match and also breaks runs across them.
        let view = self.text();
        let total = view.glyph_count();
        let mut is_marker = vec![false; total];
        for run in view.marker_runs() {
            let glyphs = run.glyphs();
            for i in glyphs {
                if (i as usize) < total {
                    is_marker[i as usize] = true;
                }
            }
        }

        let glyph_text: String = view
            .glyph_slice(0..total as u32)
            .expect("the full glyph range is valid")
            .iter()
            .enumerate()
            .map(|(i, &idx)| {
                if is_marker[i] {
                    return '\u{0}';
                }

                view.glyph_metric(idx).expect("layout glyph IDs are registered").ch()
            })
            .collect();
        find_text_matches(&glyph_text, query, options).into_iter().map(|(start, end)| (start as u32, end as u32)).collect()
    }
}
