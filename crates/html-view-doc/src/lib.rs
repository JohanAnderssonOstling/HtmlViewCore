//! Read-only queries over a laid-out document.
//!
//! Every method here depends on the document alone: no viewport, no frame, no
//! navigation or selection state. Holding them behind [`DocQuery`] makes that
//! independence checkable by the compiler instead of by convention, and lets
//! the renderer pass document access around as an ordinary parameter.

use kurbo::Point;
use std::collections::HashSet;
use std::ops::Range;

use html::layout::{LaidOutDocument, RenderTableCell};
use html_view_types::{ColumnLayout, FormatState, RendererAnnotation, SearchOptions, VisibleFrame};
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

/// Where glyphs land on screen for the page currently laid out.
///
/// Screen positions depend on the document, the shaped lines held by the
/// current frame, and the column geometry -- and on nothing else. Overlay
/// painting, hit testing and selection all need this, so it lives with the
/// document queries rather than with any one of them.
#[derive(Clone, Copy)]
pub struct TextGeometry<'a> {
    pub doc: DocQuery<'a>,
    pub frame: &'a VisibleFrame,
    pub layout: &'a ColumnLayout,
}

impl<'a> TextGeometry<'a> {
    pub fn new(doc: DocQuery<'a>, frame: &'a VisibleFrame, layout: &'a ColumnLayout) -> Self {
        Self { doc, frame, layout }
    }

    pub fn shaped_line_fragments(&self, line_idx: usize) -> impl Iterator<Item = (f64, &html::layout::ShapedLine)> {
        self.frame.shaped_lines.iter().filter(move |fragment| fragment.shaped.line_index == line_idx).map(|fragment| (fragment.offset_x, &fragment.shaped))
    }

    pub fn glyph_x_in_line(&self, line: &html::layout::RenderLine, glyph_idx: u32) -> f64 {
        self.glyph_x_in_line_with_affinity(line, glyph_idx, false)
    }

    pub fn glyph_x_in_line_trailing(&self, line: &html::layout::RenderLine, glyph_idx: u32) -> f64 {
        self.glyph_x_in_line_with_affinity(line, glyph_idx, true)
    }

    fn glyph_x_in_line_with_affinity(&self, line: &html::layout::RenderLine, glyph_idx: u32, trailing: bool) -> f64 {
        // measure x offset of glyph within line
        let line_idx = Some(line.index());
        let position = glyph_idx.clamp(line.start(), line.end());
        if let Some(line_idx) = line_idx {
            let leading = || self.shaped_line_fragments(line_idx).find(|(_, shaped)| shaped.text_range.start == position);
            let trailing_fragment = || self.shaped_line_fragments(line_idx).find(|(_, shaped)| shaped.text_range.start < position && position <= shaped.text_range.end);
            let containing = if trailing { trailing_fragment().or_else(leading) } else { leading().or_else(trailing_fragment) };
            if let Some((offset_x, shaped)) = containing
                && let Some(x) = shaped.x_for_text_position(position)
            {
                return line.optical_offset_x() + offset_x + x;
            }
        }
        let text = self.doc.text();
        let fallback_fragment = line_idx.and_then(|line_idx| {
            let leading = || text.line_text_fragments(line_idx)?.find(|fragment| fragment.glyphs().start == position);
            let trailing_fragment = || {
                text.line_text_fragments(line_idx)?.find(|fragment| {
                    let range = fragment.glyphs();
                    range.start < position && position <= range.end
                })
            };
            if trailing { trailing_fragment().or_else(leading) } else { leading().or_else(trailing_fragment) }
        });
        let (glyphs, mut x) = fallback_fragment.map_or_else(|| (line.glyphs(), line.optical_offset_x()), |fragment| (fragment.glyphs(), line.optical_offset_x() + fragment.offset_x()));
        for i in glyphs.start..glyphs.end {
            if i >= glyph_idx {
                break;
            }
            let glyph_index = text.glyph_at(i as usize).unwrap_or_default();
            let metric = text.glyph_metric(glyph_index).expect("layout glyph IDs are registered");
            if let Some(advance) = line_idx.and_then(|line_idx| self.doc.glyph_advance_override(line_idx, i)) {
                x += advance;
            } else {
                let next_is_conditional_hyphen = text.glyph_at(i as usize + 1).and_then(|glyph| text.glyph_metric(glyph)).is_some_and(|next| next.ch() == '\u{00ad}');
                let tracking = if i + 1 < line.end() && text.is_character_cluster_boundary(i + 1) && metric.ch() != '\u{00ad}' && !next_is_conditional_hyphen { line.letter_spacing() } else { 0.0 };
                x += text.character_advance(i).unwrap_or_else(|| metric.advance()) as f64 + tracking;
                if metric.ch() == ' ' {
                    x += line.word_spacing();
                }
            }
        }
        x
    }

