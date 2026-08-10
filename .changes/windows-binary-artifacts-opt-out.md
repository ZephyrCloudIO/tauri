---
tauri-build: patch:bug
---

Add `Attributes::build_windows_binary_artifacts(bool)` (default `true`) so metadata-only packages can skip Windows resource compilation, WebView2 loader staging, and static runtime link configuration while retaining Tauri ACL generation.
