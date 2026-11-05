#[cfg(test)]
use html_engine_model::page_placements;
#[cfg(test)]
use html_pipeline::engine::{CaretAffinity, geometry_queries};
#[cfg(test)]
use html::targets::{DocumentTarget, ResolvedDestination};
#[cfg(test)]
use html_pipeline::engine::export_queries;
#[cfg(test)]
use html_dom as dom_values;
use html::export::{ExportRequest, TableExportFormat};
#[cfg(test)]
use html::layout::BoxId;
use html::selection::{SelectionInput, SelectionKind, SelectionOutcome, SelectionUnit};
use html::text::GlyphResourceGeneration;
#[cfg(test)]
use html::text::{GlyphRequest, FontMetrics, ShapedGlyph};
use std::ops::Range;
use std::sync::Arc;
use web_time::Instant;

use kurbo::{Point, Rect, Size};

use crate::{
    ClipboardWrite, FrameDisplayCacheKeys, GlyphShaper, InteractionPalette, Painter,
    ReadingTarget, RendererCommand, RendererCore, RendererEffects,
    RendererOperation, RendererRevisions, RendererServices, ResourceProvider,
};

pub use html::pagination::PageReadingLine as ReadingLine;

/// A match on the current page, with a publication address ready for overlays.
#[derive(Clone, Debug)]
pub struct VisibleTextMatch<'a> {
    pub pattern: usize,
    pub glyphs: Range<u32>,
    pub text: &'a str,
    pub cfi_range: String,
}

/// A matched page whose text is retained until its occurrences are consumed.
/// CFI formatting happens on demand; the application copies only accepted text.
pub struct VisibleTextMatches<'a> {
    source: Option<html::search::SourceTextMatches<'a>>,
    document: usize,
    cfi: &'a crate::cfi::PublicationCfi,
}

impl VisibleTextMatches<'_> {
    pub fn iter(&self) -> impl Iterator<Item = VisibleTextMatch<'_>> + '_ {
        self.source
            .iter()
            .flat_map(|source| source.iter())
            .filter_map(move |found| {
                let cfi = crate::cfi::generate_cfi_range_from_source_positions(
                    self.document,
                    &found.source.start,
                    &found.source.end,
                );
                Some(VisibleTextMatch {
                    pattern: found.pattern,
                    glyphs: found.glyphs,
                    text: found.text,
                    cfi_range: self.cfi.to_publication(&cfi)?,
                })
            })
    }
}

/// The high-level renderer API used by UI backends.
///
/// A session owns both the renderer state and its glyph shaper so callers do
/// not have to coordinate them for navigation, relayout, or link activation.
/// After construction and each operation, drain effects into a reusable buffer
/// and perform them in the shell. Clipboard methods queue a write; platform
/// clipboard failures belong to the shell. Use `apply` for command-backed
/// settings, document jumps, and search activation or stepping. Intrinsic
/// document queries are available through `render_view`.
///
/// Document interaction positions and returned bounds use painted coordinates
/// relative to the document's top-left. Scale conversion stays inside the
/// session. Note positions use the note's own coordinates; `render_view` exposes
/// intrinsic document coordinates.
pub struct RendererSession<S> {
    core: RendererCore,
    glyph_shaper: S,
    pointer_down_position: Option<Point>,
    navigation_revision: u64,
}

