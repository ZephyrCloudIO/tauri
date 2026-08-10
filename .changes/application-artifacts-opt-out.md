---
tauri-build: patch:bug
---

Add `Attributes::build_application_artifacts(bool)` (default `true`) so metadata-only packages can skip application artifact staging and executable-specific link configuration while retaining Tauri ACL generation.
