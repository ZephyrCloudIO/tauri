use std::{
  collections::HashMap,
  sync::{Arc, Mutex, OnceLock},
};

use cef::{args::Args, *};

const IPC_MESSAGE_NAME: &str = "tauri:ipc";
const IPC_POST_MESSAGE_FUNCTION: &str = "postMessage";
const ALL_FRAME_INITIALIZATION_SCRIPTS_KEY: &str = "tauri:all-frame-initialization-scripts";

#[derive(Debug)]
struct BrowserInitializationScriptState {
  // CEF creates a replacement browser with the same identifier before destroying the old one
  // during cross-origin navigation, so cleanup must follow browser instances rather than IDs.
  active_browser_count: usize,
  scripts: Arc<[String]>,
}

type BrowserInitializationScripts = Arc<Mutex<HashMap<i32, BrowserInitializationScriptState>>>;

wrap_v8_handler! {
  struct IpcPostMessageV8Handler;

  impl V8Handler {
    fn execute(
      &self,
      name: Option<&CefString>,
      _object: Option<&mut V8Value>,
      arguments: Option<&[Option<V8Value>]>,
      retval: Option<&mut Option<V8Value>>,
      exception: Option<&mut CefString>,
    ) -> std::os::raw::c_int {
      let Some(name) = name else {
        return 0;
      };
      if name.to_string() != IPC_POST_MESSAGE_FUNCTION {
        return 0;
      }

      let Some(message) = arguments
        .filter(|arguments| arguments.len() == 1)
        .and_then(|arguments| arguments[0].as_ref())
        .filter(|argument| argument.is_string() != 0)
      else {
        if let Some(exception) = exception {
          *exception = CefString::from("window.ipc.postMessage expects a string argument");
        }
        return 1;
      };

      let Some(context) = v8_context_get_current_context() else {
        return 1;
      };
      let Some(frame) = context.frame() else {
        return 1;
      };

      let body = CefString::from(&message.string_value()).to_string();
      let url = CefString::from(&frame.url()).to_string();
      let mut process_message = process_message_create(Some(&CefString::from(IPC_MESSAGE_NAME)));
      if let Some(args) = process_message.as_ref().and_then(ProcessMessage::argument_list) {
        args.set_string(0, Some(&CefString::from(url.as_str())));
        args.set_string(1, Some(&CefString::from(body.as_str())));
        frame.send_process_message(ProcessId::BROWSER, process_message.as_mut());
      }

      if let Some(retval) = retval {
        *retval = v8_value_create_undefined();
      }
      1
    }
  }
}

fn install_ipc_post_message(context: &mut V8Context) {
  let Some(window) = context.global() else {
    return;
  };

  let attributes = sys::cef_v8_propertyattribute_t(
    [
      sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY,
      sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTENUM,
      sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTDELETE,
    ]
    .into_iter()
    .fold(0, |acc, attr| acc | attr.0),
  )
  .into();

  let Some(mut ipc) = v8_value_create_object(None, None) else {
    return;
  };
  let mut handler = IpcPostMessageV8Handler::new();
  let post_message_name = CefString::from(IPC_POST_MESSAGE_FUNCTION);
  let Some(mut post_message) =
    v8_value_create_function(Some(&post_message_name), Some(&mut handler))
  else {
    return;
  };

  ipc.set_value_bykey(
    Some(&post_message_name),
    Some(&mut post_message),
    attributes,
  );
  window.set_value_bykey(Some(&CefString::from("ipc")), Some(&mut ipc), attributes);
}

fn initialization_scripts_from_extra_info(
  extra_info: Option<&mut DictionaryValue>,
) -> Arc<[String]> {
  let Some(script_list) = extra_info.and_then(|extra_info| {
    extra_info.list(Some(&CefString::from(ALL_FRAME_INITIALIZATION_SCRIPTS_KEY)))
  }) else {
    return Arc::default();
  };

  (0..script_list.size())
    .map(|index| CefString::from(&script_list.string(index)).to_string())
    .collect::<Vec<_>>()
    .into()
}

fn register_browser_initialization_scripts(
  browser_initialization_scripts: &mut HashMap<i32, BrowserInitializationScriptState>,
  browser_identifier: i32,
  scripts: Arc<[String]>,
) {
  if let Some(state) = browser_initialization_scripts.get_mut(&browser_identifier) {
    state.active_browser_count += 1;
    if !scripts.is_empty() {
      state.scripts = scripts;
    }
  } else if !scripts.is_empty() {
    browser_initialization_scripts.insert(
      browser_identifier,
      BrowserInitializationScriptState {
        active_browser_count: 1,
        scripts,
      },
    );
  }
}

