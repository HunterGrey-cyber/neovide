//! A [`crate::demo_harness::DemoHarness`]-shaped harness backed by a **real** `nvim --embed`
//! connection, for an external host (e.g. a GTK4 `GtkGLArea`) to embed frame-by-frame.
//!
//! This exists for the neovibe P2 feasibility phase: P1's [`crate::demo_harness::DemoHarness`]
//! proved a bare [`renderer::Renderer`] can be driven with fabricated content and no winit window;
//! this module proves the same renderer can instead be driven by a *live* Neovim session — still
//! with no winit-owned `Window`, no `window::application::Application`, and no
//! `window::window_wrapper::WinitWindowWrapper` anywhere in the picture — while a headless
//! `winit::event_loop::EventLoop<window::EventPayload>` (never `run_app`, never a `Window`
//! created on it) delivers that session's async redraw traffic to the render thread.
//!
//! It reuses exactly the real plumbing the baseline P2 phase traced and
//! `examples/embedded_nvim_smoke.rs` proved end-to-end:
//! - [`bridge::NeovimRuntime::launch`] to spawn/attach the real `nvim --embed` child (internally
//!   this also spawns and owns the real `editor::Editor`, on its own tokio task, that turns
//!   nvim's redraw notifications into [`renderer::DrawCommand`] batches — this module never
//!   constructs an `Editor` itself, it only ever sees the batches that side already produced,
//!   arriving as `UserEvent::DrawCommandBatch`);
//! - [`winit::platform::pump_events::EventLoopExtPumpEvents::pump_app_events`] to pump that event
//!   loop non-blockingly once per frame (see [`LiveHarness::pump`]) — safe to call from inside a
//!   shared GLib main loop per the sibling `poc/pump_events_spike` finding this phase's task
//!   background cites, as long as nothing on that thread blocks synchronously for long;
//! - a bare [`renderer::Renderer::handle_draw_commands`] to apply each arriving batch — the exact
//!   seam `window::window_wrapper::WinitWindowWrapper`'s own pre-window-creation `RouteCore` path
//!   already uses internally for real sessions, and the one `DemoHarness` already uses for
//!   fabricated content;
//! - [`bridge::send_ui`] + [`bridge::SerialCommand::Keyboard`] to forward keyboard input, and
//!   [`bridge::ParallelCommand::Quit`] + [`crate::bridge::NeovimRuntime::shutdown_timeout`] for
//!   shutdown — both confirmed end-to-end by the baseline phase and by
//!   `examples/embedded_nvim_smoke.rs`, including that baseline's concretely-reproduced orphaned-
//!   process failure mode and its fix (forcing `WindowSettings::confirm_quit` to `false` — see
//!   [`LiveHarness::with_options`]'s doc).
//!
//! One thing this module does that the baseline's own smoke test deliberately sidestepped: it
//! reproduces `window_wrapper.rs`'s private `flush_startup_messages_if_ready` (see the free
//! function of the same name below) instead of disabling `startup_message_capture` outright, so a
//! real embedding host's actual nvim config (start-screen plugins included) works without extra
//! flags. See that function's own doc for exactly why this is a small, self-contained duplication
//! rather than a change to `window_wrapper.rs` (which stays completely untouched, per this phase's
//! scope discipline — as does everything else in `bridge`/`window::application`). A host whose
//! own config externalises the cmdline, and which therefore loses a screen row to the capture's
//! restore step, can turn the capture off through
//! [`LiveHarnessOptions::startup_message_capture`] — that field's doc has the measurement and the
//! cost.
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::Parser;
use skia_safe::Canvas;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    platform::pump_events::EventLoopExtPumpEvents,
    window::WindowId,
};

use crate::{
    bridge::{NeovimHandler, NeovimRuntime, OpenMode, ParallelCommand, SerialCommand, send_ui},
    clipboard::{Clipboard, ClipboardHandle},
    cmd_line::CmdLineSettings,
    renderer::{
        DrawCommandResult, Renderer, RendererSettings, StartupMessageFlush,
        cursor_renderer::CursorSettings, progress_bar::ProgressBarSettings,
        rendered_window::BASE_GRID_ID,
    },
    running_tracker::RunningTracker,
    settings::{Config, Settings, clamped_grid_size},
    units::{GridRect, GridScale, GridSize, PixelPos, PixelRect},
    window::{EventPayload, EventTarget, RouteId, UserEvent, WindowSettings, create_event_loop},
};

/// [`LiveHarness::render_frame`]'s internal pump never blocks: it only ever drains whatever is
/// already queued. A host driving this once per paint (e.g. a `GtkGLArea` inside a shared GLib
/// main loop) needs exactly that — nothing on that thread may block synchronously for long.
const NON_BLOCKING: Duration = Duration::ZERO;

