# Parse-level search index

> **Implemented compactly.** HtmlEngine builds a parse-only
> `DocumentTextIndex` containing collapsed text plus one source record per
> contiguous source run, not per character; DOM paths are interned across
> those runs. Whole-book results turn positions into CFIs, and progress weights
> count the same indexed text. Each document index is dropped after that search
> or counting pass; no publication-
> wide position map is retained. Semantic note subtrees are excluded by the
> same predicate that holds them out of reader flow.

Plan for the `html-engine` side of a document text index: what the engine
builds, where it lives, and why it is not a stage of the layout pipeline.

Companion to [`scraper-usage.md`](scraper-usage.md), which surveys the reparsing
this is meant to retire. The reader-side work is summarised at the end but not
expanded here.

## Why

The original survey found three consumers of text from spine documents that
were **not laid out**:

| feature | derives text via |
| --- | --- |
| footnote previews | engine-scoped note layout; no second parser |
| whole-book search | transient `DocumentTextIndex` |
| progress weights | transient `DocumentTextIndex`, using the same text definition |

The engine already answers this for the current document. It cannot answer it
for the rest without laying them out, so the reader re-derives it — badly, and
three times over.

Whole-book search is the constraint that rules out the obvious fix. Text
extraction requires a parse, and search needs it for every document, so any
design that routes through layout is too expensive by construction.

## What the engine provides

A parse-level index, built from a source string without style or layout.

```rust
pub struct DocumentTextIndex {
    text: String,
    runs: Vec<SourceTextRun>,
    path_count: usize,
}

struct SourceTextRun {
    text_start: usize,
    text_end: usize,
    byte_start: usize,
    byte_end: usize,
    element_steps: Arc<[usize]>,
    text_step: usize,
    utf16_offset: usize,
}
```

Lookup binary-searches run ends, then counts UTF-16 units only in the matching
run's text slice. That yields the `{ elements, text_step, utf16_offset }` triple
the CFI resolver consumes. A 2,500-word fixture retains the necessary 2,500
whitespace-delimited runs but only one shared DOM path. Direct tests pin both
properties.

### Runs split at whitespace discontinuities

`plain_text_from_fragment` collapses whitespace (`html-parse/src/lib.rs:423`),
so collapsed text does not map linearly onto source characters. A run must be
cut at **every collapse discontinuity**, not merely at each text node. Within a
run the mapping is then linear by construction.

This is the fiddliest part of the build and needs direct tests: runs of spaces,
newlines between block elements, leading and trailing whitespace, and text nodes
split by inline elements.

UTF-16 conversion needs no extra storage. `text` holds the run's characters, so
surrogate pairs are counted from the slice when converting.

## Why this is verified, not assumed

The design rests on a parse-derived `SourcePosition` resolving identically to a
layout-derived one. It does. `source_position_for_glyph`
(`html-layout/src/stages_output.rs:1660-1680`) is layout-dependent in exactly one
line:

```rust
let (text_node_idx, local_offset) = self.doc.get_dom_node_for_glyph(glyph_idx)?;
```

Everything after is pure DOM — `get_text_node_step`, `find_text_nodes_by_step`,
`get_dom_path_to_node`, `get_element_step`, `text_ref(..).encode_utf16()`. No box
indices, no layout tree.

So element steps are numbered over the DOM. Hidden content does not perturb
them, which was the failure mode worth worrying about: it is precisely where
footnotes are suppressed today. An indexer entering from the text-node side
instead of the glyph side produces the same values.

The type's own doc comment (`stages_output.rs:1061`) states it is generic and
that CFI is built outside layout, so this reuse is the intended shape.

## Not a pipeline stage

The layout pipeline runs per document, on documents that get laid out. Search
needs the documents that never do. Making indexing a pipeline stage would force
layout across the whole book — the cost the index exists to avoid.

The builder therefore lives in `html-parse` and accepts an already parsed
document through `DocumentTextIndex::from_parsed`. Reader-side
`document_text_index(source, uri)` owns syntax selection and parsing. Search and
progress both use that helper, process one document, retain only their small
result (CFIs or a length), and drop the index before reading the next document.

## Engine changes

Mostly relocation. Nothing here touches layout.

1. **Lift DOM-path construction out of `html-layout`.** The DOM-walking half of
   `source_position_for_glyph` moves to `html-dom`/`html-parse` so it can run
   without a laid-out document. `html-layout` keeps calling it, entering from
   `get_dom_node_for_glyph` as it does now.

2. **Lift the note predicate.** `element_is_note_target` /
   `element_is_note_reference` (`html-layout/src/stages.rs:938-944`) move beside
   `ElementRef` in `html-dom`. They are pure attribute work — `element_has_token`
   has no layout dependency. This gives one definition of "note" to layout, to
   the indexer, and to preview queries.

3. **Add the index builder** in `html-parse`: walk text nodes, emit `text` plus
   `runs`, sharing the lifted path construction.

4. **Exclude semantic note subtrees** from indexed reading-flow text using the
   shared `html_dom::element_is_note_target` predicate. Note preview content is
   supplied independently by scoped engine layout.

## Decisions

**Searchable notes.** The implemented policy is that search and progress model
reader flow, so semantic note targets are excluded. A future feature can index
notes separately and attach popup navigation without changing flow search.

**Memory.** Neither text nor source runs are session-lived. Search retains only
the result CFI and excerpt; progress retains only one integer weight per
document. Within a transient index, storage scales with collapsed text plus DOM
text-run records and unique paths rather than characters multiplied by
source-path objects.

## Non-goals

- **Persistence.** Indexes are transient and are not serialized. Consumers that
  later want persistence will need an extraction-version stamp and a source
  fingerprint, since a change to extraction rules silently invalidates stored
  indexes.
- **Search semantics.** Case folding, diacritics and whole-word matching stay
  reader-side in `find_text_matches` (`html-view-doc/src/lib.rs:594`). The engine
  supplies text and positions; the reader decides what matches.
- **Layout flow changes.** Moving `READER_FOOTNOTE_CSS`
  (`html-view-load/src/lib.rs:17-28`) into the engine is a separate, later piece.

## Sequencing

The source-position index, reader search integration, progress-weight reuse,
compact run storage, and note-flow exclusion are complete. Persistence remains
out of scope unless a future consumer needs it.

The ordinal-join regression is pinned with styled-hidden text between visible
matches, which previously selected the wrong later occurrence.

**Constraint.** The `HtmlEngine` clone is on the pinned commit `7b97c5e` but
carries uncommitted work across `html-layout` (flex/grid, block flow, absolute
positioning). Steps 1–4 avoid layout, but land in the same tree. Sequencing
against that work needs deciding before starting.

For development, both sides can build against the working tree with a patch
override in `Cargo.toml`, removed when the engine change lands and the pin moves:

```toml
[patch."https://github.com/JohanAnderssonOstling/HtmlEngine"]
html-engine = { path = "../HtmlEngine/crates/html" }
```
