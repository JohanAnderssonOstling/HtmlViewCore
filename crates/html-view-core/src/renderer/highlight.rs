use kurbo::{Point, Rect};

use crate::{PaintDirection, Painter, RendererCore, RendererEvent};
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

impl RendererCore {
    /// Scan document glyphs for all occurrences of `query` (case-insensitive).
    /// Returns sorted (start_glyph, end_glyph) pairs.
    pub fn find_matches(&self, query: &str) -> Vec<(u32, u32)> {
        self.find_matches_with_options(query, crate::SearchOptions { scope: crate::SearchScope::CurrentDocument, ..self.highlight.options })
    }

    pub fn find_matches_with_options(&self, query: &str, options: crate::SearchOptions) -> Vec<(u32, u32)> {
        if query.is_empty() {
            return Vec::new();
        }

        // Generated marker glyphs are not part of the document's searchable text
        // (matching how browsers exclude ::marker content), so mask them out with
        // a sentinel that cannot match and also breaks runs across them.
        let view = self.document.render_view().text();
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

    /// Scroll so that the current match is visible at the top of the viewport.
    pub fn navigate_to_current_match(&mut self) {
        let Some(&(start, _)) = self.highlight.matches.get(self.highlight.current_local_match) else {
            return;
        };
        let Some(line_idx) = self.find_line_for_glyph(start) else {
            return;
        };
        // Only scroll if the match is not already on screen
        let is_visible = self.frame.last_line_positions.contains(line_idx);
        if is_visible {
            self.request_overlay_repaint();
            return;
        }
        let line_y = self.document.render_view().text().line(line_idx).map(|line| line.point().y).unwrap_or_default();
        self.viewport.start_offset_y = (line_y - 40.0).max(0.0);
        self.viewport.direction = PaintDirection::Forward;
        self.request_overlay_repaint();
    }

    pub fn update_match_signal(&self) {
        let total = self.highlight.book_matches.len();
        let current = if total == 0 { 0 } else { self.highlight.current_match + 1 };
        self.host.emit(RendererEvent::MatchInfo { current, total });
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

            for (start, end) in self.line_text_intersections(line.index(), match_start, match_end) {
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