/// How long [`LiveHarness::shutdown`] waits for a real `UserEvent::NeovimExited` after asking
/// nvim to quit, before giving up and force-tearing-down the tokio runtime anyway. See that
/// method's own doc for what "giving up" means in practice.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// Configuration for [`LiveHarness::with_options`]. [`LiveHarness::new`] is shorthand for
/// `LiveHarness::with_options(LiveHarnessOptions { os_scale_factor, ..Default::default() })`.
pub struct LiveHarnessOptions {
    /// Forwarded unchanged to [`Renderer::new`] — the host's own display scale factor (`1.0` if
    /// unknown/not applicable), exactly like [`crate::demo_harness::DemoHarness::new`]'s own
    /// parameter of the same name.
    pub os_scale_factor: f64,
    /// Initial `nvim_ui_attach` grid size. `None` defers to Neovide's own default
    /// (`settings::DEFAULT_GRID_SIZE`, 100x50 at the time of writing). Purely a launch-time
    /// value — call [`LiveHarness::resize_grid`] any time afterward to change it. (Earlier,
    /// this doc said no such method existed and whatever was chosen here was the grid size for
    /// the harness's entire lifetime; that was true until the P2 "frozen scroll" bug — a host
    /// resize never reaching nvim's own idea of the grid — made it clear a live resize path was
    /// needed. See `resize_grid`'s own doc for the mechanism and why a host should still pick a
    /// sensible starting value here rather than relying on the default-then-immediately-resize
    /// pattern for every construction.)
    pub grid_size: Option<GridSize<u32>>,
    /// Working directory for the spawned `nvim --embed` child. `None` inherits this process's own
    /// cwd — the same meaning `None` has for [`bridge::NeovimRuntime::launch`]'s own `cwd`
    /// parameter.
    pub cwd: Option<std::path::PathBuf>,
    /// Extra arguments passed straight through to the `nvim` binary itself (after nvim's own
    /// `--`), e.g. `vec!["--clean".to_string()]` for a deterministic session with no user
    /// plugins/config loaded — the same passthrough `examples/embedded_nvim_smoke.rs` used.
    /// Empty by default: a real embedding host gets the person's actual nvim config, startup
    /// messages and all (see this module's own doc on why that is safe to do here).
    pub extra_nvim_args: Vec<String>,
    /// Extra `(name, value)` environment variables set on the spawned `nvim --embed` child
    /// process **only** — never on the host process that constructs this harness. Applied on top
    /// of the environment the child inherits, so a name that already exists in the host's own
    /// environment is replaced (a host prepending to `PATH` must compose the whole value itself).
    ///
    /// This exists because an embedding host may need the embedded nvim to believe something
    /// about its environment that must not be true of the host: neovibe's `shell` sets `TMUX`,
    /// `TMUX_PANE` and a `PATH` carrying a fake `tmux` shim, so `vim-tmux-navigator` running
    /// inside the embedded nvim forwards a `Ctrl-h`/`Ctrl-l` that hit nvim's own window boundary
    /// out to the host as a pane-switch request — while the host process itself, and every other
    /// subprocess it spawns, keep a completely untouched environment.
    ///
    /// Empty by default. Forwarded to [`crate::cmd_line::CmdLineSettings::child_env`]; see that
    /// field's doc for why this is a per-child channel rather than `std::env::set_var`.
    pub child_env: Vec<(String, String)>,
    /// Whether to run Neovide's startup-message capture — the same thing
    /// [`crate::cmd_line::CmdLineSettings::startup_message_capture`] (`--startup-message-capture`
    /// / `--no-startup-message-capture`) controls for the real binary. **`true` by default, so a
    /// host that does not mention this field gets exactly the behaviour this harness has always
    /// had.**
    ///
    /// What the capture does, in order (`bridge::launch` and
    /// `bridge::ui_commands::restore_builtin_message_ui`): read `cmdheight` *before* the
    /// `nvim_ui_attach` that triggers loading the user's config, attach with `ext_messages` on so
    /// that anything printed before the first grid render arrives as a `msg_show` this side can
    /// hold rather than as a screenful behind a hit-enter prompt, then — on the first flush —
    /// turn `ext_messages`/`ext_cmdline` back off, write that pre-attach `cmdheight` back, and
    /// replay the held messages. See <https://github.com/neovide/neovide/issues/3499>.
    ///
    /// Why an embedding host may want it **off**: that last step hands the built-in cmdline row
    /// back unconditionally, and for a config that externalises the cmdline itself (noice.nvim
    /// and friends, through `vim.ui_attach`) nothing ever paints there again. The pre-attach
    /// `cmdheight` is the stock `1` — nvim has not read the user's config at that point — so the
    /// restore writes `1` over the `0` such a config chose, and the bottom row of the host's pane
    /// is dead for the rest of the session. Measured at the nvim protocol level on a 36-row grid:
    /// with the capture on, nvim lays out 34 window rows + 1 global statusline and never uses the
    /// 36th; with it off, 35 window rows + 1 statusline fill the grid exactly.
    ///
    /// What opting out costs, measured rather than assumed (`nvim --embed` attached with the
    /// exact options `bridge` uses, against a config that `error()`s while loading):
    /// `ext_messages` is **not** attached at all — it is set only inside the same
    /// `if capture_startup_messages` that reads the pre-attach `cmdheight` — so nvim keeps its
    /// own message UI, and a startup error is painted onto the built-in message grid
    /// (`msg_set_pos`, which this codebase already renders) instead of being captured and
    /// replayed. Such an error is therefore still shown; what is lost is that it can land in a
    /// scrolled message area the user has to dismiss, which is the ergonomic the capture exists
    /// to avoid. A host whose config externalises messages never sees that prompt anyway, because
    /// its own handler takes the message first.
    pub startup_message_capture: bool,
}

impl Default for LiveHarnessOptions {
    fn default() -> Self {
        Self {
            os_scale_factor: 1.0,
            grid_size: None,
            cwd: None,
            extra_nvim_args: Vec::new(),
            child_env: Vec::new(),
            startup_message_capture: true,
        }
    }
}

/// The [`ApplicationHandler`] side of [`LiveHarness`]: the whole "turn an arriving `EventPayload`
/// into renderer state" seam, reproduced from what `WinitWindowWrapper`/`Application` do for a
/// real window today (see this module's doc for exactly what is, and isn't, reproduced).
struct RouteEventHandler {
    renderer: Renderer,
    neovim_handler: NeovimHandler,
    route_id: RouteId,
    /// Set once a [`DrawCommandResult::should_show`] is observed — the same signal real Neovide
    /// uses to decide it's safe to first reveal its window
    /// (`WinitWindowWrapper::handle_draw_commands`'s `UIState::Initing -> FirstFrame`
    /// transition). Monotonic: once true, stays true, mirroring `RouteCore::should_show_observed`.
    is_ready: bool,
    /// Count of `DrawCommandBatch` events applied so far — a cheap "did anything new happen"
    /// signal for a host that wants one without an async round-trip into nvim.
    redraw_batches_seen: u64,
    neovim_exited: bool,
}

impl ApplicationHandler<EventPayload> for RouteEventHandler {
    // Never called: this harness never creates a winit `Window` for the event loop to resume/
    // deliver window events to — an external host owns its own GL surface instead, exactly like
    // `DemoHarness`'s caller does. Required only to satisfy the trait.
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: EventPayload) {
        let EventPayload { payload, target } = event;
        match payload {
            UserEvent::DrawCommandBatch(batch) => {
                if !matches!(target, EventTarget::Route(route_id) if route_id == self.route_id) {
                    return;
                }
                let mut result = self.renderer.handle_draw_commands(batch);
                flush_startup_messages_if_ready(&mut result, &self.neovim_handler);
                self.is_ready |= result.should_show;
                self.redraw_batches_seen += 1;
            }
            UserEvent::NeovimExited => self.neovim_exited = true,
            UserEvent::NeovimLaunchError { message } => {
                // Confirmed (by reading every emitter of this variant) to not currently be
                // reachable via this harness's own construction path: the only place in the
                // codebase that ever sends it is `WinitWindowWrapper::request_window_creation`'s
                // deferred-launch retry logic, which `LiveHarness` never calls — it calls
                // `NeovimRuntime::launch` directly and surfaces that call's own real launch
                // failures (missing/too-old nvim, tokio runtime build failure, ...) synchronously
                // as an `Err` from `LiveHarness::new`/`with_options` instead. Logged (not silently
                // dropped) in case that ever changes rather than exposed as public API for a path
                // that provably never fires today — see this phase's report for the parallel to
                // the baseline report's own §b finding (a real, currently-dead match arm).
                log::warn!("[LiveHarness] unexpected UserEvent::NeovimLaunchError: {message}");
            }
            _ => {}
        }
    }
}

/// Reproduces `window::window_wrapper`'s private `flush_startup_messages_if_ready` free function
/// (see the top of `src/window/window_wrapper.rs`) rather than silently sidestepping it via
/// `--no-startup-message-capture` the way `examples/embedded_nvim_smoke.rs` did. That baseline
/// report's own judgment call #3 flagged this as a decision the next phase (this one) should make
/// deliberately: a real embedding host presumably wants a person's actual config, including
/// plugins that print startup messages, and without this those messages/the cmdline stay
/// externalized (`ext_messages`) forever with nothing on our side to un-externalize them.
///
/// Every type this touches — [`DrawCommandResult`], [`StartupMessageFlush`], [`ParallelCommand`],
/// [`send_ui`] — is already `pub`, so this is a small, self-contained duplication of already-
/// public-surface logic, not a change to `window_wrapper.rs` itself, which stays untouched.
///
/// Note that this function still runs when a host sets
/// [`LiveHarnessOptions::startup_message_capture`] to `false`, and that is correct rather than an
/// oversight: `editor::Editor::set_ui_ready` arms the flush on the first real grid event whether
/// or not the capture was ever enabled, so `DrawCommand::UIReady` — the same command that carries
/// `should_show`, which is how a host learns it is safe to reveal its surface — always arrives
/// paired with a `StartupMessageFlush::RestoreMessageUi`. With the capture off that flush is a
/// no-op round trip (nothing was externalized to restore, no messages were held to replay, and
/// `restore_builtin_message_ui` writes `cmdheight` back to the value it just read), which is
/// exactly what the real binary does under `--no-startup-message-capture`. Suppressing it here
/// would suppress `should_show` with it.
fn flush_startup_messages_if_ready(result: &mut DrawCommandResult, neovim_handler: &NeovimHandler) {
    let Some(flush) = result.startup_message_flush.take() else {
        return;
    };
    let messages = std::mem::take(&mut result.startup_messages);
    let command = match flush {
        StartupMessageFlush::Replay if messages.is_empty() => None,
        StartupMessageFlush::Replay => Some(ParallelCommand::ReplayStartupMessages { messages }),
        StartupMessageFlush::RestoreMessageUi => {
            Some(ParallelCommand::FlushStartupMessages { messages })
        }
    };
    if let Some(command) = command {
        send_ui(command, neovim_handler);
    }
}

