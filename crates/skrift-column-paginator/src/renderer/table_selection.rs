use html::selection::{SelectionInput, SelectionKind, SelectionUnit};
use html::export::TableExportFormat;
use html::selection::SelectionExport;
use kurbo::Point;

use crate::{ClipboardWrite, RendererCore};

impl RendererCore {

    pub fn begin_table_selection_at(&mut self, position: Point) -> bool {
        self.interact_selection(SelectionInput::SelectAt {
            position,
            unit: SelectionUnit::TableCell,
        })
        .handled
    }

    pub fn clear_table_selection(&mut self) -> bool {
        self.interact_selection(SelectionInput::Clear(SelectionKind::Table))
            .handled
    }

    pub fn copy_table_at(&mut self, position: Point, format: TableExportFormat) -> bool {
        self.copy_to_clipboard(self.page.geometry().export_selection(
            &self.page.selection,
            SelectionExport::TableAt { position, format },
        ))
    }

    pub fn copy_table_selection(&mut self) -> bool {
        self.copy_to_clipboard(
            self.page
                .geometry()
                .export_selection(&self.page.selection, SelectionExport::TableCellsMarkdown),
        )
    }

    fn copy_to_clipboard(&mut self, payload: Option<String>) -> bool {
        let Some(text) = payload else {
            return false;
        };
        self.effects.clipboard.push(ClipboardWrite::Text(text));
        true
    }
}