/// Content actions available at a painted document point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentHit {
    /// An image can be copied even when text paints over it.
    pub image: bool,
    /// LaTeX is offered only when an equation wins paint order at the point.
    pub equation: bool,
    pub table: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerDownOutcome {
    pub handled: bool,
    /// Retained from the image hit used to handle this press.
    pub image: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerDownOptions {
    /// Promote intersected semantic structures, currently table cells, while
    /// retaining ordinary text-range selection everywhere else.
    pub selection_mode: crate::SelectionMode,
    pub copy_image: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerMoveOutcome {
    pub handled: bool,
    pub over_link: bool,
    pub link_hover_changed: bool,
}

impl<S: GlyphShaper> RendererSession<S> {
    pub(crate) fn from_core(core: RendererCore, glyph_shaper: S) -> Self {
        Self {
            core,
            glyph_shaper,
            pointer_down_position: None,
            navigation_revision: 0,
        }
    }

    /// A note as a paintable scene: `paint` it through any [`Painter`], and
    /// size the surface from its `content_height`.
    ///
    /// [`Painter`]: html::render::Painter
    pub fn note_scene(&self) -> Option<html::render::RenderScene> {
        self.core.note_scene()
    }

    /// Starts a selection in the note's own coordinates.
    pub fn note_pointer_down(&mut self, position: Point, mode: crate::SelectionMode) -> bool {
        self.interact_note(SelectionInput::PointerDown {
            position,
            mode,
            select_text: true,
            clicks: 1,
        })
        .handled
    }

    pub fn note_pointer_move(&mut self, position: Point, mode: crate::SelectionMode) -> bool {
        self.interact_note(SelectionInput::PointerMove {
            position,
            mode,
            drag_distance: f64::from(crate::WORD_SELECTION_DRAG_DISTANCE),
        })
        .handled
    }

    pub fn note_pointer_up(&mut self, position: Point, mode: crate::SelectionMode) -> bool {
        self.interact_note(SelectionInput::PointerUp {
            position,
            mode,
            drag_distance: f64::from(crate::WORD_SELECTION_DRAG_DISTANCE),
        })
        .finished
    }

    fn interact_note(&mut self, input: SelectionInput) -> SelectionOutcome {
        let Some((note, owner)) = self.core.note_parts_mut() else {
            return SelectionOutcome::default();
        };
        note.interact(owner, input)
    }

    /// Text currently selected inside the note, for the host to copy.
    pub fn note_selection_text(&self) -> Option<String> {
        self.core
            .note_parts()
            .and_then(|(note, owner)| note.selection(owner)?.plain_text())
    }

    /// Paints the note's selection highlight. The note's own content comes
    /// from [`Self::note_scene`]; this goes over it.
    pub fn paint_note_selection(&mut self, painter: &mut impl Painter) {
        self.core.paint_note_selection(painter);
    }

    /// Drops the open note, clearing anything selected in it.
    pub fn close_note(&mut self) {
        self.core.close_note();
        self.core.update_resources(&mut self.glyph_shaper);
    }

    /// Builds the preview a note reference would open, without waiting for the
    /// reader to click one. A host with its own way into a note -- a notes
    /// index, a keyboard shortcut, a test -- goes through here.
    ///
    /// Returns `None` when the href names no note, or when notes read in place
    /// and there is nothing to show separately.
    pub fn note_preview(&mut self, href: &str) -> Option<crate::FootnotePreview> {
        self.core.footnote_preview(&mut self.glyph_shaper, href)
    }

    pub fn from_provider_with_nav(
        services: RendererServices,
        mut glyph_shaper: S,
        provider: Arc<dyn ResourceProvider>,
        document_uris: Vec<String>,
        start_index: usize,
        nav_state: Option<&str>,
        config: crate::RendererInitialConfig,
    ) -> Result<Self, String> {
        let started = (std::env::var_os("BOKHEIM_PROFILE_RESIZE").is_some()
            || std::env::var_os("GPUI_KOBO_PROFILE").is_some())
        .then(Instant::now);
        let core = RendererCore::from_provider_with_nav(
            services,
            &mut glyph_shaper,
            provider,
            document_uris,
            start_index,
            nav_state,
            config,
        )?;
        if let Some(started) = &started {
            println!(
                "HTML_CORE_STARTUP phase=core_ready elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
        let session = Self::from_core(core, glyph_shaper);
        if let Some(started) = &started {
            println!(
                "HTML_CORE_STARTUP phase=session_ready elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
        Ok(session)
    }

    pub fn from_preparation(
        services: RendererServices,
        mut glyph_shaper: S,
        prepared: crate::RendererPreparation,
    ) -> Result<Self, String> {
        let core = RendererCore::from_preparation(services, &mut glyph_shaper, prepared)?;
        Ok(Self::from_core(core, glyph_shaper))
    }

    /// Where the current selection sits on screen, for a caller placing a
    /// panel that must not cover it. `None` when nothing is selected, or when
    /// the selection is not on the page now shown.
    pub fn selection_bounds(&self) -> Option<kurbo::Rect> {
        self.core.selection_bounds()
    }

    /// Where an annotation sits on screen, for the same reason.
    pub fn annotation_bounds(&self, id: &str) -> Option<kurbo::Rect> {
        self.core.annotation_bounds(id)
    }

    /// Visible rendered lines in document order, retaining table-cell and
    /// page-column boundaries. Each line counts separately even if it overlaps
    /// another line vertically. Bands follow the cell or text block's width.
    pub fn visible_reading_lines(&self) -> Vec<ReadingLine> {
        if self.reading_document().is_none() {
            return Vec::new();
        }
        let mut lines = self.core.page.geometry().reading_lines();
        for line in &mut lines {
            let start = self
                .core
                .from_layout_point(Point::new(line.bounds.x0, line.bounds.y0));
            let end = self
                .core
                .from_layout_point(Point::new(line.bounds.x1, line.bounds.y1));
            let column_start = self
                .core
                .from_layout_point(Point::new(line.column_bounds.x0, line.column_bounds.y0));
            let column_end = self
                .core
                .from_layout_point(Point::new(line.column_bounds.x1, line.column_bounds.y1));
            line.bounds = Rect::new(start.x, start.y, end.x, end.y);
            line.column_bounds =
                Rect::new(column_start.x, column_start.y, column_end.x, column_end.y);
        }
        lines
    }

    pub fn render_view(&self) -> html::layout::RenderView<'_> {
        self.core.page.render_view()
    }

    pub fn visible_page_key(&self) -> Option<(usize, crate::FrameGeometryCacheKey)> {
        Some((self.reading_document()?, self.core.page.frame.geometry_key()?))
    }


    /// Match visible source text without exposing extraction or offset mapping.
    pub fn match_visible_text(
        &self,
        patterns: &crate::TextPatternMatcher,
    ) -> VisibleTextMatches<'_> {
        let document = self.reading_document();
        VisibleTextMatches {
            source: document.map(|_| self.core.page.geometry().match_text(patterns)),
            document: document.unwrap_or_default(),
            cfi: &self.core.nav.cfi,
        }
    }

    /// The reader palette for hosts adapting image paint. This is separate
    /// from text foreground overrides and never changes image resources.
    pub fn image_theme(&self) -> Option<html::style::ThemeAdaptation> {
        self.core.page.configuration().reader_overrides().theme
    }

    pub fn glyph_shaper(&self) -> &S {
        &self.glyph_shaper
    }

    /// Backend access between renderer operations, for example to register a
    /// font that finished loading before issuing `RendererCommand::RefreshFonts`.
    pub fn glyph_shaper_mut(&mut self) -> &mut S {
        &mut self.glyph_shaper
    }

    pub fn document_resource_generation(&self) -> GlyphResourceGeneration {
        self.core.page.session.glyph_resource_generation()
    }

    pub fn note_resource_generation(&self) -> Option<GlyphResourceGeneration> {
        self.core
            .note_parts()
            .map(|(_, owner)| owner.glyph_resource_generation())
    }

    pub fn revisions(&self) -> RendererRevisions {
        self.core.revisions
    }

    /// Returns whether visible resources are still being loaded or decoded.
    pub fn has_pending_resources(&self) -> bool {
        self.core.page.session.resources_pending()
            || self.core.note.as_ref().and_then(|note| note.owner.as_ref())
                .is_some_and(|owner| owner.resources_pending())
    }

    /// Appends pending effects to the shell's reusable output buffer.
    pub fn drain_effects(&mut self, out: &mut RendererEffects) {
        self.core.effects.drain_into(out);
    }

    /// Supplies the width used by subsequently opened popup notes.
    pub fn set_note_popup_width(&mut self, width: Option<f64>) {
        self.core.note_popup_width = width;
    }

    pub fn emit_state_snapshot(&mut self) {
        self.core.emit_state_snapshot();
    }

    pub fn apply(&mut self, command: RendererCommand) {
        let navigates = match &command {
            RendererCommand::NavigatePage(step) | RendererCommand::NavigateLine(step) => *step != 0,
            RendererCommand::NavigateToHref(_)
            | RendererCommand::NavigateToCfi(_)
            | RendererCommand::NavigateDocument(_)
            | RendererCommand::SetPosition { .. }
            | RendererCommand::SetCfiPosition(_)
            | RendererCommand::SetProgressFraction(_)
            | RendererCommand::NavigateSearch(_)
            | RendererCommand::NavigateSearchTo(_) => true,
            _ => false,
        };
        if navigates {
            self.navigation_revision = self.navigation_revision.wrapping_add(1);
        }
        self.core.apply(&mut self.glyph_shaper, command);
    }

    pub fn navigation_revision(&self) -> u64 {
        self.navigation_revision
    }

    pub fn advance_reading_document(&mut self) {
        self.core.next_document(&mut self.glyph_shaper);
    }

    pub fn current_cfi(&self) -> Option<String> {
        self.core.nav.current_cfi(self.core.nav_view())
    }

    /// Cheap readiness check: narration must not extract the old chapter during navigation.
    pub fn reading_document(&self) -> Option<usize> {
        self.core
            .pending_navigation
            .is_none()
            .then(|| self.core.nav.current_document())
    }

    /// Readable block addresses and the source glyph at the visible position.
    /// Text is materialized by the caller as each item is requested.
    pub fn reading_items(&self) -> Option<(usize, usize, u32, Vec<ReadingTarget>)> {
        let doc = self.reading_document()?;
        let start = self.core.page.geometry().reading_start();
        let (index, items) = self.render_view().addressing().reading_targets_at(start);
        Some((doc, index, start, items))
    }

    pub fn reading_target_cfi(&self, target: &ReadingTarget) -> Option<String> {
        crate::cfi::generate_cfi(
            &self.core.page,
            self.core.nav.current_document(),
            target.glyph_ranges.first()?.start,
        )
        .and_then(|cfi| self.core.nav.cfi.to_publication(&cfi))
    }

    /// Highlights a block and follows it only if its beginning is off screen.
    /// Returns whether following it changed the page, without recording manual
    /// navigation (which would interrupt narration).
    pub fn follow_reading_target(&mut self, target: ReadingTarget) -> bool {
        let mut moved = false;
        let start = target.glyph_ranges.first().map(|range| range.start);
        if let Some(start) = start {
            let visible = self.core.page.geometry().contains_glyph(start);
            if !visible {
                self.core.set_position(
                    &mut self.glyph_shaper,
                    self.core.nav.current_document(),
                    Some(start),
                );
                moved = true;
            }
        }
        self.apply(RendererCommand::SetReadingTarget(Some(target)));
        moved
    }

    pub fn resolve_href(&self, href: &str) -> Option<(usize, Option<String>)> {
        self.core.resolve_href(href)
    }

    pub fn navigate_to_href(&mut self, href: &str) -> bool {
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        self.core.navigate_to_href(&mut self.glyph_shaper, href)
    }

    pub fn set_cfi_position(&mut self, cfi: &str) -> bool {
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        self.core.navigate_cfi(&mut self.glyph_shaper, cfi, false)
    }

    pub fn column_width(&self) -> f64 {
        self.core.page.layout.base_col_width
    }

    /// Updates the horizontal space supplied by the host while retaining the
    /// user's preferred column width.
    pub fn set_available_width(&mut self, width: f64) -> bool {
        self.core.set_available_width(&mut self.glyph_shaper, width)
    }

    pub fn scale(&self) -> f64 {
        self.core.page.layout.scale
    }

    pub fn copy_selection(&mut self) -> bool {
        self.core.copy_selection_to_clipboard()
    }

    pub fn content_at(&self, position: Point) -> ContentHit {
        let position = self.core.to_layout_point(position);
        let geometry = self.core.page.geometry();
        let hit = geometry.content_at(position);
        let foreground_image = hit.as_ref().and_then(|hit| hit.image);
        ContentHit {
            image: hit
                .as_ref()
                .is_some_and(|hit| hit.underlying_image.is_some()),
            equation: foreground_image.is_some_and(|image| image.is_equation),
            table: hit.is_some_and(|hit| hit.table.is_some()),
        }
    }

    /// Convert the retained equation only on request; no LaTeX cache is kept.
    pub fn copy_equation_latex_at(&mut self, position: Point) -> bool {
        let position = self.core.to_layout_point(position);
        let Some(image) = self.core.hit_test_document_image(position) else {
            return false;
        };
        let Some(export) = self.core.page.session.export() else {
            return false;
        };
        let result = match export.request(ExportRequest::EquationLatex(image.image)) {
            Ok(None) => return false,
            Ok(Some(payload)) => payload
                .into_text()
                .ok_or_else(|| "expected a formula export".into()),
            Err(error) => Err(error.to_string()),
        };
        let result = result.map(|latex| {
            self.core
                .effects
                .clipboard
                .push(ClipboardWrite::Text(latex))
        });
        if let Err(message) = result {
            self.core
                .emit_operation_failed(RendererOperation::CopyEquationLatex, message);
        }
        true
    }

    pub fn copy_image_at(&mut self, position: Point) -> bool {
        let position = self.core.to_layout_point(position);
        let Some(image) = self.core.hit_test_document_image(position) else {
            return false;
        };
        if let Err(message) = self.core.copy_document_image_to_clipboard(image) {
            self.core
                .emit_operation_failed(RendererOperation::CopyImage, message);
        }
        true
    }

    /// Select a word without triggering a link or image action.
    pub fn select_word_at(&mut self, position: Point) -> bool {
        let position = self.core.to_layout_point(position);
        self.pointer_down_position = None;
        self.core
            .interact_selection(SelectionInput::SelectAt {
                position,
                unit: SelectionUnit::Word,
            })
            .handled
    }

    /// Select the engine-defined containing element and publish its full CFI.
    pub fn select_element_at(&mut self, position: Point) -> bool {
        let position = self.core.to_layout_point(position);
        self.pointer_down_position = None;
        self.core
            .interact_selection(SelectionInput::SelectAt {
                position,
                unit: SelectionUnit::Element,
            })
            .handled
    }

    /// Fix the opposite boundary while a selection handle moves.
    pub fn begin_selection_handle(&mut self, start: bool) -> bool {
        self.core
            .interact_selection(SelectionInput::BeginHandle { start })
            .handled
    }

    pub fn update_touch_selection(&mut self, position: Point) -> bool {
        let position = self.core.to_layout_point(position);
        self.core
            .interact_selection(SelectionInput::PointerMove {
                position,
                mode: crate::SelectionMode::Plain,
                drag_distance: f64::from(crate::WORD_SELECTION_DRAG_DISTANCE),
            })
            .handled
    }

    pub fn end_touch_selection(&mut self) {
        self.core.interact_selection(SelectionInput::Finish);
    }

    pub fn clear_text_selection(&mut self) {
        self.core
            .interact_selection(SelectionInput::Clear(SelectionKind::Text));
    }

    /// Presence checks never prepare text or clipboard payloads.
    pub fn has_text_selection(&self) -> bool {
        self.core
            .page
            .selection
            .selection(self.core.doc())
            .is_some()
    }

    /// Read the current source address without exporting selected text.
    pub fn selection_cfi(&self) -> Option<String> {
        let view = &self.core.page;
        let range = view.selection.selection(view.render_view())?.range();
        let cfi = crate::cfi::generate_cfi_range(
            view,
            self.core.nav.current_document(),
            range.start,
            range.end,
        )?;
        self.core.nav.cfi.to_publication(&cfi)
    }

    /// Prepare plain text when a copy, annotation or lookup action requests it.
    pub fn selected_passage(&self) -> Option<(String, String)> {
        Some((self.selection_cfi()?, self.core.page.selected_text()?))
    }

    pub fn selection_endpoints(&self) -> (Option<Point>, Option<Point>) {
        let (start, end) = self
            .core
            .page
            .geometry()
            .selection_geometry(&self.core.page.selection)
            .endpoints;
        (
            start.map(|point| self.core.from_layout_point(point)),
            end.map(|point| self.core.from_layout_point(point)),
        )
    }

    /// Starts the complete primary-pointer interaction. Image copying takes
    /// precedence over text selection when `copy_image` is true.
    pub fn pointer_down(
        &mut self,
        position: Point,
        options: PointerDownOptions,
    ) -> PointerDownOutcome {
        let position = self.core.to_layout_point(position);
        self.pointer_down_position = Some(position);
        let image = self.core.hit_test_document_image(position);
        let mut outcome = PointerDownOutcome {
            handled: true,
            image: image.is_some(),
        };
        let selection = self.core.interact_selection(SelectionInput::PointerDown {
            position,
            mode: options.selection_mode,
            select_text: image.is_none(),
            clicks: 1,
        });
        if selection.handled {
            return outcome;
        }
        if options.copy_image
            && let Some(image) = image
        {
            if let Err(message) = self.core.copy_document_image_to_clipboard(image) {
                self.core
                    .emit_operation_failed(RendererOperation::CopyImage, message);
            }
            return outcome;
        }
        if let Some(image) = image
            && self.core.open_image(image)
        {
            return outcome;
        }
        outcome.handled = if image.is_some() {
            self.core
                .interact_selection(SelectionInput::PointerDown {
                    position,
                    mode: options.selection_mode,
                    select_text: true,
                    clicks: 1,
                })
                .handled
        } else {
            selection.handled
        };
        outcome
    }

    /// Updates both link hover state and an active selection.
    pub fn pointer_move(
        &mut self,
        position: Point,
        mode: crate::SelectionMode,
    ) -> PointerMoveOutcome {
        let position = self.core.to_layout_point(position);
        let was_over_link = self.core.page.link_hovered;
        let over_link = self.core.link_at(position);
        if over_link {
            self.core.activate_link_cursor();
        } else {
            self.core.deactivate_link_cursor();
        }
        let handled = self
            .core
            .interact_selection(SelectionInput::PointerMove {
                position,
                mode,
                drag_distance: f64::from(crate::WORD_SELECTION_DRAG_DISTANCE),
            })
            .handled;
        PointerMoveOutcome {
            handled,
            over_link,
            link_hover_changed: was_over_link != over_link,
        }
    }

    /// Finishes selection and activates a link if the interaction remained a click.
    /// Returns whether a selection or a document action handled the release.
    pub fn pointer_up(&mut self, position: Point, mode: crate::SelectionMode) -> bool {
        let position = self.core.to_layout_point(position);
        let selection = self.core.interact_selection(SelectionInput::PointerUp {
            position,
            mode,
            drag_distance: f64::from(crate::WORD_SELECTION_DRAG_DISTANCE),
        });
        let click_position = self.pointer_down_position.take();
        let Some(glyph) = selection.click else {
            return selection.handled;
        };
        let hit = crate::DocumentGlyph {
            doc: self.core.nav.current_document(),
            glyph,
        };
        let before = (
            self.core.nav.current_document(),
            self.core.nav.anchor_glyph_here(),
        );
        let handled = self
            .core
            .handle_document_click(&mut self.glyph_shaper, hit, click_position);
        if before
            != (
                self.core.nav.current_document(),
                self.core.nav.anchor_glyph_here(),
            )
        {
            self.navigation_revision = self.navigation_revision.wrapping_add(1);
        }
        handled
    }

    pub fn pointer_leave(&mut self) {
        self.pointer_down_position = None;
        self.core.deactivate_link_cursor();
    }

    pub fn link_cursor_active(&self) -> bool {
        self.core.page.link_hovered
    }

    pub fn set_interaction_palette(&mut self, palette: InteractionPalette) {
        self.core.set_interaction_palette(palette);
    }

    pub fn selection_contains_point(&self, position: Point) -> bool {
        let position = self.core.to_layout_point(position);
        self.core
            .page
            .geometry()
            .selection_hit(&self.core.page.selection, position)
            .selected
    }

    pub fn table_selection_at(&self, position: Point) -> bool {
        let position = self.core.to_layout_point(position);
        self.core
            .page
            .geometry()
            .selection_hit(&self.core.page.selection, position)
            .selected_table
    }

    pub fn begin_table_selection_at(&mut self, position: Point) -> bool {
        let position = self.core.to_layout_point(position);
        self.core.begin_table_selection_at(position)
    }

    pub fn clear_table_selection(&mut self) -> bool {
        self.core.clear_table_selection()
    }

    pub fn copy_table_at(&mut self, position: Point, format: TableExportFormat) -> bool {
        let position = self.core.to_layout_point(position);
        self.core.copy_table_at(position, format)
    }

    pub fn copy_table_selection(&mut self) -> bool {
        self.core.copy_table_selection()
    }

    /// Prepares layout and returns a frame that can be painted exactly once.
    pub fn prepare_frame(&mut self, viewport_size: Size) -> PreparedFrame<'_, S> {
        self.core
            .prepare_frame(&mut self.glyph_shaper, viewport_size);
        self.prepared_frame()
    }

    /// Incorporates completed resources. This operation may update document
    /// layout and must run from application state update code.
    pub fn update_resources(&mut self) {
        let deferred_click = self
            .core
            .pending_navigation
            .as_ref()
            .is_some_and(|pending| pending.is_click());
        let before = (
            self.core.nav.current_document(),
            self.core.nav.anchor_glyph_here(),
        );
        self.core.finish_pending_navigation(&mut self.glyph_shaper);
        if deferred_click
            && before
                != (
                    self.core.nav.current_document(),
                    self.core.nav.anchor_glyph_here(),
                )
        {
            self.navigation_revision = self.navigation_revision.wrapping_add(1);
        }
        self.core.update_resources(&mut self.glyph_shaper);
    }

    /// Prepares pagination and viewport composition without invoking the HTML
    /// pipeline.
    pub fn prepare_viewport(&mut self, viewport_size: Size) -> PreparedFrame<'_, S> {
        self.core.prepare_viewport(viewport_size);
        self.prepared_frame()
    }

    fn prepared_frame(&mut self) -> PreparedFrame<'_, S> {
        PreparedFrame {
            core: &mut self.core,
            glyph_shaper: &self.glyph_shaper,
        }
    }
}

pub struct PreparedFrame<'a, S> {
    core: &'a mut RendererCore,
    glyph_shaper: &'a S,
}

