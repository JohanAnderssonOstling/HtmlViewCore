use html_view_select::SelectionView;
use kurbo::Point;

use crate::RendererCore;

impl RendererCore {
    pub(crate) fn selection_view(&self) -> SelectionView<'_> {
        SelectionView::new(
            self.text_geometry(),
            &self.page.view.selection,
            &self.page.view.table_selection,
        )
    }

    pub fn begin_table_selection_at(&mut self, position: Point) -> bool {
        let Some(hit) = self.selection_view().table_hit(position) else {
            return false;
        };
        self.page.view.table_selection.begin(hit);
        self.request_overlay_repaint();
        true
    }

    pub(crate) fn begin_table_selection_drag(&mut self, position: Point) -> bool {
        let Some(hit) = self.selection_view().table_hit(position) else {
            return false;
        };
        if !self.page.view.table_selection.begin_drag(hit) {
            return false;
        }
        self.request_overlay_repaint();
        true
    }

    pub(crate) fn update_table_selection_drag(&mut self, position: Point) -> bool {
        let hit = self.selection_view().table_hit(position);
        let Some(changed) = self.page.view.table_selection.update_drag(hit) else {
            return false;
        };
        if changed {
            self.request_overlay_repaint();
        }
        true
    }

    pub fn clear_table_selection(&mut self) -> bool {
        if !self.page.view.table_selection.clear() {
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
        let Some(text) = payload else {
            return Ok(false);
        };
        self.host.set_clipboard(&text)?;
        Ok(true)
    }
}
