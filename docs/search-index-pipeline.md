# Parse-level search index

> **Not built.** Notes were addressed by moving flow exclusion and scoped note
> layout into the engine instead, which removed `scraper` without an index.
> This plan is kept for two findings that stand on their own: `SourcePosition`
> is DOM-numbered rather than box-numbered, and whole-book search joins its
> results to the laid-out document by a bare ordinal. The second is a live
> defect and is still unfixed.

Plan for the `html-engine` side of a document text index: what the engine
builds, where it lives, and why it is not a stage of the layout pipeline.

Companion to [`scraper-usage.md`](scraper-usage.md), which surveys the reparsing
this is meant to retire. The reader-side work is summarised at the end but not
expanded here.

## Why

Three reader features need the text of spine documents that are **not laid
out**, and each derives it separately:

| feature | site | derives text via |
| --- | --- | --- |
| footnote previews | `renderer/nav.rs:250-292` | `scraper`, its own parsed DOM |
| whole-book search | `lib.rs:75-108` | `plain_text_from_fragment` per search |
| progress weights | `lib.rs:110-112` | `plain_text_from_fragment`, second pass |

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
    /// Plain text, whitespace-collapsed. Callers match against this.
    text: String,
    /// Sorted by `text_start`. Maps offsets in `text` to source positions.
    runs: Vec<TextRun>,
    /// Element paths, deduped — consecutive runs usually share one.
    paths: Vec<Box<[SourceElementStep]>>,
}

struct TextRun {
    text_start: u32,   // char offset into `text`
    len: u32,
    path: u32,         // index into `paths`
    text_step: u32,    // text node within that element
    utf16_base: u32,   // UTF-16 offset in the source node where the run starts
}
```

Lookup is a binary search on `text_start`, then `utf16_base + (offset -
text_start)`. That yields the `{ elements, text_step, utf16_offset }` triple
`resolve_source_position` already consumes (`html-view-cfi/src/lib.rs:99`).

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

So the builder lives in `html-parse`, beside `plain_text_from_fragment`, with two
entry points onto one implementation:

1. **Standalone** — `DocumentTextIndex::build(source)`, parse only. The path
   search uses, for nearly every document.
2. **Pipeline byproduct** — the pipeline exposes the index for the document it
   has just parsed. Free, since the parse already happened.

Entry point 2 matters more than it looks. Navigating to a search hit loads that
document through the pipeline anyway, so the position map for the target comes
out of that same parse. No reparse on the interactive path.

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

4. **Add note queries** on `ParsedHtml` (already public, already reachable as
   `html::parse`): `is_note_target(id)`, `note_blocks(id)`. These retire the
   `scraper` path; they use the predicate from step 2.

## Open decisions

**Should note text be searchable?** Left open deliberately. Excluding it means
users cannot find text the book plainly contains and the reader will happily
show in a popup. The likely right answer is to index note text and mark the hit
as belonging to a note, so activating it opens the popup rather than scrolling
the flow. That changes what a run records, so it should be settled before the
builder is written.

**Memory.** Text is order 1–2 MB per book. Runs are the larger half and were
estimated, not measured — plausibly tens of megabytes for a large book if held
for every document. This is why text and positions are separate structures: text
for every document, session-lived; runs built on demand for the one document
being resolved. Measure before revisiting.

## Non-goals

- **Persistence.** Built per session, held in memory. No serialization, no
  version stamps, no fingerprints. Consumers that later want persistence will
  need an extraction-version stamp and a source fingerprint, since a change to
  extraction rules silently invalidates stored indexes.
- **Search semantics.** Case folding, diacritics and whole-word matching stay
  reader-side in `find_text_matches` (`html-view-doc/src/lib.rs:594`). The engine
  supplies text and positions; the reader decides what matches.
- **Layout flow changes.** Moving `READER_FOOTNOTE_CSS`
  (`html-view-load/src/lib.rs:17-28`) into the engine is a separate, later piece.

## Sequencing

The engine work is steps 1–4 above and gates the reader-side changes:

- reader search consumes the index; `BookSearchMatch` carries a text offset
  instead of `occurrence`, and `navigate_to_book_match` (`lib.rs:682-690`)
  resolves it structurally
- progress weights read from the same text
- footnote previews move to `note_blocks`; `scraper`, `FootnoteDocumentIndex`
  and its LRU are deleted
- `READER_FOOTNOTE_CSS` moves into the engine, last

Two things stand independently of the engine and can land first: an in-memory
text store using today's `plain_text_from_fragment`, and a test pinning the
ordinal-join defect described in `scraper-usage.md`.

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
