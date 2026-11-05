# Reader integrations

This separate workspace owns `html-view-gpui`, the GPUI `HtmlView`, page snapshots,
note elements and reading ruler. It uses `skrift-column-paginator` for reader policy and
`HtmlEngine/adapters/html-gpui` for native shaping and painting.

Applications that only need continuous HTML can use `html-gpui` directly. Neither
adapter library enables GPUI platform features; applications own that choice.
The footnote example selects its desktop platform through a dev dependency.

The core HtmlViewCore workspace remains framework independent. With sibling
HtmlEngine and GPUI-Fork checkouts, run:

```sh
cargo test --manifest-path adapters/Cargo.toml -p html-view-gpui
```

The footnote example and font fixtures are owned by this package:

```sh
cargo run --manifest-path adapters/Cargo.toml -p html-view-gpui --example footnote_popup
```