impl<'a, S> PreparedFrame<'a, S> {
    pub fn glyph_shaper(&self) -> &S {
        self.glyph_shaper
    }

    pub fn paint(self, painter: &mut impl Painter) {
        self.core.paint_frame(painter);
    }

    pub fn cache_keys(&self) -> FrameDisplayCacheKeys {
        self.core.display_cache_keys()
    }

    /// Emits the frame into independently cacheable layers while preserving
    /// the renderer's established z-order.
    pub fn paint_all_layers(self, base: &mut impl Painter, overlay: &mut impl Painter) {
        self.core.paint_base(base);
        self.core.paint_interaction_overlay(overlay);
    }

    pub fn paint_base_layer(self, base: &mut impl Painter) {
        self.core.paint_base(base);
    }

    pub fn paint_overlay_layer(self, overlay: &mut impl Painter) {
        self.core.paint_interaction_overlay(overlay);
    }

    pub fn into_parts(self) -> (&'a S, FramePainter<'a>) {
        (self.glyph_shaper, FramePainter { core: self.core })
    }
}

/// The paint-only half of a prepared frame. This lets a backend construct a
/// painter that borrows the glyph shaper without exposing renderer internals.
pub struct FramePainter<'a> {
    core: &'a mut RendererCore,
}

impl FramePainter<'_> {
    pub fn paint(self, painter: &mut impl Painter) {
        self.core.paint_frame(painter);
    }

    /// Paints the page, lets a host place the open note's base scene, then
    /// paints the note selection through the same interaction renderer.
    pub fn paint_with_note_selection<P: Painter>(
        self,
        painter: &mut P,
        paint_note_base: impl FnOnce(&mut P),
    ) {
        self.core.paint_frame(painter);
        paint_note_base(painter);
        self.core.paint_note_selection(painter);
    }
}

