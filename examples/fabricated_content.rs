//! Standalone smoke test for the `neovide` library target.
//!
//! Constructs a `Renderer` exactly the way `src/renderer/mod.rs`'s own `#[cfg(test)]` helper
//! does, then hand-builds a batch of `DrawCommand`s — a couple of styled text lines (more than
//! one highlight/color), a cursor placed at a specific grid position, and a scroll-region update
//! — entirely without a live `nvim --embed` connection, a winit `EventLoopProxy`, or a GL/window
//! surface. Run with:
//!
//!     cargo run --example fabricated_content
use std::sync::Arc;

use neovide::{
    bridge::GridLineCell,
    cmd_line::CmdLineSettings,
    editor::{Colors, Cursor, CursorShape, DrawCommandBatcher, Style, Window, WindowType},
    renderer::{
        DrawCommand, Renderer, RendererSettings, cursor_renderer::CursorSettings,
        progress_bar::ProgressBarSettings,
    },
    settings::{Config, Settings},
    units::{GridRect, GridSize},
    window::WindowSettings,
};

fn main() {
    // --- Construct a Renderer with no live Neovim connection and no winit window/GL surface. ---
    let settings = Arc::new(Settings::new());
    settings.register::<WindowSettings>();
    settings.register::<RendererSettings>();
    settings.register::<CursorSettings>();
    settings.register::<ProgressBarSettings>();
    // CmdLineSettings isn't an nvim-syncable `SettingGroup` (real Neovide populates it straight
    // from parsed CLI args via `settings.set(...)` in src/cmd_line.rs, not `.register()`); we have
    // no CLI args here, so just seed it with its `Default` impl.
    settings.set(&CmdLineSettings::default());
    let mut renderer = Renderer::new(/* os_scale_factor */ 1.0, Config::default(), settings);

    // --- Two highlights: reversed-video "error" red-on-default, and a bold-italic accent. ---
    let mut defined_styles = std::collections::HashMap::new();
    let error_style = {
        let mut s = Style::new(Colors::new(
            Some(skia_safe::Color4f::new(1.0, 0.0, 0.0, 1.0)), // foreground: red
            None,
            None,
        ));
        s.reverse = true;
        s
    };
    let accent_style = {
        let mut s = Style::new(Colors::new(
            Some(skia_safe::Color4f::new(0.2, 0.6, 1.0, 1.0)), // foreground: light blue
            None,
            None,
        ));
        s.bold = true;
        s.italic = true;
        s
    };
    defined_styles.insert(1u64, Arc::new(error_style));
    defined_styles.insert(2u64, Arc::new(accent_style));

    // --- Build grid 1 (the base/root grid) directly via `editor::Window`, bypassing bridge RPC
    // and the RedrawEvent parser entirely: these are the same pub methods `Editor` itself calls
    // in response to parsed RedrawEvents (see src/editor/mod.rs's `resize_window`/`draw_grid_line`
    // /`scroll_region`), just invoked by hand here. ---
    let mut batcher = DrawCommandBatcher::new();
    let mut window = Window::new(
        /* grid_id */ 1,
        WindowType::Editor,
        None,
        (0.0, 0.0),
        /* grid_size (cols, rows) */ (40, 6),
        &mut batcher,
    );

    // Row 0: plain text, default highlight.
    window.draw_grid_line(
        &mut batcher,
        0,
        0,
        vec![GridLineCell {
            text: "Hello, neovibe!".to_string(),
            highlight_id: Some(0),
            repeat: None,
        }],
        &defined_styles,
    );
    // Row 1: two different highlights on the same line (exercises >1 highlight/color per line).
    window.draw_grid_line(
        &mut batcher,
        1,
        0,
        vec![
            GridLineCell { text: "ERROR".to_string(), highlight_id: Some(1), repeat: None },
            GridLineCell { text: " ".to_string(), highlight_id: Some(0), repeat: None },
            GridLineCell { text: "accent".to_string(), highlight_id: Some(2), repeat: None },
        ],
        &defined_styles,
    );
    // Row 2: content that will be scrolled.
    window.draw_grid_line(
        &mut batcher,
        2,
        0,
        vec![GridLineCell { text: "scroll me".to_string(), highlight_id: Some(0), repeat: None }],
        &defined_styles,
    );

    // --- Scroll region update (rows 0..6, whole width, up by 1 row) — exercises scroll animation. ---
    window.scroll_region(
        &mut batcher,
        GridRect::from_min_max((0u64, 0u64), (40u64, 6u64)),
        GridSize::new(0i64, 1i64),
    );

    // --- Cursor at a specific grid position, fully hand-built (all fields are `pub`). ---
    let cursor = Cursor {
        grid_position: (5, 1),
        parent_window_id: 1,
        shape: CursorShape::Block,
        enabled: true,
        ..Cursor::new()
    };
    batcher.queue(DrawCommand::UpdateCursor(cursor));

    // --- Pull the batch out of the DrawCommandBatcher *without* a winit EventLoopProxy (the
    // seam added in this pass: `DrawCommandBatcher::take_batch`) and feed it to the Renderer,
    // exactly like `WinitWindowWrapper::handle_draw_commands` does with a batch it received off
    // the winit event loop. ---
    let batch = batcher.take_batch();
    let result = renderer.handle_draw_commands(batch);
    println!("handle_draw_commands result: should_show={}", result.should_show);

    // --- `RenderedWindow::line_text_range` is the only public "read the grid content back"
    // accessor, and it reads `Line::cells` — which `Window::redraw_line` (src/editor/window.rs)
    // only ever populates for `WindowType::Message` grids, never `WindowType::Editor` ones. So for
    // our `WindowType::Editor` grid 1 above, this *always* returns `None`, regardless of whether
    // the content landed — there is no public API to read an editor grid's text/highlight state
    // back out short of actually painting a frame to a real `skia_safe::Canvas` and inspecting
    // pixels. Demonstrated here rather than just asserted: ---
    let rendered = renderer.rendered_windows.get(&1).expect("grid 1 should exist");
    println!(
        "editor-grid line_text_range (expected None — see comment above): row0={:?} row1={:?}",
        rendered.line_text_range(0, 0, 20),
        rendered.line_text_range(1, 0, 20)
    );

    // Same content, same `Window::draw_grid_line` call, but on a `WindowType::Message` grid: now
    // `line_text_range` *does* read it back, confirming the theory above precisely.
    let mut msg_batcher = DrawCommandBatcher::new();
    let mut msg_window = Window::new(
        2,
        WindowType::Message { scrolled: false },
        None,
        (0.0, 0.0),
        (40, 1),
        &mut msg_batcher,
    );
    msg_window.draw_grid_line(
        &mut msg_batcher,
        0,
        0,
        vec![GridLineCell {
            text: "read me back".to_string(),
            highlight_id: Some(0),
            repeat: None,
        }],
        &defined_styles,
    );
    renderer.handle_draw_commands(msg_batcher.take_batch());
    let msg_rendered = renderer.rendered_windows.get(&2).expect("grid 2 should exist");
    println!("message-grid line_text_range: {:?}", msg_rendered.line_text_range(0, 0, 20));

    // --- Tick the animation loop a few times (scroll + cursor blink/move animations settle over
    // multiple frames), the same two calls `WinitWindowWrapper::animate_frame`/`draw_frame` make
    // each frame — just without a real Skia canvas to paint into. ---
    let grid_rect = GridRect::from_min_max((0.0f32, 0.0), (40.0, 6.0));
    for i in 0..5 {
        let animating = renderer.animate_frame(&grid_rect, 1.0 / 60.0);
        renderer.prepare_lines(false);
        println!(
            "frame {i}: animating={animating} cursor_destination={:?}",
            renderer.get_cursor_destination()
        );
        if !animating {
            break;
        }
    }
    println!("done");
}