/// Drives a real `nvim --embed` connection into a bare [`Renderer`], for an external host (e.g. a
/// GTK4 `GtkGLArea`) to embed frame-by-frame — the P2 counterpart of
/// [`crate::demo_harness::DemoHarness`]'s fabricated content. See this module's own doc for the
/// full picture of what real plumbing this reuses and what it deliberately does not reproduce
/// (full keyboard/IME translation, `nvim_ui_resize`, window/geometry lifecycle — all later
/// neovibe phases' jobs, not this one's).
///
/// Construct with [`LiveHarness::new`] (or [`LiveHarness::with_options`] for more control), then
/// each frame call [`LiveHarness::render_frame`] (which pumps internally — see its own doc) and
/// forward any input via [`LiveHarness::send_text_input`]. Call [`LiveHarness::resize_grid`]
/// whenever the host's own visible viewport size changes cell-count — see that method's own doc
/// for why this exists and what breaks without it. Call [`LiveHarness::shutdown`]
/// explicitly when the host is done with it (e.g. on window close) and read its return value;
/// dropping without calling it first falls back to the same sequence from `Drop`, but see
/// `shutdown`'s own doc for why that fallback is the less defensible choice.
pub struct LiveHarness {
    event_loop: EventLoop<EventPayload>,
    state: RouteEventHandler,
    neovim_handler: NeovimHandler,
    runtime: NeovimRuntime,
    // `Some` until `shutdown` drops it (while the event loop is still alive — see `shutdown`'s own
    // comment on why that ordering matters for Wayland).
    clipboard: Option<Arc<Mutex<Clipboard>>>,
    shut_down: bool,
    /// The last focus state [`set_focused`](Self::set_focused) passed on, so a host that reports
    /// focus on every event does not re-fire nvim's `FocusGained`/`FocusLost` autocmds each time.
    /// `None` until the first call, so the host's first report always goes through.
    last_focus: Option<bool>,
    /// The `g:neovide_scale_factor` the renderer was last synced to (neovibe). `None` until the
    /// first frame, so whatever `init.lua` set before `ui_attach` is applied on that frame rather
    /// than never. See [`apply_scale_factor_setting`](Self::apply_scale_factor_setting).
    last_user_scale_factor: Option<f32>,
    /// Diagnostics (neovibe): how many times
    /// [`apply_scale_factor_setting`](Self::apply_scale_factor_setting) has actually resynced the
    /// renderer, i.e. how many times [`scale_factor_changed`] returned `true`. Exposed via
    /// [`scale_factor_resyncs`](Self::scale_factor_resyncs) so a test (or a host) can tell "the
    /// change gate fired N times" apart from "the renderer resyncs on every frame regardless" --
    /// the latter is exactly the P11-class idle-cost regression removing the gate in
    /// [`render_frame`](Self::render_frame) would reintroduce.
    scale_factor_resyncs: u64,
    /// Set by [`set_os_scale_factor`](Self::set_os_scale_factor) when it actually changes the
    /// renderer's OS scale, and taken (reset to `false`) by the next
    /// [`render_frame`](Self::render_frame) call, which folds it into that frame's forced-redraw
    /// decision (neovibe, v1 P2). Mirrors [`last_user_scale_factor`]'s role for the *user*-scale
    /// side of the same product (`os_scale_factor * user_scale_factor`,
    /// `Renderer::update_scale_factor`) -- kept as a separate flag rather than reusing
    /// `last_user_scale_factor`'s comparison because the two vary independently and a host may
    /// call `set_os_scale_factor` on a frame where nvim's own `g:neovide_scale_factor` did not
    /// move at all.
    os_scale_redraw_pending: bool,
    /// Diagnostics (neovibe, v1 P2): how many times
    /// [`set_os_scale_factor`](Self::set_os_scale_factor) has actually changed the renderer's OS
    /// scale since construction. Exposed via
    /// [`os_scale_factor_resyncs`](Self::os_scale_factor_resyncs) for the same reason
    /// [`scale_factor_resyncs`] is: a test (or a host) can hold this to "does not move across N
    /// idle frames" to prove the change gate, not just the absence of a crash, is still there.
    os_scale_factor_resyncs: u64,
    /// The same `Settings` nvim's `setting_changed` notifications update, kept so a host can read
    /// a `g:neovide_*` value it acts on itself ([`fullscreen_setting`](Self::fullscreen_setting)).
    settings: Arc<Settings>,
}

/// Whether the renderer needs re-syncing to `g:neovide_scale_factor` (neovibe). Split out of
/// [`LiveHarness::apply_scale_factor_setting`] for the same reason [`focus_changed`] is: the
/// harness itself needs a live nvim to construct, and this is the one decision worth a test.
/// Exact comparison is deliberate -- the value comes from nvim verbatim, so an unchanged
/// variable compares equal bit for bit, and any assignment at all is worth one resync.
fn scale_factor_changed(last: Option<f32>, current: f32) -> bool {
    last != Some(current)
}

/// Whether a focus report is a change worth sending on. Split out of
/// [`LiveHarness::set_focused`] because the harness itself needs a live nvim to construct.
fn focus_changed(last: Option<bool>, now: bool) -> bool {
    last != Some(now)
}

impl LiveHarness {
    /// `LiveHarness::with_options(LiveHarnessOptions { os_scale_factor, ..Default::default() })`
    /// — the common case: a real ambient nvim config, Neovide's own default grid size, this
    /// process's own cwd. See [`LiveHarnessOptions`] for what to override and why.
    pub fn new(os_scale_factor: f64) -> Result<Self> {
        Self::with_options(LiveHarnessOptions { os_scale_factor, ..Default::default() })
    }

