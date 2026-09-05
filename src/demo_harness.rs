//! A minimal, self-contained harness that drives Neovide's [`renderer::Renderer`] with hand-built
//! (not nvim-sourced) content, for an external host to embed frame-by-frame.
//!
//! This exists for the neovibe P1 feasibility phase: prove the renderer can run without a live
//! Neovim connection and without its own winit window, so a later phase (in a different repo) can
//! host it inside a GTK4 `GtkGLArea`. It reuses exactly the seam the baseline phase identified —
//! `editor::Window`/`Cursor`/`Style` construction feeding `renderer::Renderer::handle_draw_commands`
//! — the same one demonstrated in `examples/fabricated_content.rs`, just packaged as a small public
//! API (['DemoHarness']) instead of a one-shot example `main`.
//!
//! Nothing in this module touches `bridge`'s real RPC/msgpack parsing, `window::application`, or
//! `window::window_wrapper`'s actual event-loop-driving code — it only calls already-`pub` methods
//! on `editor`/`renderer` types, exactly like a real `Editor` does internally.
use std::{collections::HashMap, sync::Arc};

use skia_safe::{Canvas, Color4f};

use crate::{
    bridge::GridLineCell,
    cmd_line::CmdLineSettings,
    editor::{Colors, Cursor, CursorShape, DrawCommandBatcher, Style, Window, WindowType},
    renderer::{
        DrawCommand, Renderer, RendererSettings, cursor_renderer::CursorSettings,
        progress_bar::ProgressBarSettings,
    },
    settings::{Config, Settings},
    units::{GridRect, GridSize, PixelRect},
    window::WindowSettings,
};

/// Grid dimensions (columns, rows) of the demo's single fabricated editor grid.
const DEMO_GRID_SIZE: (u64, u64) = (40, 10);
/// Grid id for the demo's single editor window — `1` matches what a real session's first/base
/// grid would be assigned.
const DEMO_GRID_ID: u64 = 1;
/// Grid row/column the demo cursor starts at (and returns to after the scroll below).
const DEMO_CURSOR_POSITION: (u64, u64) = (5, 1);
/// How many seconds of demo time (summed `dt`, not wall-clock) elapse before the one-shot
/// scroll-region update fires, so a caller watching from frame zero sees the "before" state for a
/// moment first.
const SCROLL_DELAY_SECS: f32 = 2.0;

/// Drives a [`Renderer`] with fabricated content: a few lines of text across more than one
/// highlight group, a blinking cursor at a fixed grid position, and a one-shot scroll-region
/// update fired a couple of seconds in — enough to exercise font/highlight rendering, cursor +
/// blink animation, and scroll animation without any live `nvim --embed` connection.
///
/// Construct one with [`DemoHarness::new`], then call [`DemoHarness::render_frame`] once per
/// frame from the host's own render loop/timer. The intended caller is an external GTK/Skia host
/// that knows nothing about `bridge`/`window`/`application` internals — the whole
/// prepare/animate/draw pipeline `WinitWindowWrapper` normally spreads across several calls
/// (`prepare_frame`, `animate_frame`, `prepare_lines`, `draw_frame`) is folded into the one method
/// here for that caller's convenience.
pub struct DemoHarness {
    renderer: Renderer,
    /// Kept alive so the delayed scroll-region update below can be queued through the same
    /// `editor::Window` the initial content was drawn through (a `Window` is a caller-side content
    /// builder, not something `Renderer`/`DrawCommandBatcher` retain internally).
    window: Window,
    elapsed: f32,
    scrolled: bool,
}

impl DemoHarness {
    /// Builds a `Renderer` (no live Neovim connection, no winit window/GL surface required — see
    /// `Renderer::new`'s own doc/the baseline phase report) and queues the fixed demo content into
    /// it: styled text, then a cursor. The one-shot scroll update is queued lazily out of
    /// [`render_frame`], `SCROLL_DELAY_SECS` of demo time in.
    ///
    /// `os_scale_factor` is forwarded to [`Renderer::new`] unchanged — pass the host's own display
    /// scale factor (`1.0` if unknown/not applicable).
    pub fn new(os_scale_factor: f64) -> Self {
        let settings = Arc::new(Settings::new());
        settings.register::<WindowSettings>();
        settings.register::<RendererSettings>();
        settings.register::<CursorSettings>();
        settings.register::<ProgressBarSettings>();
        // CmdLineSettings isn't an nvim-syncable SettingGroup (real Neovide populates it from
        // parsed CLI args via `.set()` in src/cmd_line.rs, not `.register()`); we have no CLI args
        // here, so just seed it with its `Default` impl, same as `examples/fabricated_content.rs`.
        settings.set(&CmdLineSettings::default());

        let mut renderer = Renderer::new(os_scale_factor, Config::default(), settings);

        let mut batcher = DrawCommandBatcher::new();
        let mut window = Window::new(
            DEMO_GRID_ID,
            WindowType::Editor,
            None,
            (0.0, 0.0),
            DEMO_GRID_SIZE,
            &mut batcher,
        );

        let defined_styles = Self::build_styles();
        Self::draw_demo_lines(&mut window, &mut batcher, &defined_styles);

        // Cursor at a specific grid position, blinking: `blinkon`/`blinkoff` both non-zero makes
        // `renderer::cursor_renderer::blink::BlinkStatus` actually cycle on/off over time (see
        // `is_static` there) instead of rendering statically — visible animation with no further
        // action from the caller beyond calling `render_frame` repeatedly.
        let cursor = Cursor {
            grid_position: DEMO_CURSOR_POSITION,
            parent_window_id: DEMO_GRID_ID,
            shape: CursorShape::Block,
            enabled: true,
            blinkwait: Some(0),
            blinkon: Some(400),
            blinkoff: Some(400),
            ..Cursor::new()
        };
        batcher.queue(DrawCommand::UpdateCursor(cursor));

        renderer.handle_draw_commands(batcher.take_batch());

        DemoHarness { renderer, window, elapsed: 0.0, scrolled: false }
    }

