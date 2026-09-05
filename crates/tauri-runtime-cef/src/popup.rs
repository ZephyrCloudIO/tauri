// Copyright 2019-2026 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! CEF-owned popup lifetimes. CEF keeps its native window and opener semantics;
//! the runtime owns observation, teardown, and the separate protocol observer.

use crate::{
  FrameNavigationState, NativeWindowToken,
  webview::{DevToolsProtocolHandler, Webview, WebviewSnapshot, add_dev_tools_observer},
};
use cef::*;
use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, Ordering},
};
use tauri_runtime::dpi::{LogicalPosition, LogicalSize, Rect};

const MAX_POPUPS: usize = 128;

struct Popup {
  browser: Browser,
  state: FrameNavigationState,
  opener: FrameNavigationState,
  closing: AtomicBool,
  window: Mutex<Option<(Window, NativeWindowToken)>>,
  _observer: Registration,
}

pub(crate) struct PopupFamily {
  root: FrameNavigationState,
  closing: AtomicBool,
  popups: Mutex<Vec<Arc<Popup>>>,
  pub(crate) handlers: Arc<Mutex<Vec<Arc<DevToolsProtocolHandler>>>>,
}

impl PopupFamily {
  pub(crate) fn new(root: FrameNavigationState) -> Self {
    Self {
      root,
      closing: AtomicBool::new(false),
      popups: Mutex::default(),
      handlers: Arc::default(),
    }
  }

  pub(crate) fn admits(&self, opener: &FrameNavigationState, browser_id: i32) -> bool {
    if self.closing.load(Ordering::Acquire) || !opener.has_browser_id(browser_id) {
      return false;
    }
    let Ok(popups) = self.popups.lock() else {
      return false;
    };
    popups.len() < MAX_POPUPS
      && (self.root.is_same_browser(opener)
        || popups.iter().any(|popup| {
          !popup.closing.load(Ordering::Acquire) && popup.state.is_same_browser(opener)
        }))
  }

  pub(crate) fn created(
    &self,
    browser: &Browser,
    opener: &FrameNavigationState,
    state: &FrameNavigationState,
  ) {
    let Some(host) = browser.host() else {
      return;
    };
    if !self.admits(opener, host.opener_identifier()) {
      state.close();
      host.close_browser(1);
      return;
    }
    let Some(observer) = add_dev_tools_observer(browser, self.handlers.clone(), Arc::default())
    else {
      state.close();
      host.close_browser(1);
      return;
    };
    let popup = Arc::new(Popup {
      browser: browser.clone(),
      state: state.clone(),
      opener: opener.clone(),
      closing: AtomicBool::new(false),
      window: Mutex::default(),
      _observer: observer,
    });
    if let Ok(mut popups) = self.popups.lock() {
      popups.push(popup);
    } else {
      state.close();
      host.close_browser(1);
    }
  }

  /// Revoke descendants before asking CEF to close them. The native close
  /// callbacks remove their browser references; no family lock crosses CEF.
  pub(crate) fn closed(&self, state: &FrameNavigationState, browser_id: i32) {
    if !state.has_browser_id(browser_id) {
      return;
    }
    state.close();
    let root_closed = self.root.is_same_browser(state);
    if root_closed {
      self.closing.store(true, Ordering::Release);
    }
    let descendants = {
      let Ok(mut popups) = self.popups.lock() else {
        return;
      };
      popups.retain(|popup| !popup.state.is_same_browser(state));
      revoke_descendants(
        state,
        root_closed,
        popups
          .iter()
          .enumerate()
          .map(|(index, popup)| (index, &popup.state, &popup.opener, &popup.closing)),
      )
      .into_iter()
      .map(|index| Arc::clone(&popups[index]))
      .collect::<Vec<_>>()
    };
    for popup in descendants {
      if popup.browser.is_valid() != 0
        && let Some(host) = popup.browser.host()
      {
        host.close_browser(1);
      }
    }
  }

