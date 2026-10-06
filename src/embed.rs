//! Hooks for an application that hosts Neovide's own window and reserves a strip of it on the right for a view
//! of its own. Plain Neovide registers no embedder, and every hook then returns what Neovide computes itself.
//!
//! An embedder is called on the main thread from inside the window's own event handling, so it must not call
//! back into the window synchronously: it posts an `EmbedEvent` through its `EmbedProxy` instead.
// The binary compiles this module only so that the window code's calls into it resolve in both crate roots; nothing
// in the binary starts `run`, so most of it is unused there.
#![allow(dead_code)]

use std::{
    cell::{Cell, RefCell},
    env,
    fs::{File, OpenOptions, create_dir_all},
    io::Write,
    panic::{PanicHookInfo, set_hook},
    path::PathBuf,
    process::ExitCode,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime},
};

use anyhow::Result;
use backtrace::Backtrace;
use flexi_logger::Logger;
use log::trace;
use time::{OffsetDateTime, macros::format_description};
use winit::{error::EventLoopError, event_loop::EventLoopProxy, window::WindowId};

use crate::{
    clipboard::{Clipboard, ClipboardHandle},
    cmd_line,
    error_handling::{StartupErrorOutput, handle_startup_errors, report_startup_error},
    profiling::startup_profiler,
    renderer::{
        RendererSettings, cursor_renderer::CursorSettings, progress_bar::ProgressBarSettings,
    },
    settings::{Config, Settings, load_last_window_settings, neovide_std_datapath},
    version::BUILD_VERSION,
    window::{
        Application, EventPayload, UserEvent, WindowSettings, create_event_loop,
        determine_grid_size, determine_window_size,
    },
};

const DEFAULT_BACKTRACES_FILE: &str = "neovide_backtraces.log";
const BACKTRACES_FILE_ENV_VAR: &str = "NEOVIDE_BACKTRACES";
const REQUEST_MESSAGE: &str = "This is a bug and we would love for it to be reported to https://github.com/neovide/neovide/issues";