    pub fn glyph_x_span_in_line(&self, line: &html::layout::RenderLine, start: u32, end: u32) -> (f64, f64) {
        let x0 = self.glyph_x_in_line(line, start);
        let x1 = self.glyph_x_in_line_trailing(line, end);
        if x0 <= x1 { (x0, x1) } else { (x1, x0) }
    }

    pub fn hit_test_glyph(&self, pos: Point) -> Option<u32> {
        // find nearest glyph index for a screen position
        let lines = self.doc.text().lines();
        if self.frame.last_line_positions.is_empty() || lines.is_empty() || self.layout.col_count <= 0.0 {
            return None;
        }

        let max_col = self.layout.col_count as i32 - 1;
        let target_col = (((pos.x - self.layout.col_gap) / (self.layout.col_width + self.layout.col_gap)).floor() as i32).clamp(0, max_col); // hit-test only within column

        let mut best_idx: Option<usize> = None;
        let mut best_vertical_dist = f64::INFINITY;
        let mut best_horizontal_dist = f64::INFINITY;
        for (idx, screen) in self.frame.last_line_positions.iter() {
            if screen.col_index != target_col {
                continue;
            }
            let Some(line) = lines.get(idx) else {
                continue;
            };
            let y0 = screen.point.y;
            let y1 = y0 + line.height();
            let vertical_dist = if pos.y < y0 {
                y0 - pos.y
            } else if pos.y > y1 {
                pos.y - y1
            } else {
                0.0
            };

            // Multiple independent line boxes can occupy the same vertical
            // band, most commonly in adjacent table cells. Resolve those
            // candidates by their painted horizontal text span instead of
            // letting the first cell in source order capture every click.
            let glyphs = line.glyphs();
            if glyphs.is_empty() {
                continue;
            }
            let (x0, x1) = self.glyph_x_span_in_line(&line, glyphs.start, glyphs.end);
            let (x0, x1) = (screen.point.x + x0, screen.point.x + x1);
            let horizontal_dist = if pos.x < x0 {
                x0 - pos.x
            } else if pos.x > x1 {
                pos.x - x1
            } else {
                0.0
            };
            if vertical_dist < best_vertical_dist || (vertical_dist == best_vertical_dist && horizontal_dist < best_horizontal_dist) {
                best_vertical_dist = vertical_dist;
                best_horizontal_dist = horizontal_dist;
                best_idx = Some(idx);
            }
        }

        let line_idx = best_idx?;
        let line = lines.get(line_idx)?;
        let screen = self.frame.last_line_positions.get(line_idx)?;
        let rel_x = pos.x - screen.point.x;
        let mut native_hit = None;
        let mut native_distance = f64::INFINITY;
        let text = self.doc.text();
        let contains_conditional_hyphen = line.glyphs().any(|index| text.glyph_at(index as usize).and_then(|glyph| text.glyph_metric(glyph)).is_some_and(|metric| metric.ch() == '\u{00ad}'));
        if line.letter_spacing() == 0.0 && line.word_spacing() == 0.0 && !contains_conditional_hyphen {
            for (offset_x, shaped) in self.shaped_line_fragments(line_idx) {
                let position = shaped.closest_text_position(rel_x - line.optical_offset_x() - offset_x);
                if let Some(x) = shaped.x_for_text_position(position) {
                    let distance = (rel_x - (line.optical_offset_x() + offset_x + x)).abs();
                    if distance < native_distance {
                        native_distance = distance;
                        native_hit = Some(position);
                    }
                }
            }
        }
        if native_hit.is_some() {
            return native_hit;
        }
        if rel_x <= line.optical_offset_x() {
            return Some(line.start());
        }

        let fragments = text.line_text_fragments(line_idx)?;
        let mut fallback_hit = None;
        let mut fallback_distance = f64::INFINITY;
        for fragment in fragments {
            let mut x = line.optical_offset_x() + fragment.offset_x();
            let glyphs = fragment.glyphs();
            for i in glyphs.start..glyphs.end {
                let glyph_index = text.glyph_at(i as usize).unwrap_or_default();
                let metric = text.glyph_metric(glyph_index).expect("layout glyph IDs are registered");
                let override_advance = self.doc.glyph_advance_override(line_idx, i);
                let next_is_conditional_hyphen = text.glyph_at(i as usize + 1).and_then(|glyph| text.glyph_metric(glyph)).is_some_and(|next| next.ch() == '\u{00ad}');
                let tracking = if i + 1 < line.end() && text.is_character_cluster_boundary(i + 1) && metric.ch() != '\u{00ad}' && !next_is_conditional_hyphen { line.letter_spacing() } else { 0.0 };
                let w = override_advance.unwrap_or(text.character_advance(i).unwrap_or_else(|| metric.advance()) as f64 + tracking + if metric.ch() == ' ' { line.word_spacing() } else { 0.0 });
                let midpoint = x + w / 2.0;
                let distance = (rel_x - midpoint).abs();
                if distance < fallback_distance {
                    fallback_distance = distance;
                    fallback_hit = Some(if rel_x < midpoint { i } else { i + 1 });
                }
                x += w;
            }
        }
        fallback_hit.or(Some(line.end()))
    }

