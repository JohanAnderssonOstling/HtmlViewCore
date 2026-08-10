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
use html_view_select::{SelectionState, SelectionView, TableSelectionState};
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
        Self { document, layout, frame: VisibleFrame::default(), selection: SelectionState::default(), table_selection: TableSelectionState::default() }
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

    pub(crate) fn hit_test_glyph(&self, position: Point) -> Option<u32> {
        self.geometry().hit_test_glyph(position)
    }

    /// Frames a document shown whole, on its own, rather than a page of one:
    /// every line visible, one column, no scroll offset. A note in a popup is
    /// displayed this way, and needs a frame for the same reasons the page
    /// does -- hit testing and selection are geometry, not painting.
    pub(crate) fn framed_whole(mut self) -> Self {
        let line_count = self.doc().text().line_count();
        self.layout.col_count = 1.0;
        self.frame.current_page_start_line = Some(0);
        self.frame.current_page_end_line = Some(line_count);
        self.frame.paint_start_offset_y = 0.0;
        self
    }
}