#[derive(Clone, Debug, Default)]
pub struct EmbedConfig {
    /// Replaces Neovide's data directory (window size, backtraces). `None`: Neovide's own.
    pub data_dir: Option<PathBuf>,
    /// Replaces Neovide's config file; a missing file means Neovide's defaults, never the user's Neovide config.
    pub config_file: Option<PathBuf>,
    /// The command line handed to Neovide's parser instead of the process's own (argv[0] first).
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbedEvent {
    /// Move the keys: to the editor (`true`) or away from it (`false`).
    PaneFocus { editor: bool },
    /// The reserved strip changed width: lay the window out again.
    Layout,
}

pub trait Embedder {
    /// Physical pixels to keep free at the window's right edge, for the given inner width.
    fn reserved_right(&self, _inner_width_px: u32, _scale_factor: f64) -> u32 {
        0
    }
    /// Whether the editor holds the keys; while it does not, window focus changes are not passed to nvim.
    fn editor_has_keys(&self) -> bool {
        true
    }
    /// The window exists: add views to it.
    fn window_created(&self, _window: &winit::window::Window, _proxy: EmbedProxy) {}
    /// A frame was drawn, in this much render time (not counting the wait for the next drawable).
    fn frame_drawn(&self, _render: Duration) {}
    /// The window's close button was pressed: `false` keeps the window (the embedder asks the user first).
    fn close_requested(&self) -> bool {
        true
    }
}

#[derive(Clone)]
pub struct EmbedProxy {
    proxy: EventLoopProxy<EventPayload>,
    window: WindowId,
}

impl EmbedProxy {
    pub fn send(&self, event: EmbedEvent) -> bool {
        self.proxy
            .send_event(EventPayload::for_window(UserEvent::Embedder(event), self.window))
            .is_ok()
    }
}

static CONFIG: OnceLock<EmbedConfig> = OnceLock::new();
thread_local! {
    static EMBEDDER: RefCell<Option<Rc<dyn Embedder>>> = const { RefCell::new(None) };
    /// The last layout: where the reserved strip starts, in physical pixels from the window's left edge. One value
    /// for the process, not one per window: with several windows the latest layout computed decides pointer
    /// hit-testing in all of them, which is right only while a single window is hosted.
    static STRIP_LEFT_PX: Cell<Option<f64>> = const { Cell::new(None) };
}

fn with_embedder<T>(f: impl FnOnce(&dyn Embedder) -> T) -> Option<T> {
    // Cloned out of the cell so a hook that registers or drops an embedder cannot meet a live borrow.
    let embedder = EMBEDDER.with(|e| e.borrow().clone());
    embedder.map(|e| f(&*e))
}

pub(crate) fn data_dir() -> Option<PathBuf> {
    CONFIG.get().and_then(|c| c.data_dir.clone())
}

pub(crate) fn config_file() -> Option<PathBuf> {
    CONFIG.get().and_then(|c| c.config_file.clone())
}

/// The width to keep free at the right edge. Never more than what Neovide's own padding leaves of the window, so
/// the editor keeps a non-negative width however wide the embedder asks.
pub(crate) fn reserved_right(
    inner_width_px: u32,
    already_padded_px: u32,
    scale_factor: f64,
) -> u32 {
    let Some(wanted) = with_embedder(|e| e.reserved_right(inner_width_px, scale_factor)) else {
        return 0;
    };
    let reserved = wanted.min(inner_width_px.saturating_sub(already_padded_px));
    let left = inner_width_px - reserved;
    STRIP_LEFT_PX.with(|s| s.set((reserved > 0).then_some(f64::from(left))));
    reserved
}

pub(crate) fn pointer_in_reserved(x_px: f64) -> bool {
    STRIP_LEFT_PX.with(|s| s.get()).is_some_and(|left| x_px >= left)
}

pub(crate) fn absorb_window_focus() -> bool {
    with_embedder(|e| !e.editor_has_keys()).unwrap_or(false)
}

pub(crate) fn window_created(
    window: &winit::window::Window,
    proxy: EventLoopProxy<EventPayload>,
    id: WindowId,
) {
    with_embedder(|e| e.window_created(window, EmbedProxy { proxy, window: id }));
}

pub(crate) fn frame_drawn(render: Duration) {
    with_embedder(|e| e.frame_drawn(render));
}

pub(crate) fn close_requested() -> bool {
    with_embedder(|e| e.close_requested()).unwrap_or(true)
}

/// Runs Neovide with the window hosted by the calling application. Returns when the event loop ends.
pub fn run(config: EmbedConfig, embedder: Rc<dyn Embedder>) -> ExitCode {
    let args = config.args.clone();
    let _ = CONFIG.set(config);
    EMBEDDER.with(|e| *e.borrow_mut() = Some(embedder));

    install_panic_hook(&None);

    #[cfg(target_os = "windows")]
    {
        crate::windows_fix_dpi();
    }

    // This variable is set by the AppImage runtime and causes problems for child processes
    #[cfg(target_os = "linux")]
    unsafe {
        env::remove_var("ARGV0")
    };

    let settings = Arc::new(Settings::new());
    let config = Config::init();
    if let Err(err) = preflight(&settings, args) {
        return report_startup_error(err, StartupErrorOutput::Stderr);
    }

    init_logger();

    let event_loop = create_event_loop();
    let clipboard = Clipboard::new(&event_loop);
    let clipboard_handle = ClipboardHandle::new(&clipboard);
    let setup_proxy = event_loop.create_proxy();
    if let Err(err) = setup(setup_proxy, settings.clone(), &config) {
        return handle_startup_errors(err, event_loop, settings.clone(), clipboard);
    }

    // Upstream's `main` also sets the window theme to the background color for a transparent frame, on macOS only.
    // That is left out: this module is platform-neutral, so an embedder that wants it sets the theme itself.
    let window_settings = load_last_window_settings().ok();
    let window_size = determine_window_size(window_settings.as_ref(), &settings);
    let grid_size = determine_grid_size(&window_size, window_settings);

    let mut application = Application::new(
        window_size,
        grid_size,
        config.font,
        event_loop.create_proxy(),
        settings.clone(),
        clipboard,
        clipboard_handle,
    );

    let result = application.run(event_loop);
    match result {
        Ok(_) => application.runtime_tracker.exit_code(),
        Err(EventLoopError::ExitFailure(code)) => ExitCode::from(code as u8),
        _ => ExitCode::FAILURE,
    }
}

/// `preflight` of upstream's `main.rs`, without the passthrough to a plain nvim, the handoff to a running Neovide
/// and the disown: an embedding application owns its window and its process.
fn preflight(settings: &Settings, args: Vec<String>) -> Result<()> {
    // will exit if -h or -v
    cmd_line::handle_command_line_arguments(args, settings)?;

    startup_profiler();

    trace!("Neovide version: {}", BUILD_VERSION);

    Ok(())
}

/// `setup` of upstream's `main.rs`: the settings groups, the config file watcher and the panic hook that writes to
/// the configured backtraces path.
fn setup(
    proxy: EventLoopProxy<EventPayload>,
    settings: Arc<Settings>,
    config: &Config,
) -> Result<()> {
    settings.register::<WindowSettings>();
    settings.register::<RendererSettings>();
    settings.register::<CursorSettings>();
    settings.register::<ProgressBarSettings>();

    Config::watch_config_file(config.clone(), proxy.clone());

    install_panic_hook(&config.backtraces_path);

    Ok(())
}

/// The logger of upstream's `init_logger` without its log-to-file branch. A logger the host application already
/// installed stays: failing to start a second one is not a reason to stop.
fn init_logger() {
    let started = Logger::try_with_env_or_str("neovide = error").and_then(|logger| logger.start());
    if let Err(err) = started {
        eprintln!("Could not start the logger: {err}");
    }
}

/// Where a panic's backtrace goes: the embedder's data directory when it set one, else `configured`, else
/// upstream's own choice (`NEOVIDE_BACKTRACES`, then Neovide's data directory).
fn install_panic_hook(configured: &Option<PathBuf>) {
    let path =
        data_dir().map(|dir| dir.join(DEFAULT_BACKTRACES_FILE)).or_else(|| configured.clone());
    set_hook(Box::new(move |panic_info: &PanicHookInfo<'_>| {
        let backtrace = Backtrace::new();

        let stderr_msg = generate_stderr_log_message(panic_info, &backtrace);
        eprintln!("{stderr_msg}");

        log_panic_to_file(panic_info, &backtrace, &path);
    }));
}