    pub fn hit_test_image(&self, pos: Point) -> Option<u32> {
        if self.frame.last_line_positions.is_empty() || self.layout.col_count <= 0.0 {
            return None;
        }

        let max_col = self.layout.col_count as i32 - 1;
        let target_col = (((pos.x - self.layout.col_gap) / (self.layout.col_width + self.layout.col_gap)).floor() as i32).clamp(0, max_col);

        let fragments = self.doc.view().fragments();
        for (line_idx, screen) in self.frame.last_line_positions.iter() {
            if screen.col_index != target_col || fragments.images_for_line(line_idx).is_empty() {
                continue;
            }
            for frag in fragments.images_for_line(line_idx).iter() {
                let size = frag.size();
                let offset = frag.offset();
                let (width, height) = (size.width, size.height);
                let x0 = screen.point.x + offset.x;
                let y0 = screen.point.y + offset.y;
                let x1 = x0 + width;
                let y1 = y0 + height;
                if pos.x >= x0 && pos.x <= x1 && pos.y >= y0 && pos.y <= y1 {
                    return Some(frag.image_idx());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use html::layout::{FontSlant, GlyphId, GlyphMetric, GlyphRegistry, GlyphShaper, LayoutConstraints};
    use html::pipeline::DocumentFactory;
    use html_view_types::{SearchScope, SearchOptions as Opts};
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestShaper {
        glyphs: HashMap<(char, u32), GlyphId>,
    }

    impl GlyphShaper for TestShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
        }

        fn shape_glyph<'a>(&mut self, registry: &mut GlyphRegistry<'a>, ch: char, font_size: f32, _weight: u16, _slant: FontSlant, _color: u32, _family: Option<&str>) -> Result<GlyphId, html::layout::ShapeError> {
            let key = (ch, font_size.to_bits());
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(glyph);
            }
            let metric = GlyphMetric::try_new(ch, font_size * 0.5, font_size * 0.75, font_size * 0.25, font_size * 0.75).map_err(html::layout::ShapeError::rejected_metric)?;
            let glyph = registry.register(metric)?;
            self.glyphs.insert(key, glyph);
            Ok(glyph)
        }
    }

    fn layout(html: &str) -> LaidOutDocument {
        let mut factory = DocumentFactory::new();
        let mut shaper = TestShaper::default();
        factory
            .parse_with_new_pipeline(html, None)
            .shape(&mut shaper)
            .expect("the test shaper registers every glyph")
            .layout(LayoutConstraints::new(600.0, 16.0).expect("constraints must be valid"))
    }

    fn options() -> Opts {
        Opts { match_case: false, whole_word: false, match_diacritics: false, scope: SearchScope::WholeBook }
    }

