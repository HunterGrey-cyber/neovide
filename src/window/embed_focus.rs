//! Window focus and pane focus are separate states for an embedding application: the window can be key while the
//! application's own view holds the keys. nvim hears FocusLost when the view takes the keys and FocusGained when
//! the editor gets them back, and the cursor is drawn unfocused in between. Declared from `window_wrapper.rs` so
//! it can reach the route's state.
use winit::{event::WindowEvent, window::WindowId};

use super::WinitWindowWrapper;
use crate::{embed::EmbedEvent, window::EventTarget};

impl WinitWindowWrapper {
    pub(super) fn handle_embed_event(&mut self, target: EventTarget, event: EmbedEvent) {
        let Some(window_id) = self.resolve_target_window_id(target) else {
            return;
        };
        match event {
            EmbedEvent::PaneFocus { editor } => self.set_editor_pane_focused(window_id, editor),
            // Nothing to do here: the event marks the window for a frame, and the frame's
            // `prepare_frame` reads the new padding.
            EmbedEvent::Layout => {}
        }
    }

    /// Takes back the focus the renderer was told about when a window focus change is absorbed.
    pub(super) fn keep_cursor_unfocused(&self, window_id: WindowId) {
        if let Some(route) = self.routes.get(&window_id) {
            route.window.renderer.borrow_mut().handle_event(&WindowEvent::Focused(false));
        }
    }

    fn set_editor_pane_focused(&mut self, window_id: WindowId, editor: bool) {
        let Some(route) = self.routes.get(&window_id) else {
            return;
        };
        let window_focused = route.window.winit_window.has_focus();
        // nvim only hears about the pane while the window itself has focus; otherwise the
        // window's own FocusLost already told it.
        if window_focused {
            if editor {
                self.handle_focus_gained(window_id);
            } else {
                self.handle_focus_lost(window_id);
            }
        }
        if let Some(route) = self.routes.get(&window_id) {
            route
                .window
                .renderer
                .borrow_mut()
                .handle_event(&WindowEvent::Focused(editor && window_focused));
        }
        if editor {
            self.update_ime_position(window_id, true);
        }
    }
}