    /// Builds a headless `winit::event_loop::EventLoop<EventPayload>` (via
    /// [`crate::window::create_event_loop`] — the exact function the real `neovide` binary calls
    /// — no window ever created on it), a real [`ClipboardHandle`] wired through it, and a real
    /// `nvim --embed` connection via [`bridge::NeovimRuntime::launch`] with `OpenMode::None`
    /// ("launch a blank embedded instance" — the same semantically-minimal choice the baseline
    /// report confirmed), then a bare [`Renderer`] to receive that session's redraw traffic.
    ///
    /// Registers the same `SettingGroup`s [`crate::demo_harness::DemoHarness::new`] and the
    /// baseline's own smoke test register (`Renderer::new`/`NeovimRuntime::launch` both panic via
    /// `Settings::get` on an unregistered/unset type otherwise), with one deliberate override:
    /// `WindowSettings::confirm_quit` is forced to `false` unconditionally. The baseline report
    /// found, and reproduced concretely (a real orphaned `nvim --embed` process, reparented to
    /// init, confirmed via `ps`), that leaving this at its `true` default makes nvim's own quit
    /// path run `:confirm qa` instead of `:qa!` — which blocks forever on an interactive save
    /// prompt nothing here ever answers, and this codebase has **no force-kill fallback** for a
    /// child stuck like that (see [`LiveHarness::shutdown`]'s own doc for the full detail, still
    /// true here). Forcing this override is what makes that fallback-free shutdown path actually
    /// reliable rather than merely hoped-for — empirically verified for this module the same way
    /// the baseline verified it for its own example: see `examples/live_harness_offscreen.rs` and
    /// this phase's report.
    pub fn with_options(options: LiveHarnessOptions) -> Result<Self> {
        let LiveHarnessOptions {
            os_scale_factor,
            grid_size,
            cwd,
            extra_nvim_args,
            child_env,
            startup_message_capture,
        } = options;

        let event_loop = create_event_loop();
        let proxy = event_loop.create_proxy();

        let settings = Arc::new(Settings::new());
        settings.register::<WindowSettings>();
        settings.register::<RendererSettings>();
        settings.register::<CursorSettings>();
        settings.register::<ProgressBarSettings>();

        settings.set(&WindowSettings { confirm_quit: false, ..Default::default() });

        // Mirrors the two established patterns already in this codebase rather than inventing a
        // third: `CmdLineSettings::default()` (== `Self::parse_from(iter::empty::<String>())`,
        // used identically by `DemoHarness::new`) when there is nothing to pass through, or the
        // same `argv0, "--", ...` passthrough shape `examples/embedded_nvim_smoke.rs` used when
        // there is. Deliberately does *not* pass `--no-startup-message-capture`: the capture is on
        // by default here as it is for the real binary, and a host that wants it off says so
        // through `LiveHarnessOptions::startup_message_capture` (assigned below) rather than by
        // this module deciding for every host — see this module's own doc on
        // `flush_startup_messages_if_ready` and that field's own doc.
        let mut cmdline_settings = if extra_nvim_args.is_empty() {
            CmdLineSettings::default()
        } else {
            let mut argv = vec!["neovide".to_string(), "--".to_string()];
            argv.extend(extra_nvim_args);
            CmdLineSettings::parse_from(argv)
        };
        // `child_env` has no argv spelling by design (`#[arg(skip)]`), so it is assigned here
        // rather than folded into the passthrough above. This `Settings` instance belongs to this
        // one `LiveHarness`, so the injection can never reach another harness, the host process,
        // or any other subprocess the host spawns.
        cmdline_settings.child_env = child_env;
        // Same reasoning, one layer up: `--no-startup-message-capture` *does* have an argv
        // spelling, but folding it into the passthrough above would mean synthesizing a flag
        // string for a boolean the caller already handed us, and would only work on the branch
        // that has extra nvim args to pass. Assigned directly instead, against the same
        // per-`LiveHarness` `Settings` instance `child_env` uses, so a host opting out here can
        // never affect another harness or the host process. `true` (the `LiveHarnessOptions`
        // default) leaves this exactly as `CmdLineSettings::default()` already had it.
        cmdline_settings.startup_message_capture = startup_message_capture;
        settings.set(&cmdline_settings);

        // Needs the `EventLoop` for the Wayland/X11 display handle even though no window exists —
        // confirmed by the baseline report and `Clipboard::new`'s own signature.
        let clipboard = Clipboard::new(&event_loop);
        let clipboard_handle = ClipboardHandle::new(&clipboard);

        let mut runtime = NeovimRuntime::new(clipboard_handle)
            .context("failed to build the tokio runtime backing NeovimRuntime")?;

        let route_id = RouteId::next();
        let config = Config::default();
        let running_tracker = RunningTracker::new();

        let neovim_handler = runtime
            .launch(
                route_id,
                proxy,
                grid_size,
                running_tracker,
                settings.clone(),
                &config,
                cwd.as_deref(),
                OpenMode::None,
            )
            .context("NeovimRuntime::launch failed — is `nvim` (>= 0.10) on $PATH?")?;

        let renderer = Renderer::new(os_scale_factor, config, settings.clone());

        Ok(LiveHarness {
            event_loop,
            state: RouteEventHandler {
                renderer,
                neovim_handler: neovim_handler.clone(),
                route_id,
                is_ready: false,
                redraw_batches_seen: 0,
                neovim_exited: false,
            },
            neovim_handler,
            runtime,
            clipboard: Some(clipboard),
            shut_down: false,
            last_focus: None,
            last_user_scale_factor: None,
            scale_factor_resyncs: 0,
            os_scale_redraw_pending: false,
            os_scale_factor_resyncs: 0,
            settings,
        })
    }

    /// Drains whatever `EventPayload`s (redraw batches, `NeovimExited`, ...) are already queued on
    /// the headless event loop, blocking for at most `timeout` waiting for more if none are
    /// immediately available. [`render_frame`](Self::render_frame) already calls this internally
    /// with a zero timeout (never blocks) every time it's called — call this directly yourself
    /// only if the host wants to drain events on a different cadence than it repaints (e.g. an
    /// idle callback that runs more often than paints, for lower input-round-trip latency).
    /// Pumping redundantly is harmless.
    pub fn pump(&mut self, timeout: Duration) {
        self.event_loop.pump_app_events(Some(timeout), &mut self.state);
    }

