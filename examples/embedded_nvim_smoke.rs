//! P2 smoke test: a REAL `nvim --embed` child process, connected through the exact same
//! [`bridge::NeovimRuntime`]/[`bridge::NeovimHandler`] plumbing the real `neovide` binary uses —
//! but with no winit-owned `Window`, no `window::application::Application`, and no
//! `window::window_wrapper::WinitWindowWrapper` anywhere in the picture. This is the P1
//! [`demo_harness::DemoHarness`] seam (a bare [`renderer::Renderer`] fed [`renderer::DrawCommand`]
//! batches) extended to a *live* Neovim session instead of fabricated content.
//!
//! What this proves, in order:
//! 1. A headless `winit::event_loop::EventLoop<window::EventPayload>` (no window ever created)
//!    can be pumped with [`winit::platform::pump_events::EventLoopExtPumpEvents::pump_app_events`]
//!    while a real `nvim --embed` child, launched via [`bridge::NeovimRuntime::launch`], sends
//!    redraw traffic through it as `UserEvent::DrawCommandBatch` events.
//! 2. Those batches, fed straight to a bare `Renderer::handle_draw_commands` (bypassing
//!    `WinitWindowWrapper` entirely — see this file's doc comment in the report this validates),
//!    are accepted without needing any window/GL surface, exactly like `DemoHarness` already
//!    proved for fabricated content.
//! 3. A real keystroke sent via `bridge::send_ui(SerialCommand::Keyboard(..), &handler)` — the
//!    exact mechanism `WinitWindowWrapper`'s keyboard handling uses — round-trips through nvim and
//!    produces new redraw traffic, and the typed text is independently confirmed to have landed in
//!    nvim's buffer via `NeovimHandler::clone_current_neovim` + a direct `nvim_get_current_line`
//!    call.
//! 4. `ParallelCommand::Quit` exits the real nvim child cleanly (observed as `UserEvent::
//!    NeovimExited`), *given* one specific, verified-the-hard-way precondition: `WindowSettings`'s
//!    `confirm_quit` must not be left at its `true` default, or nvim's own `:confirm qa` (rather
//!    than `:qa!`) blocks forever on an unanswered interactive save prompt (this example has an
//!    unsaved change, from the keystroke in point 3) — and this codebase has **no fallback**
//!    force-kill for a child stuck like that (see the long comment at this example's shutdown
//!    step): dropping a `tokio::process::Child` does not kill the OS process, and neither
//!    `NeovimRuntime::shutdown_timeout` nor its `Drop` impl reach far enough to do so either. A
//!    real orphaned `nvim --embed` process (confirmed via `ps`, reparented to init) was the
//!    concrete, reproduced result of this example's first run before this override was added.
//!
//! Requires a real `nvim` (>= 0.10, this was validated against 0.12.5) on `$PATH`. Run with:
//!
//!     cargo run --example embedded_nvim_smoke
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser;
use neovide::{
    bridge::{NeovimRuntime, OpenMode, ParallelCommand, SerialCommand, send_ui},
    clipboard::{Clipboard, ClipboardHandle},
    cmd_line::CmdLineSettings,
    renderer::{
        Renderer, RendererSettings, cursor_renderer::CursorSettings,
        progress_bar::ProgressBarSettings,
    },
    running_tracker::RunningTracker,
    settings::{Config, Settings},
    units::GridSize,
    window::{EventPayload, EventTarget, RouteId, UserEvent, WindowSettings, create_event_loop},
};
use winit::{
    application::ApplicationHandler, event::WindowEvent, event_loop::ActiveEventLoop,
    platform::pump_events::EventLoopExtPumpEvents, window::WindowId,
};

/// The whole "turn arriving `EventPayload`s into renderer/editor state" seam, minus everything
/// `WinitWindowWrapper`/`Application` do beyond that (window lifecycle, IME, settings hot-reload,
/// startup-message replay, ...). See this example's module doc and the accompanying report for
/// exactly what is and isn't reproduced here, and why.
struct Harness {
    renderer: Renderer,
    route_id: RouteId,
    redraw_batches_seen: u32,
    neovim_exited: bool,
}

