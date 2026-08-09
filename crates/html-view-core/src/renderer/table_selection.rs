use html_view_select::SelectionView;
use kurbo::Point;

use crate::RendererCore;

impl RendererCore {
    pub(crate) fn selection_view(&self) -> SelectionView<'_> {
        SelectionView::new(self.text_geometry(), &self.selection, &self.table_selection)
    }

    pub fn begin_table_selection_at(&mut self, position: Point) -> bool {
        let Some(hit) = self.selection_view().table_hit(position) else { return false };
        self.table_selection.begin(hit);
        self.request_overlay_repaint();
        true
    }

    pub(crate) fn begin_table_selection_drag(&mut self, position: Point) -> bool {
        let Some(hit) = self.selection_view().table_hit(position) else { return false };
        if !self.table_selection.begin_drag(hit) {
            return false;
        }
        self.request_overlay_repaint();
        true
    }

    pub(crate) fn update_table_selection_drag(&mut self, position: Point) -> bool {
        let hit = self.selection_view().table_hit(position);
        let Some(changed) = self.table_selection.update_drag(hit) else {
            return false;
        };
        if changed {
            self.request_overlay_repaint();
        }
        true
    }

    pub(crate) fn finish_table_selection_drag(&mut self) -> bool {
        self.table_selection.finish_drag()
    }

    pub fn clear_table_selection(&mut self) -> bool {
        if !self.table_selection.clear() {
            return false;
        }
        self.request_overlay_repaint();
        true
    }
    pub fn copy_table_at(&self, position: Point) -> Result<bool, String> {
        self.copy_to_clipboard(self.selection_view().copy_table_at(position))
    }

    pub fn copy_table_unstyled_html_at(&self, position: Point) -> Result<bool, String> {
        self.copy_to_clipboard(self.selection_view().copy_table_unstyled_html_at(position))
    }

    pub fn copy_table_styled_html_at(&self, position: Point) -> Result<bool, String> {
        self.copy_to_clipboard(self.selection_view().copy_table_styled_html_at(position))
    }

    pub fn copy_table_selection(&self) -> Result<bool, String> {
        self.copy_to_clipboard(self.selection_view().copy_table_selection())
    }

    fn copy_to_clipboard(&self, payload: Option<String>) -> Result<bool, String> {
        let Some(text) = payload else { return Ok(false) };
        self.host.set_clipboard(&text)?;
        Ok(true)
    }

    pub(crate) fn table_selection_active(&self) -> bool {
        self.table_selection.is_active()
    }

    pub fn table_at(&self, position: Point) -> bool {
        self.selection_view().table_at(position)
    }

    pub fn table_selection_at(&self, position: Point) -> bool {
        self.selection_view().table_selection_at(position)
    }

    pub fn selection_range(&self) -> Option<(u32, u32)> {
        self.selection_view().range()
    }

    pub fn selection_contains_point(&self, position: Point) -> bool {
        self.selection_view().contains_point(position)
    }

    pub fn update_selection_text(&mut self) {
        let (plain, markdown) = {
            let view = self.selection_view();
            let Some((start, end)) = view.range() else {
                self.selection.selected_text = None;
                self.selection.selected_text_markdown = None;
                return;
            };
            let (plain, ordinary) = view.geo.doc.build_selection_text(start, end);
            let markdown = if self.selection.semantic_selection { view.build_semantic_selection_text(start, end).1 } else { ordinary };
            (plain, markdown)
        };
        self.selection.selected_text = plain;
        self.selection.selected_text_markdown = markdown;
    }
}
