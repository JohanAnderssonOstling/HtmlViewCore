use crate::doc_query::DocQuery;
use crate::{FormatState, RendererCore};
use kurbo::Point;

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

impl<'a> DocQuery<'a> {
    pub(crate) fn block_ancestor(self, box_idx: usize) -> usize {
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

    pub(crate) fn box_tag(self, box_idx: usize) -> String {
        // get tag name for a box
        self.boxes().tag(box_idx).unwrap_or("").to_string()
    }

    pub(crate) fn header_prefix(self, block_idx: usize) -> Option<&'static str> {
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

    pub(crate) fn format_state_for_box(self, box_idx: usize) -> FormatState {
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

    pub(crate) fn build_selection_text(self, start: u32, end: u32) -> (Option<String>, Option<String>) {
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

    pub(crate) fn find_line_for_glyph(self, glyph_idx: u32) -> Option<usize> {
        self.text().line_index_for_glyph(glyph_idx)
    }

    pub(crate) fn line_text_intersections(self, line_idx: usize, start: u32, end: u32) -> impl Iterator<Item = (u32, u32)> + 'a {
        self.text().line_text_fragments(line_idx).into_iter().flatten().filter_map(move |fragment| {
            let glyphs = fragment.glyphs();
            let fragment_start = start.max(glyphs.start);
            let fragment_end = end.min(glyphs.end);
            (start < end && fragment_start < fragment_end).then_some((fragment_start, fragment_end))
        })
    }

    pub(crate) fn glyph_advance_override(self, line_idx: usize, glyph_idx: u32) -> Option<f64> {
        self.text().line_glyph_advances(line_idx)?.iter().find(|run| run.range().contains(&glyph_idx)).map(|run| run.advance())
    }
}

impl RendererCore {
    pub fn selection_range(&self) -> Option<(u32, u32)> {
        // normalize selection anchor/active into a range
        let (Some(a), Some(b)) = (self.selection.selection_anchor, self.selection.selection_active) else {
            return None;
        };
        if a == b {
            return None;
        }
        let (start, end) = if a < b { (a, b) } else { (b, a) }; // normalize range order
        Some((start, end))
    }

    pub fn update_selection_text(&mut self) {
        // refresh selection buffers based on current range
        let Some((start, end)) = self.selection_range() else {
            self.selection.selected_text = None;
            self.selection.selected_text_markdown = None;
            return;
        };
        let (plain, ordinary_markdown) = self.doc().build_selection_text(start, end);
        let markdown = if self.selection.semantic_selection { self.build_semantic_selection_text(start, end).1 } else { ordinary_markdown };
        self.selection.selected_text = plain;
        self.selection.selected_text_markdown = markdown;
    }

    /// Returns whether a point intersects the selection as it is painted.
    pub fn selection_contains_point(&self, position: Point) -> bool {
        let Some((selection_start, selection_end)) = self.selection_range() else {
            return false;
        };
        if self.semantic_selection_contains_point(position, selection_start, selection_end) {
            return true;
        }
        let promoted = self.semantic_promoted_glyph_ranges(selection_start, selection_end);
        for (line_idx, screen) in self.frame.last_line_positions.iter() {
            let Some(line) = self.doc().text().line(line_idx) else {
                continue;
            };
            if position.y < screen.point.y || position.y > screen.point.y + line.height() {
                continue;
            }

            let mut ranges = self.doc().line_text_intersections(line_idx, selection_start, selection_end).collect::<Vec<_>>();
            for range in &promoted {
                ranges.extend(self.doc().line_text_intersections(line_idx, range.start, range.end));
            }
            for (start, end) in ranges {
                let (x0, x1) = self.glyph_x_span_in_line(&line, start, end);
                let (x0, x1) = (screen.point.x + x0, screen.point.x + x1);
                let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };

                if position.x >= x0 && position.x <= x1 {
                    return true;
                }
            }
        }
        false
    }

    pub fn glyph_x_in_line(&self, line: &html::layout::RenderLine, glyph_idx: u32) -> f64 {
        self.glyph_x_in_line_with_affinity(line, glyph_idx, false)
    }

    pub(crate) fn glyph_x_in_line_trailing(&self, line: &html::layout::RenderLine, glyph_idx: u32) -> f64 {
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
        let text = self.doc().text();
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
            if let Some(advance) = line_idx.and_then(|line_idx| self.doc().glyph_advance_override(line_idx, i)) {
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
        let lines = self.doc().text().lines();
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
        let text = self.doc().text();
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
                let override_advance = self.doc().glyph_advance_override(line_idx, i);
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

    pub(crate) fn shaped_line_fragments(&self, line_idx: usize) -> impl Iterator<Item = (f64, &html::layout::ShapedLine)> {
        self.frame.shaped_lines.iter().filter(move |fragment| fragment.shaped.line_index == line_idx).map(|fragment| (fragment.offset_x, &fragment.shaped))
    }

    pub fn hit_test_image(&self, pos: Point) -> Option<u32> {
        if self.frame.last_line_positions.is_empty() || self.layout.col_count <= 0.0 {
            return None;
        }

        let max_col = self.layout.col_count as i32 - 1;
        let target_col = (((pos.x - self.layout.col_gap) / (self.layout.col_width + self.layout.col_gap)).floor() as i32).clamp(0, max_col);

        let fragments = self.doc().view().fragments();
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
