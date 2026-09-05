use crate::{
    editor::DrawCommand,
    window::{EventPayload, RouteId},
};

use winit::event_loop::EventLoopProxy;

pub struct DrawCommandBatcher {
    batch: Vec<DrawCommand>,
    enabled: bool,
    queued: Vec<Vec<DrawCommand>>,
}

impl DrawCommandBatcher {
    pub fn new() -> DrawCommandBatcher {
        Self { batch: Vec::new(), enabled: true, queued: Vec::new() }
    }

    pub fn queue(&mut self, draw_command: DrawCommand) {
        self.batch.push(draw_command);
    }

    /// Take the currently queued batch of [`DrawCommand`]s without routing it through a winit
    /// [`EventLoopProxy`]. This exists for callers that drive a [`crate::renderer::Renderer`]
    /// directly (e.g. embedding it outside of Neovide's own winit-owned event loop, or feeding it
    /// hand-constructed content) and therefore have no `EventLoopProxy<EventPayload>` to hand to
    /// [`Self::send_batch`]. It intentionally ignores the `enabled`/`queued` bookkeeping used by
    /// `NeovideSetRedraw`, since that mechanism only matters when commands are routed through the
    /// event loop.
    ///
    /// Unused by the `neovide` binary itself (hence `#[allow(dead_code)]` here: the bin crate's
    /// own copy of this module has no caller for it) — it's for the `neovide` *library* target's
    /// consumers, where a `pub fn` is reachable API and not flagged as dead code.
    #[allow(dead_code)]
    pub fn take_batch(&mut self) -> Vec<DrawCommand> {
        self.batch.split_off(0)
    }

    pub fn set_enabled(
        &mut self,
        enabled: bool,
        route_id: RouteId,
        proxy: &EventLoopProxy<EventPayload>,
    ) {
        log::info!("Set redraw {enabled}");
        if enabled && !self.enabled {
            for queued in self.queued.drain(..) {
                proxy.send_event(EventPayload::for_route(queued.into(), route_id)).ok();
            }
        }
        self.enabled = enabled;
    }

    pub fn send_batch(&mut self, route_id: RouteId, proxy: &EventLoopProxy<EventPayload>) {
        if self.enabled {
            proxy
                .send_event(EventPayload::for_route(self.batch.split_off(0).into(), route_id))
                .ok();
        } else {
            self.queued.push(self.batch.split_off(0));
        }
    }
}