    /// Advances animation state and paints one frame into `canvas` — the exact per-frame sequence
    /// `WinitWindowWrapper` runs (`prepare_frame`, `animate_frame`, `prepare_lines`, `draw_frame`)
    /// folded into one call, matching [`crate::demo_harness::DemoHarness::render_frame`]'s own
    /// signature and doc exactly:
    /// - `content_region`, when given, is the pixel rect within `canvas` this harness owns;
    ///   drawing is clipped to it and the root grid is positioned to start at its top-left corner.
    ///   `None` means "own the whole canvas".
    /// - `dt` is the elapsed time in seconds since the previous call.
    ///
    /// Unlike `DemoHarness` (whose grid is a fixed compile-time constant), the fallback grid rect
    /// used when `content_region` is `None` is read from the real, live
    /// [`Renderer::get_grid_size`] — the actual `nvim_ui_attach` grid size in effect, once redraw
    /// traffic has established one.
    ///
    /// Calls [`pump`](Self::pump) with a zero timeout first, so any redraw traffic that arrived
    /// since the last call is applied before this frame paints. Returns `true` while ongoing
    /// position/scroll/cursor animation would like another `render_frame` call soon — advisory
    /// only, exactly like `DemoHarness::render_frame`'s own return value.
    pub fn render_frame(
        &mut self,
        canvas: &Canvas,
        content_region: Option<&PixelRect<f32>>,
        dt: f32,
    ) -> bool {
        self.pump(NON_BLOCKING);
        // Non-short-circuit `|` (not `||`): both sides must run every frame regardless of the
        // other's result. `apply_scale_factor_setting`'s own resync counter (P11's idle-cost
        // signal) must keep incrementing on its own terms even on a frame an OS-scale change also
        // forces, and conversely `std::mem::take` must always run to actually clear the pending
        // flag -- short-circuiting either would silently stop counting or leave a stale `true`
        // pending forever once the other side is also `true` on the same frame (neovibe, v1 P2).
        let scale_factor_changed =
            self.apply_scale_factor_setting() | std::mem::take(&mut self.os_scale_redraw_pending);

        let renderer = &mut self.state.renderer;
        renderer.prepare_frame();

        let grid_scale = renderer.grid_renderer.grid_scale;
        let grid_rect = content_region.map(|region| *region / grid_scale).unwrap_or_else(|| {
            let grid_size = renderer.get_grid_size();
            GridRect::from_min_max((0.0, 0.0), (grid_size.width as f32, grid_size.height as f32))
        });

        let animating = renderer.animate_frame(&grid_rect, dt);
        // Standalone forces this same `prepare_lines(true)` on the frame `font_changed_last_frame`
        // is set (`window_wrapper.rs`'s `prepare_frame`), and a scale change needs the same force
        // here for the same reason: `prepare_lines(false)` only re-records a line whose picture is
        // already invalid (a fresh `RenderedLine`, moved content, a moved boxchar) — it treats a
        // still-`is_valid` line as nothing to do, and a scale change invalidates none of that, it
        // just changes the cell size the *next* recording would use. Without forcing this, a line
        // keeps its old-size glyph picture until nvim's own async resize round-trip marks it
        // invalid some other way — and if the zoom step leaves the integer grid size unchanged (a
        // plausible, even common, `Ctrl+=` step), no resize is ever sent, and the stale glyphs
        // never get redrawn at all. An OS-scale change (`set_os_scale_factor`, neovibe v1 P2)
        // leaves the integer grid unchanged by construction too -- the logical widget size did not
        // move, only the cell size did -- so it needs exactly the same forced redraw, and
        // `os_scale_redraw_pending` folds into this same flag for that reason.
        renderer.prepare_lines(scale_factor_changed);
        renderer.draw_frame(canvas, content_region, dt);
        animating
    }

    /// Forwards `text` toward the real nvim connection as one `nvim.input(...)` call — the exact
    /// mechanism `WinitWindowWrapper`'s keyboard handling uses
    /// ([`bridge::send_ui`] + [`bridge::SerialCommand::Keyboard`]), confirmed end-to-end by the
    /// baseline report and `examples/embedded_nvim_smoke.rs`. Plain UTF-8 text, including nvim's
    /// own `<key>` notation (e.g. `"ihello world<Esc>"` enters insert mode, types text, then
    /// returns to normal mode) — full GTK-keyevent-to-Neovim-keycode translation is a later
    /// neovibe input-system phase's job, not this one's.
    pub fn send_text_input(&mut self, text: &str) {
        send_ui(SerialCommand::Keyboard(text.to_string()), &self.neovim_handler);
    }

    /// Forwards a mouse button press/release toward the real nvim connection as one
    /// `nvim.input_mouse(...)` call — the exact mechanism
    /// `window::mouse_manager::MouseManager::send_nvim_mouse_button` uses
    /// ([`bridge::send_ui`] + [`bridge::SerialCommand::MouseButton`]), just without that struct's
    /// own per-window hit-testing (see below).
    ///
    /// `button` is nvim's own button-text notation — `"left"`/`"right"`/`"middle"`/`"x1"`/`"x2"`
    /// (see `window::mouse_manager::mouse_button_to_button_text` for the mapping this mirrors);
    /// any other string is passed straight through to nvim, which will reject it. `pressed`
    /// selects `"press"` vs `"release"`. `grid_pos` is the (col, row) grid cell the event
    /// happened at, in nvim's own row/col convention — the host is expected to have already
    /// converted its own pixel coordinates via [`grid_scale`](Self::grid_scale) the same way it
    /// already does for [`resize_grid`](Self::resize_grid)'s own grid-size math, then clamped
    /// against [`get_grid_size`](Self::get_grid_size). `modifier_string` is nvim's own
    /// modifier-prefix notation (e.g. `"C-S-"`, or `""` for none) — a caller with no modifier
    /// tracker of its own can pass `""`, exactly like [`send_text_input`](Self::send_text_input)'s
    /// own callers may already do for plain keystrokes.
    ///
    /// Always targets the single base grid ([`BASE_GRID_ID`], nvim's always-present grid 1) —
    /// unlike the reference `MouseManager`, this harness does not track per-window pixel regions
    /// the way a real multi-split Neovide window does
    /// (`window::mouse_manager::MouseManager::get_window_details_under_mouse`), so this method has
    /// no way to route a click to whichever split/floating window it visually landed on. That
    /// matches every host of this harness built so far (a single undivided editor pane, per
    /// `poc/neovide_embed_live`'s own scope) — a later phase that adds split-aware layout tracking
    /// would need to extend this rather than call it as-is.
    pub fn send_mouse_button(
        &mut self,
        button: &str,
        pressed: bool,
        grid_pos: (u32, u32),
        modifier_string: &str,
    ) {
        send_ui(
            SerialCommand::MouseButton {
                button: button.to_string(),
                action: if pressed { "press".to_string() } else { "release".to_string() },
                grid_id: BASE_GRID_ID,
                position: grid_pos,
                modifier_string: modifier_string.to_string(),
            },
            &self.neovim_handler,
        );
    }

    /// Forwards a mouse-moved-while-a-button-is-held event, via [`bridge::SerialCommand::Drag`]
    /// — the same RPC `window::mouse_manager::MouseManager::handle_pointer_motion` sends once
    /// per grid-cell change while `drag_details` is `Some`. Unlike that reference (which tracks
    /// the drag's own previous grid cell internally and only calls `send_ui` when it actually
    /// changed), this method sends unconditionally on every call — callers should dedupe on
    /// `grid_pos` actually changing themselves first, exactly like
    /// [`resize_grid`](Self::resize_grid)'s own doc asks resize callers to dedupe on grid-cell
    /// size. See [`send_mouse_button`](Self::send_mouse_button) for what `button`/`grid_pos`/
    /// `modifier_string` mean and the single-base-grid caveat.
    pub fn send_mouse_drag(&mut self, button: &str, grid_pos: (u32, u32), modifier_string: &str) {
        send_ui(
            SerialCommand::Drag {
                button: button.to_string(),
                grid_id: BASE_GRID_ID,
                position: grid_pos,
                modifier_string: modifier_string.to_string(),
            },
            &self.neovim_handler,
        );
    }

    /// Forwards one wheel-scroll "line crossed" event, via [`bridge::SerialCommand::Scroll`] —
    /// the same RPC `window::mouse_manager::MouseManager::handle_line_scroll` sends, once per
    /// call. `direction` is nvim's own scroll-direction notation — `"up"`/`"down"`/`"left"`/
    /// `"right"`. A caller with a fractional/pixel-based scroll delta (a touchpad, or a high-
    /// resolution wheel) should accumulate it and call this once per whole grid-line crossed,
    /// exactly like `handle_line_scroll`/`handle_pixel_scroll` do (accumulate a running total,
    /// compare `floor()` before and after, call this once per integer step crossed) — this
    /// method itself does no accumulation. See [`send_mouse_button`](Self::send_mouse_button) for
    /// what `grid_pos`/`modifier_string` mean and the single-base-grid caveat.
    pub fn send_mouse_scroll(&mut self, direction: &str, grid_pos: (u32, u32), modifier_string: &str) {
        send_ui(
            SerialCommand::Scroll {
                direction: direction.to_string(),
                grid_id: BASE_GRID_ID,
                position: grid_pos,
                modifier_string: modifier_string.to_string(),
            },
            &self.neovim_handler,
        );
    }