#[cfg(test)]
mod tests {
    use html_resources::resources::images::decoded_images_get;
    use html_pipeline::engine::text_paint;

    use html_pipeline::engine::TextExportFormat;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use html::document::ImageId;
    use html_dom::ImageSource;
    use html::style::{ReaderStyleOverrides, ThemeAdaptation};
    use html_resources::DecodedImage;

    use crate::text_backend::{FontSlant, GlyphHandle, ShapeError};
    use html_pipeline::engine::GlyphResourceStore;
    use crate::FileSystemProvider;
    use crate::RendererEvent;
    use html_render_core::RecordingPainter;
    
    use peniko::{Color, Image};

    use super::*;

    #[derive(Default)]
    struct TestShaper {
        glyphs: HashMap<(char, u32), GlyphHandle>,
        generation: GlyphResourceGeneration,
        append_checkpoint: Option<(HashMap<(char, u32), GlyphHandle>, GlyphResourceGeneration)>,
        glyph_shape_calls: usize,
    }

    impl GlyphShaper for TestShaper {
        fn reset(&mut self) {
            self.glyphs.clear();
            self.generation = GlyphResourceGeneration::default();
        }

        fn resource_generation(&self) -> GlyphResourceGeneration {
            self.generation
        }

        fn begin_append_shaping(&mut self) -> Result<(), ShapeError> {
            assert!(self.append_checkpoint.is_none());
            self.append_checkpoint = Some((self.glyphs.clone(), self.generation.clone()));
            Ok(())
        }

        fn commit_append_shaping(&mut self) {
            self.append_checkpoint = None;
        }

        fn rollback_append_shaping(&mut self) {
            if let Some((glyphs, glyph_store)) = self.append_checkpoint.take() {
                self.glyphs = glyphs;
                self.generation = glyph_store;
            }
        }

        fn shape_glyph(&mut self, request: GlyphRequest<'_>) -> Result<ShapedGlyph, ShapeError> {
            self.glyph_shape_calls += 1;
            let key = (request.character, request.style.font_size.to_bits());
            let advance = request.style.font_size * 0.5;
            let metrics = FontMetrics {
                ascent: request.style.font_size * 0.75,
                descent: request.style.font_size * 0.25,
            };
            if let Some(&glyph) = self.glyphs.get(&key) {
                return Ok(ShapedGlyph::new(glyph, advance, metrics));
            }
            let glyph = GlyphHandle(self.glyphs.len() as u32);
            self.glyphs.insert(key, glyph);
            Ok(ShapedGlyph::new(glyph, advance, metrics))
        }
    }

    #[derive(Default)]
    struct EquationPainter {
        colors: Vec<u32>,
    }

    #[derive(Default)]
    struct SvgThemePainter {
        ordinary_svgs: usize,
        raster_images: usize,
    }

    impl Painter for SvgThemePainter {
        fn push_clip(&mut self, _: Rect) {}
        fn pop_clip(&mut self) {}
        fn fill_rect(&mut self, _rect: Rect, _color: Color) {}
        fn draw_glyph(&mut self, _glyph: GlyphHandle, _origin: Point, _color: u32) {}
        fn draw_image(&mut self, _image: &Image, _hash: &[u8], _rect: Rect) {
            self.raster_images += 1;
        }
        fn draw_svg(&mut self, _bytes: &[u8], _hash: &[u8], _size: (u32, u32), _rect: Rect) {
            self.ordinary_svgs += 1;
        }
    }

    impl Painter for EquationPainter {
        fn push_clip(&mut self, _: Rect) {}
        fn pop_clip(&mut self) {}
        fn fill_rect(&mut self, _rect: Rect, _color: Color) {}
        fn draw_glyph(&mut self, _glyph: GlyphHandle, _origin: Point, _color: u32) {}
        fn draw_image(&mut self, _image: &Image, _hash: &[u8], _rect: Rect) {}
        fn draw_math_svg(
            &mut self,
            _bytes: &[u8],
            _hash: &[u8],
            _size: (u32, u32),
            _rect: Rect,
            color: u32,
        ) {
            self.colors.push(color);
        }
    }

