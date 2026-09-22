pub mod bootstrap;
pub mod extensions;
pub mod fetch;
pub mod manifest;
pub mod ops;
pub mod platform;
pub mod profile;
pub mod progress;
pub mod runtime;
pub mod settings;
pub mod state;
pub mod system;
pub mod uninstall;
pub mod verify;
pub mod vscode;
/// Windows-only: the GUI's own WebView2 runtime. Gated because macOS uses
/// in-box WKWebView and has nothing to provision.
#[cfg(windows)]
pub mod webview2;