    /// The renderer's current font-derived grid scale (pixels per grid cell) — mirrors what
    /// [`render_frame`](Self::render_frame) already reads internally
    /// (`renderer.grid_renderer.grid_scale`) and what
    /// `WinitWindowWrapper::update_grid_size_from_window` reads for a real OS window. A host
    /// needs this to convert a pixel-sized viewport into the (cols, rows) to pass to
    /// [`resize_grid`](Self::resize_grid) — e.g. `pixel_size / harness.grid_scale()` (see
    /// `units::GridScale`'s `Div` impl) gives a `GridSize<f32>` to floor and clamp.
    pub fn grid_scale(&self) -> GridScale {
        self.state.renderer.grid_renderer.grid_scale
    }

    /// The top-left corner of the cursor's current *destination* cell, in the same pixel space the
    /// `content_region` handed to [`render_frame`](Self::render_frame) is expressed in — i.e. the
    /// canvas's own device-pixel space, already including the `content_region.min` offset, so a
    /// host can use it directly against its own surface geometry without re-adding that origin.
    ///
    /// Reads [`Renderer::get_cursor_destination`] (`CursorRenderer::destination`), which
    /// [`render_frame`](Self::render_frame) itself refreshes every frame via
    /// `Renderer::animate_frame` → `CursorRenderer::update_cursor_destination`. That value is
    /// computed as `(cursor_grid_position + window.grid_current_position) * grid_scale`, and
    /// `grid_current_position` is in turn derived from the `grid_rect` (`content_region /
    /// grid_scale`) `render_frame` passes down — which is exactly why the returned position is
    /// already content-region-relative rather than grid-origin-relative.
    ///
    /// Note this is the *destination* (where the cursor is settling), not the smoothed, mid-flight
    /// animated position — deliberately, since the one consumer this exists for is an embedding
    /// host telling the platform input method where to place its candidate window
    /// (`gtk_im_context_set_cursor_location`), which wants the settled cell, not a position that
    /// moves under the popup for the length of a cursor animation. Pair it with
    /// [`grid_scale`](Self::grid_scale) to get the cell's size.
    ///
    /// Added for neovibe's P4 (IME candidate-window placement); a thin read-only geometry getter,
    /// deliberately within the fork's own surface/geometry divergence budget.
    pub fn cursor_pixel_position(&self) -> PixelPos<f32> {
        self.state.renderer.get_cursor_destination()
    }

    /// The live grid size currently in effect, as last established by redraw traffic — the same
    /// value [`render_frame`](Self::render_frame) falls back to when its own `content_region`
    /// argument is `None`.
    pub fn get_grid_size(&self) -> GridSize<u32> {
        self.state.renderer.get_grid_size()
    }

    /// Asks nvim to resize its own UI grid to `grid_size` (clamped via
    /// [`crate::settings::clamped_grid_size`], the same clamp
    /// `WinitWindowWrapper::update_window_size_from_grid`/`update_grid_size_from_window` apply to
    /// their own grid sizes) — callable any time after construction, unlike
    /// [`LiveHarnessOptions::grid_size`], which only ever set nvim's size once at
    /// `nvim_ui_attach` time.
    ///
    /// This is the fix for the neovibe P2 "frozen scroll" bug: before this method existed,
    /// nothing in this module ever revised nvim's own idea of the grid size after launch, so a
    /// host's `content_region` could permanently diverge from what nvim thought the window's
    /// row/col count was — including across every subsequent host-side resize, since neither
    /// `render_frame` nor anything else in this file ever called into nvim's grid on a resize.
    /// Per Neovim's own UI protocol, a cursor motion whose target line stays within nvim's
    /// (stale, host-visible-size-agnostic) row count is legitimately not a scroll at all, so
    /// Neovim correctly never emits `grid_scroll`/`win_viewport` redraw traffic for it — while
    /// cursor-position and gutter redraws (driven independently of whether a scroll occurred)
    /// keep updating on every cursor move regardless. That combination is exactly the reported
    /// symptom: the line-number gutter tracks the cursor correctly while the buffer text itself
    /// stays visually frozen on whatever screenful was showing when the fixed grid size was
    /// established. Calling this method whenever the host's real viewport's cell-count changes —
    /// including once right after construction, since [`LiveHarnessOptions::grid_size`]'s launch
    /// value may not match the host's actual initial content region either — keeps nvim's own
    /// grid state honest, which is what lets it decide correctly (and start emitting real
    /// `grid_scroll` traffic) once a motion actually needs to move the viewport.
    ///
    /// Uses the exact same RPC `WinitWindowWrapper::update_grid_size_from_window` uses
    /// (`ParallelCommand::Resize` → `nvim.ui_try_resize`), just without that struct's own
    /// per-route `last_synced_grid_size` bookkeeping: this method sends the RPC unconditionally
    /// on every call, so a caller that resizes on every pixel-level event (e.g. a live window
    /// drag) should dedupe on the resulting *grid-cell* size itself first — comparing against
    /// [`get_grid_size`](Self::get_grid_size) or a value cached from a previous call — to avoid
    /// spamming nvim with redundant resize RPCs mid-drag, exactly like
    /// `poc/neovide_embed_live`'s own resize handler does.
    pub fn resize_grid(&mut self, grid_size: GridSize<u32>) {
        let grid_size = clamped_grid_size(&grid_size);
        send_ui(
            ParallelCommand::Resize { width: grid_size.width.into(), height: grid_size.height.into() },
            &self.neovim_handler,
        );
    }

    /// Tells Neovide that the host's keyboard focus moved onto (`true`) or off (`false`) this
    /// surface. It is the embedded equivalent of the winit `WindowEvent::Focused` a real Neovide
    /// window gets, and it reaches both halves that a real window's focus change reaches:
    ///
    /// - the renderer, whose cursor renderer draws a `Block` cursor as a hollow outline while
    ///   unfocused (`cursor_renderer::CursorRenderer::draw`, `unfocused_outline_width`), or no
    ///   cursor at all, of any shape, when that width is `<= 0`. This is the reason it exists
    ///   (neovibe): the host shows which pane has the keys through the cursor itself rather than
    ///   by drawing a frame around the pane; neovibe sets the width to 0 by default.
    /// - nvim, over `nvim_ui_set_focus` (`ParallelCommand::FocusGained`/`FocusLost`, exactly as
    ///   `WinitWindowWrapper::handle_focus_gained`/`handle_focus_lost` send them), which fires
    ///   nvim's own `FocusGained`/`FocusLost` autocmds.
    ///
    /// Without a call the renderer keeps its constructed default (focused) and nvim hears
    /// nothing, which is how every embedding behaved before this existed. Repeated reports of the
    /// same state are dropped here, so a host may call this on every focus event it sees. The
    /// cursor change shows on the next frame; the host still owns deciding when to draw one.
    pub fn set_focused(&mut self, focused: bool) {
        if !focus_changed(self.last_focus, focused) {
            return;
        }
        self.last_focus = Some(focused);
        self.state.renderer.handle_event(&WindowEvent::Focused(focused));
        let command = if focused { ParallelCommand::FocusGained } else { ParallelCommand::FocusLost };
        send_ui(command, &self.neovim_handler);
    }

