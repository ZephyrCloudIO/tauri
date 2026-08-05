---
tauri-build: patch:perf
---

Add `Attributes::watch_resources(bool)` (default `true`) to control whether the build script emits a `cargo:rerun-if-changed` instruction for every file matched by `bundle > resources`. Those instructions only keep the copies staged next to the executable in the cargo target directory in sync; the bundler stages its own copies from the source paths, so opting out does not change what a bundled application contains. Applications whose resource set is large, non-Rust content can now set it to `false` to stop editing a data file from recompiling the crate.