    /// The glyph index of the first occurrence of `wanted`.
    fn glyph_of(doc: DocQuery<'_>, wanted: char) -> u32 {
        let text = doc.text();
        (0..text.glyph_count() as u32).find(|index| text.glyph_at(*index as usize).and_then(|glyph| text.glyph_metric(glyph)).is_some_and(|metric| metric.ch() == wanted)).expect("character must be present")
    }

    // -- text extraction ----------------------------------------------------

    #[test]
    fn selection_text_spans_block_boundaries_with_newlines() {
        let document = layout("<p>alpha</p><p>beta</p>");
        let doc = DocQuery::new(&document);
        let (plain, _) = doc.build_selection_text(0, doc.text().glyph_count() as u32);

        let plain = plain.expect("a non-empty selection yields text");
        assert!(plain.contains("alpha") && plain.contains("beta"));
        assert!(plain.contains('\n'), "separate blocks must not run together: {plain:?}");
    }

    #[test]
    fn selection_markdown_marks_emphasis_and_headings() {
        let document = layout("<h2>Title</h2><p><em>soft</em> and <strong>hard</strong></p>");
        let doc = DocQuery::new(&document);
        let (_, markdown) = doc.build_selection_text(0, doc.text().glyph_count() as u32);

        let markdown = markdown.expect("a non-empty selection yields markdown");
        // Headings also carry a bold run, because the default h2 font weight
        // trips the same threshold as <strong>. The result is redundant but
        // valid markdown: `## **Title**`.
        assert!(markdown.starts_with("## "), "h2 becomes a level-two heading: {markdown:?}");
        assert!(markdown.contains("Title"), "the heading text survives: {markdown:?}");
        assert!(markdown.contains("*soft*"), "em becomes emphasis: {markdown:?}");
        assert!(markdown.contains("**hard**"), "strong becomes bold: {markdown:?}");
    }

    #[test]
    fn an_empty_range_yields_no_text() {
        let document = layout("<p>alpha</p>");
        let doc = DocQuery::new(&document);
        assert_eq!(doc.build_selection_text(3, 3), (None, None));
    }

    #[test]
    fn code_spans_escape_backticks_rather_than_closing_early() {
        let document = layout("<p><code>a`b</code></p>");
        let doc = DocQuery::new(&document);
        let (_, markdown) = doc.build_selection_text(0, doc.text().glyph_count() as u32);
        assert!(markdown.expect("markdown").contains("\\`"), "a literal backtick inside code must be escaped");
    }

    // -- box tree -----------------------------------------------------------

    #[test]
    fn ancestor_lookup_finds_the_nearest_match_and_stops_at_the_root() {
        let document = layout("<table><tr><td><p id='cell'>x</p></td></tr></table>");
        let doc = DocQuery::new(&document);
        let boxes = doc.boxes();
        let paragraph = (0..boxes.len()).find(|&i| boxes.attribute(i, "id") == Some("cell")).expect("the paragraph is in the box tree");

        let table = doc.nearest_table_ancestor(paragraph).expect("the paragraph sits in a table");
        assert!(doc.box_descends_from(paragraph, table));
        assert!(doc.nearest_tag_ancestor(paragraph, "td").is_some());
        assert!(doc.nearest_tag_ancestor(paragraph, "blockquote").is_none(), "a tag that is not an ancestor must not match");
    }

    #[test]
    fn a_box_descends_from_itself() {
        let document = layout("<p>alpha</p>");
        let doc = DocQuery::new(&document);
        assert!(doc.box_descends_from(0, 0));
    }

    #[test]
    fn heading_prefixes_cover_every_level() {
        let document = layout("<h1>a</h1><h3>b</h3><p>c</p>");
        let doc = DocQuery::new(&document);
        let boxes = doc.boxes();
        let tagged = |want: &str| (0..boxes.len()).find(|&i| boxes.tag(i).is_some_and(|t| t.eq_ignore_ascii_case(want))).expect("tag present");

        assert_eq!(doc.header_prefix(tagged("h1")), Some("# "));
        assert_eq!(doc.header_prefix(tagged("h3")), Some("### "));
        assert_eq!(doc.header_prefix(tagged("p")), None);
    }

    // -- line lookup --------------------------------------------------------

