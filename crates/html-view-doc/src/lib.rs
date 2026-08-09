//! Read-only queries over a laid-out document.
//!
//! Every method here depends on the document alone: no viewport, no frame, no
//! navigation or selection state. Holding them behind [`DocQuery`] makes that
//! independence checkable by the compiler instead of by convention, and lets
//! the renderer pass document access around as an ordinary parameter.

use std::collections::HashSet;
use std::ops::Range;

use html::layout::{LaidOutDocument, RenderTableCell};
use html_view_types::{FormatState, RendererAnnotation, SearchOptions};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

#[derive(Clone, Copy)]
pub struct DocQuery<'a> {
    document: &'a LaidOutDocument,
}

impl<'a> DocQuery<'a> {
    pub fn new(document: &'a LaidOutDocument) -> Self {
        Self { document }
    }

    /// The underlying document, for the few APIs that take it directly.
    pub fn document(self) -> &'a LaidOutDocument {
        self.document
    }

    pub fn view(self) -> html::layout::RenderView<'a> {
        self.document.render_view()
    }

    pub fn text(self) -> html::layout::RenderTextView<'a> {
        self.document.render_view().text()
    }

    pub fn boxes(self) -> html::layout::RenderBoxView<'a> {
        self.document.render_view().boxes()
    }

    pub fn block_ancestor(self, box_idx: usize) -> usize {
        // walk up to the nearest block box
        let boxes = self.boxes();
        let mut idx = box_idx;
        loop {
            if boxes.is_block_container(idx) {
                return idx;
            }
            let Some(parent) = boxes.parent(idx) else {
                return idx;
            };
            idx = parent;
        }
    }

    pub fn box_tag(self, box_idx: usize) -> String {
        // get tag name for a box
        self.boxes().tag(box_idx).unwrap_or("").to_string()
    }

    pub fn header_prefix(self, block_idx: usize) -> Option<&'static str> {
        // map header tags to markdown prefixes
        let tag = self.box_tag(block_idx);
        if tag.eq_ignore_ascii_case("h1") {
            Some("# ")
        } else if tag.eq_ignore_ascii_case("h2") {
            Some("## ")
        } else if tag.eq_ignore_ascii_case("h3") {
            Some("### ")
        } else if tag.eq_ignore_ascii_case("h4") {
            Some("#### ")
        } else if tag.eq_ignore_ascii_case("h5") {
            Some("##### ")
        } else if tag.eq_ignore_ascii_case("h6") {
            Some("###### ")
        } else {
            None
        }
    }

    pub fn format_state_for_box(self, box_idx: usize) -> FormatState {
        // derive formatting from style + tag
        let tag = self.box_tag(box_idx);
        let style = self.boxes().text_format(box_idx);

        let mut state = FormatState::default();
        state.italic = matches!(style.font_style, html::layout::FontStyle::Italic | html::layout::FontStyle::Oblique);
        state.bold = style.font_weight >= 600;
        state.strike = style.text_decoration.line_through();

        if tag_is_one_of(&tag, &["em", "i"]) {
            state.italic = true;
        }
        if tag_is_one_of(&tag, &["strong", "b"]) {
            state.bold = true;
        }
        if tag_is_one_of(&tag, &["s", "del", "strike"]) {
            state.strike = true;
        }
        if tag_is_one_of(&tag, &["code"]) {
            state.code = true;
            state.bold = false;
            state.italic = false;
            state.strike = false;
        }
        state
    }

    pub fn build_selection_text(self, start: u32, end: u32) -> (Option<String>, Option<String>) {
        // extract selected text as plain and markdown
        let mut plain = String::new();
        let mut markdown = String::new();
        let mut current_format = FormatState::default();
        let mut prev_block: Option<usize> = None;
        let mut at_block_start = true;

        for run in self.text().text_runs() {
            let glyphs = run.glyphs();
            if glyphs.end <= start || glyphs.start >= end {
                continue;
            }
            let run_start = glyphs.start.max(start);
            let run_end = glyphs.end.min(end);
            let box_idx = run.box_idx();
            let block = self.block_ancestor(box_idx);
            let desired_format = self.format_state_for_box(box_idx);

            if let Some(prev) = prev_block
                && prev != block
            {
                apply_format_transition(&mut markdown, current_format, FormatState::default());
                current_format = FormatState::default();
                push_newline(&mut plain, &mut markdown);
                at_block_start = true;
            }

            if at_block_start {
                if let Some(prefix) = self.header_prefix(block) {
                    markdown.push_str(prefix);
                }
                at_block_start = false;
            }

            if desired_format != current_format {
                apply_format_transition(&mut markdown, current_format, desired_format);
                current_format = desired_format;
            }

            for i in run_start..run_end {
                let text = self.text();
                let glyph_index = text.glyph_at(i as usize).unwrap_or_default();
                let c = text.glyph_metric(glyph_index).expect("layout glyph IDs are registered").ch();
                if c == '\n' {
                    apply_format_transition(&mut markdown, current_format, FormatState::default());
                    current_format = FormatState::default();
                    push_newline(&mut plain, &mut markdown);
                    at_block_start = true;
                } else {
                    plain.push(c);
                    if current_format.code && c == '`' {
                        markdown.push_str("\\`");
                    } else {
                        markdown.push(c);
                    }
                }
            }
            prev_block = Some(block);
        }

        apply_format_transition(&mut markdown, current_format, FormatState::default());
        if plain.is_empty() {
            return (None, None);
        }
        (Some(plain), Some(markdown))
    }

    pub fn find_line_for_glyph(self, glyph_idx: u32) -> Option<usize> {
        self.text().line_index_for_glyph(glyph_idx)
    }

    pub fn line_text_intersections(self, line_idx: usize, start: u32, end: u32) -> impl Iterator<Item = (u32, u32)> + 'a {
        self.text().line_text_fragments(line_idx).into_iter().flatten().filter_map(move |fragment| {
            let glyphs = fragment.glyphs();
            let fragment_start = start.max(glyphs.start);
            let fragment_end = end.min(glyphs.end);
            (start < end && fragment_start < fragment_end).then_some((fragment_start, fragment_end))
        })
    }

    pub fn glyph_advance_override(self, line_idx: usize, glyph_idx: u32) -> Option<f64> {
        self.text().line_glyph_advances(line_idx)?.iter().find(|run| run.range().contains(&glyph_idx)).map(|run| run.advance())
    }

    pub fn box_descends_from(self, mut box_idx: usize, ancestor: usize) -> bool {
        let boxes = self.boxes();
        loop {
            if box_idx == ancestor {
                return true;
            }
            let Some(parent) = boxes.parent(box_idx) else { return false };
            box_idx = parent;
        }
    }

    pub fn nearest_table_ancestor(self, mut box_idx: usize) -> Option<usize> {
        let boxes = self.boxes();
        loop {
            if boxes.is_table(box_idx) {
                return Some(box_idx);
            }
            box_idx = boxes.parent(box_idx)?;
        }
    }

    pub fn nearest_tag_ancestor(self, mut box_idx: usize, wanted: &str) -> Option<usize> {
        let boxes = self.boxes();
        loop {
            if boxes.tag(box_idx).is_some_and(|tag| tag.eq_ignore_ascii_case(wanted)) {
                return Some(box_idx);
            }
            box_idx = boxes.parent(box_idx)?;
        }
    }

    pub fn nearest_code_block_ancestor(self, mut box_idx: usize) -> Option<usize> {
        let boxes = self.boxes();
        loop {
            let tag = boxes.tag(box_idx);
            if tag.is_some_and(|tag| tag.eq_ignore_ascii_case("pre") || (tag.eq_ignore_ascii_case("code") && boxes.is_block_container(box_idx))) {
                return Some(box_idx);
            }
            box_idx = boxes.parent(box_idx)?;
        }
    }

    pub fn box_text_glyph_ranges(self, box_idx: usize, include_marker: bool) -> Vec<Range<u32>> {
        let text = self.text();
        let mut ranges = text.text_runs().filter(|run| self.box_descends_from(run.box_idx(), box_idx)).map(|run| run.glyphs()).collect::<Vec<_>>();
        if include_marker && let Some(marker) = self.boxes().list_marker(box_idx) {
            ranges.extend(text.marker_runs().only_box(marker.marker_box()).map(|run| run.glyphs()));
        }
        ranges.sort_by_key(|range| range.start);
        ranges
    }

    pub fn cell_glyph_range(self, table_box: usize, cell: &RenderTableCell) -> Option<std::ops::Range<u32>> {
        let mut start = u32::MAX;
        let mut end = 0;
        for run in self.text().text_runs() {
            if self.box_descends_from(run.box_idx(), cell.box_idx()) && self.nearest_table_ancestor(run.box_idx()) == Some(table_box) {
                start = start.min(run.glyphs().start);
                end = end.max(run.glyphs().end);
            }
        }
        (start < end).then_some(start..end)
    }

    pub fn glyph_text(self, ranges: &[Range<u32>]) -> String {
        let text = self.text();
        let mut output = String::new();
        for range in ranges {
            for index in range.clone() {
                if let Some(metric) = text.glyph_at(index as usize).and_then(|glyph| text.glyph_metric(glyph)) {
                    output.push(metric.ch());
                }
            }
        }
        output
    }

    pub fn list_marker_markdown(self, list_item: usize) -> String {
        let text = self.text();
        let marker = self.boxes().list_marker(list_item).map(|marker| {
            text.marker_runs().only_box(marker.marker_box()).flat_map(|run| run.glyphs()).filter_map(|index| text.glyph_at(index as usize).and_then(|glyph| text.glyph_metric(glyph)).map(|metric| metric.ch())).collect::<String>()
        });
        let fallback = if self.boxes().ancestors(list_item).any(|ancestor| self.boxes().tag(ancestor).is_some_and(|tag| tag.eq_ignore_ascii_case("ol"))) { "1." } else { "-" };
        let marker = marker.as_deref().unwrap_or(fallback).trim();
        if marker.chars().next().is_some_and(|character| character.is_alphanumeric()) && (marker.ends_with('.') || marker.ends_with(')')) { marker.to_owned() } else { "-".to_owned() }
    }

    pub fn direct_list_item_text_ranges(self, list_item: usize) -> Vec<Range<u32>> {
        self.text().text_runs().filter(|run| self.box_descends_from(run.box_idx(), list_item) && self.nearest_tag_ancestor(run.box_idx(), "li") == Some(list_item)).map(|run| run.glyphs()).collect()
    }

    pub fn list_item_markdown(self, list_item: usize, depth: usize) -> String {
        let own_ranges = self.direct_list_item_text_ranges(list_item);
        let mut body = String::new();
        for range in own_ranges {
            if let Some(segment) = self.build_selection_text(range.start, range.end).1 {
                body.push_str(&segment);
            }
        }
        let indent = "    ".repeat(depth);
        let continuation = format!("\n{indent}    ");
        let mut output = format!("{indent}{} {}", self.list_marker_markdown(list_item), body.trim_end().replace('\n', &continuation));

        let boxes = self.boxes();
        for child in (0..boxes.len()).filter(|&candidate| boxes.tag(candidate).is_some_and(|tag| tag.eq_ignore_ascii_case("li")) && self.nearest_tag_ancestor(boxes.parent(candidate).unwrap_or(candidate), "li") == Some(list_item)) {
            output.push('\n');
            output.push_str(&self.list_item_markdown(child, depth + 1));
        }
        output
    }

    pub fn code_block_language(self, block: usize) -> Option<String> {
        let boxes = self.boxes();
        let language_from_class = |class: &str| class.split_ascii_whitespace().find_map(|token| token.strip_prefix("language-").or_else(|| token.strip_prefix("lang-")).filter(|language| !language.is_empty()).map(str::to_owned));
        if let Some(language) = boxes.attribute(block, "class").and_then(language_from_class) {
            return Some(language);
        }
        if let Some(language) = boxes.attribute(block, "data-language").filter(|language| !language.is_empty()) {
            return Some(language.to_owned());
        }
        for candidate in 0..boxes.len() {
            if !self.box_descends_from(candidate, block) || !boxes.tag(candidate).is_some_and(|tag| tag.eq_ignore_ascii_case("code")) {
                continue;
            }
            if let Some(language) = boxes.attribute(candidate, "class").and_then(language_from_class) {
                return Some(language);
            }
            if let Some(language) = boxes.attribute(candidate, "data-language").filter(|language| !language.is_empty()) {
                return Some(language.to_owned());
            }
        }
        None
    }

    pub fn code_block_markdown(self, block: usize, ranges: &[Range<u32>]) -> String {
        let source = self.glyph_text(ranges);
        let longest_backticks = source.split(|character| character != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(3.max(longest_backticks + 1));
        let language = self.code_block_language(block).unwrap_or_default();
        let trailing_newline = if source.ends_with('\n') { "" } else { "\n" };
        format!("{fence}{language}\n{source}{trailing_newline}{fence}")
    }

    pub fn effective_line_height(self, line_idx: usize, fallback: f64) -> f64 {
        if line_idx >= self.text().line_count() {
            return fallback;
        }
        let mut max_height = 0.0;
        for frag in self.view().fragments().images_for_line(line_idx).iter() {
            let (_, height) = scaled_image_size(&frag);
            if height > max_height {
                max_height = height;
            }
        }
        if max_height > 0.0 { max_height } else { fallback }
    }

    pub fn find_line_index_for_decoration(self, y0: f64, y1: f64) -> Option<usize> {
        // find first line overlapping a decoration rect
        let lines = self.text().lines();
        if lines.is_empty() {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = lines.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let line = lines.get(mid).expect("binary-search index must be within line count");
            if line.point().y + line.height() < y0 {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        for idx in lo..lines.len() {
            let line = lines.get(idx).expect("line index must be within line count");
            let ly0 = line.point().y;
            let ly1 = line.point().y + line.height();
            if ly0 > y1 {
                break;
            }
            // Inline top and bottom borders sit immediately outside the line
            // box and therefore only touch its edge. Keep that owning line so
            // pagination can project the decoration into the correct column.
            if ly1 >= y0 && ly0 <= y1 {
                return Some(idx);
            }
        }
        None
    }

    pub fn first_line_in_vertical_range(self, top: f64, bottom: f64) -> Option<usize> {
        let lines = self.text().lines();
        // Bottom captions can occur before their rows in source/line order
        // while being below them geometrically, so this cannot be a binary
        // search over line Y coordinates.
        lines.iter().position(|line| line.point().y >= top - 0.01 && line.point().y < bottom - 0.01)
    }

    pub fn start_line_at(self, offset_y: f64, epsilon: f64) -> usize {
        let lines = self.text().lines();
        let mut lo = 0;
        let mut hi = lines.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let line = lines.get(mid).expect("binary-search index must be within line count");
            if line.point().y + line.height() <= offset_y + epsilon {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub fn end_line_at(self, offset_y: f64, epsilon: f64) -> Option<usize> {
        let lines = self.text().lines();
        let Some(first_line) = lines.first() else {
            return None;
        };
        if first_line.point().y + first_line.height() > offset_y + epsilon {
            return None;
        }
        let mut lo = 0;
        let mut hi = lines.len() - 1;
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            let line = lines.get(mid).expect("binary-search index must be within line count");
            if line.point().y + line.height() <= offset_y + epsilon {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        Some(lo)
    }


    pub fn best_anchor_from_glyphs(self, glyph_limit: u32, filter: Option<&HashSet<u16>>) -> Option<u16> {
        let mut best: Option<(u16, u32)> = None;
        for (id_idx, glyph_idx) in self.view().addressing().anchor_glyphs() {
            if filter.is_some_and(|f| !f.contains(&id_idx)) {
                continue;
            }
            let is_better = match best {
                None => true,
                Some((_, best_glyph)) => glyph_idx > best_glyph,
            };
            if glyph_idx <= glyph_limit && is_better {
                best = Some((id_idx, glyph_idx));
            }
        }
        best.map(|(id_idx, _)| id_idx)
    }

    pub fn best_anchor_from_positions(self, current_y: f64, filter: Option<&HashSet<u16>>) -> Option<u16> {
        let mut best: Option<(u16, f64, u32)> = None;
        for (id_idx, pos) in self.view().addressing().anchor_positions().iter() {
            if filter.is_some_and(|f| !f.contains(&id_idx)) {
                continue;
            }
            let is_better = match best {
                None => true,
                Some((_, best_y, best_order)) => pos.y() > best_y || (pos.y() == best_y && pos.order() > best_order),
            };
            if pos.y() <= current_y + 0.1 && is_better {
                best = Some((id_idx, pos.y(), pos.order()));
            }
        }
        best.map(|(id_idx, _, _)| id_idx)
    }

    pub fn find_matches_with_options(self, query: &str, options: SearchOptions) -> Vec<(u32, u32)> {
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

    pub fn glyph_range_for_anchor(self, fragment: &str) -> Option<(u32, u32)> {
        self.view().addressing().glyph_range_for_anchor(fragment).map(|range| (range.start, range.end))
    }

    /// Locates an annotation whose CFI no longer resolves, by matching its
    /// quoted text against the document with the recorded prefix and suffix as
    /// disambiguators. Returns `None` for annotations belonging to another
    /// spine document.
    pub fn recover_annotation_range(self, annotation: &RendererAnnotation, current_doc: usize) -> Option<(u32, u32)> {
        if html_view_cfi::parse_cfi_spine_only(&annotation.cfi_range)? != current_doc {
            return None;
        }
        let view = self.text();
        let document = view.glyph_slice(0..view.glyph_count() as u32)?.iter().map(|glyph| view.glyph_metric(*glyph).map(|metric| metric.ch())).collect::<Option<Vec<_>>>()?;
        let exact: Vec<char> = annotation.exact_text.chars().collect();
        if exact.is_empty() || exact.len() > document.len() {
            return None;
        }
        let prefix: Vec<char> = annotation.prefix.as_deref().unwrap_or_default().chars().collect();
        let suffix: Vec<char> = annotation.suffix.as_deref().unwrap_or_default().chars().collect();
        (0..=document.len() - exact.len()).find_map(|start| {
            let end = start + exact.len();
            if document[start..end] != exact {
                return None;
            }
            let prefix_matches = prefix.is_empty() || document[..start].ends_with(&prefix);
            let suffix_matches = suffix.is_empty() || document[end..].starts_with(&suffix);
            (prefix_matches && suffix_matches).then_some((start as u32, end as u32))
        })
    }
}

fn tag_is_one_of(tag: &str, candidates: &[&str]) -> bool {
    // case-insensitive tag membership check
    candidates.iter().any(|c| tag.eq_ignore_ascii_case(c))
}

fn push_newline(plain: &mut String, markdown: &mut String) {
    // keep plain/markdown buffers in sync on line breaks
    if !plain.ends_with('\n') {
        plain.push('\n');
    }
    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }
}

fn apply_format_transition(markdown: &mut String, prev: FormatState, next: FormatState) {
    // emit markdown toggles for format changes
    if prev.italic && !next.italic {
        markdown.push('*');
    }
    if prev.bold && !next.bold {
        markdown.push_str("**");
    }
    if prev.strike && !next.strike {
        markdown.push_str("~~");
    }
    if prev.code && !next.code {
        markdown.push('`');
    }
    if !prev.strike && next.strike {
        markdown.push_str("~~");
    }
    if !prev.bold && next.bold {
        markdown.push_str("**");
    }
    if !prev.italic && next.italic {
        markdown.push('*');
    }
    if !prev.code && next.code {
        markdown.push('`');
    }
}

fn normalized_chars(chars: impl IntoIterator<Item = (usize, char)>, options: SearchOptions) -> Vec<(char, usize)> {
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

pub fn find_text_matches(text: &str, query: &str, options: SearchOptions) -> Vec<(usize, usize)> {
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

/// Compute scaled image dimensions to fit column width
pub fn scaled_image_size(frag: &html::layout::RenderImageFragment) -> (f64, f64) {
    let size = frag.size();
    (size.width, size.height)
}