fn unregister_browser_initialization_scripts(
  browser_initialization_scripts: &mut HashMap<i32, BrowserInitializationScriptState>,
  browser_identifier: i32,
) {
  let should_remove = browser_initialization_scripts
    .get_mut(&browser_identifier)
    .is_some_and(|state| {
      if state.active_browser_count > 1 {
        state.active_browser_count -= 1;
        false
      } else {
        true
      }
    });
  if should_remove {
    browser_initialization_scripts.remove(&browser_identifier);
  }
}

wrap_render_process_handler! {
  struct TauriRenderProcessHandler {
    browser_initialization_scripts: BrowserInitializationScripts,
  }

  impl RenderProcessHandler {
    fn on_browser_created(
      &self,
      browser: Option<&mut Browser>,
      extra_info: Option<&mut DictionaryValue>,
    ) {
      let Some(browser) = browser else {
        return;
      };
      register_browser_initialization_scripts(
        &mut self.browser_initialization_scripts.lock().unwrap(),
        browser.identifier(),
        initialization_scripts_from_extra_info(extra_info),
      );
    }

    fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
      let Some(browser) = browser else {
        return;
      };
      unregister_browser_initialization_scripts(
        &mut self.browser_initialization_scripts.lock().unwrap(),
        browser.identifier(),
      );
    }

    fn on_context_created(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      context: Option<&mut V8Context>,
    ) {
      let Some(context) = context else {
        return;
      };
      install_ipc_post_message(context);

      let (Some(browser), Some(frame)) = (browser, frame) else {
        return;
      };
      if frame.is_main() != 0 {
        return;
      }

      let scripts = self
        .browser_initialization_scripts
        .lock()
        .unwrap()
        .get(&browser.identifier())
        .map(|state| state.scripts.clone());
      let Some(scripts) = scripts else {
        return;
      };
      for script in scripts.iter() {
        let mut retval = None;
        let mut exception = None;
        if context.eval(
          Some(&CefString::from(script.as_str())),
          Some(&CefString::from("tauri://initialization-script")),
          0,
          Some(&mut retval),
          Some(&mut exception),
        ) == 0
        {
          let message = exception
            .map(|exception| CefString::from(&exception.message()).to_string())
            .unwrap_or_else(|| "unknown V8 error".to_string());
          eprintln!(
            "failed to evaluate an all-frame initialization script in a child frame: {message}"
          );
        }
      }
    }
  }
}

fn render_process_handler() -> RenderProcessHandler {
  static BROWSER_INITIALIZATION_SCRIPTS: OnceLock<BrowserInitializationScripts> = OnceLock::new();
  TauriRenderProcessHandler::new(
    BROWSER_INITIALIZATION_SCRIPTS
      .get_or_init(Arc::default)
      .clone(),
  )
}

wrap_app! {
  struct TauriRenderApp;

  impl App {
    fn render_process_handler(&self) -> Option<RenderProcessHandler> {
      Some(render_process_handler())
    }
  }
}

fn main() {
  let args = Args::new();

  #[cfg(all(target_os = "macos", feature = "sandbox"))]
  let _sandbox = {
    let mut sandbox = cef::sandbox::Sandbox::new();
    sandbox.initialize(args.as_main_args());
    sandbox
  };

  #[cfg(target_os = "macos")]
  let _loader = {
    let loader = library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), true);
    assert!(loader.load());
    loader
  };

  let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
  let mut app = TauriRenderApp::new();
  execute_process(
    Some(args.as_main_args()),
    Some(&mut app),
    std::ptr::null_mut(),
  );
}

#[cfg(test)]
mod tests {
  use super::*;

  fn scripts(source: &str) -> Arc<[String]> {
    vec![source.to_string()].into()
  }

  #[test]
  fn cross_origin_browser_replacement_retains_scripts_when_old_browser_is_destroyed() {
    let mut browser_initialization_scripts = HashMap::new();

    register_browser_initialization_scripts(
      &mut browser_initialization_scripts,
      7,
      scripts("initial"),
    );
    register_browser_initialization_scripts(
      &mut browser_initialization_scripts,
      7,
      scripts("replacement"),
    );
    unregister_browser_initialization_scripts(&mut browser_initialization_scripts, 7);

    let state = browser_initialization_scripts.get(&7).unwrap();
    assert_eq!(state.active_browser_count, 1);
    assert_eq!(&*state.scripts, ["replacement"]);
  }

  #[test]
  fn final_browser_destruction_removes_initialization_scripts() {
    let mut browser_initialization_scripts = HashMap::new();

    register_browser_initialization_scripts(
      &mut browser_initialization_scripts,
      7,
      scripts("initial"),
    );
    unregister_browser_initialization_scripts(&mut browser_initialization_scripts, 7);

    assert!(!browser_initialization_scripts.contains_key(&7));
  }
}