    #[test]
    fn equation_context_copy_uses_retained_mathml() {
        let file =
            std::env::temp_dir().join(format!("html-view-mathml-{}.html", std::process::id()));
        std::fs::write(&file, "<!doctype html><p>Equation <math><mfrac><mn>1</mn><mi>x</mi></mfrac></math></p><svg width='20' height='20'><rect width='20' height='20'/></svg>").unwrap();
        let uri = file.to_string_lossy().into_owned();
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            Arc::new(FileSystemProvider::new()),
            vec![uri],
            0,
            None,
            crate::RendererInitialConfig::default(),
        )
        .expect("fixture document must load");
        renderer
            .prepare_frame(Size::new(600.0, 400.0))
            .paint(&mut RecordingPainter::default());
        let mut equation = None;
        let mut ordinary_svg = None;
        for y in (0..250).step_by(2) {
            for x in (0..600).step_by(2) {
                let point = Point::new(x as f64, y as f64);
                let hit = renderer.content_at(point);
                if hit.equation {
                    equation.get_or_insert(point);
                } else if hit.image {
                    ordinary_svg.get_or_insert(point);
                }
            }
        }
        let equation = equation.expect("rendered equation hit target");
        let ordinary_svg = ordinary_svg.expect("ordinary SVG hit target");
        assert!(!renderer.copy_equation_latex_at(ordinary_svg));
        assert!(renderer.copy_equation_latex_at(equation));
        assert!(
            matches!(renderer.core.effects.clipboard.last(), Some(ClipboardWrite::Text(text)) if text == r"\frac{1}{x}")
        );
        let old_source = text_paint::render_view_images(&renderer.core.page.render_view())[0]
            .source
            .clone();
        let old_width = text_paint::render_view_images(&renderer.core.page.render_view())[0].width;
        renderer.apply(RendererCommand::SetReaderStyleOverrides(
            html::style::ReaderStyleOverrides {
                foreground: Some(0xeeeeeeff),
                ..Default::default()
            },
        ));
        renderer.update_resources();
        let mut painter = EquationPainter::default();
        renderer
            .prepare_frame(Size::new(600.0, 400.0))
            .paint(&mut painter);
        assert_eq!(
            painter.colors,
            [0xeeeeeeff],
            "equation painting must use the current reader theme"
        );
        renderer.apply(RendererCommand::SetFontSize(32.0));
        renderer
            .prepare_frame(Size::new(600.0, 400.0))
            .paint(&mut RecordingPainter::default());
        renderer.update_resources();
        assert!(
            text_paint::render_view_images(&renderer.core.page.render_view())[0].width > old_width
        );
        let ImageSource::MathMl { svg: old_svg, .. } = old_source else {
            panic!("equation source")
        };
        let ImageSource::MathMl { svg: new_svg, .. } =
            &text_paint::render_view_images(&renderer.core.page.render_view())[0].source
        else {
            panic!("equation source")
        };
        assert_ne!(&old_svg, new_svg);
        let DecodedImage::Svg { bytes, .. } = decoded_images_get(
            renderer.core.page.session.images(),
            dom_values::image_id_new(0),
        )
        .unwrap() else {
            panic!("decoded math SVG")
        };
        assert_eq!(
            bytes, new_svg,
            "font-size changes must replace the decoded SVG source"
        );
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn inline_and_external_svgs_receive_current_theme_without_replacing_decoded_sources() {
        let directory =
            std::env::temp_dir().join(format!("html-view-svg-theme-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("chapter.html");
        let source = "<svg xmlns='http://www.w3.org/2000/svg' width='20' height='20'><rect width='20' height='20' fill='black'/></svg>";
        std::fs::write(directory.join("diagram.svg"), source).unwrap();
        std::fs::write(
            &file,
            format!("<!doctype html><p>Diagram {source}<img src='diagram.svg'></p>"),
        )
        .unwrap();
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            Arc::new(FileSystemProvider::new()),
            vec![file.to_string_lossy().into_owned()],
            0,
            None,
            crate::RendererInitialConfig::default(),
        )
        .expect("fixture document must load");
        let size = Size::new(600.0, 400.0);
        renderer
            .prepare_frame(size)
            .paint(&mut RecordingPainter::default());
        renderer.update_resources();
        let mut painter = SvgThemePainter::default();
        renderer.prepare_frame(size).paint(&mut painter);
        assert_eq!(painter.ordinary_svgs, 2);
        assert_eq!(renderer.image_theme(), None);
        let sources: Vec<_> = (0..2)
            .map(dom_values::image_id_new)
            .map(|index| {
                let DecodedImage::Svg { bytes, .. } =
                    decoded_images_get(renderer.core.page.session.images(), index).unwrap()
                else {
                    panic!("decoded SVG")
                };
                bytes.clone()
            })
            .collect();
        for theme in [
            ThemeAdaptation {
                ink: 0xe8e7e4ff,
                surface: 0x2d2b28ff,
            },
            ThemeAdaptation {
                ink: 0x272624ff,
                surface: 0xfef8edff,
            },
        ] {
            renderer.apply(RendererCommand::SetReaderStyleOverrides(
                ReaderStyleOverrides {
                    theme: Some(theme),
                    ..Default::default()
                },
            ));
            let mut painter = SvgThemePainter::default();
            renderer.prepare_frame(size).paint(&mut painter);
            assert_eq!(renderer.image_theme(), Some(theme));
            assert_eq!(painter.ordinary_svgs, 2);
            for (index, original) in sources.iter().enumerate() {
                let DecodedImage::Svg { bytes, .. } = decoded_images_get(
                    renderer.core.page.session.images(),
                    dom_values::image_id_new(index as u32),
                )
                .unwrap() else {
                    panic!("decoded SVG")
                };
                assert_eq!(
                    bytes, original,
                    "theme changes only affect rasterized paint"
                );
            }
        }
        renderer.apply(RendererCommand::SetReaderStyleOverrides(Default::default()));
        assert_eq!(renderer.image_theme(), None);
        let mut painter = SvgThemePainter::default();
        renderer.prepare_frame(size).paint(&mut painter);
        assert_eq!(painter.ordinary_svgs, 2);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn touch_selection_tracks_words_handles_wrapped_lines_and_final_cfi_without_links() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let file = std::env::temp_dir().join(format!("html-view-touch-{nonce}.html"));
        std::fs::write(&file, "<html><head><meta charset='utf-8'></head><body><p><a href='#target'>alpha</a> beta gamma delta epsilon zeta eta theta</p><p>cafe\u{301} 👩‍👧</p><p id='target'>target</p></body></html>").unwrap();
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            Arc::new(FileSystemProvider::new()),
            vec![file.to_string_lossy().into_owned()],
            0,
            Some("0:0"),
            crate::RendererInitialConfig::default(),
        )
        .expect("fixture document must load");
        renderer.apply(RendererCommand::SetColumnWidth(120.0));
        renderer.apply(RendererCommand::SetMaxColumnCount(Some(1)));
        renderer
            .prepare_frame(Size::new(120.0, 500.0))
            .paint(&mut RecordingPainter::default());
        let point_at = |renderer: &RendererSession<TestShaper>, word: &str| {
            let doc = renderer.core.doc();
            let text = doc.text();
            let all: String = (0..text.glyph_count())
                .filter_map(|index| {
                    text_paint::glyph_at(text, index)
                        .and_then(|glyph| text_paint::glyph_metric(text, glyph))
                        .map(|metric| metric.ch())
                })
                .collect();
            let byte = all.find(word).unwrap();
            let glyph = all[..byte].chars().count() as u32;
            let bounds = renderer
                .core
                .page
                .geometry()
                .glyph_range_bounds(glyph, glyph + 1)
                .unwrap();
            Point::new(
                bounds.x0 + bounds.width() * 0.8,
                bounds.y0 + bounds.height() * 0.5,
            )
        };
        let location = renderer.current_cfi();
        // Ordinary mouse drags use the same word boundaries as touch handles,
        // including across inline links and wrapped lines.
        assert!(
            renderer
                .pointer_down(point_at(&renderer, "pha"), PointerDownOptions::default())
                .handled
        );
        // A release can carry the final position without an intervening move.
        assert!(renderer.pointer_up(point_at(&renderer, "psilon"), crate::SelectionMode::Plain));
        assert!(renderer.has_text_selection());
        let selected_cfi = renderer.selection_cfi().unwrap();
        assert!(renderer.core.effects.clipboard.is_empty());
        assert!(renderer.core.effects.events.iter().any(|event| matches!(event, RendererEvent::SelectionFinished { cfi_range, .. } if cfi_range == &selected_cfi)));
        assert_eq!(
            renderer.selected_passage().unwrap().1,
            "alpha beta gamma delta epsilon"
        );
        assert!(
            renderer
                .pointer_down(point_at(&renderer, "psilon"), PointerDownOptions::default())
                .handled
        );
        assert!(
            renderer
                .pointer_move(point_at(&renderer, "pha"), crate::SelectionMode::Plain)
                .handled
        );
        assert!(renderer.pointer_up(point_at(&renderer, "pha"), crate::SelectionMode::Plain));
        assert_eq!(
            renderer.selected_passage().unwrap().1,
            "alpha beta gamma delta epsilon"
        );
        assert_eq!(renderer.current_cfi(), location);
        renderer.clear_text_selection();
        assert!(!renderer.select_element_at(Point::new(-50.0, -50.0)));
        assert!(renderer.select_element_at(point_at(&renderer, "gamma")));
        let (element_cfi, element_text) = renderer.selected_passage().unwrap();
        assert_eq!(
            element_text,
            "alpha beta gamma delta epsilon zeta eta theta"
        );
        let (_, first, last) =
            crate::cfi::parse_cfi_range(&renderer.core.page, &element_cfi).unwrap();
        assert_eq!(
            export_queries::render_export_view_text(
                renderer.core.doc().export(),
                first..last,
                TextExportFormat::PlainAndMarkdown
            )
            .plain
            .unwrap(),
            element_text
        );
        assert_eq!(renderer.current_cfi(), location);
        assert!(!renderer.select_word_at(Point::new(-50.0, -50.0)));
        assert!(renderer.select_word_at(point_at(&renderer, "alpha")));
        assert_eq!(renderer.selected_passage().unwrap().1, "alpha");
        renderer.end_touch_selection();
        assert_eq!(renderer.current_cfi(), location);
        assert!(renderer.begin_selection_handle(false));
        let event_count = renderer.core.effects.events.len();
        assert!(!renderer.update_touch_selection(point_at(&renderer, "pha")));
        assert_eq!(renderer.core.effects.events.len(), event_count);
        assert!(renderer.update_touch_selection(point_at(&renderer, "epsilon")));
        renderer.end_touch_selection();
        let (cfi, text) = renderer.selected_passage().unwrap();
        assert_eq!(text, "alpha beta gamma delta epsilon");
        let (_, first, last) = crate::cfi::parse_cfi_range(&renderer.core.page, &cfi).unwrap();
        assert_eq!(
            export_queries::render_export_view_text(
                renderer.core.doc().export(),
                first..last,
                TextExportFormat::PlainAndMarkdown
            )
            .plain
            .unwrap(),
            text
        );
        let (start, end) = renderer.selection_endpoints();
        assert!(
            end.unwrap().y > start.unwrap().y,
            "wrapped selection handles must use their own lines"
        );
        assert!(renderer.begin_selection_handle(true));
        assert!(renderer.update_touch_selection(point_at(&renderer, "zeta")));
        renderer.end_touch_selection();
        assert!(renderer.selected_passage().unwrap().1.contains("zeta"));
        assert_eq!(renderer.current_cfi(), location);
        assert!(renderer.copy_selection());
        assert!(renderer.begin_selection_handle(false));
        assert!(renderer.update_touch_selection(Point::new(3000.0, 5000.0)));
        assert!(renderer.selected_passage().unwrap().1.ends_with("target"));
        assert!(
            renderer.selection_endpoints().1.is_some(),
            "the edge drag must end on visible text"
        );
        renderer.end_touch_selection();
        for word in ["cafe\u{301}", "👩‍👧"] {
            assert!(renderer.select_word_at(point_at(&renderer, word)));
            assert_eq!(renderer.selected_passage().unwrap().1, word);
            renderer.apply(RendererCommand::CopySelectionCitation {
                title: "Book".into(),
                cfi: None,
            });
            assert!(
                matches!(renderer.core.effects.clipboard.last(), Some(ClipboardWrite::Text(text)) if text == &format!("“{word}” — Book"))
            );
            let (cfi, _) = renderer.selected_passage().unwrap();
            let (_, first, last) = crate::cfi::parse_cfi_range(&renderer.core.page, &cfi).unwrap();
            assert_eq!(
                export_queries::render_export_view_text(
                    renderer.core.doc().export(),
                    first..last,
                    TextExportFormat::PlainAndMarkdown
                )
                .plain
                .as_deref(),
                Some(word)
            );
            renderer.end_touch_selection();
        }
        renderer.clear_text_selection();
        assert!(!renderer.has_text_selection());
        assert!(renderer.selection_cfi().is_none());
        assert!(renderer.selected_passage().is_none());
        assert_eq!(renderer.selection_endpoints(), (None, None));
        assert!(!renderer.update_touch_selection(point_at(&renderer, "gamma")));
        assert!(renderer.core.effects.events.iter().any(|event| matches!(event, RendererEvent::SelectionFinished { cfi_range, .. } if cfi_range == &element_cfi)));
        // Only the first lines now fit in the viewport. Element selection and
        // its saved CFI must still include the paragraph's off-screen end.
        renderer.apply(RendererCommand::SetPosition {
            doc: 0,
            glyph: Some(0),
        });
        renderer
            .prepare_frame(Size::new(120.0, 48.0))
            .paint(&mut RecordingPainter::default());
        let location = renderer.current_cfi();
        assert!(renderer.select_element_at(point_at(&renderer, "alpha")));
        let (cfi, text) = renderer.selected_passage().unwrap();
        assert_eq!(text, "alpha beta gamma delta epsilon zeta eta theta");
        let (_, first, last) = crate::cfi::parse_cfi_range(&renderer.core.page, &cfi).unwrap();
        assert_eq!(last - first, text.chars().count() as u32);
        assert_eq!(renderer.current_cfi(), location);
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn session_owns_navigation_shaping_and_pointer_choreography() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-session-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let first = fixture.join("one.html");
        let second = fixture.join("two.html");
        std::fs::write(
            &first,
            "<html><body><a href=\"two.html#target\">Next</a></body></html>",
        )
        .unwrap();
        std::fs::write(
            &second,
            "<html><body><h1 id=\"target\">Target</h1></body></html>",
        )
        .unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let uris = vec![
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ];
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            provider,
            uris,
            0,
            Some("0:0"),
            crate::RendererInitialConfig::default(),
        )
        .expect("fixture document must load");