// The four functions below repeat the functions of the same name in upstream's `main.rs`, which the library
// cannot reach.

fn generate_stderr_log_message(panic_info: &PanicHookInfo, backtrace: &Backtrace) -> String {
    if cfg!(debug_assertions) {
        let print_backtrace = match env::var("RUST_BACKTRACE") {
            Ok(x) => x == "full" || x == "1",
            Err(_) => false,
        };

        let backtrace_msg = match print_backtrace {
            true => format!("{backtrace:?}"),
            false => {
                "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"
                    .to_owned()
            }
        };

        let panic_msg = generate_panic_message(panic_info);

        format!("{panic_msg}\n{REQUEST_MESSAGE}\n{backtrace_msg}")
    } else {
        let panic_msg = generate_panic_message(panic_info);
        format!("{panic_msg}\n{REQUEST_MESSAGE}")
    }
}

fn log_panic_to_file(panic_info: &PanicHookInfo, backtrace: &Backtrace, path: &Option<PathBuf>) {
    let log_msg = generate_panic_log_message(panic_info, backtrace);

    let file_path = match path {
        Some(v) => v,
        None => &match env::var(BACKTRACES_FILE_ENV_VAR) {
            Ok(v) => PathBuf::from(v),
            Err(_) => neovide_std_datapath().join(DEFAULT_BACKTRACES_FILE),
        },
    };

    if let Some(parent) = file_path.parent() {
        create_dir_all(parent).ok();
    }

    let mut file = match OpenOptions::new()
        .append(true)
        .open(file_path)
        .or_else(|_| File::create(file_path))
    {
        Ok(x) => x,
        Err(e) => {
            eprintln!("Could not create backtraces file. ({e})");
            return;
        }
    };

    match file.write_all(log_msg.as_bytes()) {
        Ok(()) => eprintln!("\nBacktrace saved to {file_path:?}!"),
        Err(e) => eprintln!("Failed writing panic to {file_path:?}: {e}"),
    }
}

