//! End-to-end smoke test for `neovide::live_harness::LiveHarness` against a *real* `nvim --embed`
//! connection, painted onto a real `skia_safe::Canvas` (an offscreen CPU raster surface — no GPU/
//! window/GL context needed) — the P2 counterpart of `examples/demo_harness_offscreen.rs`, which
//! proved the same viewport-clip invariant for `DemoHarness`'s fabricated content.
//!
//! What this proves, in order:
//! 1. `LiveHarness::with_options` establishes a real `nvim --embed` connection and its first real
//!    redraw traffic reaches `LiveHarness::is_ready()`/`render_frame` within a few seconds.
//! 2. Real (not fabricated) content painted by that redraw traffic is actually visible on a real
//!    canvas, respecting the P1 viewport-clear fix (pixels outside `content_region` stay
//!    untouched; pixels inside it don't).
//! 3. A real keystroke sent via `LiveHarness::send_text_input` round-trips through nvim (observed
//!    as new redraw traffic) and the typed text is independently confirmed to have landed in
//!    nvim's buffer, via `LiveHarness::neovim_handler()` + a direct `nvim_get_current_line` call —
//!    the same independent-oracle technique the baseline phase's own
//!    `examples/embedded_nvim_smoke.rs` used, since (as the P1 baseline phase found)
//!    `RenderedWindow::line_text_range` only ever works for `WindowType::Message` grids, never
//!    ordinary editor grids, so it cannot serve as that oracle here either.
//! 4. `LiveHarness::shutdown` returns `true` (a real `UserEvent::NeovimExited` was observed, not a
//!    timeout) and leaves no orphaned `nvim` process behind — verified the same way the baseline
//!    phase verified it: run this example, then check `pgrep`/`ps` from outside the process for
//!    any lingering `nvim` afterward.
//!
//! Requires a real `nvim` (>= 0.10, this was validated against 0.12.5) on `$PATH`. Run with:
//!
//!     cargo run --example live_harness_offscreen
use std::time::{Duration, Instant};

