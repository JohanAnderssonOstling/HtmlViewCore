# Navigation decomposition

Follow-on to the `RendererCore` decomposition (commits `45f970b`..`4011bc1`),
which took the struct from 30 fields to 24 by extracting `DocQuery`,
`DocumentCache`, `PreparedPageCache`, and by moving behavior onto
`TableSelectionState`, `HighlightState`, `AnnotationOverlayState` and
`ColumnLayout`.

## Why navigation is separable

Measured over `renderer/nav.rs`:

- 42 methods touch `self.nav`. Only **5** have a direct field footprint of 5 or
  more; **33** touch three fields or fewer.
- Only **12** can transitively reach a document swap
  (`replace_document_state` / `cache_current_document` / `load_document_at`).
  The other 30 cannot.

The apparent density is an artifact of everything funnelling through three wide
primitives:

| primitive | direct fields |
| --- | --- |
| `replace_document_state` | 10 |
| `cache_current_document` | 6 |
| `reset_view_state` | 5 |

So navigation is already layered: a thin cross-document orchestration layer over
a large within-document layer. The within-document layer needs only the
document, the viewport, the current frame and the host.

## Defects this fixes

1. **`emit_state_snapshot` bypasses the history dedup.** It emits
   `HistoryAvailability` inline with its own copy of the predicate instead of
   calling `update_history_availability_signals`, so `last_history_availability`
   is never written and the next history change re-emits an identical event.
   The predicates are logically equivalent, so no wrong value is emitted.
2. **Five copies of the dedup pattern.** Each `last_*` field is written by
   exactly one method, all in the shape *compare, assign, emit*.
3. **Progress arithmetic implemented twice, in opposite directions.**
   `update_nav_signal` maps glyph to location number; `set_progress_fraction`
   maps fraction to document and glyph. Both are pure over
   `document_text_lengths`.

## Phase 1 — split `NavigationState` (14 fields to 8)

| step | change | status |
| --- | --- | --- |
| 1a | `Debounced<T>` owning compare/assign/emit; bundle the four `update_nav_signal` markers into `NavSignals` | done |
| 1b | `LocationHistory` owning the stack, cursor and availability marker; route `emit_state_snapshot` through it | done |
| 1c | `TocAnchors` owning `toc_anchor_strings_by_doc` + `toc_anchor_filter`, rebuilt from `DocQuery` | done |

Left bare on `NavigationState`: `current_doc_index`, `document_uris`,
`document_text_lengths`, `nav_anchor_glyph`, `pending_nav_anchor_update`. These
are read 50+ times across the renderer and are genuinely shared; wrapping them
buys nothing.

## Phase 2 — extract `Navigator`

| step | change | status |
| --- | --- | --- |
| 2a | `NavView` (read-only) and `NavContext` (read-write) over `DocQuery`, viewport, frame, host | done |
| 2b | Move within-document navigation onto `NavigationState` taking those contexts | done |

Stays on `RendererCore`: the 12 cross-document entry points, which ask the
navigator for a target and then perform the swap through the wide primitives.

Four methods need `provider` or `document_cache` rather than the context and
stay behind: `toc`, `resolve_doc_index_for_path`,
`footnote_preview_for_reference`, `load_document_for_index`.

### Expected friction

`NavContext` borrows four `RendererCore` fields at once, so orchestration that
builds a context and then touches another field will fight the borrow checker.
Mitigation: build the context at the point of use and keep it short-lived.

## Invariants

- All 117 tests pass after every step.
- Warning count stays at 4 (the pre-existing set).
- No public API change: `RendererCore` is `pub(crate)`, so its methods are not
  externally reachable.

## Outcome

### Phase 1 (done)

`NavigationState`: 14 fields to 8. Defects 1 and 2 fixed; defect 3 outstanding.

`emit_state_snapshot` now calls `LocationHistory::emit_availability` instead of
emitting `HistoryAvailability` from its own inline predicate, so the dedup
marker is always written. `Debounced<T>` makes the old failure mode
unrepresentable: emitting requires going through the type that stores.

One test assertion changed shape. It reached into `location_history` and
`location_history_index` to assert the cursor sat on the last entry; it now
asserts `history.is_at_newest()`, which states the same property without
depending on the representation.

117 tests pass, 4 warnings.

### Phase 2 (done)

23 methods moved to `impl NavigationState` in `renderer/navigate.rs` (302
lines). The file references no `RendererCore` field outside the two contexts --
no pipeline, provider, cache, revisions or column layout -- so the claim that
within-document navigation needs only document, viewport, frame and host is now
checkable by the compiler rather than asserted.

Two deviations from the plan as written:

- **No separate `Navigator` type.** The methods went onto `NavigationState`
  itself, matching how `TableSelectionState`, `HighlightState` and
  `ColumnLayout` were handled in the earlier phase. A wrapper would have added a
  name without adding a boundary.
- **Two contexts, not one.** Queries only read the viewport while movement
  writes it. Forcing both through `&mut` would have made every read-only
  forwarder on `RendererCore` take `&mut self`, which several callers cannot
  provide. `NavContext::view()` produces the read-only form.

Defect 3 is fixed: `set_progress_fraction` now calls
`NavigationState::document_at_fraction`, the named inverse of
`reading_progress`, instead of repeating the walk over `document_text_lengths`.
Both are now pure functions of that field and testable directly.

`RendererCore` keeps thin forwarders, so no call site outside `nav.rs` changed.
The predicted borrow-checker friction appeared exactly once, in the two
forwarders that mutate `nav` while reading the document; `nav_cx` returning
`(&mut NavigationState, NavContext)` as disjoint field borrows resolves it.

117 tests pass; warning count unchanged at 4.

## Unit tests for the extracted components

The refactor argued repeatedly that these components became testable in
isolation, but nothing had exercised them directly: test volume went from 2874
to 2888 lines across ten commits, and that difference was one changed
assertion. The renderer-level suite covered them only incidentally.

31 unit tests added, none of which construct a pipeline, provider or glyph
shaper:

| component | covers |
| --- | --- |
| `Debounced` | suppression, re-emission after `invalidate` |
| `LocationHistory` | availability from the cursor, truncate-on-push after stepping back, refused steps at both ends, repeated pushes of one location |
| `TocAnchors` | spine-length padding without truncation |
| `PreparedPageCache` | eviction, replace-not-duplicate, geometry mismatch, end-line lookup, prefetch claim protocol, shaping-handle discard |
| `DocumentCache` | footnote LRU, and the capacity-zero asymmetry where previews survive but documents do not |
| `progress_at` / `document_at_fraction` | location numbering, empty publication, and a round-trip property between the two |

`reading_progress` was split so the arithmetic no longer needs a laid-out
document: `progress_at` takes the in-document fraction, and `reading_progress`
supplies it from the glyph count.

### Defect found

The round-trip property failed immediately at fraction `1.0`. In
`document_at_fraction` the search loop never breaks for the final document, so
the accumulator ends up including that document's own length and the result is
`(last_doc, 0.0)` instead of `(last_doc, 1.0)` -- dragging a progress slider to
100% lands at the start of the last chapter rather than its end.

Pre-existing: the same logic was inline in `set_progress_fraction` before the
extraction (verified against `4011bc1`). Fixed by locating the document first
and summing what precedes it, rather than accumulating during the search.

148 tests pass; warning count unchanged at 4.

## Extracting the paginator

Pagination turned out to be the one genuine subsystem left, and it needed no
redesign: every method was already `&self` and already returned its result.

| method | reads |
| --- | --- |
| `table_pagination_events` | document, composition policy |
| `automatic_keep_events` | document, composition policy |
| `forced_break_events` | document, composition policy |
| `build_forward_positions` | document, column layout, composition policy |
| `apply_vertical_rhythm` | document, column layout |

None touched the frame, viewport, host, revisions, pipeline or caches. The only
mutating methods in the cluster -- `prepare_forward`, `prepare_backward`,
`install_positioned_page` -- store the returned `PagePositions`; they compute
nothing.

`Paginator<'a>` bundles `DocQuery`, `&ColumnLayout`, and the two policy flags.
`renderer/paginate.rs` (665 lines) references no `RendererCore` field at all, so
pagination is now a pure function of document and column geometry by
construction. `render.rs` drops from 1414 to 817 lines.

`PaginationEvents` is computed once and reused: backward navigation
binary-searches for a start offset, so recomputing events per candidate would
have been a regression.

### A near-duplicate that is not a duplicate

`render.rs` and `semantic_keeps.rs` each carried `has_token` and `has_epub_type`,
flagged early on as obvious duplication. They are not identical: the
`semantic_keeps` version also matches namespace-prefixed tokens (`foo:chapter`).
Unifying them would widen what counts as a semantic sidebar or chapter start, so
both were preserved and the divergence documented at the definition. Worth
deciding deliberately, but not as a side effect of an extraction.

148 tests pass; warning count unchanged at 4.

## Crate split

Six crates, dependencies flowing one way:

```
html-view-types   shared vocabulary: column geometry, viewport and frame state,
                  commands, events, RendererHost, palettes, revisions, cache keys
html-view-cfi     EPUB CFI generation and parsing            -> html
html-view-load    document loading through the pipeline      -> html
html-view-doc     32 read-only document queries              -> types, cfi
html-view-paginate pagination rules and page building        -> types, doc
html-view-core    RendererCore, session, painting, navigation, interaction
```

