mod chapter_preparation;
mod view;

pub use chapter_preparation::ChapterPreparationWorker;

pub use html_gpui::TextShapeCacheStats;
pub use view::{DocumentTap, HtmlLayoutGeometry, HtmlNoteElement, HtmlPageSnapshot, HtmlView, PaintStats, PreparedNote, ReadingRulerStyle};

#[cfg(not(target_arch = "wasm32"))]
pub use html_gpui::backend::configure_cache_directory as configure_font_cache;
pub use html_gpui::backend::is_private_font_family;

pub use html_gpui::backend::configure_font_loader;