    #[test]
    fn start_and_end_line_lookup_agree_on_a_single_line_document() {
        let document = layout("<p>alpha</p>");
        let doc = DocQuery::new(&document);
        assert_eq!(doc.start_line_at(0.0, 0.1), 0);
        assert_eq!(doc.find_line_for_glyph(0), Some(0));
    }

    #[test]
    fn line_lookup_on_an_empty_document_does_not_panic() {
        let document = layout("");
        let doc = DocQuery::new(&document);
        assert_eq!(doc.start_line_at(0.0, 0.1), 0);
        assert_eq!(doc.end_line_at(0.0, 0.1), None);
        assert_eq!(doc.find_line_for_glyph(0), None);
    }

    #[test]
    fn start_line_advances_past_earlier_lines() {
        let document = layout("<p>alpha</p><p>beta</p><p>gamma</p>");
        let doc = DocQuery::new(&document);
        let last = doc.text().lines().len() - 1;
        let last_y = doc.text().line(last).expect("last line").point().y;
        assert_eq!(doc.start_line_at(last_y, 0.1), last, "an offset at the last line's top selects it");
        assert_eq!(doc.start_line_at(0.0, 0.1), 0);
    }

    // -- search -------------------------------------------------------------

    #[test]
    fn matching_is_case_and_diacritic_insensitive_by_default() {
        assert_eq!(find_text_matches("Café CAFE cafeteria", "cafe", options()).len(), 3);
        assert_eq!(find_text_matches("Café CAFE cafeteria", "cafe", Opts { whole_word: true, ..options() }).len(), 2);
        assert_eq!(find_text_matches("Café CAFE", "cafe", Opts { match_case: true, ..options() }).len(), 0);
        assert_eq!(find_text_matches("Café CAFE", "cafe", Opts { match_diacritics: true, ..options() }).len(), 1);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert!(find_text_matches("alpha", "", options()).is_empty());
        let document = layout("<p>alpha</p>");
        assert!(DocQuery::new(&document).find_matches_with_options("", options()).is_empty());
    }

    #[test]
    fn document_matching_returns_glyph_ranges_covering_the_query() {
        let document = layout("<p>alpha beta alpha</p>");
        let doc = DocQuery::new(&document);
        let matches = doc.find_matches_with_options("alpha", options());

        assert_eq!(matches.len(), 2, "both occurrences are found");
        for (start, end) in matches {
            assert_eq!(end - start, 5, "each match spans the query length");
            assert_eq!(doc.glyph_text(&[start..end]).to_lowercase(), "alpha");
        }
    }

    #[test]
    fn list_markers_are_excluded_from_search() {
        // Browsers exclude generated ::marker content from find-in-page.
        let document = layout("<ol><li>item</li></ol>");
        let doc = DocQuery::new(&document);
        assert!(doc.find_matches_with_options("1", options()).is_empty(), "the generated list marker must not be searchable");
    }

    // -- anchors ------------------------------------------------------------

    #[test]
    fn anchor_lookup_finds_the_nearest_preceding_id() {
        let document = layout("<p id='first'>alpha</p><p id='second'>beta</p>");
        let doc = DocQuery::new(&document);
        let beta = glyph_of(doc, 'b');

        let anchor = doc.best_anchor_from_glyphs(beta, None).expect("an anchor precedes the second paragraph");
        assert_eq!(doc.view().string(anchor), "second");
    }

    #[test]
    fn anchor_lookup_respects_a_filter() {
        let document = layout("<p id='first'>alpha</p><p id='second'>beta</p>");
        let doc = DocQuery::new(&document);
        let beta = glyph_of(doc, 'b');
        let only_first: HashSet<u16> = [doc.view().lookup_string("first").expect("id is interned")].into_iter().collect();

        let anchor = doc.best_anchor_from_glyphs(beta, Some(&only_first)).expect("the filtered anchor still resolves");
        assert_eq!(doc.view().string(anchor), "first", "a filter restricts the answer to listed anchors");
    }

    #[test]
    fn glyph_range_for_a_missing_anchor_is_none() {
        let document = layout("<p id='here'>alpha</p>");
        let doc = DocQuery::new(&document);
        assert!(doc.glyph_range_for_anchor("absent").is_none());
    }
}
