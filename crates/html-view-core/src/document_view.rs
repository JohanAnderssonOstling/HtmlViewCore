//! One displayed document and the state that belongs to it.
//!
//! The reader shows more than one document at a time: the page being read, and
//! a note held out of the flow and shown on its own. They are the same kind of
//! thing -- a laid-out document with a geometry, a visible frame and a
//! selection -- so they are the same type here rather than one being the
//! renderer's fields and the other a special case beside them.
//!
//! What is genuinely particular to a note lives elsewhere: whether notes
//! generate boxes at all (`NoteFlow`), how a subtree is laid out
//! (`scoped_to_element_id`), and how the reader presents one (`NoteDisplay`).
//! Displaying it is not particular, and is not reimplemented here.

use html_view_doc::{DocQuery, TextGeometry};
use html_view_select::{SelectionMode, SelectionState, SelectionView, TableSelectionState};
use kurbo::Point;

use crate::{ColumnLayout, LaidOutDocument, VisibleFrame};

pub(crate) struct DocumentView {
    pub(crate) document: LaidOutDocument,
    pub(crate) layout: ColumnLayout,
    pub(crate) frame: VisibleFrame,
    pub(crate) selection: SelectionState,
    pub(crate) table_selection: TableSelectionState,
}

impl DocumentView {
    pub(crate) fn new(document: LaidOutDocument, layout: ColumnLayout) -> Self {
        Self {
            document,
            layout,
            frame: VisibleFrame::default(),
            selection: SelectionState::default(),
            table_selection: TableSelectionState::default(),
        }
    }

    pub(crate) fn doc(&self) -> DocQuery<'_> {
        DocQuery::new(&self.document)
    }

    pub(crate) fn geometry(&self) -> TextGeometry<'_> {
        TextGeometry::new(self.doc(), &self.frame, &self.layout)
    }

    pub(crate) fn selection_view(&self) -> SelectionView<'_> {
        SelectionView::new(self.geometry(), &self.selection, &self.table_selection)
    }

    /// Clears every screen- and interaction-derived product while retaining
    /// the document and reader-owned column layout.
    pub(crate) fn reset_view_state(&mut self) {
        self.frame.clear();
        self.selection = SelectionState::default();
        self.table_selection = TableSelectionState::default();
    }

    pub(crate) fn hit_test_glyph(&self, position: Point) -> Option<u32> {
        self.geometry().hit_test_glyph(position)
    }

    /// Frames a document shown whole, on its own, rather than a page of one:
    /// every line visible, one column, no scroll offset. A note in a popup is
    /// displayed this way, and needs a frame for the same reasons the page
    /// does -- hit testing and selection are geometry, not painting.
    pub(crate) fn framed_whole(mut self) -> Self {
        let mut positions = crate::VisibleLinePositions::with_buffer(0, Vec::new());
        let line_count = {
            let text = self.doc().text();
            for line_idx in 0..text.line_count() {
                let Some(line) = text.line(line_idx) else {
                    continue;
                };
                positions.push(crate::LineScreen {
                    point: line.point(),
                    col_index: 0,
                });
            }
            text.line_count()
        };
        self.layout = self.layout.for_column_count(1);
        // Selection and hit testing read line positions from the frame, so a
        // document shown whole records every line at its own layout point.
        self.frame.install_whole_document(line_count, positions);
        self
    }

    /// Starts a selection at `position`. The mechanics are the same wherever a
    /// document is shown; what the page does *around* them -- emitting a CFI,
    /// repainting an overlay -- stays with the page.
    pub(crate) fn begin_selection_at(&mut self, position: Point, mode: SelectionMode) -> bool {
        let Some(glyph_idx) = self.hit_test_glyph(position) else {
            return false;
        };
        self.selection.begin_at_glyph(glyph_idx, mode);
        true
    }

    pub(crate) fn update_selection_at(&mut self, position: Point, mode: SelectionMode) -> bool {
        let glyph_idx = self.hit_test_glyph(position);
        self.selection.update_interaction(glyph_idx, mode)
    }

    /// Ends a selection and reports the glyph that still counts as a click.
    pub(crate) fn finish_selection(&mut self) -> Option<u32> {
        self.selection.finish_interaction()
    }

    pub(crate) fn selection_text(&self) -> Option<String> {
        let (start, end) = self.selection_view().range()?;
        self.selection_view()
            .build_semantic_selection_text(start, end)
            .0
    }

    pub(crate) fn update_selection_text(&mut self) {
        let Some((start, end)) = self.selection_view().range() else {
            self.selection.clear_text();
            return;
        };
        let (plain, ordinary) = self.doc().build_selection_text(start, end);
        let markdown = if self.selection.interaction().is_semantic() {
            self.selection_view()
                .build_semantic_selection_text(start, end)
                .1
        } else {
            ordinary
        };
        self.selection.install_text(plain, markdown);
    }
}