    /// Two highlights beyond the default one: reversed-video "error" (red-on-default) and a
    /// bold-italic accent color — enough to exercise more-than-one-highlight-group rendering.
    fn build_styles() -> HashMap<u64, Arc<Style>> {
        let mut styles = HashMap::new();

        let mut error_style =
            Style::new(Colors::new(Some(Color4f::new(1.0, 0.0, 0.0, 1.0)), None, None));
        error_style.reverse = true;

        let mut accent_style =
            Style::new(Colors::new(Some(Color4f::new(0.2, 0.6, 1.0, 1.0)), None, None));
        accent_style.bold = true;
        accent_style.italic = true;

        styles.insert(1u64, Arc::new(error_style));
        styles.insert(2u64, Arc::new(accent_style));
        styles
    }

    fn draw_demo_lines(
        window: &mut Window,
        batcher: &mut DrawCommandBatcher,
        styles: &HashMap<u64, Arc<Style>>,
    ) {
        window.draw_grid_line(
            batcher,
            0,
            0,
            vec![GridLineCell {
                text: "Hello, neovibe!".to_string(),
                highlight_id: Some(0),
                repeat: None,
            }],
            styles,
        );
        // Two different highlights on the same line, exercising >1 highlight/color per line.
        window.draw_grid_line(
            batcher,
            1,
            0,
            vec![
                GridLineCell { text: "ERROR".to_string(), highlight_id: Some(1), repeat: None },
                GridLineCell { text: " ".to_string(), highlight_id: Some(0), repeat: None },
                GridLineCell { text: "accent".to_string(), highlight_id: Some(2), repeat: None },
            ],
            styles,
        );
        window.draw_grid_line(
            batcher,
            2,
            0,
            vec![GridLineCell {
                text: "scroll me".to_string(),
                highlight_id: Some(0),
                repeat: None,
            }],
            styles,
        );
    }

    fn queue_scroll(&mut self) {
        let mut batcher = DrawCommandBatcher::new();
        self.window.scroll_region(
            &mut batcher,
            GridRect::from_min_max((0u64, 0u64), (DEMO_GRID_SIZE.0, DEMO_GRID_SIZE.1)),
            GridSize::new(0i64, 1i64),
        );
        self.renderer.handle_draw_commands(batcher.take_batch());
    }

    /// Advances animation state and paints one frame into `canvas`.
    ///
    /// - `content_region`, when given, is the pixel rect within `canvas` this harness owns —
    ///   matching [`Renderer::draw_frame`]'s own parameter (see the P1 viewport-clear fix in
    ///   `src/renderer/mod.rs`): drawing is clipped to it, and the demo's grid is positioned to
    ///   start at its top-left corner, so a host can render into a sub-rect of a larger canvas
    ///   (e.g. a `GtkGLArea` shared with other widgets) without the demo bleeding outside it.
    ///   `None` means "own the whole canvas", matching the standalone `neovide` binary's own
    ///   default behavior.
    /// - `dt` is the elapsed time in seconds since the previous call — drive it from the host's
    ///   own frame clock (real elapsed wall-time is the natural choice for a live GTK render
    ///   loop).
    ///
    /// Internally this runs the same sequence `WinitWindowWrapper` runs each frame —
    /// `Renderer::prepare_frame`, `Renderer::animate_frame`, `Renderer::prepare_lines`, then
    /// `Renderer::draw_frame` — folded into one call for this caller's convenience.
    ///
    /// Returns `true` while the demo has ongoing position/scroll/vfx animation and would like
    /// another `render_frame` call soon; `false` once that settles. This is advisory only — cursor
    /// blink visibility is driven by wall-clock time inside `Renderer` regardless of this return
    /// value, so a host that just redraws unconditionally every frame (e.g. per-vblank) works
    /// fine too.
    pub fn render_frame(
        &mut self,
        canvas: &Canvas,
        content_region: Option<&PixelRect<f32>>,
        dt: f32,
    ) -> bool {
        self.elapsed += dt;
        if !self.scrolled && self.elapsed >= SCROLL_DELAY_SECS {
            self.scrolled = true;
            self.queue_scroll();
        }

        // Drives cursor blink-state transitions (see `CursorRenderer::prepare_frame` /
        // `BlinkStatus::update_status`) — must run every frame regardless of `dt`, since blink
        // timing is wall-clock (`Instant`) based, not `dt`-accumulated.
        self.renderer.prepare_frame();

        let grid_scale = self.renderer.grid_renderer.grid_scale;
        // Mirrors `WinitWindowWrapper::get_grid_rect_from_window`: the grid-space rect passed to
        // `animate_frame` needs its origin derived from wherever `content_region` starts in pixel
        // space (divided through `grid_scale`), or the demo's root window would end up positioned
        // as if it always started at the canvas's absolute (0, 0) regardless of where the caller's
        // viewport actually begins (see `RenderedWindow::get_target_position`).
        let grid_rect = content_region.map(|region| *region / grid_scale).unwrap_or_else(|| {
            GridRect::from_min_max((0.0, 0.0), (DEMO_GRID_SIZE.0 as f32, DEMO_GRID_SIZE.1 as f32))
        });

        let animating = self.renderer.animate_frame(&grid_rect, dt);
        self.renderer.prepare_lines(false);
        self.renderer.draw_frame(canvas, content_region, dt);
        animating
    }
}