        renderer
            .prepare_frame(Size::new(600.0, 800.0))
            .paint(&mut RecordingPainter::default());
        let root = renderer.core.page.render_view();
        let text = root.text();
        let addressing = root.addressing();
        let linked_glyph = (0..text.glyph_count() as u32)
            .find(|glyph| addressing.link_for_glyph(*glyph).is_some())
            .expect("fixture should contain a linked glyph");
        let line_idx =
            text_paint::render_text_view_line_for_glyph(renderer.core.doc().text(), linked_glyph)
                .unwrap();
        let screen = renderer
            .core
            .page
            .frame
            .page()
            .line_positions()
            .get(line_idx)
            .unwrap();
        let line = renderer
            .core
            .page
            .render_view()
            .text()
            .line(line_idx)
            .expect("linked line should be present");
        let position = Point::new(
            screen.point.x
                + geometry_queries::render_view_geometry((renderer.core.page.geometry()).doc)
                    .caret_x((&line).id(), linked_glyph, CaretAffinity::Leading)
                    .unwrap_or(0.0)
                + 0.1,
            screen.point.y + line.height() / 2.0,
        );

        let hover = renderer.pointer_move(position, crate::SelectionMode::Plain);
        assert!(hover.over_link);
        assert!(hover.link_hover_changed);
        assert!(
            renderer
                .pointer_down(position, PointerDownOptions::default())
                .handled
        );
        let click_selection = renderer.core.page.selection.clone();
        renderer.core.interact_selection(SelectionInput::SetRange {
            range: linked_glyph..linked_glyph + 1,
            mode: crate::SelectionMode::Plain,
        });
        assert!(renderer.selection_contains_point(position));
        assert!(
            !renderer
                .selection_contains_point(Point::new(position.x, position.y + line.height() * 2.0))
        );
        renderer.core.interact_selection(SelectionInput::SetRange {
            range: linked_glyph..linked_glyph + 4,
            mode: crate::SelectionMode::Plain,
        });
        assert!(renderer.copy_selection());
        renderer.core.page.selection = click_selection;
        assert!(renderer.pointer_up(position, crate::SelectionMode::Plain));
        assert_eq!(renderer.core.nav.current_document(), 1);

