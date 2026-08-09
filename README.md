# HTML View Core

Framework-neutral reader pagination, navigation, interaction, and painting
contracts built on `html-engine`.

Consumers should depend on the `html-view-core` package:

```toml
[dependencies]
html-view-core = { git = "https://github.com/JohanAnderssonOstling/HtmlViewCore" }
```

The ignored corpus-profiling tests accept a local HTML corpus through
`HTML_VIEW_PROFILE_CORPUS`; application test data is deliberately not part of
this repository.
