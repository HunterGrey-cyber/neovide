//! Library target for the `neovide` package.
//!
//! This exposes Neovide's internals — renderer, editor/draw-command state machine, bridge
//! protocol types, settings, units, etc. — as a regular Rust library so an external crate can
//! embed Neovide's renderer as a component (constructing a [`renderer::Renderer`] and driving it
//! with hand-built or externally-sourced [`renderer::DrawCommand`]s) instead of only being
//! reachable as the standalone `neovide` binary.
//!
//! [`demo_harness::DemoHarness`] packages that seam as a small, ready-to-embed API: a struct that
//! owns a `Renderer` fed entirely fabricated content (no live `nvim --embed` connection) and a
//! single per-frame method an external GTK/Skia host can call in its own render loop.
//!
//! This file is a deliberately separate crate root from `src/main.rs`, not a re-point of it:
//! `main.rs` keeps its own private `mod` declarations exactly as before, so the existing
//! `neovide` binary is completely unaffected by this file's existence (it still owns
//! `window::application`'s `run_app` call and starts up exactly as it always has). The two crate
//! roots duplicate the module tree (and so, at the object-code level, duplicate compilation of
//! every shared module) rather than sharing type identities between the `neovide` binary and the
//! `neovide` library — this was the less invasive option: `main.rs` needed zero changes, versus
//! rewriting it to `use neovide::...` and deleting its own `mod` tree, which would have carried a
//! real (if probably small) risk of behavior changes in the always-shipped binary for a phase
//! that isn't supposed to touch it. The duplication costs extra build time, and means a type from
//! `neovide::renderer::Renderer` (this lib) is *not* interchangeable with the binary's own
//! internal `Renderer` type — but nothing needs them to be, since the binary never depends on
//! this lib target.
//!
//! Crate-level attributes and `extern crate` macro imports below are copied from `main.rs` (minus
//! the `windows_subsystem` attribute, which is only meaningful on a `bin`/`cdylib` target) because
//! attributes on one crate root do not apply to the other.

#![allow(unknown_lints)]

#[macro_use]
extern crate neovide_derive;

#[macro_use]
extern crate clap;

#[macro_use]
extern crate derive_new;

pub mod bridge;
pub mod channel_utils;
pub mod clipboard;
pub mod cmd_line;
pub mod demo_harness;
pub mod dimensions;
pub mod editor;
pub mod error_handling;
pub mod frame;
#[cfg(target_os = "macos")]
pub mod ipc;
pub mod live_harness;
pub mod platform;
pub mod profiling;
pub mod renderer;
pub mod running_tracker;
pub mod settings;
pub mod units;
pub mod utils;
pub mod version;
pub mod window;

#[cfg(target_os = "windows")]
pub mod windows_utils;

// The following bindings mirror a subset of src/main.rs's own top-level `use`/`pub use`
// statements. They are needed for the same reason main.rs needs them: some modules below refer to
// these items via `crate::Foo` rather than their full path (e.g. `renderer/mod.rs` uses
// `crate::WindowSettings`, `window/window_wrapper.rs` uses `crate::CmdLineSettings`,
// `bridge/handler.rs` uses `crate::{LoggingReceiver, LoggingSender}`). A plain (non-`pub`) `use`
// at a crate root is visible to every descendant module of that root, which is sufficient here
// since none of this needs to be part of this lib's own public API (a consuming crate should
// reach these types through `neovide::window::WindowSettings` etc. instead) — it just has to
// exist so the crate compiles, exactly mirroring main.rs's own (non-pub, in these two cases)
// imports of the same items. Unlike main.rs, only the two names that other modules actually
// reference via `crate::` are imported here — main.rs's other same-looking imports (`Config`,
// `Settings`, `Application`, ...) are there for main.rs's own direct use in `fn main`, which this
// lib has no equivalent of.
use cmd_line::CmdLineSettings;
use window::WindowSettings;

pub use channel_utils::*;
#[cfg(target_os = "windows")]
pub use windows_utils::*;