The orphan rule shaped this more than anything else: an inherent impl must live
in the crate defining the type. Three collisions, each resolved rather than
worked around:

- **`DocQuery` had impls in eight files.** Consolidating all 32 queries into
  `html-view-doc` is what made the split possible, and it turns out to be the
  right grouping anyway.
- **`ColumnLayout` had impls in three.** Its pure geometry moved into the types
  crate with the struct; pagination's three `apply_*_event` methods became free
  functions taking `&ColumnLayout`, since they carry event types that belong to
  the paginator.
- **`pagination_boundary_anchors` and `semantic_keep_ranges`** could not follow
  the other queries into `html-view-doc` because they carry `BoundaryAnchors`.
  They became free functions taking `DocQuery`.

That is the general rule this split follows: **cross-crate additions to a
foreign type become free functions**, costing `foo(doc)` instead of `doc.foo()`.

### Consequences worth knowing

Cross-crate access forced some fields and methods from private to public:
`VisibleFrame`'s shaping handles, `VisibleLinePositions`' buffer operations,
`RendererRevisions`' invalidation. Encapsulation is weaker than it was inside
one crate; the compensating gain is that the dependency direction is now
enforced.

`#[cfg(test)]` does not cross crates. `table_row_group_bounds_containing` was
test-only in `render.rs` and is exercised by a test that stayed in
`html-view-core`, so it is now an ordinary public query with a comment saying
why.

`html-view-core` re-exports `html-view-types` and the `cfi` and `load` modules
under their old paths, so the public API is unchanged.

148 tests pass across the workspace; warning count unchanged at 4.

## Final crate layout

Cross-document orchestration is now split by responsibility instead of
accumulating again in one `renderer/nav.rs`: href/history transactions live in
`nav/href.rs`, note acquisition in `nav/notes.rs`, and spine residency plus
document activation in `nav/spine.rs`. The facade retains only shared movement,
signals, setup, and reader settings. Tests live in `nav/tests.rs` so production
navigation is roughly 1,200 lines across small modules rather than one file of
more than 4,000 lines.

| crate | code | tests | depends on |
| --- | ---: | ---: | --- |
| `html-view-core` | 3880 | 2805 | all of the below |
| `html-view-paginate` | 877 | 0 | types, doc |
| `html-view-doc` | 840 | 0 | types, cfi |
| `html-view-types` | 569 | 37 | html |
| `html-view-select` | 566 | 56 | types, doc |
| `html-view-load` | 192 | 338 | html |
| `html-view-cfi` | 136 | 72 | html |

`html-view-core` holds what genuinely coordinates: the `RendererCore` facade and
`RendererSession` public API, painting, cross-document navigation orchestration,
frame preparation and pointer handling.

### Text geometry went to `html-view-doc`, not to selection

`glyph_x_span_in_line` and the hit tests were in `selection.rs`, but every
overlay painter uses them -- annotations, search highlights, selection and
pointer handling. Putting them in the selection crate would have made
`render.rs` depend on selection in order to paint search matches. They depend on
document, frame and column geometry, so `TextGeometry` sits with the document
queries where all four callers can reach it.

### Selection stopped touching the clipboard

The four `copy_table_*` methods returned `Result<bool, String>` and called
`host.set_clipboard` themselves. They now return `Option<String>` and
`RendererCore` performs the copy, which removes `RendererHost` from the crate's
requirements entirely -- the same move that made the drag machine host-free
earlier.

### Tests for the extracted crates

27 tests added to the two crates that had none, constructing no provider, host
or `RendererCore`. Documents come from `DocumentFactory` with a fixed-metric
shaper; column geometry is a `ColumnLayout` literal.

`html-view-paginate` (10): column overflow, the invariant that no placed line
escapes its column, authored break-before, semantic chapter starts, the compact
list keep, forward paging, determinism, the empty document. The chapter fixture
is asserted both ways -- breaking under book composition and not breaking under
web -- so neither test can pass by accident.

Verified by mutation rather than trusting green: disabling
`is_semantic_page_start` fails the chapter test, and neutralising the
forced-break offset fails two. Both restored.

`html-view-doc` (17): selection text and markdown, box-tree ancestor lookup,
heading prefixes, line lookup including the empty document, search matching with
each option, marker exclusion, and anchor lookup with and without a filter.

One behavior worth knowing, found while writing them: a heading emits
`## **Title**`, because the default `h2` font weight trips the same threshold as
`<strong>`. Redundant but valid markdown; recorded at the assertion rather than
changed.
