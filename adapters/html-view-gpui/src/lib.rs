mod view;

pub use html_gpui::TextShapeCacheStats;
pub use view::{DocumentTap, HtmlLayoutGeometry, HtmlNoteElement, HtmlPageSnapshot, HtmlView, PaintCacheStats, PreparedNote, ReadingRulerStyle};

#[cfg(not(target_arch = "wasm32"))]
pub use html_gpui::backend::configure_cache_directory as configure_font_cache;
pub use html_gpui::backend::is_private_font_family;