        let font_size = renderer.render_view().root_font_size();
        let column_width = renderer.column_width();
        let scale = renderer.scale();
        renderer.apply(RendererCommand::SetFontSize(font_size + 2.0));
        renderer.apply(RendererCommand::SetColumnWidth(
            renderer.column_width() - 20.0,
        ));
        renderer.apply(RendererCommand::SetScale(scale + 0.1));
        assert_eq!(renderer.render_view().root_font_size(), font_size + 2.0);
        assert_eq!(renderer.column_width(), column_width - 20.0);
        assert_eq!(renderer.scale(), scale + 0.1);

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn initial_cross_spine_target_is_selected_during_worker_preparation() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-target-preparation-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let first = fixture.join("one.html");
        let second = fixture.join("two.html");
        std::fs::write(&first, "<html><body>First</body></html>").unwrap();
        std::fs::write(
            &second,
            "<html><body><h1 id=\"target\">Prepared target</h1></body></html>",
        )
        .unwrap();
        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let uris = vec![
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ];
        let preparation = crate::RendererPreparation::from_provider_with_nav_and_target(
            provider,
            uris,
            0,
            Some("0:12"),
            Some("two.html#target"),
            crate::RendererInitialConfig::default(),
        )
        .unwrap();
        assert_eq!(
            preparation.doc_index, 1,
            "the target document must be chosen before UI-thread shaping"
        );
        assert_eq!(preparation.initial_anchor.as_deref(), Some("target"));
        let renderer = RendererSession::from_preparation(
            RendererServices::default(),
            TestShaper::default(),
            preparation,
        )
        .unwrap();
        assert_eq!(renderer.core.nav.current_document(), 1);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn viewport_resize_does_not_relayout_percentage_height_document() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-percentage-height-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.xht");
        std::fs::write(
            &document,
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><style><![CDATA[
                html { background-color: red; height: 100%; }
                body, p { height: 100%; margin: 0px; }
                p { background-color: green; color: white; }
            ]]></style></head><body><p>Test passes if the background of this page is green and if there is <strong>no red and no vertical scrollbar</strong>.</p></body></html>"#,
        )
        .unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            provider,
            vec![document.to_string_lossy().into_owned()],
            0,
            Some("0:0"),
            crate::RendererInitialConfig::default(),
        ).expect("fixture document must load");
        renderer.apply(RendererCommand::SetFontSize(16.0));
        renderer.apply(RendererCommand::SetColumnWidth(800.0));
        renderer.apply(RendererCommand::SetMaxColumnCount(Some(1)));
        renderer.apply(RendererCommand::SetImageSizingPolicy(
            html::layout::ImageSizingPolicy::WebCompatible,
        ));
        renderer.apply(RendererCommand::SetTextCompositionPolicy(
            html::layout::TextCompositionPolicy::WebCompatible,
        ));
        let layout_revision = renderer.revisions().layout;
        renderer.prepare_frame(Size::new(800.0, 600.0));
        assert_eq!(
            renderer.revisions().layout,
            layout_revision,
            "viewport preparation must not relayout height-dependent HTML"
        );
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn positioned_background_paints_after_an_earlier_inline_image() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-positioned-cover-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.xht");
        std::fs::write(
            &document,
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><style><![CDATA[
                div.a { width:300px; height:100px; background:green }
                div.b { width:300px; position:relative } div.b p { width:200% }
                div.b img { width:50%; height:100px }
                div.b span { position:absolute; top:0; left:0; background:green; width:300px; height:100px }
            ]]></style></head><body><p>preceding flow</p><div class="a"></div><div class="b"><p><img/><span></span></p></div></body></html>"#,
        )
        .unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            provider,
            vec![document.to_string_lossy().into_owned()],
            0,
            Some("0:0"),
            crate::RendererInitialConfig::default(),
        ).expect("fixture document must load");
        renderer.apply(RendererCommand::SetColumnWidth(800.0));
        renderer.apply(RendererCommand::SetMaxColumnCount(Some(1)));
        renderer.apply(RendererCommand::SetImageSizingPolicy(
            html::layout::ImageSizingPolicy::WebCompatible,
        ));
        renderer.apply(RendererCommand::SetTextCompositionPolicy(
            html::layout::TextCompositionPolicy::WebCompatible,
        ));
        let mut painter = RecordingPainter::default();
        renderer
            .prepare_frame(Size::new(800.0, 600.0))
            .paint(&mut painter);

        assert!(
            painter.fills.iter().any(|(rect, color)| {
                *color == Color::rgba8(0, 128, 0, 255)
                    && (rect.width() - 300.0).abs() < 0.01
                    && (rect.height() - 100.0).abs() < 0.01
                    && rect.y0 > 100.0
            }),
            "the positioned cover must retain the vertical translation of its later containing block; fills={:?}",
            painter.fills
        );
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn identical_prepared_frame_reuses_authoritative_shaped_lines() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-line-cache-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        std::fs::write(
            &document,
            "<html><body><p>Kerning AV and office ligatures.</p><p>Second line.</p></body></html>",
        )
        .unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            provider,
            vec![document.to_string_lossy().into_owned()],
            0,
            Some("0:0"),
            crate::RendererInitialConfig::default(),
        ).expect("fixture document must load");

        renderer
            .prepare_frame(Size::new(600.0, 800.0))
            .paint(&mut RecordingPainter::default());
        let first_calls = renderer.glyph_shaper().glyph_shape_calls;
        assert!(first_calls > 0, "the engine must have shaped the document");
        renderer
            .prepare_frame(Size::new(600.0, 800.0))
            .paint(&mut RecordingPainter::default());
        assert_eq!(
            renderer.glyph_shaper().glyph_shape_calls,
            first_calls,
            "an unchanged cached frame must not reshape its lines"
        );
        renderer
            .prepare_frame(Size::new(620.0, 800.0))
            .paint(&mut RecordingPainter::default());
        assert_eq!(
            renderer.glyph_shaper().glyph_shape_calls,
            first_calls,
            "a position-only width resize must reuse shaped visible lines"
        );
        renderer.apply(RendererCommand::SetColumnWidth(renderer.column_width() - 20.0));
        renderer
            .prepare_frame(Size::new(620.0, 800.0))
            .paint(&mut RecordingPainter::default());
        assert_eq!(
            renderer.glyph_shaper().glyph_shape_calls,
            first_calls,
            "a layout-changing resize must not invoke a second shaping owner"
        );

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn pagination_prepares_only_the_requested_page() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-prepared-page-runs-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        let paragraphs = (0..40)
            .map(|index| format!("<p style='margin:0;height:24px'>Page line {index}</p>"))
            .collect::<String>();
        std::fs::write(
            &document,
            format!("<html><body style='margin:0'>{paragraphs}</body></html>"),
        )
        .unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            provider,
            vec![document.to_string_lossy().into_owned()],
            0,
            Some("0:0"),
            crate::RendererInitialConfig::default(),
        ).expect("fixture document must load");
        let viewport = Size::new(320.0, 96.0);

        renderer.prepare_frame(viewport);
        assert!(
            !renderer.core.page.frame.page().content().reached_end,
            "the fixture must span multiple pages"
        );
        let first_page_start = renderer.core.page.frame.page().start_line();

        let glyph_shape_calls = renderer.glyph_shaper.glyph_shape_calls;
        assert!(
            glyph_shape_calls > 0,
            "the engine must have shaped the document"
        );
        let advances = || {
            let text = renderer.render_view().text();
            (0..text.glyph_count() as u32)
                .map(|glyph| text_paint::character_advance(text, glyph))
                .collect::<Vec<_>>()
        };
        let shaped_advances = advances();
        assert!(
            shaped_advances
                .iter()
                .flatten()
                .any(|advance| *advance > 0.0)
        );

        assert_eq!(
            renderer.core.page.frame.page().start_line(),
            first_page_start
        );

        renderer.apply(RendererCommand::NavigatePage(1));
        renderer.prepare_frame(viewport);
        assert_ne!(
            renderer.core.page.frame.page().start_line(),
            first_page_start
        );

        assert_eq!(
            renderer.glyph_shaper.glyph_shape_calls, glyph_shape_calls,
            "pagination must reuse the document's glyph resources"
        );
        let text = renderer.render_view().text();
        let after = (0..text.glyph_count() as u32)
            .map(|glyph| text_paint::character_advance(text, glyph))
            .collect::<Vec<_>>();
        assert_eq!(
            after, shaped_advances,
            "pagination must preserve engine shaping geometry"
        );

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn viewport_resize_never_relayouts_html() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-height-resize-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let ordinary_path = fixture.join("ordinary.html");
        let dependent_path = fixture.join("dependent.html");
        std::fs::write(
            &ordinary_path,
            "<html><body><p>ordinary book text</p></body></html>",
        )
        .unwrap();
        std::fs::write(&dependent_path, "<html><head><style>html,body,main{height:100%;margin:0}</style></head><body><main>height-sensitive</main></body></html>").unwrap();

        let make_renderer = |path: &std::path::Path| {
            let mut config = crate::RendererInitialConfig::default();
            config.column_width = 300.0;
            RendererSession::from_provider_with_nav(
                RendererServices::default(),
                TestShaper::default(),
                Arc::new(FileSystemProvider::new()),
                vec![path.to_string_lossy().into_owned()],
                0,
                Some("0:0"),
                config,
            )
            .expect("fixture document must load")
        };

        let mut ordinary = make_renderer(&ordinary_path);
        ordinary.prepare_frame(Size::new(320.0, 400.0));
        let ordinary_revision = ordinary.revisions().layout;
        ordinary.prepare_frame(Size::new(320.0, 500.0));
        assert_eq!(
            ordinary.revisions().layout,
            ordinary_revision,
            "height-independent prose must only be repaginated"
        );
        let page_range = (
            ordinary.core.page.frame.page().start_line(),
            ordinary.core.page.frame.page().end_line(),
        );
        let first_line = page_range.0.expect("ordinary fixture has a visible line");
        let old_x = page_placements::at_cursor(
            &ordinary.core.page.frame.page().line_positions(),
            first_line,
        )
        .expect("first line is positioned")
        .point
        .x;
        let old_geometry = ordinary.core.page.frame.geometry_key();
        ordinary.prepare_frame(Size::new(340.0, 500.0));
        let new_x = page_placements::at_cursor(
            &ordinary.core.page.frame.page().line_positions(),
            first_line,
        )
        .expect("first line remains positioned")
        .point
        .x;
        let new_geometry = ordinary.core.page.frame.geometry_key();
        assert_eq!(
            (
                ordinary.core.page.frame.page().start_line(),
                ordinary.core.page.frame.page().end_line()
            ),
            page_range,
            "width-only geometry must retain page membership"
        );
        assert!(
            (new_x - old_x - 10.0).abs() < 0.01,
            "the installed page should be reprojected into the new 10px column gap; old_x={old_x}, new_x={new_x}, old_geometry={old_geometry:?}, new_geometry={new_geometry:?}"
        );

        let mut dependent = make_renderer(&dependent_path);
        dependent.prepare_frame(Size::new(320.0, 400.0));
        let dependent_revision = dependent.revisions().layout;
        dependent.prepare_frame(Size::new(320.0, 500.0));
        assert_eq!(
            dependent.revisions().layout,
            dependent_revision,
            "height-dependent HTML must retain its existing layout during resize"
        );
        dependent.prepare_frame(Size::new(320.0, 500.0));
        assert_eq!(
            dependent.revisions().layout,
            dependent_revision,
            "settling at the new viewport must not trigger delayed relayout"
        );
        dependent.prepare_frame(Size::new(180.0, 500.0));
        assert_eq!(
            dependent.revisions().layout,
            dependent_revision,
            "a resize narrower than the laid-out column must not trigger HTML relayout"
        );

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn selection_boundary_affinity_preserves_the_gap_around_an_inline_image() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture =
            std::env::temp_dir().join(format!("html-view-inline-image-selection-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        std::fs::write(&document, "<html><body style='margin:0'><p style='margin:0;white-space:nowrap'>a<img src='missing.png' style='width:20px;height:10px'>b</p></body></html>").unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            provider,
            vec![document.to_string_lossy().into_owned()],
            0,
            Some("0:0"),
            crate::RendererInitialConfig::default(),
        )
        .expect("fixture document must load");
        renderer.prepare_frame(Size::new(600.0, 800.0));

        let view = renderer.core.page.render_view().text();
        let (line_idx, before_range, after_range) = text_paint::render_text_view_spatial_order(view).iter().copied()
            .find_map(|line_idx| {
                let fragments = text_paint::render_text_view_line_text_fragments(view, line_idx)?.collect::<Vec<_>>();
                (fragments.len() == 2 && fragments[0].glyphs().end == fragments[1].glyphs().start).then(|| (line_idx, fragments[0].glyphs(), fragments[1].glyphs()))
            })
            .expect("text on both sides of an inline image should remain independently positioned on one line");
        let line = view.line(line_idx).unwrap();
        let boundary = before_range.end;
        let before_image =
            geometry_queries::render_view_geometry((renderer.core.page.geometry()).doc)
                .caret_x((&line).id(), boundary, CaretAffinity::Trailing)
                .unwrap_or(0.0);
        let after_image =
            geometry_queries::render_view_geometry((renderer.core.page.geometry()).doc)
                .caret_x((&line).id(), boundary, CaretAffinity::Leading)
                .unwrap_or(0.0);
        assert!(
            after_image > before_image,
            "leading and trailing affinity must preserve the replaced-content gap"
        );
        assert_eq!(
            geometry_queries::text_intersections(
                geometry_queries::render_view_geometry(renderer.render_view()),
                line_idx,
                before_range.start..after_range.end
            )
            .map(|range| (range.start, range.end))
            .collect::<Vec<_>>(),
            vec![
                (before_range.start, before_range.end),
                (after_range.start, after_range.end)
            ],
            "interaction overlays must preserve the replaced-content gap instead of merging both text fragments",
        );

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn nested_atomic_selection_intersects_only_its_owning_line() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!("html-view-atomic-selection-{nonce}"));
        std::fs::create_dir_all(&fixture).unwrap();
        let document = fixture.join("document.html");
        std::fs::write(&document, "<html><body style='margin:0'><p style='margin:0;white-space:nowrap'>A<span style='display:inline-flex'>INNER</span>B</p></body></html>").unwrap();

        let provider: Arc<dyn ResourceProvider> = Arc::new(FileSystemProvider::new());
        let mut renderer = RendererSession::from_provider_with_nav(
            RendererServices::default(),
            TestShaper::default(),
            provider,
            vec![document.to_string_lossy().into_owned()],
            0,
            Some("0:0"),
            crate::RendererInitialConfig::default(),
        )
        .expect("fixture document must load");
        renderer.prepare_frame(Size::new(600.0, 800.0));

        let view = renderer.core.page.render_view().text();
        let glyph_for = |target| {
            (0..view.glyph_count() as u32)
                .find(|index| {
                    text_paint::glyph_at(view, *index as usize)
                        .and_then(|glyph| text_paint::glyph_metric(view, glyph))
                        .is_some_and(|metric| metric.ch() == target)
                })
                .expect("fixture character must exist")
        };
        let outer_line =
            text_paint::render_text_view_line_for_glyph(renderer.core.doc().text(), glyph_for('A'))
                .unwrap();
        let nested = glyph_for('I');
        let nested_line =
            text_paint::render_text_view_line_for_glyph(renderer.core.doc().text(), nested)
                .unwrap();

        assert_ne!(outer_line, nested_line);
        assert_eq!(
            geometry_queries::text_intersections(
                geometry_queries::render_view_geometry(renderer.render_view()),
                outer_line,
                nested..nested + 1
            )
            .map(|range| (range.start, range.end))
            .next(),
            None
        );
        assert_eq!(
            geometry_queries::text_intersections(
                geometry_queries::render_view_geometry(renderer.render_view()),
                nested_line,
                nested..nested + 1
            )
            .map(|range| (range.start, range.end))
            .next(),
            Some((nested, nested + 1))
        );
        assert_eq!(
            text_paint::render_text_view_line_for_glyph(renderer.core.doc().text(), glyph_for('B')),
            Some(outer_line)
        );
        assert_eq!(
            renderer.core.visible_text(),
            "AINNERB",
            "speakable text must merge nested atomic lines back into source reading order"
        );

        std::fs::remove_dir_all(fixture).unwrap();
    }
}