impl ApplicationHandler<EventPayload> for Harness {
    // Required by the trait; never fires because this harness never creates a winit Window (no
    // surface for it to be hosted in — an external GTK host would drive its own GL surface
    // instead, exactly like `DemoHarness`'s caller does).
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    // Never fires either, for the same reason — kept only to satisfy `ApplicationHandler`.
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
            // This is the one real content-bearing case. In the actual `neovide` binary this
            // variant is matched inside `window::application::Application::user_event` itself
            // (see the report: `WinitWindowWrapper::handle_user_event`'s own `DrawCommandBatch`
            // arm is dead code, since `Application::user_event`'s match is exhaustive on this
            // variant and never falls through to it) — but the call it bottoms out at,
            // `Renderer::handle_draw_commands`, is a bare, window-independent method we can call
            // directly, exactly as `DemoHarness` and `RouteCore`-backed routes (real Neovide's own
            // pre-window-creation path — see the report) already do.
            UserEvent::DrawCommandBatch(batch) => {
                if matches!(target, EventTarget::Route(route_id) if route_id == self.route_id) {
                    let _ = self.renderer.handle_draw_commands(batch);
                    self.redraw_batches_seen += 1;
                }
            }
            UserEvent::NeovimExited => {
                println!("[harness] UserEvent::NeovimExited received");
                self.neovim_exited = true;
            }
            UserEvent::NeovimLaunchError { message } => {
                panic!("neovim failed to launch: {message}");
            }
            _ => {}
        }
    }
}

/// Pumps `event_loop` in short bursts until `condition` is true or `timeout` elapses. Returns
/// whether `condition` became true.
fn pump_until(
    event_loop: &mut winit::event_loop::EventLoop<EventPayload>,
    harness: &mut Harness,
    timeout: Duration,
    mut condition: impl FnMut(&Harness) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while !condition(harness) {
        if Instant::now() >= deadline {
            return false;
        }
        event_loop.pump_app_events(Some(Duration::from_millis(20)), harness);
    }
    true
}

