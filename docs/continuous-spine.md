# Continuous spine

The reader can show the spine as one continuous run instead of a document at a
time: when the page runs out mid-screen, the items after it fill the columns it
left. Enabled with `RendererInitialConfig { continuous_spine: true, .. }`;
default off.

Commits `45d1906`..`d687ff5`.

## The idea it rests on

A spine item always begins at the top of a column, so **a document boundary is
always a column boundary**. Nothing flows across one. That is what makes the
mode cheap: each item is paginated on its own, from its own first column, and
then shifted into whatever columns are still free. No pagination rule has to
learn about a document that is not the current one.

Each item is paginated against *the columns it will actually get* — a copy of
the layout with `col_count` reduced to the free ones — so what it reports about
running out describes the screen it is on. Reading forward depends on that.

## Model

### A spine item is one thing

`SpineItem` (`spine_item.rs`) is a document with everything addressed by it:

```rust
pub(crate) struct SpineItem {
    session: Engine,          // can lay the document out again
    inputs: PipelineInputs,
    images: ImagePipeline,    // image_idx indexes *this* document
    view: DocumentView,       // document, layout, frame, selection
}
```

The page is one, each item beside the page is one, and `DocumentCache` holds
them rather than a shape of its own. Arriving in a document is an assignment.

A note is **not** one. It is laid out from a document that already exists and
shares that document's resources, so it is a `DocumentView` — the display half
alone.

### A position carries its document

| form | type | survives relayout | says |
| --- | --- | --- | --- |
| durable | `SpineAnchor { doc, glyph }` | yes | where the reader is |
| working | `SpinePosition { doc, offset_y }` | no | where the screen starts |

A glyph index names a place in whatever document it indexes, so it travels with
that document; an anchor from a document the reader has left simply does not
apply (`NavigationState::anchor_glyph_here`). An offset can say "the top of the
document", which is above its first glyph, and an anchor cannot — so they are
both kept, and neither is derived from the other.

`go_to(SpinePosition)` is the one way to reach a place: it opens the document if
needed, sets the offset, and takes the anchor from what is there rather than
moving the reader to the anchor.

### Indices belong to their document

Three things are per-document indices, and the rule is who resolves them:

| index | owner | how it is kept right |
| --- | --- | --- |
| `image_idx` | engine | the item's own `ImagePipeline`, resolved inside the core |
| `GlyphId` | **host** | `RendererHost::set_glyph_document(doc)` before every stretch of shaping or painting |
| glyph anchor | core | `SpineAnchor` carries `doc` |

`set_glyph_document` is irreducible, not scaffolding: a `GlyphId` indexes the
`GlyphRegistry` of the document it was shaped from, and the host resolves it
outside the core. Its default is a no-op, so a host showing one document at a
time is unaffected.

### Composing a screen

`fill_following_columns` runs each frame after the page is prepared, memoised on
`(FrameGeometryCacheKey, current_doc_index)`. It walks forward from the page
while free columns remain, paginating each item into them and recording:

```rust
FollowingItem { spine, doc_index, next_start_offset_y, reached_end }
```

`next_start_offset_y` and `reached_end` are the slice of viewport state a
following item needs — the page's own lives in `ViewportState`.

`spine_continuation()` reads them back: reading continues inside the last item
shown if it was cut off, otherwise at the start of the item after it. Both
`next_page` and `next_line` bottom out there, through `continue_past_page`.

Painting: the page paints normally, then each following item through
`paint_remaining_base` — after the page's overlay, with no canvas fill, since
they occupy columns the page does not.

## What is done

- Forward page turns and line scrolling cross document boundaries without
  repeating or skipping content.
- Following items are placed, painted, and paginated into the columns they get.
- Each has its own image pipeline, polled and prefetched.
- Glyph ids are resolved against the document that shaped them.

## What is not

In the order worth doing, with the reason.

1. **Following items are re-parsed on every screen change.**
   `fill_following_columns` calls `load_document_for_index`, which goes straight
   to the provider and never consults `document_cache` — so a page turn, a line
   scroll or a resize re-runs the full pipeline for each item beside the page.
   The memo only stops it repeating *within* one screen. Now that the cache
   holds `SpineItem`s, the fix is to ask the cache and put the item back.
   Navigating into a following item also reloads a document laid out moments
   before.

2. **Pointer input only reaches the page.** `text_geometry()` is built from
   `self.page.view`, and `hit_test_glyph`, `link_at`, `hit_test_image` and
   `pointer_down` all go through it. Text in the following columns cannot be
   clicked or selected. Hit testing has to ask each view on screen and answer
   with the document it hit.

3. **Backward pagination does not walk the spine.** `prev_page` at the top of a
   document opens the previous one at its end — a screen the reader never saw,
   because going forward they saw it beside something. Depends on (1), or every
   step back re-parses.

4. **Overlays stop at the page.** Following items are painted with
   `highlight`, `annotations` and `media_overlay` defaulted, so search hits and
   annotations there are invisible. `resolve_visible_annotations` shows the
   shape the fix takes: a book-level source resolved per view on screen.

5. **Reading position describes the page only.** Progress, CFI and the nav
   anchor report where the page is. `SpineAnchor` can express a position in any
   document; nothing sets one from a following item.

6. **Images.** `ImagePipeline` is keyed by an index into a resource table fixed
   at construction and owned by its worker thread, so one pipeline cannot serve
   several documents. A shared cache keyed by *source* would give two threads
   instead of two per on-screen document, shared decodes across chapters, and —
   the real gain — one memory budget instead of N unbounded windows. That is a
   change in HtmlEngine. Separately: a following item's image cannot trigger a
   relayout when it decodes at an unexpected size, and there is no read-ahead
   for it.

7. **No runtime toggle.** `continuous_spine` is initial config only, unlike
   `NoteDisplay`.

Minor: `columns_used(..).max(used + 1)` claims a column for a document that laid
out empty, to guarantee the loop terminates.

## The fundamental refactor, and where it got to

The reader's unit should be *a position in the spine and a screen filled from
it*, not a document with extras. Two halves:

- **Identity** — anything indexed by a document belongs with that document.
  Done for engine-owned resources (`SpineItem`), and named across the boundary
  for the host-owned one (`set_glyph_document`).
- **Position** — a spine cursor rather than an offset into whichever document is
  the page. Done for what it takes to read *forward*: positions carry their
  document, and `go_to` is the one way to reach one. Not done for backward, which
  is what would make filling a screen forwards and backwards the same walk.

Collapsing `page` / `following` / `note` into a placed sequence is deliberately
not done. It earns nothing on its own and falls out of the two halves above.

Every bug found in this work was one missing invariant. A stale `nav_anchor_glyph`
was a glyph index carried into a document it did not index. The garbled glyphs
were a per-document index resolved against the wrong document. The image
pipeline was that again, unfired. Three symptoms, one cause.

## Looking at it

`cargo run -p html-view-harness` opens a window on a built-in spine; pass a
directory to read HTML from it instead. Arrows turn pages, `↓`/`↑` scroll a
line, `n`/`p` change document, `c` switches modes (by reopening the spine, since
there is no runtime toggle), `g` column rules, `+`/`-` and `[`/`]` resize, drag
selects. `HARNESS_CONTINUOUS=0` starts a document at a time.

It is not part of the library: nothing depends on it, so winit and its relations
stay out of the builds of the crates it exercises. It found the placement bug,
the stale anchor and the glyph id collision — none of which the assertions had.
