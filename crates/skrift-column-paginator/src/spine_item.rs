//! A loaded spine document and the reader state for displaying it.

use crate::{ColumnLayout, LaidOutDocument, VisibleFrame};
use html::engine::Engine;
use html::geometry::WordHitPolicy;
use html::layout::RenderView;
use html::pipeline::PipelineInputs;
use html::resources::ImageService;
use html_view_doc::{DocQuery, TextGeometry};
use html_view_select::{SelectionMode, SelectionState, SelectionView, TableSelectionState};
use kurbo::Point;
use std::ops::Deref;

pub(crate) struct SpineItem {
    pub(crate) session: Engine,
    pub(crate) layout: ColumnLayout,
    pub(crate) frame: VisibleFrame,
    pub(crate) selection: SelectionState,
    pub(crate) table_selection: TableSelectionState,
}

impl SpineItem {
    pub(crate) fn new(
        mut session: Engine,
        layout: ColumnLayout,
        image_service: ImageService,
    ) -> Self {
        session.set_image_service(image_service);
        Self {
            session,
            layout,
            frame: VisibleFrame::default(),
            selection: SelectionState::default(),
            table_selection: TableSelectionState::default(),
        }
    }

    pub(crate) fn reset_view_state(&mut self) {
        self.frame.clear();
        self.selection = SelectionState::default();
        self.table_selection = TableSelectionState::default();
    }

    pub(crate) fn finish_selection(&mut self) -> Option<u32> {
        self.selection.finish_interaction()
    }

    pub(crate) fn doc(&self) -> DocQuery<'_> {
        DocQuery::from_view(self.render_view())
    }

    pub(crate) fn geometry(&self) -> TextGeometry<'_> {
        TextGeometry::new(self.doc(), &self.frame, &self.layout)
    }

    pub(crate) fn selection_view(&self) -> SelectionView<'_> {
        SelectionView::new(self.geometry(), &self.selection, &self.table_selection)
    }

    /// Starts a selection at `position`. The mechanics are the same wherever a
    /// document is shown; what the page does *around* them -- emitting a CFI,
    /// repainting an overlay -- stays with the page.
    pub(crate) fn begin_selection_at(&mut self, position: Point, mode: SelectionMode) -> bool {
        let Some(hit) = self.geometry().word_hit_at(position, WordHitPolicy::Nearest) else {
            return false;
        };
        self.selection.begin_at_word(hit, mode);
        true
    }

    pub(crate) fn update_selection_at(&mut self, position: Point, mode: SelectionMode) -> bool {
        if !self.selection.interaction().is_selecting() {
            return false;
        }
        let hit = self.geometry().word_hit_at(position, WordHitPolicy::Nearest);
        self.selection.update_word_interaction(hit, mode)
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        self.selection.selection(self.render_view())?.plain_text()
    }

    pub(crate) fn render_view(&self) -> RenderView<'_> {
        self.session
            .render_view()
            .expect("spine session owns matching glyph resources")
    }

    pub(crate) fn inputs(&self) -> &PipelineInputs {
        self.session
            .inputs()
            .expect("spine session owns its inputs")
    }

    pub(crate) fn root_font_size(&self) -> f32 {
        self.inputs().style_environment.root_font_size as f32
    }
}

impl Deref for SpineItem {
    type Target = LaidOutDocument<'static>;
    fn deref(&self) -> &Self::Target {
        self.session
            .document()
            .expect("spine session owns a document")
    }
}

impl<'a> From<&'a SpineItem> for RenderView<'a> {
    fn from(item: &'a SpineItem) -> Self {
        item.render_view()
    }
}