use neovide::{
    live_harness::{LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use skia_safe::{Color, IPoint, surfaces};

const CANVAS_SIZE: (i32, i32) = (800, 400);
// A sub-rect of the canvas — deliberately not starting at (0, 0) and not covering the whole
// canvas, so the viewport-clear fix (renderer must only ever paint its own assigned rect) has
// something to prove itself against, exactly like `demo_harness_offscreen.rs`.
const CONTENT_REGION: (f32, f32, f32, f32) = (120.0, 60.0, 680.0, 340.0);
const SENTINEL: Color = Color::MAGENTA;
const FRAME_DT: f32 = 1.0 / 60.0;

/// Repeatedly renders frames (so redraw traffic keeps getting pumped and painted) until
/// `condition` is true or `timeout` elapses. Returns whether `condition` became true.
fn render_until(
    harness: &mut LiveHarness,
    surface: &mut skia_safe::Surface,
    content_region: &PixelRect<f32>,
    timeout: Duration,
    mut condition: impl FnMut(&LiveHarness) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        harness.render_frame(surface.canvas(), Some(content_region), FRAME_DT);
        if condition(harness) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn main() {
    let mut surface =
        surfaces::raster_n32_premul(CANVAS_SIZE).expect("failed to create raster surface");
    surface.canvas().clear(SENTINEL);

    let content_region = PixelRect::from_min_max(
        (CONTENT_REGION.0, CONTENT_REGION.1),
        (CONTENT_REGION.2, CONTENT_REGION.3),
    );

    println!("[example] launching `nvim --embed` (--clean, 80x24) via LiveHarness...");
    let mut harness = LiveHarness::with_options(LiveHarnessOptions {
        grid_size: Some(GridSize::new(80u32, 24u32)),
        extra_nvim_args: vec!["--clean".to_string()],
        ..Default::default()
    })
    .expect("LiveHarness::with_options failed — is `nvim` (>= 0.10) on $PATH?");

    // --- 1. Wait for nvim's real initial redraw traffic (ui_attach's first grid_resize/grid_line
    // batch + Flush) to reach `is_ready()`. ---
    let became_ready = render_until(
        &mut harness,
        &mut surface,
        &content_region,
        Duration::from_secs(15),
        |h| h.is_ready(),
    );
    assert!(
        became_ready && !harness.has_neovim_exited(),
        "LiveHarness never became ready within 15s (ui_attach likely never completed) — \
         redraw_batches_seen={}, neovim_exited={}",
        harness.redraw_batches_seen(),
        harness.has_neovim_exited()
    );
    println!(
        "[example] LiveHarness ready ({} redraw batch(es) applied)",
        harness.redraw_batches_seen()
    );

    // --- 2. Confirm real (not fabricated) content actually painted, respecting the P1
    // viewport-clear fix: outside `content_region` must stay the sentinel, inside it must not. ---
    assert_outside_untouched(&mut surface, &content_region);
    assert_inside_painted(&mut surface, &content_region);
    println!("[example] real nvim content painted; viewport-clip invariant holds");

    // --- 3. Send one real keystroke through LiveHarness::send_text_input, wait for the resulting
    // redraw traffic, then independently confirm (outside of rendering entirely) that the typed
    // text actually landed in nvim's buffer. ---
    let baseline_batches = harness.redraw_batches_seen();
    println!("[example] sending keystroke: ihello neovibe<Esc>");
    harness.send_text_input("ihello neovibe<Esc>");

    let got_response = render_until(
        &mut harness,
        &mut surface,
        &content_region,
        Duration::from_secs(15),
        |h| h.redraw_batches_seen() > baseline_batches,
    );
    assert!(
        got_response && !harness.has_neovim_exited(),
        "no redraw traffic after sending a keystroke within 15s"
    );
    println!(
        "[example] keystroke round-tripped: {} additional batch(es) of redraw traffic",
        harness.redraw_batches_seen() - baseline_batches
    );

    let nvim = harness
        .neovim_handler()
        .clone_current_neovim()
        .expect("no active neovim handle after a completed keystroke");
    let line =
        futures::executor::block_on(nvim.get_current_line()).expect("nvim_get_current_line failed");
    println!("[example] buffer line after keystroke: {line:?}");
    assert_eq!(line, "hello neovibe", "typed text did not land in nvim's buffer as expected");

    // The clip invariant must still hold after real content changed, not just on frame 0.
    assert_outside_untouched(&mut surface, &content_region);

    // --- 4. Clean shutdown: LiveHarness::shutdown asks nvim to quit for real and waits for the
    // resulting UserEvent::NeovimExited. This example's own process then exits immediately after
    // printing "done" — the caller is expected to independently confirm via `pgrep`/`ps` from
    // outside this process that no `nvim` child survives it (see this file's module doc and the
    // accompanying phase report for exactly how that was verified). ---
    println!("[example] calling LiveHarness::shutdown()");
    let exited_cleanly = harness.shutdown();
    assert!(
        exited_cleanly,
        "LiveHarness::shutdown() returned false — nvim did not exit within its wait, and per \
         LiveHarness::shutdown's own doc, nothing in this codebase force-kills a child stuck like \
         that: it is almost certainly now an orphaned process"
    );
    println!("[example] shutdown() returned true (NeovimExited observed, not a timeout)");

    println!("done");
}

fn assert_outside_untouched(surface: &mut skia_safe::Surface, region: &PixelRect<f32>) {
    let pixmap = surface.peek_pixels().expect("raster surface should expose pixels directly");
    let probes = [
        (region.min.x as i32 / 2, region.min.y as i32 / 2), // above-left
        (CANVAS_SIZE.0 - 5, 5),                             // top-right corner
        (5, CANVAS_SIZE.1 - 5),                             // bottom-left corner
        (CANVAS_SIZE.0 - 5, CANVAS_SIZE.1 - 5),             // bottom-right corner
    ];
    for (x, y) in probes {
        let color = pixmap.get_color(IPoint::new(x, y));
        assert_eq!(
            color, SENTINEL,
            "pixel ({x}, {y}) outside content_region {region:?} was touched by the renderer \
             (viewport-clear fix regressed — the renderer painted outside its assigned rect)"
        );
    }
}

fn assert_inside_painted(surface: &mut skia_safe::Surface, region: &PixelRect<f32>) {
    let pixmap = surface.peek_pixels().expect("raster surface should expose pixels directly");
    let cx = ((region.min.x + region.max.x) / 2.0) as i32;
    let cy = ((region.min.y + region.max.y) / 2.0) as i32;
    let color = pixmap.get_color(IPoint::new(cx, cy));
    assert_ne!(
        color, SENTINEL,
        "pixel ({cx}, {cy}) inside content_region {region:?} was never painted by the renderer"
    );
}