fn main() {
    // A headless `EventLoop<EventPayload>` — the exact function the real binary calls
    // (`window::create_event_loop`, src/window/mod.rs:265) — with no window ever created on it.
    let mut event_loop = create_event_loop();
    let proxy = event_loop.create_proxy();

    // --- Settings: same registration set `DemoHarness::new` uses, since `Renderer::new` and
    // `NeovimRuntime::launch`/`create_neovim_session` both read from these SettingGroups (a
    // `settings.get::<T>()` on an unregistered/unset `T` panics — see `Settings::get`). ---
    let settings = Arc::new(Settings::new());
    settings.register::<WindowSettings>();
    settings.register::<RendererSettings>();
    settings.register::<CursorSettings>();
    settings.register::<ProgressBarSettings>();

    // `WindowSettings::default().confirm_quit` is `true` (src/window/settings.rs), and
    // `create_neovim_session`'s `settings.read_initial_values` sync propagates every registered
    // SettingGroup field to a same-named `g:neovide_<field>` nvim global — so without overriding
    // this, `g:neovide_confirm_quit` starts `true` on the nvim side too. That matters a lot for
    // shutdown (see below `ParallelCommand::Quit`): `lua/exit_handler.lua`'s `quit(true)` runs
    // `:confirm qa` rather than `:qa!`, and `:confirm qa` on a buffer with unsaved changes (this
    // harness deliberately leaves one, from the keystroke below) blocks on an interactive
    // save-prompt that nothing here ever answers. Concretely confirmed by this example's first
    // run without this override: the exec_lua RPC call sent by `ParallelCommand::Quit` never
    // returned, nvim never exited, `UserEvent::NeovimExited` never arrived, and the real orphaned
    // `nvim --embed` process was still running and reparented to init minutes later — exactly the
    // accumulating-orphan failure mode this phase's task asked to rule out. Overriding it here
    // makes `:qa!` run instead, which force-quits unconditionally.
    let window_settings =
        neovide::window::WindowSettings { confirm_quit: false, ..Default::default() };
    settings.set(&window_settings);

    // `CmdLineSettings::default()` is NOT a zeroed dummy struct — its `impl Default` runs the
    // real clap parser over an empty argv, so clap's `default_value`s apply. That notably means
    // `startup_message_capture` defaults to *true* on nvim >= 0.12 (ours is 0.12.5), which makes
    // `create_neovim_session` externalize messages/cmdline (`ext_messages`) at `ui_attach` and
    // expects something to later call `flush_startup_messages_if_ready` (only ever called from
    // `WinitWindowWrapper::handle_draw_commands`/`handle_draw_commands_for_route`, which this
    // harness deliberately bypasses) to restore builtin message UI. Left unhandled, nvim's cmdline
    // height/messages would stay externalized forever with nothing on our side to un-externalize
    // them. So: explicitly disable it here, exactly like passing `--no-startup-message-capture` on
    // the real command line — this is a judgment call specific to this minimal harness, not
    // something the real app needs to do.
    //
    // `-- --clean` (the same `neovim_args` passthrough `test_neovim_passthrough` in src/cmd_line.rs
    // exercises) additionally launches nvim with no user config/plugins loaded: without it, this
    // spawns *this machine's real, ambient* `nvim` config verbatim (whatever init.lua a person has
    // installed) — which the first run of this example against this environment's own config
    // concretely demonstrated, by loading a dashboard/greeter plugin's start screen instead of a
    // blank buffer, so the literal keystrokes below landed on dashboard shortcut keys instead of
    // typing text (harmless, but not a deterministic smoke test). A real embedding host presumably
    // wants the person's actual config, so `--clean` is a choice specific to this example, not
    // something implied by the plumbing itself.
    let cmdline_settings =
        CmdLineSettings::parse_from(["neovide", "--no-startup-message-capture", "--", "--clean"]);
    settings.set(&cmdline_settings);

    // Real clipboard, wired through the same `EventLoop` (needed for Wayland/X11 display-handle
    // access) — a `ClipboardHandle` is a required `NeovimRuntime::new` parameter, and nvim's
    // `neovide.get_clipboard`/`neovide.set_clipboard` requests reach it, but nothing here actually
    // exercises clipboard I/O.
    let clipboard = Clipboard::new(&event_loop);
    let clipboard_handle = ClipboardHandle::new(&clipboard);

    let mut runtime = NeovimRuntime::new(clipboard_handle)
        .expect("failed to build the tokio runtime backing NeovimRuntime");

    let route_id = RouteId::next();
    let config = Config::default();
    let running_tracker = RunningTracker::new();
    let grid_size = Some(GridSize::new(80u32, 24u32));

    println!("[harness] launching `nvim --embed`...");
    let handler = runtime
        .launch(
            route_id,
            proxy.clone(),
            grid_size,
            running_tracker,
            settings.clone(),
            &config,
            None,           // cwd: inherit the current process's cwd
            OpenMode::None, // "launch a blank embedded instance" — see bridge::command::OpenMode
        )
        .expect("NeovimRuntime::launch failed — is `nvim` (>= 0.10) on $PATH?");

    // The bare `Renderer` this harness drives directly — no `WinitWindowWrapper`/`RouteWindow`/
    // real winit `Window` anywhere. `os_scale_factor` of 1.0 mirrors `DemoHarness::new`'s default.
    let renderer = Renderer::new(1.0, config, settings);
    let mut harness = Harness { renderer, route_id, redraw_batches_seen: 0, neovim_exited: false };

    // --- 1. Wait for nvim's *initial* redraw traffic (ui_attach's first grid_resize/grid_line
    // batch + Flush) to reach us as a `DrawCommandBatch`. ---
    let got_initial_redraw =
        pump_until(&mut event_loop, &mut harness, Duration::from_secs(15), |h| {
            h.redraw_batches_seen > 0
        });
    assert!(
        got_initial_redraw && !harness.neovim_exited,
        "no redraw traffic from nvim within 15s (ui_attach likely never completed) — \
         neovim_exited={}",
        harness.neovim_exited
    );
    println!(
        "[harness] initial redraw traffic received ({} batch(es))",
        harness.redraw_batches_seen
    );
    let baseline = harness.redraw_batches_seen;

    // --- 2. Send one real keystroke through the exact mechanism `WinitWindowWrapper`'s keyboard
    // handling uses: `SerialCommand::Keyboard(String)`, delivered via `send_ui` into the
    // route-keyed serial-command channel `start_ui_command_handler` drains with `nvim.input(..)`.
    // "ihello neovibe<Esc>" enters insert mode, types text, then returns to normal mode. ---
    println!("[harness] sending keystroke: ihello neovibe<Esc>");
    send_ui(SerialCommand::Keyboard("ihello neovibe<Esc>".to_string()), &handler);

    let got_response_redraw =
        pump_until(&mut event_loop, &mut harness, Duration::from_secs(15), |h| {
            h.redraw_batches_seen > baseline
        });
    assert!(
        got_response_redraw && !harness.neovim_exited,
        "no redraw traffic after sending a keystroke within 15s"
    );
    println!(
        "[harness] keystroke round-tripped: {} additional batch(es) of redraw traffic",
        harness.redraw_batches_seen - baseline
    );

    // --- Independently confirm the typed text actually landed in nvim's buffer, via the same
    // `NeovimHandler::clone_current_neovim` accessor real Neovide code uses for anything outside
    // the Serial/ParallelCommand queues. `futures::executor::block_on` here is just this example's
    // own one-off way of awaiting an async nvim-rs call outside of `NeovimRuntime`'s tokio runtime
    // — it is not part of the real request/response mechanism. ---
    let nvim = handler
        .clone_current_neovim()
        .expect("no active neovim handle after a completed keystroke");
    let line =
        futures::executor::block_on(nvim.get_current_line()).expect("nvim_get_current_line failed");
    println!("[harness] buffer line after keystroke: {line:?}");
    assert_eq!(line, "hello neovibe", "typed text did not land in nvim's buffer as expected");

    // --- 3. Clean shutdown: ask nvim to quit for real (not just drop our side), then wait for the
    // real `UserEvent::NeovimExited` event this produces (see bridge::mod.rs's `run()` task, which
    // waits for the child process/IO stream to finish and only then sends it). ---
    println!("[harness] sending ParallelCommand::Quit");
    send_ui(ParallelCommand::Quit, &handler);
    let exited =
        pump_until(&mut event_loop, &mut harness, Duration::from_secs(10), |h| h.neovim_exited);
    // IMPORTANT, verified the hard way (this example's first run, before the `confirm_quit`
    // override above, left a real orphaned `nvim --embed` process behind — see this file's git
    // history/the accompanying report): nothing in this codebase force-kills the child if it does
    // not exit on its own. `bridge::command::create_tokio_nvim_command`'s `tokio::process::Command`
    // is never built with `.kill_on_drop(true)`, and neither `NeovimRuntime::shutdown_timeout` nor
    // its `Drop` impl reach into the `run()` task to kill the process — they only shut down the
    // *tokio runtime* that task runs on, and dropping a `tokio::process::Child` handle does NOT
    // kill the underlying process (this matches plain `std::process::Child`'s documented drop
    // behavior). So a stuck-on-shutdown nvim (a confirm-prompt is the concrete case verified here,
    // but a slow plugin, a `BufWritePre` autocommand, or anything else that blocks `:qa!` would
    // have the same effect) becomes a permanent orphan no API call in this codebase reaps. Assert
    // hard on this rather than let it slide, since the whole point of this check was to rule out
    // exactly this accumulating-orphan failure mode:
    assert!(
        exited,
        "nvim did not exit within 10s of ParallelCommand::Quit — it is almost certainly now an \
         orphaned process (see this block's comment): NOTHING in this codebase will force-kill \
         it, so it will keep running until something outside this program reaps it"
    );
    println!("[harness] observed NeovimExited before shutdown: {exited}");

    // Explicit, not relied-on-Drop: this is the same call `Application::teardown` makes (and
    // `NeovimRuntime::drop` only as a defensive fallback — see its own doc comment) — it shuts
    // down the tokio runtime driving nvim's IO tasks and, per the comment on `Application::
    // teardown`, gives a little extra time for the (already-exited, per the assert above) child's
    // stdio to be drained before force-tearing-down the runtime.
    runtime.shutdown_timeout(Duration::from_millis(500));
    println!("[harness] NeovimRuntime::shutdown_timeout returned — no nvim child should remain");

    println!("done");
}
