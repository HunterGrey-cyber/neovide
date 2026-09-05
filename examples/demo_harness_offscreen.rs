//! End-to-end smoke test for `neovide::demo_harness::DemoHarness` against a *real*
//! `skia_safe::Canvas` (an offscreen CPU raster surface — no GPU/window/GL context needed).
//!
//! This exists because the baseline phase found that `RenderedWindow::line_text_range` — the only
//! public "read grid content back" accessor — always returns `None` for `WindowType::Editor`
//! grids (it only works for `WindowType::Message`), so the only way to confirm ordinary editor
//! content actually painted is to paint to a real canvas and inspect pixels. That's what this does,
//! plus it exercises the P1 viewport-clear fix in `src/renderer/mod.rs::draw_frame` end-to-end: a
//! `content_region` smaller than the canvas is passed in, and pixels *outside* it are asserted to
//! stay untouched (a sentinel color painted before the first frame) while pixels *inside* it turn
//! into the renderer's own background color.
//!
//! Run with:
//!
//!     cargo run --example demo_harness_offscreen
use neovide::{demo_harness::DemoHarness, units::PixelRect};
use skia_safe::{Color, IPoint, surfaces};

const CANVAS_SIZE: (i32, i32) = (800, 400);
// A sub-rect of the canvas — deliberately not starting at (0, 0) and not covering the whole
// canvas, so the viewport-clear fix (renderer must only ever paint its own assigned rect) has
// something to prove itself against.
const CONTENT_REGION: (f32, f32, f32, f32) = (120.0, 60.0, 680.0, 340.0);
const SENTINEL: Color = Color::MAGENTA;

fn main() {
    let mut surface =
        surfaces::raster_n32_premul(CANVAS_SIZE).expect("failed to create raster surface");

    // Paint the whole canvas a color the renderer would never itself produce, so we can tell
    // apart "the renderer touched this pixel" from "this pixel was never touched".
    surface.canvas().clear(SENTINEL);

    let content_region = PixelRect::from_min_max(
        (CONTENT_REGION.0, CONTENT_REGION.1),
        (CONTENT_REGION.2, CONTENT_REGION.3),
    );

    let mut harness = DemoHarness::new(/* os_scale_factor */ 1.0);

    // --- Frame 0: confirm the viewport-clear fix holds even on the very first frame the demo's
    // content (text + cursor) becomes visible. ---
    harness.render_frame(surface.canvas(), Some(&content_region), 1.0 / 60.0);
    assert_outside_untouched(&mut surface, &content_region);
    assert_inside_painted(&mut surface, &content_region);
    println!("frame 0: clip fix holds (outside content_region untouched, inside painted)");

    // --- Re-paint the sentinel and run several more frames, including past the point the
    // one-shot scroll-region update fires, to confirm nothing panics across an extended run and
    // that the clip invariant keeps holding every frame (not just the first). ---
    surface.canvas().clear(SENTINEL);
    let dt = 1.0 / 60.0;
    let mut still_animating_at_end = false;
    for frame in 0..240 {
        // 240 frames @ 60fps = 4s of demo time, past SCROLL_DELAY_SECS (2s).
        still_animating_at_end = harness.render_frame(surface.canvas(), Some(&content_region), dt);
        if frame % 60 == 0 {
            assert_outside_untouched(&mut surface, &content_region);
            println!("frame {frame}: animating={still_animating_at_end}, still clipped correctly");
        }
    }
    println!("done — {} frames run without panic, final animating={still_animating_at_end}", 240);
}

fn assert_outside_untouched(surface: &mut skia_safe::Surface, region: &PixelRect<f32>) {
    let pixmap = surface.peek_pixels().expect("raster surface should expose pixels directly");
    // A handful of points strictly outside `region` on every side.
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
    // The exact center of content_region is background-colored border padding almost everywhere
    // except where glyphs/cursor land, so just confirm it's no longer the sentinel — i.e. the
    // renderer actually painted (cleared to its own background) inside its own rect.
    let cx = ((region.min.x + region.max.x) / 2.0) as i32;
    let cy = ((region.min.y + region.max.y) / 2.0) as i32;
    let color = pixmap.get_color(IPoint::new(cx, cy));
    assert_ne!(
        color, SENTINEL,
        "pixel ({cx}, {cy}) inside content_region {region:?} was never painted by the renderer"
    );
}