    /// `g:neovide_fullscreen` as this session's settings last received it (neovibe). This harness
    /// has no window, so nothing here acts on it: the host owns the real window and reads this to
    /// follow the variable. It changes when nvim assigns the variable (a `:let`, a mapping, an
    /// `init.lua` line) and Neovide's watcher reports it, and it starts at a value `init.lua` set
    /// before `ui_attach` if there was one. Cheap: one clone of a small settings struct.
    pub fn fullscreen_setting(&self) -> bool {
        self.settings.get::<WindowSettings>().fullscreen
    }

    /// Sets `g:neovide_fullscreen` in nvim (neovibe). For a host whose window changed fullscreen
    /// state for a reason nvim did not see -- its own key, the compositor -- so the variable keeps
    /// matching the window. Asynchronous: [`fullscreen_setting`](Self::fullscreen_setting) follows
    /// once nvim's watcher reports the assignment back, like any other `:let`.
    pub fn set_fullscreen_setting(&self, fullscreen: bool) {
        send_ui(
            ParallelCommand::SetGlobalVariable {
                name: "neovide_fullscreen".to_string(),
                value: fullscreen.into(),
            },
            &self.neovim_handler,
        );
    }

    /// `g:neovide_scale_factor` as this session's settings last received it (neovibe) -- Neovide's
    /// own zoom. Unlike [`fullscreen_setting`](Self::fullscreen_setting) this one is not merely
    /// reported: [`render_frame`](Self::render_frame) applies it to the renderer itself (see
    /// [`apply_scale_factor_setting`](Self::apply_scale_factor_setting)). A host reads it to scale
    /// anything of its own alongside the editor.
    pub fn scale_factor_setting(&self) -> f32 {
        self.settings.get::<WindowSettings>().scale_factor
    }

    /// Sets `g:neovide_scale_factor` in nvim (neovibe), for a host that zooms from a key nvim never
    /// sees. Asynchronous, like [`set_fullscreen_setting`](Self::set_fullscreen_setting): the
    /// renderer follows once nvim's watcher reports the assignment back, so a `:let` typed by hand
    /// and this call take exactly the same path.
    pub fn set_scale_factor_setting(&self, scale_factor: f32) {
        send_ui(
            ParallelCommand::SetGlobalVariable {
                name: "neovide_scale_factor".to_string(),
                value: (scale_factor as f64).into(),
            },
            &self.neovim_handler,
        );
    }

    /// Re-syncs the renderer to `g:neovide_scale_factor` when it changed (neovibe). Returns
    /// whether it did, so [`render_frame`](Self::render_frame) can force the redraw a scale change
    /// needs (see that call site's own comment for why).
    ///
    /// Standalone Neovide does this from `window_wrapper.rs` -- once at window creation
    /// (`sync_scale_factor`) and live from its settings-change handling
    /// (`handle_user_scale_factor_change`). An embedding never goes through that file, so before
    /// this the variable was stored and never applied: **assigning it inside an embedded session
    /// changed nothing on screen at all.** Calling it here, per frame, is cheap -- one clone of a
    /// small settings struct and a float comparison -- and the host's own per-tick grid re-derive
    /// (which reads [`grid_scale`](Self::grid_scale) live) picks the new cell size up on the next
    /// tick and resizes nvim's grid to match, so nothing else needs to know a zoom happened.
    ///
    /// `os_scale_factor` is left exactly as the host set it; `sync_scale_factor` multiplies the two,
    /// so HiDPI is preserved.
    fn apply_scale_factor_setting(&mut self) -> bool {
        let current = self.scale_factor_setting();
        if !scale_factor_changed(self.last_user_scale_factor, current) {
            return false;
        }
        self.last_user_scale_factor = Some(current);
        self.state.renderer.sync_scale_factor();
        self.scale_factor_resyncs += 1;
        true
    }

    /// Diagnostics (neovibe): how many times [`apply_scale_factor_setting`] has actually resynced
    /// the renderer since construction -- not how many frames have been rendered, and not how many
    /// times `g:neovide_scale_factor` has been read. A test can hold this to "does not move across
    /// N idle frames" to prove the change gate, not just its absence of a crash, is still there.
    pub fn scale_factor_resyncs(&self) -> u64 {
        self.scale_factor_resyncs
    }

    /// The OS/display scale the renderer rasterizes at (neovibe, v1 P2) -- as last set by
    /// [`set_os_scale_factor`](Self::set_os_scale_factor), or the value passed to
    /// [`with_options`](Self::with_options)/[`new`](Self::new) if it never has been. Distinct from
    /// [`scale_factor_setting`](Self::scale_factor_setting), which is nvim's own
    /// `g:neovide_scale_factor` (the *user* zoom); the renderer's actual cell size is their
    /// product (`renderer::Renderer`'s private `update_scale_factor`).
    pub fn os_scale_factor(&self) -> f64 {
        self.state.renderer.os_scale_factor
    }

    /// Re-rasterizes at a new OS/display scale (neovibe, v1 P2): the counterpart to
    /// [`apply_scale_factor_setting`](Self::apply_scale_factor_setting) for the *other* half of
    /// the scale-factor product. Standalone Neovide reaches the equivalent renderer call
    /// (`Renderer::handle_os_scale_factor_change`) from `window_wrapper.rs`'s
    /// `handle_scale_factor_update`, itself driven by winit's own `ScaleFactorChanged` event -- a
    /// path an embedding never runs, because there is no winit-owned `Window` here for that event
    /// to arrive on. Before this method existed, `os_scale_factor` was read exactly once, at
    /// construction (`with_options` -> `Renderer::new`), and had **no way to change afterward**:
    /// `state` is private, so a host that later observed a different OS scale (a window dragged
    /// onto another monitor, GTK's `notify::scale-factor`) had no call to make the renderer follow
    /// it. This is exactly that call. As with `apply_scale_factor_setting`, the host owns
    /// re-deriving nvim's own grid size afterward -- this only moves the renderer's idea of the
    /// cell size, and deliberately leaves the *integer* grid exactly as it was (see
    /// [`render_frame`](Self::render_frame)'s own comment on why that same-size case still needs
    /// a forced redraw).
    ///
    /// Returns `true` only when the value actually changed and was applied (compared by bits,
    /// exactly like [`scale_factor_changed`]): a non-finite or non-positive value (`NaN`, `0.0`,
    /// or a negative) is rejected outright with no effect at all, and reassigning the value
    /// already in force is a no-op -- both by construction, so a host's own
    /// `notify::scale-factor` handler can call this unconditionally on every signal with no
    /// idle-cost concern of its own (P11). A `true` return leaves a pending flag set for the
    /// *next* [`render_frame`](Self::render_frame) call rather than forcing the redraw here --
    /// see that call site's own comment for why the two must land on the same frame the
    /// framebuffer itself changes, never before it.
    pub fn set_os_scale_factor(&mut self, os_scale_factor: f64) -> bool {
        if !os_scale_factor.is_finite() || os_scale_factor <= 0.0 {
            return false;
        }
        if os_scale_factor.to_bits() == self.state.renderer.os_scale_factor.to_bits() {
            return false;
        }
        self.state.renderer.handle_os_scale_factor_change(os_scale_factor);
        self.os_scale_redraw_pending = true;
        self.os_scale_factor_resyncs += 1;
        true
    }

