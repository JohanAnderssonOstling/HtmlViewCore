//! A loaded spine document and the reader state for displaying it.

use crate::{ColumnLayout, VisibleFrame};
use html::Document;
use html::selection::{SelectionInput, SelectionOutcome};
use html::layout::RenderView;
use html::configuration::DocumentConfiguration;
use html::resources::ImageService;
use html::pagination::PageView;
use html::selection::SelectionController;

pub(crate) struct SpineItem {
    pub(crate) session: Document,
    pub(crate) layout: ColumnLayout,
    pub(crate) frame: VisibleFrame,
    pub(crate) selection: SelectionController,
    pub(crate) link_hovered: bool,
}

impl SpineItem {
    pub(crate) fn new(
        mut session: Document,
        layout: ColumnLayout,
        image_service: ImageService,
    ) -> Self {
        session.set_image_service(image_service);
        Self {
            session,
            layout,
            frame: VisibleFrame::default(),
            selection: SelectionController::default(),
            link_hovered: false,
        }
    }

    pub(crate) fn reset_view_state(&mut self) {
        self.frame.clear();
        self.selection = SelectionController::default();
        self.link_hovered = false;
    }

    pub(crate) fn geometry(&self) -> PageView<'_> {
        PageView::new(self.render_view(), self.frame.page().content())
    }


    pub(crate) fn interact_selection(&mut self, input: SelectionInput) -> SelectionOutcome {
        let geo = PageView::new(
            self.session.render_view().expect("loaded document"),
            self.frame.page().content(),
        );
        geo.interact(&mut self.selection, input)
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        self.selection.selection(self.render_view())?.plain_text()
    }

    pub(crate) fn render_view(&self) -> RenderView<'_> {
        self.session
            .render_view()
            .expect("spine session owns matching glyph resources")
    }

    pub(crate) fn configuration(&self) -> DocumentConfiguration<'_> {
        self.session.configuration()
    }

    pub(crate) fn root_font_size(&self) -> f32 {
        self.configuration().font_size().get()
    }

}

impl<'a> From<&'a SpineItem> for RenderView<'a> {
    fn from(item: &'a SpineItem) -> Self {
        item.render_view()
    }
}
