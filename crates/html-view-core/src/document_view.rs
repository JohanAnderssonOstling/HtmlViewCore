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

use crate::spine_item::SessionDocument;
use html::engine::Engine;
use html::layout::NoteLayout;
use html_view_doc::{DocQuery, TextGeometry};
use html_view_select::{SelectionMode, SelectionState, SelectionView, TableSelectionState};
use kurbo::Point;

use crate::{ColumnLayout, VisibleFrame};

/// Display state is independent of whether a session owns the document or a
/// scoped note retains geometry. Queries borrow through the preparation owner.
pub(crate) struct DocumentView<D = NoteLayout> {
    pub(crate) document: D,
    pub(crate) layout: ColumnLayout,
    pub(crate) frame: VisibleFrame,
    pub(crate) selection: SelectionState,
    pub(crate) table_selection: TableSelectionState,
}

impl<D> DocumentView<D> {
    pub(crate) fn new(document: D, layout: ColumnLayout) -> Self {
        Self {
            document,
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
}

impl DocumentView<SessionDocument> {
    pub(crate) fn doc(&self) -> DocQuery<'_> {
        DocQuery::from_view(self.document.render_view())
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

    pub(crate) fn update_selection_text(&mut self) {
        let (plain, markdown) = selection_content(
            self.selection_view(),
            self.selection.interaction().is_semantic(),
        );
        self.selection.install_text(plain, markdown);
    }
}

impl DocumentView<NoteLayout> {
    pub(crate) fn doc_for<'a>(&'a self, owner: &'a Engine) -> DocQuery<'a> {
        DocQuery::from_view(
            self.document
                .render_view(
                    owner.document().expect("note owner has a document"),
                    owner.glyph_resources(),
                )
                .expect("validated note owner"),
        )
    }

    pub(crate) fn geometry_for<'a>(&'a self, owner: &'a Engine) -> TextGeometry<'a> {
        TextGeometry::new(self.doc_for(owner), &self.frame, &self.layout)
    }

    pub(crate) fn selection_view_for<'a>(&'a self, owner: &'a Engine) -> SelectionView<'a> {
        SelectionView::new(
            self.geometry_for(owner),
            &self.selection,
            &self.table_selection,
        )
    }

    pub(crate) fn hit_test_glyph_for(&self, position: Point, owner: &Engine) -> Option<u32> {
        self.geometry_for(owner).hit_test_glyph(position)
    }

    /// Frames a document shown whole, on its own, rather than a page of one:
    /// every line visible, one column, no scroll offset. A note in a popup is
    /// displayed this way, and needs a frame for the same reasons the page
    /// does -- hit testing and selection are geometry, not painting.
    pub(crate) fn framed_whole_for(mut self, owner: &Engine) -> Self {
        let mut positions = crate::VisibleLinePositions::with_buffer(0, Vec::new());
        let line_count = {
            let text = self.doc_for(owner).text();
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
    pub(crate) fn begin_selection_at_for(
        &mut self,
        position: Point,
        mode: SelectionMode,
        owner: &Engine,
    ) -> bool {
        let Some(glyph_idx) = self.hit_test_glyph_for(position, owner) else {
            return false;
        };
        self.selection.begin_at_glyph(glyph_idx, mode);
        true
    }

    pub(crate) fn update_selection_at_for(
        &mut self,
        position: Point,
        mode: SelectionMode,
        owner: &Engine,
    ) -> bool {
        let glyph_idx = self.hit_test_glyph_for(position, owner);
        self.selection.update_interaction(glyph_idx, mode)
    }

    pub(crate) fn selection_text_for(&self, owner: &Engine) -> Option<String> {
        let (start, end) = self.selection_view_for(owner).range()?;
        self.selection_view_for(owner)
            .build_semantic_selection_text(start, end)
            .0
    }

    pub(crate) fn update_selection_text_for(&mut self, owner: &Engine) {
        let (plain, markdown) = selection_content(
            self.selection_view_for(owner),
            self.selection.interaction().is_semantic(),
        );
        self.selection.install_text(plain, markdown);
    }
}

fn selection_content(view: SelectionView<'_>, semantic: bool) -> (Option<String>, Option<String>) {
    let Some((start, end)) = view.range() else {
        return (None, None);
    };
    let (plain, ordinary) = view.geo.doc.view().export().selection_text(start..end);
    let markdown = if semantic {
        view.build_semantic_selection_text(start, end).1
    } else {
        ordinary
    };
    (plain, markdown)
}