    /// Diagnostics (neovibe, v1 P2): how many times
    /// [`set_os_scale_factor`](Self::set_os_scale_factor) has actually changed the renderer's OS
    /// scale since construction -- the same shape as [`scale_factor_resyncs`] for the user-scale
    /// side, and for the same reason: a test (or a host) can hold this to "does not move across N
    /// idle frames" to prove the change gate, not just the absence of a crash, is still there.
    pub fn os_scale_factor_resyncs(&self) -> u64 {
        self.os_scale_factor_resyncs
    }

    /// A clone of the underlying [`NeovimHandler`] — the same escape hatch real Neovide's own
    /// `window::window_wrapper::RouteWindow` exposes as a `pub` field, for anything beyond plain
    /// text input this harness's own API doesn't cover (arbitrary `nvim_command`/buffer
    /// introspection/etc., via `handler.clone_current_neovim()`). Cheap to clone (`Arc`-backed).
    pub fn neovim_handler(&self) -> NeovimHandler {
        self.neovim_handler.clone()
    }

    /// Whether nvim's UI has produced enough real content that real Neovide would consider it
    /// safe to first reveal its window ([`DrawCommandResult::should_show`] — the same flag
    /// `WinitWindowWrapper::handle_draw_commands` uses for its own `UIState::Initing ->
    /// FirstFrame` transition). Monotonic: never goes back to `false`.
    pub fn is_ready(&self) -> bool {
        self.state.is_ready
    }

    /// Count of `DrawCommandBatch` events applied so far — increases whenever new redraw traffic
    /// has been applied to the renderer since construction. A cheap "did anything new happen"
    /// signal; a host that just repaints unconditionally every frame doesn't need this.
    pub fn redraw_batches_seen(&self) -> u64 {
        self.state.redraw_batches_seen
    }

    /// Whether a real `UserEvent::NeovimExited` has been observed — nvim's child process/IO
    /// stream finished (`bridge::mod::run`'s own doc). Also `true` after
    /// [`shutdown`](Self::shutdown) observed it.
    pub fn has_neovim_exited(&self) -> bool {
        self.state.neovim_exited
    }

    /// Cleanly shuts down the real nvim connection: sends `ParallelCommand::Quit` (the exact
    /// mechanism confirmed by the baseline report — `nvim.exec_lua(.., [is_remote])` running
    /// `:qa!`, given the `confirm_quit` override [`with_options`](Self::with_options) applies),
    /// waits up to 5s for the resulting `UserEvent::NeovimExited`, drops the clipboard while the
    /// event loop is still alive (mirroring `window::application::Application::teardown`'s own
    /// comment about releasing Wayland handles safely — see
    /// <https://github.com/neovide/neovide/issues/3311>), then tears down the tokio runtime
    /// backing the connection (`NeovimRuntime::shutdown_timeout`) regardless of whether that wait
    /// succeeded.
    ///
    /// Returns `true` if `NeovimExited` was actually observed before the 5s wait elapsed; `false`
    /// otherwise. **Callers should check this.** The baseline report found, and reproduced
    /// concretely (a real orphaned `nvim --embed` process, reparented to init, confirmed via
    /// `ps`), that nothing in this codebase force-kills a stuck child: dropping a
    /// `tokio::process::Child` does not kill the OS process, and neither
    /// `NeovimRuntime::shutdown_timeout` nor its own `Drop` impl reach far enough to do so — the
    /// child PID isn't even exposed back to `bridge::NeovimRuntime::launch`'s own caller, this
    /// module included. The `confirm_quit` override closes the one concrete way this was
    /// reproduced (an unanswered `:confirm qa` save prompt), but anything else that blocks nvim's
    /// own `:qa!` (a slow `BufWritePre` autocommand, a hung plugin, ...) has the same effect, and
    /// this method's return value is your only signal that happened — there is currently no API
    /// anywhere in this codebase to hard-kill the child yourself if it does. A later, genuinely
    /// interactive phase should track the child PID independently for its own safety net, exactly
    /// as the baseline report's own judgment call #4 flagged.
    ///
    /// Idempotent: a second call returns `true` immediately, without re-sending `Quit`.
    pub fn shutdown(&mut self) -> bool {
        if self.shut_down {
            return true;
        }
        self.shut_down = true;

        send_ui(ParallelCommand::Quit, &self.neovim_handler);

        let deadline = Instant::now() + SHUTDOWN_WAIT;
        while !self.state.neovim_exited && Instant::now() < deadline {
            self.event_loop.pump_app_events(Some(Duration::from_millis(20)), &mut self.state);
        }
        let exited = self.state.neovim_exited;

        self.clipboard.take();
        self.runtime.shutdown_timeout(Duration::from_millis(500));

        exited
    }
}

impl Drop for LiveHarness {
    /// Defensive fallback only, mirroring `NeovimRuntime`'s own `Drop` impl's stance — harmless/
    /// redundant on the already-shut-down happy path, best-effort otherwise. See
    /// [`shutdown`](LiveHarness::shutdown)'s own doc for why calling it explicitly and checking
    /// its return value is the more defensible choice for anything beyond a quick throwaway
    /// script.
    fn drop(&mut self) {
        if !self.shut_down {
            self.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_first_focus_report_always_goes_through_and_repeats_are_dropped() {
        assert!(super::focus_changed(None, true));
        assert!(super::focus_changed(None, false));
        assert!(!super::focus_changed(Some(true), true));
        assert!(!super::focus_changed(Some(false), false));
        assert!(super::focus_changed(Some(true), false));
        assert!(super::focus_changed(Some(false), true));
    }

    /// The first frame always applies whatever the variable holds -- which is how an `init.lua`
    /// that set `g:neovide_scale_factor` before `ui_attach` takes effect at all -- and an
    /// unchanged variable costs no resync after that.
    #[test]
    fn the_scale_factor_is_applied_on_the_first_frame_and_then_only_on_change() {
        assert!(super::scale_factor_changed(None, 1.0));
        assert!(super::scale_factor_changed(None, 1.2));
        assert!(!super::scale_factor_changed(Some(1.0), 1.0));
        assert!(super::scale_factor_changed(Some(1.0), 1.1));
        assert!(super::scale_factor_changed(Some(1.1), 1.0));
    }

    use super::*;

    /// The one thing about this field that is worth a compiler-checked guard: its default must
    /// stay `true`, so adding it changed nothing for any existing host. A host that wants the
    /// capture off has to say so, and the blast radius of getting this backwards is every
    /// `LiveHarness` user silently losing startup-error capture.
    #[test]
    fn startup_message_capture_defaults_to_on() {
        assert!(LiveHarnessOptions::default().startup_message_capture);
    }

    /// `CmdLineSettings::default()` is where `with_options` starts from before assigning the
    /// field, so the two defaults agreeing is what makes "`true` leaves everything exactly as it
    /// was" true rather than merely intended.
    #[test]
    fn the_default_matches_cmdline_settings_own_default() {
        assert_eq!(
            LiveHarnessOptions::default().startup_message_capture,
            CmdLineSettings::default().startup_message_capture
        );
    }
}