  pub(crate) fn observe(&self) -> Vec<Webview> {
    if self.closing.load(Ordering::Acquire) {
      return Vec::new();
    }
    let popups = self
      .popups
      .lock()
      .map(|popups| popups.clone())
      .unwrap_or_default();
    popups
      .iter()
      .filter_map(|popup| {
        if popup.closing.load(Ordering::Acquire) || popup.browser.is_valid() == 0 {
          return None;
        }
        let mut browser = popup.browser.clone();
        let view = browser_view_get_for_browser(Some(&mut browser));
        let observed_window = view
          .as_ref()
          .and_then(ImplView::window)
          .filter(|window| window.is_closed() == 0);
        let previous = popup.window.lock().ok()?.clone();
        let window = observed_window.as_ref().map(|window| {
          previous
            .as_ref()
            .filter(|(previous, _)| window.is_same(Some(&mut View::from(previous))) != 0)
            .map(|(_, token)| token.clone())
            .unwrap_or_else(NativeWindowToken::new)
        });
        *popup.window.lock().ok()? = observed_window.clone().zip(window.clone());
        let bounds = view.as_ref().map(|view| {
          let rect = view.bounds();
          Rect {
            position: LogicalPosition::new(rect.x, rect.y).into(),
            size: LogicalSize::new(rect.width, rect.height).into(),
          }
        });
        let visible = view
          .as_ref()
          .zip(observed_window.as_ref())
          .map(|(view, window)| {
            view.is_drawn() != 0 && window.is_visible() != 0 && window.is_minimized() == 0
          });
        let snapshot = WebviewSnapshot {
          browser_id: browser.identifier(),
          document: popup.state.observe_document(&browser),
          window_label: None,
          window,
          parent_matches: observed_window.as_ref().map(|_| true),
          bounds,
          visible,
        };
        let mut native = Webview::new(browser, snapshot, popup.state.clone());
        native.set_opener(popup.opener.clone());
        Some(native)
      })
      .collect()
  }
}

/// The graph walk is independent of CEF calls. Revoke every descendant before
/// returning handles for native close, including when callbacks arrive out of order.
fn revoke_descendants<'a>(
  state: &FrameNavigationState,
  root_closed: bool,
  nodes: impl Iterator<
    Item = (
      usize,
      &'a FrameNavigationState,
      &'a FrameNavigationState,
      &'a AtomicBool,
    ),
  > + Clone,
) -> Vec<usize> {
  let mut revoked = vec![state.clone()];
  let mut indices = Vec::new();
  loop {
    let before = indices.len();
    for (index, state, opener, closing) in nodes.clone() {
      if !closing.load(Ordering::Acquire)
        && (root_closed || revoked.iter().any(|parent| parent.is_same_browser(opener)))
      {
        closing.store(true, Ordering::Release);
        state.close();
        revoked.push(state.clone());
        indices.push(index);
      }
    }
    if indices.len() == before {
      return indices;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  fn created(id: i32) -> FrameNavigationState {
    let state = FrameNavigationState::new();
    state.on_frame_event(&crate::FrameEvent {
      browser_id: id,
      frame_id: "main".into(),
      is_main: true,
      kind: crate::FrameEventKind::Created,
    });
    state
  }
  #[test]
  fn closing_an_opener_revokes_only_its_exact_descendants() {
    let root = created(1);
    let first = created(2);
    let nested = created(3);
    let sibling = created(4);
    // Deliberately put a descendant before its opener.
    let nodes = [
      (nested.clone(), first.clone(), AtomicBool::new(false)),
      (first.clone(), root.clone(), AtomicBool::new(false)),
      (sibling.clone(), root.clone(), AtomicBool::new(false)),
    ];
    let refs = || {
      nodes
        .iter()
        .enumerate()
        .map(|(index, (state, opener, closing))| (index, state, opener, closing))
    };
    assert!(revoke_descendants(&created(2), false, refs()).is_empty());
    assert_eq!(revoke_descendants(&first, false, refs()), vec![0]);
    assert!(nodes[0].2.load(Ordering::Acquire));
    assert!(!nodes[2].2.load(Ordering::Acquire));
    assert_eq!(revoke_descendants(&root, true, refs()), vec![1, 2]);
    assert!(nodes.iter().all(|node| node.2.load(Ordering::Acquire)));
    assert!(revoke_descendants(&root, true, refs()).is_empty());
  }
  #[test]
  fn popup_admission_requires_the_live_exact_native_opener() {
    let root = created(1);
    let family = PopupFamily::new(root.clone());
    assert!(family.admits(&root, 1));
    assert!(!family.admits(&root, 2));
    assert!(!family.admits(&created(1), 1));
    assert!(!family.admits(&FrameNavigationState::new(), 1));
    family.closed(&root, 2);
    assert!(family.admits(&root, 1));
    family.closed(&root, 1);
    assert!(!family.admits(&root, 1));
    assert!(family.observe().is_empty());
  }
}
