//! A hand-written `zwp_text_input_v3` implementation, i.e. the compositor acting as an
//! input method itself.
//!
//! Why not Smithay's `text_input` module: that module only forwards the application's
//! input-method state to a real IME client (fcitx et al., over `zwp_input_method_v2`).
//! `TextInputHandle` exposes focus/enter/leave/done but no way to submit an arbitrary
//! string. What we want is exactly "the compositor is the IME, push a Unicode string
//! in", so implementing the application side directly is shorter and more controllable.
//!
//! Capability: `commit_string` can deliver any Unicode (CJK, emoji) regardless of the
//! keyboard layout. Preedit is not supported because we need no candidate window.

use std::collections::HashMap;

use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_manager_v3::{
    self as mgr, ZwpTextInputManagerV3,
};
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::{
    self as ti, ZwpTextInputV3,
};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::Agent;

struct Inst {
    res: ZwpTextInputV3,
    enabled: bool,
    entered: bool,
}

#[derive(Default)]
pub struct Inputs {
    inst: HashMap<smithay::reexports::wayland_server::backend::ObjectId, Inst>,
    focus: Option<WlSurface>,
    serial: u32,
    global: Option<GlobalId>,
}

impl Inputs {
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<Agent, ZwpTextInputManagerV3, ()>(1, ());
        Self {
            global: Some(global),
            ..Default::default()
        }
    }

    pub fn set_global(&mut self, g: GlobalId) {
        self.global = Some(g);
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    /// Focus change: send leave to the old surface and enter to the new one.
    pub fn set_focus(&mut self, focus: Option<WlSurface>) {
        if self.focus.as_ref().map(|s| s.id()) == focus.as_ref().map(|s| s.id()) {
            return;
        }
        let old = self.focus.clone();
        for inst in self.inst.values_mut() {
            if inst.entered {
                if let Some(f) = old.as_ref() {
                    inst.res.leave(f);
                }
                let s = self.serial.wrapping_add(1).max(1);
                self.serial = s;
                inst.res.done(s);
                inst.entered = false;
            }
        }
        self.focus = focus.clone();
        if let Some(surface) = focus {
            for inst in self.inst.values_mut() {
                if inst.enabled && inst.res.id().same_client_as(&surface.id()) {
                    inst.res.enter(&surface);
                    let s = self.serial.wrapping_add(1).max(1);
                    self.serial = s;
                    inst.res.done(s);
                    inst.entered = true;
                }
            }
        }
    }

    /// Find a usable input context and deliver `text` with commit_string.
    /// Returns false when the window has no text-input (a terminal, say); the caller
    /// then falls back to the keycode path.
    pub fn commit(&mut self, surface: &WlSurface, text: &str) -> bool {
        let serial = self.next_serial();
        let mut hit = false;
        for inst in self.inst.values_mut() {
            if !inst.enabled || !inst.res.id().same_client_as(&surface.id()) {
                continue;
            }
            if !inst.entered {
                inst.res.enter(surface);
                inst.entered = true;
            }
            inst.res.commit_string(Some(text.to_string()));
            inst.res.done(serial);
            hit = true;
            break;
        }
        hit
    }

    /// Fallback check: some clients implement text-input but have not enabled it
    pub fn available(&self, surface: &WlSurface) -> bool {
        self.inst
            .values()
            .any(|i| i.enabled && i.res.id().same_client_as(&surface.id()))
    }
}

// ---------------------------------------------------------------- dispatch

impl GlobalDispatch<ZwpTextInputManagerV3, (), Agent> for Inputs {
    fn bind(
        _state: &mut Agent,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpTextInputManagerV3>,
        _data: &(),
        data_init: &mut DataInit<'_, Agent>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpTextInputManagerV3, (), Agent> for Inputs {
    fn request(
        state: &mut Agent,
        _client: &Client,
        _resource: &ZwpTextInputManagerV3,
        request: mgr::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Agent>,
    ) {
        if let mgr::Request::GetTextInput { id, .. } = request {
            let res = data_init.init(id, ());
            state.text_inputs.inst.insert(
                res.id(),
                Inst {
                    res,
                    enabled: false,
                    entered: false,
                },
            );
        }
    }
}

impl Dispatch<ZwpTextInputV3, (), Agent> for Inputs {
    fn request(
        state: &mut Agent,
        _client: &Client,
        resource: &ZwpTextInputV3,
        request: ti::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Agent>,
    ) {
        match request {
            ti::Request::Enable => {
                if let Some(inst) = state.text_inputs.inst.get_mut(&resource.id()) {
                    inst.enabled = true;
                }
            }
            ti::Request::Disable => {
                if let Some(inst) = state.text_inputs.inst.get_mut(&resource.id()) {
                    inst.enabled = false;
                    if inst.entered {
                        if let Some(f) = state.text_inputs.focus.clone() {
                            inst.res.leave(&f);
                        }
                        inst.entered = false;
                    }
                }
            }
            ti::Request::Commit => {
                // The client committed its enable/disable state; if it is the focused one, add enter + done
                let focused = state
                    .text_inputs
                    .focus
                    .as_ref()
                    .filter(|s| s.id().same_client_as(&resource.id()))
                    .cloned();
                let serial = state.text_inputs.next_serial();
                if let (Some(surface), Some(inst)) =
                    (focused, state.text_inputs.inst.get_mut(&resource.id()))
                {
                    if inst.enabled && !inst.entered {
                        inst.res.enter(&surface);
                        inst.entered = true;
                    }
                    inst.res.done(serial);
                }
            }
            ti::Request::Destroy => {
                state.text_inputs.inst.remove(&resource.id());
            }
            _ => {}
        }
    }
}

// Make Agent a valid dispatch host (what smithay's delegate_* macros would generate)
impl GlobalDispatch<ZwpTextInputManagerV3, (), Agent> for Agent {
    fn bind(
        state: &mut Agent,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwpTextInputManagerV3>,
        data: &(),
        data_init: &mut DataInit<'_, Agent>,
    ) {
        <Inputs as GlobalDispatch<ZwpTextInputManagerV3, (), Agent>>::bind(
            state, dh, client, resource, data, data_init,
        )
    }
}

impl Dispatch<ZwpTextInputManagerV3, (), Agent> for Agent {
    fn request(
        state: &mut Agent,
        client: &Client,
        resource: &ZwpTextInputManagerV3,
        request: mgr::Request,
        data: &(),
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Agent>,
    ) {
        <Inputs as Dispatch<ZwpTextInputManagerV3, (), Agent>>::request(
            state, client, resource, request, data, dh, data_init,
        )
    }
}

impl Dispatch<ZwpTextInputV3, (), Agent> for Agent {
    fn request(
        state: &mut Agent,
        client: &Client,
        resource: &ZwpTextInputV3,
        request: ti::Request,
        data: &(),
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Agent>,
    ) {
        <Inputs as Dispatch<ZwpTextInputV3, (), Agent>>::request(
            state, client, resource, request, data, dh, data_init,
        )
    }
}
