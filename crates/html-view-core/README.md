# html-view-core

This crate owns framework-independent reader state, selection, search
highlighting, pagination, painting, host interface, and command/event boundary.
The core owns document loading, cross-document navigation, selection, search, and
reader-specific paint orchestration. Generic fragment layout, retained render
scenes, and the painter contract belong to `html-render-core`. EPUB and loose-file
source discovery belongs to the consuming application. Framework adapters,
concrete glyph-layout resolution, and window-system painters live in separate
integration crates such as `html-view-gpui`.