fn generate_panic_log_message(panic_info: &PanicHookInfo, backtrace: &Backtrace) -> String {
    let system_time: OffsetDateTime = SystemTime::now().into();

    let timestamp = system_time
        .format(format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"))
        .expect("Failed to parse current time");

    let partial_panic_msg = generate_panic_message(panic_info);
    let full_panic_msg = format!("{timestamp} - {partial_panic_msg}");

    format!("{full_panic_msg}\n{backtrace:?}\n")
}

fn generate_panic_message(panic_info: &PanicHookInfo) -> String {
    // As per the documentation for `.location()`(https://doc.rust-lang.org/std/panic/struct.PanicInfo.html#method.location)
    // the call to location cannot currently return `None`, so we unwrap.
    let location_info = panic_info.location().unwrap();
    let file = location_info.file();
    let line = location_info.line();
    let column = location_info.column();

    let raw_payload = panic_info.payload();

    let payload = match raw_payload
        .downcast_ref::<&str>()
        .map(ToOwned::to_owned)
        // Some panic messages are &str, some are String, try both to see which it is
        .or_else(|| raw_payload.downcast_ref().map(String::as_str))
    {
        Some(msg) => msg.to_owned(),
        None => return "Could not parse panic payload to a string. This is a bug.".to_owned(),
    };

    format!(
        "Neovide panicked with the message '{payload}'. (File: {file}; Line: {line}, Column: {column})"
    )
}

#[cfg(test)]
fn register_for_test(embedder: Rc<dyn Embedder>) {
    EMBEDDER.with(|e| *e.borrow_mut() = Some(embedder));
}

#[cfg(test)]
fn unregister_for_test() {
    EMBEDDER.with(|e| *e.borrow_mut() = None);
    STRIP_LEFT_PX.with(|s| s.set(None));
}

#[cfg(test)]
mod tests {
    #[test]
    fn with_no_embedder_every_hook_returns_what_neovide_computes() {
        assert_eq!(super::data_dir(), None);
        assert_eq!(super::config_file(), None);
        assert_eq!(super::reserved_right(2480, 0, 2.0), 0);
        assert!(!super::pointer_in_reserved(2000.0));
        assert!(!super::absorb_window_focus());
        assert!(super::close_requested());
    }

    struct Wide(u32);
    impl super::Embedder for Wide {
        fn reserved_right(&self, _inner: u32, _scale: f64) -> u32 {
            self.0
        }
        fn editor_has_keys(&self) -> bool {
            false
        }
    }

    #[test]
    fn the_reserved_strip_never_leaves_the_editor_less_than_nothing() {
        super::register_for_test(std::rc::Rc::new(Wide(5000)));
        // 2480 px wide, 40 px of Neovide's own padding: at most 2440 px can be reserved.
        assert_eq!(super::reserved_right(2480, 40, 2.0), 2440);
        assert!(super::pointer_in_reserved(2479.0));
        assert!(super::absorb_window_focus());
        super::unregister_for_test();
    }

    #[test]
    fn the_pointer_is_in_the_strip_only_right_of_its_left_edge() {
        super::register_for_test(std::rc::Rc::new(Wide(880)));
        let _ = super::reserved_right(2480, 0, 2.0); // records the last layout: strip starts at 1600 px
        assert!(!super::pointer_in_reserved(1599.0));
        assert!(super::pointer_in_reserved(1600.0));
        super::unregister_for_test();
    }

    #[test]
    fn a_window_that_shrank_below_the_strip_keeps_a_valid_layout() {
        super::register_for_test(std::rc::Rc::new(Wide(880)));
        // 800 px wide with 10 px of Neovide's own padding: the strip takes 790 px, never the 880 asked for.
        assert_eq!(super::reserved_right(800, 10, 1.0), 790);
        assert_eq!(super::reserved_right(0, 0, 1.0), 0);
        assert!(!super::pointer_in_reserved(0.0));
        super::unregister_for_test();
    }

    /// `run` repeats upstream's startup (`src/main.rs`'s `main`, `preflight` and `setup`) without the handoff, the
    /// disown and the user's own command line. When upstream changes `main.rs`, compare `run` with it, carry over
    /// what applies, and set this hash to the new file's.
    #[test]
    fn run_was_compared_with_this_main_rs() {
        const MAIN_RS_FNV1A64: u64 = 0x6990_8412_bf50_495d;
        let source = include_str!("main.rs");
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in source.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        assert_eq!(
            hash, MAIN_RS_FNV1A64,
            "src/main.rs changed (hash {hash:#018x}): compare embed::run with it"
        );
    }
}
