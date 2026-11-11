// agentc — an experimental headless Wayland compositor.
// Goal: prove out "each window owns its own full screen + input injected in window-local
// coordinates".
// Notable: no renderer / EGL / dmabuf / output; pure wl_shm. A screenshot is simply the
// buffer the client itself submitted.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smithay::backend::input::KeyState as SKeyState;
use smithay::input::keyboard::{FilterResult, Keycode, KeysymHandle, ModifiersState};
use smithay::backend::input::{Axis, AxisSource};
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::calloop::channel as cl_channel;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::backend::{ClientData, GlobalId};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_shm, wl_surface};
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::utils::{Logical, Point, Serial, Transform, SERIAL_COUNTER};
use smithay::desktop::{PopupKind, PopupManager};
use smithay::wayland::compositor::SubsurfaceCachedState;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    self, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes, TraversalAction,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::socket::ListeningSocketSource;

mod mcp;
mod shim;
mod textinput;
mod ws;

const DW: i32 = 1280;
const DH: i32 = 720;
const MIN_W: i32 = 640;
const MIN_H: i32 = 480;
const MAX_W: i32 = 2560;
const MAX_H: i32 = 1440;
/// Change-detection block size.
const BLK: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WinMode {
    Windowed,
    Maximized,
    Fullscreen,
}

// ---------------------------------------------------------------- client data

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}
impl ClientData for ClientState {}

// ---------------------------------------------------------------- surface img

struct Image {
    w: usize,
    h: usize,
    stride: usize,
    alpha: bool, // wl_shm ARGB8888 is premultiplied alpha
    data: Vec<u8>, // BGRA / BGRX, little endian
}

#[derive(Default)]
struct SurfaceImage(Mutex<Option<Image>>);

fn image_from_buffer(buffer: &wl_buffer::WlBuffer) -> Option<Image> {
    let r = smithay::wayland::shm::with_buffer_contents(buffer, |ptr, len, spec| {
        let stride = spec.stride as usize;
        let h = spec.height as usize;
        let w = spec.width as usize;
        let off = spec.offset as usize;
        if w == 0 || h == 0 || off + stride * h > len {
            return None;
        }
        let mut data = vec![0u8; stride * h];
        unsafe { std::ptr::copy_nonoverlapping(ptr.add(off), data.as_mut_ptr(), stride * h) };
        let alpha = spec.format == wl_shm::Format::Argb8888;
        Some(Image { w, h, stride, alpha, data })
    });
    r.ok().flatten()
}

fn set_image(surface: &wl_surface::WlSurface, img: Image) {
    compositor::with_states(surface, |data| {
        if let Some(slot) = data.data_map.get::<SurfaceImage>() {
            *slot.0.lock().unwrap() = Some(img);
        }
    });
}

/// Read only the size, without copying pixels.
fn image_size(surface: &wl_surface::WlSurface) -> Option<(i32, i32)> {
    compositor::with_states(surface, |data| {
        data.data_map
            .get::<SurfaceImage>()
            .and_then(|s| s.0.lock().unwrap().as_ref().map(|i| (i.w as i32, i.h as i32)))
    })
    .filter(|(w, h)| *w > 0 && *h > 0)
}

#[allow(dead_code)]
fn get_image(surface: &wl_surface::WlSurface) -> Option<Image> {
    compositor::with_states(surface, |data| {
        data.data_map
            .get::<SurfaceImage>()
            .and_then(|s| s.0.lock().unwrap().as_ref().map(|i| Image {
                w: i.w,
                h: i.h,
                stride: i.stride,
                alpha: i.alpha,
                data: i.data.clone(),
            }))
    })
}

// ---------------------------------------------------------------- state

struct Window {
    id: u32,
    toplevel: ToplevelSurface,
    title: String,
    app_id: String,
    size: (i32, i32),
    mode: WinMode,
    /// Bumped on resize; acting on stale revision coordinates is rejected.
    revision: u64,
    /// Block hashes of the latest render; invalidated by the commit counter so it is not
    /// recomputed every tick.
    grid: Option<GridCache>,
    /// Block hashes of the last frame pushed to the client (used to compute deltas).
    sent_grid: Option<Vec<u64>>,
    sent_size: (i32, i32),
}

struct GridCache {
    commits: u64,
    cells: Vec<u64>,
    gw: usize,
    gh: usize,
    area: f64,
}

/// One injected action. Everything is tick-driven; we never sleep inside the event loop,
/// because while blocked we cannot flush events or process human input from the panel.
#[derive(Clone, Copy)]
enum Act {
    Move(f64, f64),
    Down(u32),
    Up(u32),
    Key(u32, bool),
}

#[derive(Clone, Copy)]
struct Step {
    act: Act,
    /// How long to wait after this step before the next one. This is what shapes the
    /// rhythm, e.g. the press->release gap.
    wait_ms: u64,
}

/// A deferred reply: an action sequence, waiting for the screen to change, waiting for
/// the screen to settle, and so on.
enum Pending {
    Sleep {
        at: Instant,
        sender: std_mpsc::Sender<String>,
    },
    Change {
        w: u32,
        deadline: Instant,
        baseline: Option<Vec<u64>>,
        seen_commits: u64,
        min_area: f64,
        sender: std_mpsc::Sender<String>,
        with_image: bool,
    },
    /// A sequence of injected actions, one step per tick (so each step is flushed to the
    /// client in between).
    Seq {
        w: u32,
        steps: std::sync::Arc<Vec<Step>>,
        idx: usize,
        at: Instant,
        sender: std_mpsc::Sender<String>,
    },
    /// Wait for a new window to appear (launch, or a dialog after a click).
    Window {
        deadline: Instant,
        since_id: u32,
        app_id: Option<String>,
        sender: std_mpsc::Sender<String>,
    },
    /// For the live preview: wait for the screen to change and answer with one PNG frame.
    Stream {
        w: u32,
        deadline: Instant,
        since: u64,
        /// Force a full frame. On window switch the browser canvas already holds other
        /// content, so a delta would land in the wrong place.
        full: bool,
        sender: std_mpsc::Sender<String>,
    },
    /// Clipboard paste: set the selection, wait a frame so the client receives the offer,
    /// then send Ctrl+V.
    Paste {
        w: u32,
        at: Instant,
        /// Trailing newlines are sent as Return separately, otherwise a terminal inserts
        /// them literally (bracketed paste).
        enters: u32,
        sender: std_mpsc::Sender<String>,
    },
    Idle {
        w: u32,
        deadline: Instant,
        grace: Instant,
        quiet_ms: u64,
        last_change: Instant,
        baseline: Option<Vec<u64>>,
        seen_commits: u64,
        min_area: f64,
        sender: std_mpsc::Sender<String>,
        with_image: bool,
    },
}

/// Cut the frame into 16x16 blocks and hash each one. Used to answer "did anything
/// change, and where".
fn block_grid(canvas: &[u8], (w, h): (i32, i32)) -> (Vec<u64>, usize, usize) {
    let gw = (w as usize).div_ceil(BLK);
    let gh = (h as usize).div_ceil(BLK);
    let stride = w as usize * 4;
    let mut grid = vec![0u64; gw * gh];
    const FNV: u64 = 0x100000001b3;
    for by in 0..gh {
        let y0 = by * BLK;
        let y1 = ((by + 1) * BLK).min(h as usize);
        for bx in 0..gw {
            let x0 = bx * BLK * 4;
            let x1 = (((bx + 1) * BLK).min(w as usize)) * 4;
            let mut hsh = 0xcbf29ce484222325u64;
            for y in y0..y1 {
                let row = &canvas[y * stride + x0..y * stride + x1];
                let mut it = row.chunks_exact(8);
                for c in &mut it {
                    hsh = hsh.wrapping_mul(FNV).wrapping_add(u64::from_ne_bytes([
                        c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
                    ]));
                }
                for c in it.remainder().chunks_exact(4) {
                    hsh = hsh
                        .wrapping_mul(FNV)
                        .wrapping_add(u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) as u64);
                }
            }
            grid[by * gw + bx] = hsh;
        }
    }
    (grid, gw, gh)
}

/// Fold the whole-frame block hashes into one number, to tell whether this is the same
/// frame as the last one.
fn grid_hash(cells: &[u64]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for c in cells {
        h ^= c;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Diff two frames, returning changed rects (greedily merged into as few blocks as
/// possible) and the changed pixel area.
fn diff_grid(a: &[u64], b: &[u64], gw: usize, gh: usize) -> (Vec<(i32, i32, i32, i32)>, usize) {
    let mut mask = vec![false; gw * gh];
    let mut blocks = 0usize;
    for i in 0..gw * gh {
        if a.get(i) != b.get(i) {
            mask[i] = true;
            blocks += 1;
        }
    }
    let mut rects = Vec::new();
    for by in 0..gh {
        for bx in 0..gw {
            if !mask[by * gw + bx] {
                continue;
            }
            let mut w = 1;
            while bx + w < gw && mask[by * gw + bx + w] {
                w += 1;
            }
            let mut h = 1;
            'grow: while by + h < gh {
                for k in 0..w {
                    if !mask[(by + h) * gw + bx + k] {
                        break 'grow;
                    }
                }
                h += 1;
            }
            for yy in by..by + h {
                for xx in bx..bx + w {
                    mask[yy * gw + xx] = false;
                }
            }
            rects.push((
                (bx * BLK) as i32,
                (by * BLK) as i32,
                (w * BLK) as i32,
                (h * BLK) as i32,
            ));
        }
    }
    (rects, blocks * BLK * BLK)
}

pub struct Agent {
    dh: DisplayHandle,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    data_device_state: DataDeviceState,
    primary_selection_state: PrimarySelectionState,
    seat_state: SeatState<Self>,
    seat: Seat<Self>,
    start: Instant,
    windows: Vec<Window>,
    next_id: u32,
    socket_name: String,
    popups: PopupManager,
    keymap_table: std::collections::HashMap<char, (u32, bool)>,
    shift_code: u32,
    paused: bool,
    active: u32,
    log: std::collections::VecDeque<String>,
    waits: Vec<Pending>,
    commits: u64,
    /// Remember the target surface on press: Wayland's implicit grab keeps sending events
    /// to it for the whole drag.
    grab: Option<(wl_surface::WlSurface, (f64, f64))>,
    /// Buttons that are logically held. Released before any new action sequence so a
    /// button can never stay stuck down.
    held_buttons: Vec<u32>,
    /// Window event log with sequence numbers. Tool replies and the proactive push keep
    /// independent cursors so neither steals from the other.
    windows_log: Vec<(u64, String)>,
    win_seq: u64,
    /// Highest sequence already reported in a tool reply.
    reply_cursor: u64,
    /// The surface the last motion landed on, plus its origin in window coordinates.
    last_target: Option<(wl_surface::WlSurface, (f64, f64))>,
    prof: (u64, u64, u64, u64), // frame_grid count/us, encode count/us
    tick_n: u64,
    text_inputs: textinput::Inputs,
    /// A permanent output: applications enumerate monitors before creating a window, and
    /// without one foot and friends simply exit.
    output: Output,
    #[allow(dead_code)]
    output_global: GlobalId,
    default_size: (i32, i32),
    default_mode: WinMode,
}

impl Agent {
    fn new(dh: DisplayHandle, socket_name: String) -> Self {
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(
            &dh,
            vec![wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888],
        );
        let text_inputs = textinput::Inputs::new(&dh);

        let output = Output::new(
            "agentc-0".to_string(),
            PhysicalProperties {
                size: (DW / 10, DH / 10).into(),
                subpixel: Subpixel::Unknown,
                make: "agentc".to_string(),
                model: "virtual".to_string(),
            },
        );
        let omode = Mode {
            size: (DW, DH).into(),
            refresh: 60_000,
        };
        output.set_preferred(omode);
        output.change_current_state(
            Some(omode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        let output_global = output.create_global::<Self>(&dh);

        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "agent");
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        seat.add_pointer();

        let (keymap_table, shift_code) = build_keymap_table();
        Self {
            dh,
            compositor_state,
            xdg_shell_state,
            shm_state,
            data_device_state,
            primary_selection_state,
            seat_state,
            seat,
            start: Instant::now(),
            windows: Vec::new(),
            next_id: 0,
            socket_name,
            popups: PopupManager::default(),
            keymap_table,
            shift_code,
            paused: false,
            active: 0,
            log: std::collections::VecDeque::new(),
            waits: Vec::new(),
            commits: 0,
            grab: None,
            held_buttons: Vec::new(),
            windows_log: Vec::new(),
            win_seq: 0,
            reply_cursor: 0,
            last_target: None,
            prof: (0, 0, 0, 0),
            tick_n: 0,
            text_inputs,
            output,
            output_global,
            default_size: (DW, DH),
            default_mode: WinMode::Maximized,
        }
    }

    /// Change size and/or window state, then reconfigure. Returns the new revision.
    fn reconfigure(
        &mut self,
        id: u32,
        new_size: Option<(i32, i32)>,
        new_mode: Option<WinMode>,
    ) -> Option<u64> {
        let idx = self.windows.iter().position(|w| w.id == id)?;
        if let Some((w, h)) = new_size {
            self.windows[idx].size = (
                w.clamp(MIN_W, MAX_W),
                h.clamp(MIN_H, MAX_H),
            );
        }
        if let Some(m) = new_mode {
            self.windows[idx].mode = m;
        }
        let (size, mode) = (self.windows[idx].size, self.windows[idx].mode);
        let toplevel = self.windows[idx].toplevel.clone();

        toplevel.with_pending_state(|st| {
            st.size = Some(size.into());
            st.states.set(xdg_toplevel::State::Activated);
            match mode {
                WinMode::Windowed => {
                    st.states.unset(xdg_toplevel::State::Maximized);
                    st.states.unset(xdg_toplevel::State::Fullscreen);
                }
                WinMode::Maximized => {
                    st.states.set(xdg_toplevel::State::Maximized);
                    st.states.unset(xdg_toplevel::State::Fullscreen);
                }
                WinMode::Fullscreen => {
                    st.states.set(xdg_toplevel::State::Fullscreen);
                    st.states.unset(xdg_toplevel::State::Maximized);
                }
            }
        });
        toplevel.send_configure();
        self.windows[idx].revision += 1;
        Some(self.windows[idx].revision)
    }

    fn window_json(&self, id: u32) -> Option<String> {
        let w = self.windows.iter().find(|w| w.id == id)?;
        let (cw, ch) = image_size(w.toplevel.wl_surface()).unwrap_or(w.size);
        Some(format!(
            "{{\"id\":{},\"app_id\":{:?},\"title\":{:?},\"w\":{cw},\"h\":{ch},\"dialog\":{},\"revision\":{}}}",
            w.id,
            w.app_id,
            w.title,
            w.toplevel.parent().is_some(),
            w.revision
        ))
    }

    fn window_surface(&self, id: u32) -> Option<wl_surface::WlSurface> {
        self.windows
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.toplevel.wl_surface().clone())
    }

    // ---- Screenshot: toplevel + subsurfaces + every popup, software-composited into RGBA
    fn render_window(&self, id: u32) -> Option<(Vec<u8>, u32, u32)> {
        let win = self.windows.iter().find(|w| w.id == id)?;
        let surface = win.toplevel.wl_surface().clone();
        // A fixed-size window (a dialog) commits a buffer much smaller than the configured
        // size. Use its own content size as the canvas, otherwise the shot is mostly black.
        let (ww, wh) = image_size(&surface).unwrap_or(win.size);
        let mut canvas = vec![0u8; ww as usize * wh as usize * 4];
        for px in canvas.chunks_exact_mut(4) {
            px[3] = 255;
        }

        let mut painted = compose_into(&mut canvas, (ww, wh), &surface, Point::from((0, 0)));

        // Popups (menus, dropdowns) are separate surfaces hanging off the toplevel's popup
        // tree. The compositor already resolved their absolute offsets, so use those.
        let popups: Vec<(PopupKind, Point<i32, Logical>)> =
            PopupManager::popups_for_surface(&surface).collect();
        for (kind, loc) in popups {
            let psurf = kind.wl_surface().clone();
            if compose_into(&mut canvas, (ww, wh), &psurf, loc) {
                painted = true;
            }
        }

        if !painted {
            return None;
        }
        Some((canvas, ww as u32, wh as u32))
    }

    /// Find the topmost surface at (x,y). While a popup is open the click must go to the
    /// popup itself, otherwise picking a menu item would hit the window behind it.
    fn target_at(&self, id: u32, x: f64, y: f64) -> Option<(wl_surface::WlSurface, f64, f64)> {
        // After a press the pointer is implicitly grabbed: dragging outside the widget or
        // over a popup must still go to the original surface, or the drag breaks on the
        // first out-of-bounds event.
        if let Some((surface, (ox, oy))) = &self.grab {
            return Some((surface.clone(), x - ox, y - oy));
        }
        let toplevel = self.window_surface(id)?;
        let popups: Vec<(PopupKind, Point<i32, Logical>)> =
            PopupManager::popups_for_surface(&toplevel).collect();
        for (kind, off) in popups.iter().rev() {
            let psurf = kind.wl_surface().clone();
            if let Some(img) = get_image(&psurf) {
                let (w, h) = (img.w as i32, img.h as i32);
                if (x as i32) >= off.x
                    && (x as i32) < off.x + w
                    && (y as i32) >= off.y
                    && (y as i32) < off.y + h
                {
                    return Some((psurf, x - off.x as f64, y - off.y as f64));
                }
            }
        }
        Some((toplevel, x, y))
    }

    fn apply_act(&mut self, id: u32, act: Act) {
        match act {
            Act::Move(x, y) => self.move_to(id, x, y),
            Act::Down(b) => self.button_only(true, b),
            Act::Up(b) => self.button_only(false, b),
            Act::Key(code, pressed) => {
                if let Some(kbd) = self.seat.get_keyboard() {
                    let time = self.start.elapsed().as_millis() as u32;
                    kbd.input::<(), _>(
                        self,
                        Keycode::new(code),
                        if pressed {
                            SKeyState::Pressed
                        } else {
                            SKeyState::Released
                        },
                        SERIAL_COUNTER.next_serial(),
                        time,
                        filter,
                    );
                }
            }
        }
    }

    /// Send only the button event (position taken from the last motion).
    fn button_only(&mut self, pressed: bool, button: u32) {
        let Some((surface, _origin)) = self.last_target.clone() else {
            return;
        };
        if pressed {
            self.grab = Some((surface, (0.0, 0.0)));
            if !self.held_buttons.contains(&button) {
                self.held_buttons.push(button);
            }
        } else {
            self.grab = None;
            self.held_buttons.retain(|b| *b != button);
        }
        let pointer = self.seat.get_pointer().unwrap();
        let time = self.start.elapsed().as_millis() as u32;
        let _ = surface;
        pointer.button(
            self,
            &ButtonEvent {
                time,
                button,
                serial: SERIAL_COUNTER.next_serial(),
                state: if pressed {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
            },
        );
        pointer.frame(self);
    }

    fn button_evt(&mut self, id: u32, x: f64, y: f64, button: u32, pressed: bool) {
        let Some((surface, x, y)) = self.target_at(id, x, y) else {
            return;
        };
        let pointer = self.seat.get_pointer().unwrap();
        let time = self.start.elapsed().as_millis() as u32;
        pointer.motion(
            self,
            Some((surface, Point::from((0.0, 0.0)))),
            &MotionEvent {
                location: Point::from((x, y)),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        pointer.button(
            self,
            &ButtonEvent {
                time,
                button,
                serial: SERIAL_COUNTER.next_serial(),
                state: if pressed {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
            },
        );
        pointer.frame(self);
    }

    fn move_to(&mut self, id: u32, x: f64, y: f64) {
        let (x0, y0) = (x, y);
        let Some((surface, x, y)) = self.target_at(id, x, y) else {
            return;
        };
        self.last_target = Some((surface.clone(), (x0 - x, y0 - y)));
        let pointer = self.seat.get_pointer().unwrap();
        let time = self.start.elapsed().as_millis() as u32;
        pointer.motion(
            self,
            Some((surface, Point::from((0.0, 0.0)))),
            &MotionEvent {
                location: Point::from((x, y)),
                serial: 0.into(),
                time,
            },
        );
        pointer.frame(self);
    }

    fn scroll(&mut self, id: u32, x: f64, y: f64, dy: f64) {
        let Some((surface, x, y)) = self.target_at(id, x, y) else {
            return;
        };
        let pointer = self.seat.get_pointer().unwrap();
        let time = self.start.elapsed().as_millis() as u32;
        pointer.motion(
            self,
            Some((surface, Point::from((0.0, 0.0)))),
            &MotionEvent {
                location: Point::from((x, y)),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
        frame = frame.value(Axis::Vertical, dy);
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    fn focus(&mut self, id: u32) -> bool {
        let Some(t) = self
            .windows
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.toplevel.clone())
        else {
            return false;
        };
        let surface = t.wl_surface().clone();
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(kbd) = self.seat.get_keyboard() {
            kbd.set_focus(self, Some(surface.clone()), serial);
        }
        // Clipboard is keyed on the focused client: without telling data_device who has
        // focus, the client never receives a selection offer and paste comes up empty.
        smithay::wayland::selection::data_device::set_data_device_focus::<Self>(
            &self.dh,
            &self.seat,
            surface.client(),
        );
        self.text_inputs.set_focus(Some(surface));
        // Re-send the activated state on focus change. Some GTK behaviour (menu buttons,
        // for instance) depends on the toplevel being activated; sending it once in the
        // first configure is not enough.
        t.with_pending_state(|st| {
            st.states.set(xdg_toplevel::State::Activated);
        });
        t.send_configure();
        true
    }

    fn key(&mut self, id: u32, keycode: u32, shift: bool) {
        let Some(surface) = self.window_surface(id) else {
            return;
        };
        self.focus(id);
        let _ = surface;
        let keyboard = self.seat.get_keyboard().unwrap();
        let time = self.start.elapsed().as_millis() as u32;

        if shift {
            keyboard.input::<(), _>(
                self,
                Keycode::new(self.shift_code),
                SKeyState::Pressed,
                SERIAL_COUNTER.next_serial(),
                time,
                filter,
            );
        }
        keyboard.input::<(), _>(
            self,
            Keycode::new(keycode),
            SKeyState::Pressed,
            SERIAL_COUNTER.next_serial(),
            time,
            filter,
        );
        keyboard.input::<(), _>(
            self,
            Keycode::new(keycode),
            SKeyState::Released,
            SERIAL_COUNTER.next_serial(),
            time,
            filter,
        );
        if shift {
            keyboard.input::<(), _>(
                self,
                Keycode::new(self.shift_code),
                SKeyState::Released,
                SERIAL_COUNTER.next_serial(),
                time,
                filter,
            );
        }
    }

    /// Expand text into a key action sequence (no sleeping here any more).
    fn type_steps(&self, text: &str) -> Vec<Step> {
        let mut steps = Vec::new();
        for ch in text.chars() {
            let Some((code, shift)) = self.keymap_table.get(&ch).copied() else {
                eprintln!("[agentc] cannot type char {ch:?}");
                continue;
            };
            if shift {
                steps.push(Step { act: Act::Key(self.shift_code, true), wait_ms: 8 });
            }
            steps.push(Step { act: Act::Key(code, true), wait_ms: 8 });
            steps.push(Step { act: Act::Key(code, false), wait_ms: 8 });
            if shift {
                steps.push(Step { act: Act::Key(self.shift_code, false), wait_ms: 8 });
            }
        }
        steps
    }

    fn defer(&mut self, w: u32, steps: Vec<Step>, sender: std_mpsc::Sender<String>) {
        let steps = self.release_stuck(steps);
        self.waits.push(Pending::Seq {
            w,
            steps: std::sync::Arc::new(steps),
            idx: 0,
            at: Instant::now(),
            sender,
        });
    }

    /// Release keys the previous action left down (e.g. it was interrupted by a new call).
    /// Otherwise the button stays pressed forever - in SimulIDE that means zoom runs to the
    /// end. Also: attach newly appeared windows to the reply so the model does not have to
    /// poll `windows` to discover a dialog.
    fn finish(&mut self, body: String) -> String {
        let dbg = std::env::var_os("AGENTC_DBG").is_some();
        if dbg {
            eprintln!("[finish] seq={} cur={} log={}", self.win_seq, self.reply_cursor, self.windows_log.len());
        }
        if self.win_seq == self.reply_cursor {
            return body;
        }
        let from = self.reply_cursor;
        self.reply_cursor = self.win_seq;
        let list: Vec<String> = self
            .windows_log
            .iter()
            .filter(|(seq, _)| *seq > from)
            .map(|(_, j)| j.clone())
            .collect();
        if list.is_empty() {
            return body;
        }
        let mut out = body;
        if out.ends_with('}') {
            out.pop();
            out.push_str(&format!(",\"new_windows\":[{}]}}", list.join(",")));
        }
        if dbg {
            eprintln!("[finish] -> ...{}", &out[out.len().saturating_sub(80)..]);
        }
        out
    }

    fn release_stuck(&mut self, mut steps: Vec<Step>) -> Vec<Step> {
        if self.held_buttons.is_empty() {
            return steps;
        }
        let held = std::mem::take(&mut self.held_buttons);
        let mut head: Vec<Step> = held
            .into_iter()
            .map(|b| Step {
                act: Act::Up(b),
                wait_ms: 16,
            })
            .collect();
        head.append(&mut steps);
        head
    }

    fn poll_pending(&mut self, p: Pending) -> Option<Pending> {
        let now = Instant::now();
        match p {
            Pending::Sleep { at, sender } => {
                if now >= at {
                    let _ = sender.send(self.finish("{\"ok\":true}".to_string()));
                    None
                } else {
                    Some(Pending::Sleep { at, sender })
                }
            }
            Pending::Window {
                deadline,
                since_id,
                app_id,
                sender,
            } => {
                let hit = self
                    .windows
                    .iter()
                    .filter(|w| w.id >= since_id)
                    .filter(|w| match &app_id {
                        Some(a) => &w.app_id == a,
                        None => true,
                    })
                    .map(|w| w.id)
                    .last();
                if let Some(id) = hit {
                    let body = self
                        .window_json(id)
                        .unwrap_or_else(|| "null".to_string());
                    let _ = sender.send(self.finish(format!(
                        "{{\"ok\":true,\"timed_out\":false,\"window\":{body}}}"
                    )));
                    return None;
                }
                if now >= deadline {
                    let _ = sender.send(self.finish(
                        "{\"ok\":true,\"timed_out\":true,\"window\":null}".to_string(),
                    ));
                    return None;
                }
                Some(Pending::Window {
                    deadline,
                    since_id,
                    app_id,
                    sender,
                })
            }
            Pending::Seq {
                w,
                steps,
                idx,
                at,
                sender,
            } => {
                if now < at {
                    return Some(Pending::Seq {
                        w, steps, idx, at, sender,
                    });
                }
                if idx >= steps.len() {
                    let _ = sender.send(self.finish("{\"ok\":true}".to_string()));
                    return None;
                }
                let step = steps[idx];
                self.apply_act(w, step.act);
                let next = idx + 1;
                if next >= steps.len() {
                    let _ = sender.send(self.finish("{\"ok\":true}".to_string()));
                    return None;
                }
                Some(Pending::Seq {
                    w,
                    steps,
                    idx: next,
                    at: now + Duration::from_millis(step.wait_ms.max(1)),
                    sender,
                })
            }
            Pending::Stream {
                w,
                deadline,
                since,
                full,
                sender,
            } => {
                let Some((grid, _gw, _gh, _area)) = self.frame_grid(w) else {
                    if now >= deadline {
                        let _ = sender.send(no_frame_reply(w, true));
                        return None;
                    }
                    return Some(Pending::Stream {
                        w,
                        deadline,
                        since,
                        full,
                        sender,
                    });
                };
                let h = grid_hash(&grid);
                if h != since {
                    if let Some((canvas, cw, ch)) = self.render_window(w) {
                        // Compare with what was last pushed and encode only the changed rect
                        let (rect, full) = {
                            let win = self.windows.iter().find(|x| x.id == w);
                            match win {
                                Some(win)
                                    if !full
                                        && win.sent_size == (cw as i32, ch as i32)
                                        && win.sent_grid.as_ref().map(|g| g.len())
                                            == Some(grid.len()) =>
                                {
                                    let prev = win.sent_grid.as_ref().unwrap();
                                    let (rects, _area) = diff_grid(prev, &grid, _gw, _gh);
                                    if rects.is_empty() {
                                        (None, false)
                                    } else {
                                        let x0 = rects.iter().map(|r| r.0).min().unwrap();
                                        let y0 = rects.iter().map(|r| r.1).min().unwrap();
                                        let x1 = rects.iter().map(|r| r.0 + r.2).max().unwrap();
                                        let y1 = rects.iter().map(|r| r.1 + r.3).max().unwrap();
                                        (
                                            Some((
                                                x0.max(0),
                                                y0.max(0),
                                                (x1 - x0).min(cw as i32 - x0.max(0)),
                                                (y1 - y0).min(ch as i32 - y0.max(0)),
                                            )),
                                            false,
                                        )
                                    }
                                }
                                _ => (None, true),
                            }
                        };

                        let te = Instant::now();
                        let encoded = match rect {
                            Some((x, y, rw, rh)) => {
                                encode_png_region(&canvas, cw as i32, x, y, rw, rh, true)
                                    .map(|p| (p, x, y, rw, rh))
                            }
                            None if full => encode_png(&canvas, cw, ch, true)
                                .map(|p| (p, 0, 0, cw as i32, ch as i32)),
                            None => Ok((Vec::new(), 0, 0, 0, 0)),
                        };
                        if let Ok((png, x, y, rw, rh)) = encoded {
                            self.prof.2 += 1;
                            self.prof.3 += te.elapsed().as_micros() as u64;
                            if png.is_empty() {
                                // Should not happen (hash changed but the diff is empty);
                                // send an empty reply as a safety net
                                let _ = sender.send(format!(
                                    "{{\"ok\":true,\"changed\":false,\"hash\":{h}}}"
                                ));
                            } else {
                                let _ = sender.send(format!(
                                    "{{\"ok\":true,\"changed\":true,\"hash\":{h},\"id\":{w},\"fw\":{cw},\"fh\":{ch},\"x\":{x},\"y\":{y},\"w\":{rw},\"h\":{rh},\"png\":\"{}\"}}",
                                    b64(&png)
                                ));
                            }
                        }
                        if let Some(win) = self.windows.iter_mut().find(|x| x.id == w) {
                            win.sent_grid = Some(grid);
                            win.sent_size = (cw as i32, ch as i32);
                        }
                    }
                    return None;
                }
                if now >= deadline {
                    let _ = sender.send(format!(
                        "{{\"ok\":true,\"changed\":false,\"hash\":{h}}}"
                    ));
                    return None;
                }
                Some(Pending::Stream {
                    w,
                    deadline,
                    since,
                    full,
                    sender,
                })
            }
            Pending::Paste {
                w,
                at,
                enters,
                sender,
            } => {
                if now >= at {
                    self.paste_now(w);
                    for _ in 0..enters {
                        self.send_combo(w, &[], 36, false); // Return
                    }
                    let _ = sender.send(self.finish("{\"ok\":true,\"via\":\"clipboard\"}".to_string()));
                    None
                } else {
                    Some(Pending::Paste {
                        w,
                        at,
                        enters,
                        sender,
                    })
                }
            }
            Pending::Change {
                w,
                deadline,
                baseline,
                seen_commits,
                min_area,
                sender,
                with_image,
            } => {
                // Without a new commit there is nothing to re-render or re-hash, so a static
                // screen costs almost nothing
                let (grid, gw, gh, area) = match self.frame_grid(w) {
                    Some(v) => v,
                    None => {
                        if now >= deadline {
                            let _ = sender.send(no_frame_reply(w, true));
                            return None;
                        }
                        return Some(Pending::Change {
                            w,
                            deadline,
                            baseline,
                            seen_commits,
                            min_area,
                            sender,
                            with_image,
                        });
                    }
                };
                let dirty_since = self.commits != seen_commits;
                let base = baseline.unwrap_or_else(|| grid.clone());
                let (rects, changed_area) = diff_grid(&base, &grid, gw, gh);
                let changed = !rects.is_empty() && (changed_area as f64) >= min_area * area;

                if changed {
                    let _ = sender.send(self.finish(self.wait_reply(w, true, true, false, &rects, with_image)));
                    return None;
                }
                if now >= deadline {
                    let _ = sender.send(self.finish(self.wait_reply(w, false, false, true, &rects, with_image)));
                    return None;
                }
                Some(Pending::Change {
                    w,
                    deadline,
                    baseline: Some(if dirty_since { grid } else { base }),
                    seen_commits: self.commits,
                    min_area,
                    sender,
                    with_image,
                })
            }
            Pending::Idle {
                w,
                deadline,
                grace,
                quiet_ms,
                last_change,
                baseline,
                seen_commits,
                min_area,
                sender,
                with_image,
            } => {
                let (grid, gw, gh, area) = match self.frame_grid(w) {
                    Some(v) => v,
                    None => {
                        if now >= deadline {
                            let _ = sender.send(no_frame_reply(w, true));
                            return None;
                        }
                        return Some(Pending::Idle {
                            w,
                            deadline,
                            grace,
                            quiet_ms,
                            last_change,
                            baseline,
                            seen_commits,
                            min_area,
                            sender,
                            with_image,
                        });
                    }
                };
                let base = baseline.unwrap_or_else(|| grid.clone());
                let (rects, changed_area) = diff_grid(&base, &grid, gw, gh);
                let changed = !rects.is_empty() && (changed_area as f64) >= min_area * area;

                let mut last = last_change;
                if changed {
                    last = now;
                }
                // No settling before the grace period, or we would return before the UI has
                // even started reacting
                let quiet_enough =
                    now >= grace && now.duration_since(last).as_millis() as u64 >= quiet_ms;

                if quiet_enough && !changed {
                    let _ = sender.send(self.finish(self.wait_reply(w, false, true, false, &[], with_image)));
                    return None;
                }
                if now >= deadline {
                    let _ = sender.send(self.finish(self.wait_reply(w, false, false, true, &rects, with_image)));
                    return None;
                }
                Some(Pending::Idle {
                    w,
                    deadline,
                    grace,
                    quiet_ms,
                    last_change: last,
                    baseline: Some(grid),
                    seen_commits: self.commits,
                    min_area,
                    sender,
                    with_image,
                })
            }
        }
    }

    /// Render and compute block hashes, cached by the commit counter so each frame
    /// generation is hashed once.
    fn frame_grid(&mut self, id: u32) -> Option<(Vec<u64>, usize, usize, f64)> {
        let stale = match self.windows.iter().find(|w| w.id == id) {
            Some(w) => match &w.grid {
                Some(g) => g.commits != self.commits,
                None => true,
            },
            None => return None,
        };
        if stale {
            let t = Instant::now();
            let (canvas, w, h) = self.render_window(id)?;
            let (cells, gw, gh) = block_grid(&canvas, (w as i32, h as i32));
            self.prof.0 += 1;
            self.prof.1 += t.elapsed().as_micros() as u64;
            let cache = GridCache {
                commits: self.commits,
                cells,
                gw,
                gh,
                area: (w * h) as f64,
            };
            if let Some(win) = self.windows.iter_mut().find(|x| x.id == id) {
                win.grid = Some(cache);
            }
        }
        let win = self.windows.iter().find(|x| x.id == id)?;
        let g = win.grid.as_ref()?;
        Some((g.cells.clone(), g.gw, g.gh, g.area))
    }

    fn wait_reply(
        &self,
        id: u32,
        changed: bool,
        ok_state: bool,
        timed_out: bool,
        rects: &[(i32, i32, i32, i32)],
        with_image: bool,
    ) -> String {
        let rev = self
            .windows
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.revision)
            .unwrap_or(0);
        let rect_json: Vec<String> = rects
            .iter()
            .map(|r| format!("[{},{},{},{}]", r.0, r.1, r.2, r.3))
            .collect();
        let mut out = format!(
            "{{\"ok\":true,\"changed\":{changed},\"settled\":{ok_state},\"timed_out\":{timed_out},\"revision\":{rev},\"rects\":[{}]",
            rect_json.join(",")
        );
        if with_image {
            if let Some((canvas, w, h)) = self.render_window(id) {
                if let Ok(png) = encode_png(&canvas, w, h, true) {
                    out.push_str(&format!(
                        ",\"id\":{id},\"w\":{w},\"h\":{h},\"png\":\"{}\"",
                        b64(&png)
                    ));
                }
            }
        }
        out.push('}');
        out
    }

    fn tick(&mut self) {
        self.tick_n = self.tick_n.wrapping_add(1);
        if std::env::var_os("AGENTC_PROFILE").is_some()
            && self.tick_n % 300 == 0
            && (self.prof.0 > 0 || self.prof.2 > 0)
        {
            let (g_n, g_us, e_n, e_us) = self.prof;
            eprintln!(
                "[prof] 5s: grid {}x avg {}us | encode {}x avg {}us | commits {}",
                g_n,
                g_us / g_n.max(1),
                e_n,
                e_us / e_n.max(1),
                self.commits
            );
            self.prof = (0, 0, 0, 0);
        }

        // All deferred replies live here. Never sleep the event loop, or the applications
        // freeze along with it.
        let pending = std::mem::take(&mut self.waits);
        for p in pending {
            if let Some(p) = self.poll_pending(p) {
                self.waits.push(p);
            }
        }

        let time = self.start.elapsed().as_millis() as u32;
        for w in &self.windows {
            let surface = w.toplevel.wl_surface().clone();
            compositor::with_surface_tree_downward(
                &surface,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surf, states, _| {
                    let mut g = states.cached_state.get::<SurfaceAttributes>();
                    let attrs = g.current();
                    for cb in attrs.frame_callbacks.drain(..) {
                        cb.done(time);
                    }
                    let _ = surf;
                },
                |_, _, _| true,
            );
        }
    }
}

fn filter(
    _: &mut Agent,
    _mods: &ModifiersState,
    _sym: KeysymHandle<'_>,
) -> FilterResult<()> {
    FilterResult::Forward
}

/// Composite a surface and all of its children into the canvas, applying subsurface
/// offsets correctly.
fn compose_into(
    canvas: &mut [u8],
    (cw, ch): (i32, i32),
    surface: &wl_surface::WlSurface,
    base: Point<i32, Logical>,
) -> bool {
    let mut painted = false;
    compositor::with_surface_tree_downward(
        surface,
        base,
        |_, states, loc: &Point<i32, Logical>| {
            let off = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            TraversalAction::DoChildren(*loc + off)
        },
        |_, states, loc: &Point<i32, Logical>| {
            let off = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            if let Some(slot) = states.data_map.get::<SurfaceImage>() {
                if let Some(img) = slot.0.lock().unwrap().as_ref() {
                    blit(canvas, (cw, ch), img, loc.x + off.x, loc.y + off.y);
                    painted = true;
                }
            }
        },
        |_, _, _| true,
    );
    painted
}

fn blit(canvas: &mut [u8], (cw, ch): (i32, i32), img: &Image, ox: i32, oy: i32) {
    for row in 0..img.h {
        let cy = oy + row as i32;
        if cy < 0 || cy >= ch {
            continue;
        }
        for col in 0..img.w {
            let cx = ox + col as i32;
            if cx < 0 || cx >= cw {
                continue;
            }
            let s = row * img.stride + col * 4;
            let d = ((cy * cw + cx) * 4) as usize;
            let a = img.data[s + 3] as u32;
            if img.alpha && a != 255 {
                // Canvas is treated as opaque: out = src + dst * (1 - a), src is premultiplied
                let inv = 255 - a;
                for k in 0..3 {
                    let src = img.data[s + (2 - k)] as u32; // BGR -> RGB
                    canvas[d + k] = (src + canvas[d + k] as u32 * inv / 255).min(255) as u8;
                }
                canvas[d + 3] = 255;
            } else {
                canvas[d] = img.data[s + 2]; // R
                canvas[d + 1] = img.data[s + 1]; // G
                canvas[d + 2] = img.data[s]; // B
                canvas[d + 3] = 255;
            }
        }
    }
}

// ---------------------------------------------------------------- delegates

impl CompositorHandler for Agent {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }
    fn new_surface(&mut self, surface: &wl_surface::WlSurface) {
        compositor::with_states(surface, |data| {
            data.data_map
                .insert_if_missing_threadsafe(SurfaceImage::default);
        });
    }
    fn commit(&mut self, surface: &wl_surface::WlSurface) {
        self.commits = self.commits.wrapping_add(1);
        // Important: on_commit_buffer_handler *consumes* the buffer stored in
        // SurfaceAttributes, so it has to be taken out first and only then handed to
        // smithay for lifetime management.
        let buf = compositor::with_states(surface, |data| {
            let mut g = data.cached_state.get::<SurfaceAttributes>();
            match g.current().buffer.as_ref() {
                Some(BufferAssignment::NewBuffer(b)) => Some(b.clone()),
                _ => None,
            }
        });

        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);
        // Let PopupManager update popup positions (recomputed when the parent geometry changes)
        self.popups.commit(surface);

        if let Some(b) = buf {
            if let Some(img) = image_from_buffer(&b) {
                set_image(surface, img);
                let time = self.start.elapsed().as_millis() as u32;
                compositor::with_states(surface, |data| {
                    let mut g = data.cached_state.get::<SurfaceAttributes>();
                    for cb in g.current().frame_callbacks.drain(..) {
                        cb.done(time);
                    }
                });
            }
        }
    }
}

impl BufferHandler for Agent {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Agent {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for Agent {}

impl SelectionHandler for Agent {
    type SelectionUserData = String;

    fn send_selection(
        &mut self,
        _ty: smithay::wayland::selection::SelectionTarget,
        _mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        user_data: &String,
    ) {
        use std::io::Write;
        let mut f = std::fs::File::from(fd);
        let _ = f.write_all(user_data.as_bytes());
    }
}

impl DataDeviceHandler for Agent {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Agent {}
impl ServerDndGrabHandler for Agent {}

impl PrimarySelectionHandler for Agent {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl SeatHandler for Agent {
    type KeyboardFocus = wl_surface::WlSurface;
    type PointerFocus = wl_surface::WlSurface;
    type TouchFocus = wl_surface::WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl XdgShellHandler for Agent {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_id;
        self.next_id += 1;
        let size = self.default_size;
        let sent_size = size;
        // A toplevel with a parent is a dialog: do not force 1280x720 + maximized on it, or
        // a fixed-size dialog ends up drawn in the middle of a huge black canvas.
        let is_dialog = surface.parent().is_some();
        let mode = if is_dialog {
            WinMode::Windowed
        } else {
            self.default_mode
        };

        surface.with_pending_state(|st| {
            if !is_dialog {
                st.size = Some(size.into());
                if mode == WinMode::Maximized {
                    st.states.set(xdg_toplevel::State::Maximized);
                } else if mode == WinMode::Fullscreen {
                    st.states.set(xdg_toplevel::State::Fullscreen);
                }
            }
            st.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();

        self.windows.push(Window {
            id,
            title: String::new(),
            app_id: String::new(),
            toplevel: surface,
            size,
            mode,
            revision: 1,
            grid: None,
            sent_grid: None,
            sent_size,
        });
        self.win_seq += 1;
        self.windows_log.push((
            self.win_seq,
            format!("{{\"id\":{id},\"dialog\":{is_dialog},\"title\":\"\"}}"),
        ));
        while self.windows_log.len() > 32 {
            self.windows_log.remove(0);
        }
        eprintln!(
            "[agentc] new toplevel id={id} {}x{} {:?}{}",
            size.0,
            size.1,
            mode,
            if is_dialog { " (dialog)" } else { "" }
        );
        // A new window (especially a dialog) takes focus, otherwise `active` stays on the old one
        self.focus(id);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Dialogs close all the time; `active` must not point at a window that is gone
        let dead = self
            .windows
            .iter()
            .find(|w| w.toplevel == surface)
            .map(|w| w.id);
        if let Some(id) = dead {
            self.windows.retain(|w| w.id != id);
            if self.active == id {
                if let Some(last) = self.windows.last().map(|w| w.id) {
                    self.focus(last);
                }
            }
            eprintln!("[agentc] toplevel {id} closed");
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let title = compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().title.clone())
        });
        if let (Some(title), Some(w)) = (
            title,
            self.windows.iter_mut().find(|w| w.toplevel == surface),
        ) {
            w.title = title;
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        let app_id = compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().app_id.clone())
        });
        if let (Some(app_id), Some(w)) = (
            app_id,
            self.windows.iter_mut().find(|w| w.toplevel == surface),
        ) {
            w.app_id = app_id;
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = surface.send_configure();
        eprintln!("[agentc] new_popup");
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            eprintln!("[agentc] track_popup failed: {e:?}");
        }
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: Serial,
    ) {
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }
}

smithay::delegate_compositor!(Agent);
smithay::delegate_shm!(Agent);
smithay::delegate_seat!(Agent);
smithay::delegate_data_device!(Agent);
smithay::delegate_output!(Agent);
smithay::delegate_primary_selection!(Agent);
smithay::delegate_xdg_shell!(Agent);

// ---------------------------------------------------------------- control protocol

pub enum Msg {
    Tick,
    Cmd(String, std_mpsc::Sender<String>),
}

fn spawn_control(path: String, tx: cl_channel::Sender<Msg>) {
    std::thread::spawn(move || {
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[agentc] control bind failed: {e}");
                return;
            }
        };
        eprintln!("[agentc] control socket: {path}");
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || handle_conn(stream, tx));
        }
    });
}

fn handle_conn(stream: UnixStream, tx: cl_channel::Sender<Msg>) {
    let mut writer = stream.try_clone().unwrap();
    let reader = BufReader::new(stream);
    for line in reader.lines().flatten() {
        if line.trim().is_empty() {
            continue;
        }
        let (rtx, rrx) = std_mpsc::channel();
        if tx.send(Msg::Cmd(line, rtx)).is_err() {
            return;
        }
        match rrx.recv_timeout(Duration::from_secs(10)) {
            Ok(resp) => {
                let _ = writeln!(writer, "{resp}");
            }
            Err(_) => {
                let _ = writeln!(writer, "{{\"ok\":false,\"error\":\"timeout\"}}");
            }
        }
    }
}

/// Resolve keys against the keymap the compositor actually uses, instead of hardcoding a
/// layout or an offset.
fn build_keymap_table() -> (std::collections::HashMap<char, (u32, bool)>, u32) {
    use xkbcommon::xkb::{Keycode as XKeycode, Keymap};
    let mut map = std::collections::HashMap::new();
    let mut shift_code = 50u32;
    let Some(keymap) = Keymap::new_from_names(
        &xkbcommon::xkb::Context::new(0),
        "",
        "",
        "us",
        "",
        None,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) else {
        return (map, shift_code);
    };
    for code in 8u32..=255 {
        for level in 0..2u32 {
            let syms = keymap.key_get_syms_by_level(XKeycode::new(code), 0, level);
            if let Some(&sym) = syms.first() {
                if let Some(ch) = char::from_u32(sym.raw()) {
                    map.entry(ch).or_insert((code, level == 1));
                }
                if sym.raw() == 0xffe1 {
                    shift_code = code; // Shift_L
                }
            }
        }
    }
    // Control characters have no printable keysym; look those up by keysym separately
    let specials: [(char, u32); 4] = [
        ('\n', 0xff0d),  // Return
        ('\t', 0xff09),  // Tab
        ('\u{8}', 0xff08), // BackSpace
        ('\u{1b}', 0xff1b), // Escape
    ];
    for (ch, ks) in specials {
        for code in 8u32..=255 {
            let syms = keymap.key_get_syms_by_level(XKeycode::new(code), 0, 0);
            if syms.first().map(|s| s.raw()) == Some(ks) {
                map.insert(ch, (code, false));
                break;
            }
        }
    }
    (map, shift_code)
}

#[allow(dead_code)]
fn char_to_key(ch: char) -> Option<(u32, bool)> {
    // US layout, evdev keycodes
    let (code, shift) = match ch {
        'a'..='z' => {
            const ROW: [u32; 26] = [
                30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22,
                47, 17, 45, 21, 44,
            ];
            (ROW[(ch as u8 - b'a') as usize], false)
        }
        'A'..='Z' => {
            let (c, _) = char_to_key(ch.to_ascii_lowercase())?;
            (c, true)
        }
        '1' => (2, false),
        '2' => (3, false),
        '3' => (4, false),
        '4' => (5, false),
        '5' => (6, false),
        '6' => (7, false),
        '7' => (8, false),
        '8' => (9, false),
        '9' => (10, false),
        '0' => (11, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        ',' => (51, false),
        '.' => (52, false),
        '/' => (53, false),
        '?' => (53, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '[' => (26, false),
        ']' => (27, false),
        '\\' => (43, false),
        '`' => (41, false),
        '~' => (41, true),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '<' => (51, true),
        '>' => (52, true),
        _ => return None,
    };
    Some((code, shift))
}

impl Agent {
    /// Parse a combo like "CTRL+SHIFT+T" into (modifier xkb keycodes, main key, shift?).
    fn parse_combo(&self, combo: &str) -> Option<(Vec<u32>, u32, bool)> {
        let parts: Vec<&str> = combo
            .split('+')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            return None;
        }
        let mut mods = Vec::new();
        for p in &parts[..parts.len() - 1] {
            mods.push(match p.to_ascii_uppercase().as_str() {
                "CTRL" | "CONTROL" => 37,  // LCTL
                "SHIFT" => 50,             // LFSH
                "ALT" => 64,               // LALT
                "SUPER" | "META" | "WIN" => 133,
                _ => return None,
            });
        }
        let last = parts[parts.len() - 1];
        let (code, shift) = match last.to_ascii_uppercase().as_str() {
            "ENTER" | "RETURN" => (36, false),
            "TAB" => (23, false),
            "ESC" | "ESCAPE" => (9, false),
            "BACKSPACE" => (22, false),
            "SPACE" => (65, false),
            "UP" => (111, false),
            "DOWN" => (116, false),
            "LEFT" => (113, false),
            "RIGHT" => (114, false),
            "DELETE" => (119, false),
            "HOME" => (110, false),
            "END" => (115, false),
            other => {
                let ch = other.chars().next()?;
                *self.keymap_table.get(&ch.to_ascii_lowercase())?
            }
        };
        Some((mods, code, shift))
    }

    /// Hold the modifiers down and then tap the main key. (An earlier version pressed and
    /// released each key in turn, which never forms a combination at all.)
    fn send_combo(&mut self, id: u32, mods: &[u32], code: u32, shift: bool) {
        if !self.focus(id) {
            return;
        }
        self.active = id;
        let keyboard = self.seat.get_keyboard().unwrap();
        let time = self.start.elapsed().as_millis() as u32;
        let shift_code = self.shift_code;
        fn tap(
            st: &mut Agent,
            kbd: &smithay::input::keyboard::KeyboardHandle<Agent>,
            k: u32,
            pressed: bool,
            time: u32,
        ) {
            kbd.input::<(), _>(
                st,
                Keycode::new(k),
                if pressed { SKeyState::Pressed } else { SKeyState::Released },
                SERIAL_COUNTER.next_serial(),
                time,
                filter,
            );
        }
        for m in mods {
            tap(self, &keyboard, *m, true, time);
        }
        if shift {
            tap(self, &keyboard, shift_code, true, time);
        }
        keyboard.input::<(), _>(
            self,
            Keycode::new(code),
            SKeyState::Pressed,
            SERIAL_COUNTER.next_serial(),
            time,
            filter,
        );
        keyboard.input::<(), _>(
            self,
            Keycode::new(code),
            SKeyState::Released,
            SERIAL_COUNTER.next_serial(),
            time,
            filter,
        );
        if shift {
            keyboard.input::<(), _>(
                self,
                Keycode::new(self.shift_code),
                SKeyState::Released,
                SERIAL_COUNTER.next_serial(),
                time,
                filter,
            );
        }
        for m in mods.iter().rev() {
            keyboard.input::<(), _>(
                self,
                Keycode::new(*m),
                SKeyState::Released,
                SERIAL_COUNTER.next_serial(),
                time,
                filter,
            );
        }
    }

    /// Put text on the clipboard (the compositor acts as the selection owner).
    fn set_clipboard(&mut self, text: &str) {
        smithay::wayland::selection::data_device::set_data_device_selection::<Self>(
            &self.dh,
            &self.seat,
            vec![
                "text/plain;charset=utf-8".to_string(),
                "text/plain".to_string(),
                "UTF8_STRING".to_string(),
                "STRING".to_string(),
            ],
            text.to_string(),
        );
    }

    /// Paste via the clipboard. Terminals want Ctrl+Shift+V, everything else Ctrl+V.
    fn paste_now(&mut self, id: u32) {
        let is_term = self
            .windows
            .iter()
            .find(|w| w.id == id)
            .map(|w| {
                matches!(
                    w.app_id.as_str(),
                    "foot" | "footclient" | "alacritty" | "kitty" | "wezterm"
                        | "org.wezfurlong.wezterm" | "gnome-terminal-server" | "xterm"
                )
            })
            .unwrap_or(false);
        let v = self.keymap_table.get(&'v').copied().unwrap_or((55, false));
        if is_term {
            self.send_combo(id, &[37, 50], v.0, v.1);
        } else {
            self.send_combo(id, &[37], v.0, v.1);
        }
    }
}

// ---------------------------------------------------------------- main

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let sub = argv.get(1).map(|s| s.as_str()).unwrap_or("help");
    match sub {
        "daemon" | "run" => run_daemon(argv.iter().skip(2).cloned().collect()),
        "mcp" => {
            let rest: Vec<String> = argv.iter().skip(2).cloned().collect();
            shim::run_stdio(flag(&rest, "--url"));
        }
        "stop" => {
            if let Some(url) = shim::read_url() {
                let _ = std::process::Command::new("curl")
                    .args(["-s", "-o", "/dev/null", "-X", "POST", &format!("{url}/shutdown")])
                    .status();
            }
            let _ = std::fs::remove_file(shim::url_file());
            println!("stopped");
        }
        "status" => match shim::read_url() {
            Some(url) => println!("daemon: {url}"),
            None => println!("daemon: not running"),
        },
        "help" | "--help" | "-h" => {
            println!(
                "agentc — a headless Wayland compositor built for agents\n\n\
                 usage:\n  \
                 agentc daemon [--http 127.0.0.1:7788] [--control PATH]   long-running: compositor + MCP + panel\n  \
                 agentc mcp [--url URL]                                   stdio MCP relay, starts the daemon if needed\n  \
                 agentc status                                            print the daemon URL\n  \
                 agentc stop                                              stop the daemon\n"
            );
        }
        // Backwards compatibility: `agentc --http ...`
        _ => run_daemon(argv.iter().skip(1).cloned().collect()),
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn run_daemon(args: Vec<String>) {
    let http = flag(&args, "--http").unwrap_or_else(|| "127.0.0.1:7788".to_string());

    let sock = flag(&args, "--control")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| shim::runtime_dir().join("agentc.sock"))
        .to_string_lossy()
        .to_string();

    let mut event_loop: EventLoop<Agent> = EventLoop::try_new().unwrap();
    let handle = event_loop.handle();

    let mut display: Display<Agent> = Display::new().unwrap();
    let dh = display.handle();

    let listening = ListeningSocketSource::new_auto().unwrap();
    let socket_name = listening.socket_name().to_string_lossy().to_string();
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);
    eprintln!("[agentc] WAYLAND_DISPLAY={socket_name}");

    let mut state = Agent::new(dh.clone(), socket_name);

    handle
        .insert_source(listening, |stream, _, state: &mut Agent| {
            match state.dh.insert_client(stream, Arc::new(ClientState::default())) {
                Ok(_) => eprintln!("[agentc] client connected"),
                Err(e) => eprintln!("[agentc] insert_client failed: {e}"),
            }
        })
        .unwrap();

    let (tx, rx) = cl_channel::channel::<Msg>();
    handle
        .insert_source(rx, |event, _, state: &mut Agent| {
            if let cl_channel::Event::Msg(msg) = event {
                match msg {
                    Msg::Tick => state.tick(),
                    Msg::Cmd(cmd, reply) => {
                        let later = reply.clone();
                        // Internal polling (events/stream) must not consume the new-window
                        // cursor, or the shim drains it every 250ms.
                        let parsed = serde_json::from_str::<serde_json::Value>(&cmd).ok();
                        let op = parsed
                            .as_ref()
                            .and_then(|v| v.get("cmd").and_then(|x| x.as_str()).map(str::to_string))
                            .unwrap_or_default();
                        let actor = parsed
                            .as_ref()
                            .and_then(|v| v.get("actor").and_then(|x| x.as_str()).map(str::to_string))
                            .unwrap_or_else(|| "agent".to_string());
                        if let Some(resp) = handle_cmd(state, &cmd, later) {
                            // Only agent replies may consume the new-window queue. Neither
                            // internal polling nor the human panel (status) may steal it, or
                            // notifications would never reach the model once the panel is open.
                            let body = if matches!(op.as_str(), "events" | "stream")
                                || actor == "human"
                            {
                                resp
                            } else {
                                state.finish(resp)
                            };
                            let _ = reply.send(body);
                        }
                    }
                }
            }
        })
        .unwrap();

    let (addr, listener) = match mcp::bind(&http) {
        Some(v) => v,
        None => {
            eprintln!("[agentc] failed to bind port: {http}");
            std::process::exit(1);
        }
    };
    let url = format!("http://{addr}");
    let _ = std::fs::write(shim::url_file(), &url);
    eprintln!("[agentc] MCP + panel: {url}");
    eprintln!("[agentc] log: {}", shim::log_file().display());

    spawn_control(sock, tx.clone());
    mcp::spawn_http(listener, tx.clone());

    let tick_tx = tx.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(16));
        if tick_tx.send(Msg::Tick).is_err() {
            break;
        }
    });

    eprintln!("[agentc] daemon ready");
    event_loop
        .run(Duration::from_millis(16), &mut state, move |state| {
            let _ = display.dispatch_clients(state);
            let _ = display.flush_clients();
        })
        .unwrap();
}

fn no_frame_reply(id: u32, timed_out: bool) -> String {
    format!(
        "{{\"ok\":true,\"changed\":false,\"settled\":false,\"timed_out\":{timed_out},\"revision\":0,\"rects\":[],\"id\":{id},\"error\":\"window has not committed a buffer yet\"}}"
    )
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Encode just one region of the canvas. The live stream uses this for incremental
/// refresh: typing one line changes a few hundred pixels, so recompressing the whole frame
/// would be wasteful.
fn encode_png_region(
    rgba: &[u8],
    canvas_w: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    fast: bool,
) -> std::io::Result<Vec<u8>> {
    let (w, h) = (w.max(1) as usize, h.max(1) as usize);
    let cw = canvas_w as usize;
    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in 0..h {
        let start = ((y as usize + row) * cw + x as usize) * 4;
        for i in 0..w {
            let p = start + i * 4;
            rgb.extend_from_slice(&rgba[p..p + 3]);
        }
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(if fast {
            png::Compression::Fast
        } else {
            png::Compression::Default
        });
        let mut writer = enc.write_header()?;
        writer.write_image_data(&rgb)?;
    }
    Ok(buf)
}

/// Encode PNG. Screenshots and the live stream share this, with different trade-offs:
/// - the canvas is always opaque, so RGB instead of RGBA means 25% less data to filter
///   and compress
/// - `fast` is for the live stream (lower zlib level: faster, slightly larger)
fn encode_png(rgba: &[u8], w: u32, h: u32, fast: bool) -> std::io::Result<Vec<u8>> {
    let px = (w as usize) * (h as usize);
    let mut rgb = Vec::with_capacity(px * 3);
    for i in 0..px {
        rgb.extend_from_slice(&rgba[i * 4..i * 4 + 3]);
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(if fast {
            png::Compression::Fast
        } else {
            png::Compression::Default
        });
        let mut writer = enc.write_header()?;
        writer.write_image_data(&rgb)?;
    }
    Ok(buf)
}

fn windows_json(state: &Agent) -> String {
    let list: Vec<String> = state
        .windows
        .iter()
        .map(|w| {
            format!(
                "{{\"id\":{},\"app_id\":{:?},\"title\":{:?},\"w\":{},\"h\":{},\"mode\":\"{}\",\"revision\":{},\"dialog\":{}}}",
                w.id,
                w.app_id,
                w.title,
                w.size.0,
                w.size.1,
                match w.mode {
                    WinMode::Windowed => "windowed",
                    WinMode::Maximized => "maximized",
                    WinMode::Fullscreen => "fullscreen",
                },
                w.revision,
                w.toplevel.parent().is_some()
            )
        })
        .collect();
    format!("[{}]", list.join(","))
}

const ACTIONS: &[&str] = &[
    "click", "dblclick", "move", "drag", "scroll", "type", "key", "launch", "focus", "press",
    "release",
];

fn handle_cmd(state: &mut Agent, cmd: &str, reply: std_mpsc::Sender<String>) -> Option<String> {
    let v: serde_json::Value = match serde_json::from_str(cmd) {
        Ok(v) => v,
        Err(e) => return Some(format!("{{\"ok\":false,\"error\":\"bad json: {e}\"}}")),
    };
    let op = v.get("cmd").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let actor = v
        .get("actor")
        .and_then(|x| x.as_str())
        .unwrap_or("agent")
        .to_string();
    let win = v
        .get("id")
        .and_then(|x| x.as_u64())
        .map(|w| w as u32)
        .unwrap_or(state.active);

    // ---- Human-only: control
    match op.as_str() {
        "pause" => {
            state.paused = true;
            log_action(state, "human", "pause", "");
            return Some("{\"ok\":true,\"paused\":true}".into());
        }
        "resume" => {
            state.paused = false;
            log_action(state, "human", "resume", "");
            return Some("{\"ok\":true,\"paused\":false}".into());
        }
        "status" => {
            let log: Vec<String> = state.log.iter().cloned().collect();
            return Some(format!(
                "{{\"ok\":true,\"paused\":{},\"active\":{},\"windows\":{},\"log\":{}}}",
                state.paused,
                state.active,
                windows_json(state),
                serde_json::to_string(&log).unwrap_or_else(|_| "[]".into())
            ));
        }
        _ => {}
    }

    // ---- Pause gate: every agent action is refused with explicit instructions
    if state.paused && actor != "human" && ACTIONS.contains(&op.as_str()) {
        log_action(state, "sys", "blocked", &op);
        return Some(
            "{\"ok\":false,\"error\":\"PAUSED: a human took over. Do not retry; stop and wait for them to resume\"}".into(),
        );
    }

    if matches!(op.as_str(), "wait_for_change" | "wait_for_idle") {
        let timeout = v.get("timeout_ms").and_then(|x| x.as_u64()).unwrap_or(3000).min(30_000);
        let min_area = v.get("min_area").and_then(|x| x.as_f64()).unwrap_or(0.002);
        let with_image = v.get("image").and_then(|x| x.as_bool()).unwrap_or(true);
        let now = Instant::now();
        if op == "wait_for_change" {
            state.waits.push(Pending::Change {
                w: win,
                deadline: now + Duration::from_millis(timeout),
                baseline: None,
                seen_commits: state.commits,
                min_area,
                sender: reply,
                with_image,
            });
            log_action(state, &actor, "wait_for_change", "");
        } else {
            let quiet = v.get("quiet_ms").and_then(|x| x.as_u64()).unwrap_or(120);
            let grace = v.get("grace_ms").and_then(|x| x.as_u64()).unwrap_or(200);
            state.waits.push(Pending::Idle {
                w: win,
                deadline: now + Duration::from_millis(timeout),
                grace: now + Duration::from_millis(grace),
                quiet_ms: quiet,
                last_change: now,
                baseline: None,
                seen_commits: state.commits,
                min_area,
                sender: reply,
                with_image,
            });
            log_action(state, &actor, "wait_for_idle", "");
        }
        return None;
    }

    if op == "drag" {
        let (x1, y1) = (
            v.get("x1").and_then(|x| x.as_f64()).unwrap_or(0.0),
            v.get("y1").and_then(|x| x.as_f64()).unwrap_or(0.0),
        );
        let (x2, y2) = (
            v.get("x2").and_then(|x| x.as_f64()).unwrap_or(0.0),
            v.get("y2").and_then(|x| x.as_f64()).unwrap_or(0.0),
        );
        state.focus(win);
        state.active = win;
        let mut steps = vec![Step {
            act: Act::Move(x1, y1),
            wait_ms: 16,
        }];
        let hold = v.get("hold_ms").and_then(|x| x.as_u64()).unwrap_or(60).clamp(10, 800);
        let n = v.get("steps").and_then(|x| x.as_u64()).unwrap_or(10).clamp(2, 40);
        steps.push(Step {
            act: Act::Down(0x110),
            wait_ms: hold,
        });
        for i in 1..=n {
            let t = i as f64 / n as f64;
            steps.push(Step {
                act: Act::Move(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t),
                wait_ms: 16,
            });
        }
        steps.push(Step {
            act: Act::Up(0x110),
            wait_ms: 1,
        });
        state.waits.push(Pending::Seq {
            w: win,
            steps: std::sync::Arc::new(steps),
            idx: 0,
            at: Instant::now(),
            sender: reply,
        });
        log_action(state, &actor, "drag", "");
        return None;
    }

    if op == "launch" {
        let argv: Vec<String> = v
            .get("argv")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if argv.is_empty() {
            return Some("{\"ok\":false,\"error\":\"empty argv\"}".into());
        }
        let since = state.next_id;
        let spawned = Command::new(&argv[0])
            .args(&argv[1..])
            .env("WAYLAND_DISPLAY", &state.socket_name)
            .spawn();
        match spawned {
            Ok(c) => {
                let pid = c.id();
                let wait = v.get("wait_ms").and_then(|x| x.as_u64()).unwrap_or(3000).min(15000);
                if wait == 0 {
                    return Some(format!("{{\"ok\":true,\"pid\":{pid},\"window\":null}}"));
                }
                state.waits.push(Pending::Window {
                    deadline: Instant::now() + Duration::from_millis(wait),
                    since_id: since,
                    app_id: None,
                    sender: reply,
                });
                log_action(state, &actor, "launch", "");
                return None;
            }
            Err(e) => return Some(format!("{{\"ok\":false,\"error\":\"spawn: {e}\"}}")),
        }
    }

    if op == "wait_for_window" {
        let timeout = v.get("timeout_ms").and_then(|x| x.as_u64()).unwrap_or(3000).min(15000);
        // With an explicit after_id only later windows count; otherwise any window does
        let since = match v.get("after_id").and_then(|x| x.as_u64()) {
            Some(x) => (x as u32).saturating_add(1),
            None => 0,
        };
        let app_id = v.get("app_id").and_then(|x| x.as_str()).map(str::to_string);
        state.waits.push(Pending::Window {
            deadline: Instant::now() + Duration::from_millis(timeout),
            since_id: since,
            app_id,
            sender: reply,
        });
        return None;
    }

    if op == "events" {
        let since = v.get("since").and_then(|x| x.as_u64()).unwrap_or(0);
        let items: Vec<String> = state
            .windows_log
            .iter()
            .filter(|(seq, _)| *seq > since)
            .map(|(seq, j)| format!("{{\"seq\":{seq},\"window\":{j}}}"))
            .collect();
        return Some(format!(
            "{{\"ok\":true,\"seq\":{},\"events\":[{}]}}",
            state.win_seq,
            items.join(",")
        ));
    }

    if op == "stream" {
        let since = v.get("since").and_then(|x| x.as_u64()).unwrap_or(0);
        let timeout = v
            .get("timeout_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(150)
            .clamp(20, 2000);
        let full = v.get("full").and_then(|x| x.as_bool()).unwrap_or(false);
        state.waits.push(Pending::Stream {
            w: win,
            deadline: Instant::now() + Duration::from_millis(timeout),
            since,
            full,
            sender: reply,
        });
        return None;
    }

    // Raw key events from a human (the panel), press and release separately
    if matches!(op.as_str(), "keydown" | "keyup") {
        let code = v.get("code").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        state.focus(win);
        state.active = win;
        if let Some(kbd) = state.seat.get_keyboard() {
            let time = state.start.elapsed().as_millis() as u32;
            kbd.input::<(), _>(
                state,
                Keycode::new(code),
                if op == "keydown" {
                    SKeyState::Pressed
                } else {
                    SKeyState::Released
                },
                SERIAL_COUNTER.next_serial(),
                time,
                filter,
            );
        }
        return Some("{\"ok\":true}".into());
    }

    // Pointer actions: all go through the action sequence, no sleeping in the event loop
    if matches!(op.as_str(), "click" | "dblclick" | "press" | "release") {
        let btn = match v.get("button").and_then(|b| b.as_str()).unwrap_or("left") {
            "right" => 0x111,
            "middle" => 0x112,
            _ => 0x110,
        };
        state.focus(win);
        state.active = win;
        let mut steps = Vec::new();
        if op == "press" || op == "release" {
            steps.push(Step {
                act: Act::Move(
                    v.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    v.get("y").and_then(|x| x.as_f64()).unwrap_or(0.0),
                ),
                wait_ms: 8,
            });
            steps.push(Step {
                act: if op == "press" {
                    Act::Down(btn)
                } else {
                    Act::Up(btn)
                },
                wait_ms: 1,
            });
        } else {
            let x = v.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let y = v.get("y").and_then(|x| x.as_f64()).unwrap_or(0.0);
            // Hold duration and double-click interval are adjustable: some widgets are
            // sensitive to how long a press lasts
            let hold = v
                .get("duration_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(60)
                .clamp(10, 800);
            let gap = v
                .get("interval_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(160)
                .clamp(20, 400);
            let taps = if op == "dblclick" { 2 } else { 1 };
            steps.push(Step {
                act: Act::Move(x, y),
                wait_ms: 16,
            });
            for i in 0..taps {
                steps.push(Step {
                    act: Act::Down(btn),
                    wait_ms: hold,
                });
                steps.push(Step {
                    act: Act::Up(btn),
                    wait_ms: if i + 1 < taps { gap } else { 1 },
                });
            }
        }
        state.defer(win, steps, reply);
        log_action(state, &actor, &op, "");
        return None;
    }

    if op == "type" {
        let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if text.chars().any(|c| !c.is_ascii()) {
            state.focus(win);
            state.active = win;
            let body = text.trim_end_matches('\n');
            let enters = (text.len() - body.len()) as u32;
            state.set_clipboard(body);
            state.waits.push(Pending::Paste {
                w: win,
                at: Instant::now() + Duration::from_millis(90),
                enters,
                sender: reply,
            });
            log_action(state, &actor, "type", "(clipboard)");
            return None;
        }
        state.focus(win);
        state.active = win;
        let steps = state.type_steps(&text);
        if !steps.is_empty() {
            state.defer(win, steps, reply);
            log_action(state, &actor, "type", "");
            return None;
        }
        return Some("{\"ok\":true}".into());
    }

    if op == "wait" {
        let ms = v.get("ms").and_then(|x| x.as_u64()).unwrap_or(0).min(10_000);
        state.waits.push(Pending::Sleep {
            at: Instant::now() + Duration::from_millis(ms),
            sender: reply,
        });
        log_action(state, &actor, "wait", "");
        return None;
    }

    let resp = dispatch(state, &op, &v, win, &actor);
    // Screenshots and listings are read-only and polled at high frequency by the panel, so
    // they stay out of the audit log
    if !matches!(op.as_str(), "screenshot" | "windows" | "popups" | "status" | "stream") {
        let ok = resp.starts_with("{\"ok\":true");
        log_action(state, &actor, if ok { &op } else { "failed" }, "");
    }
    Some(resp)
}

fn summarize(op: &str) -> String {
    op.to_string()
}

fn log_action(state: &mut Agent, who: &str, what: &str, detail: &str) {
    let t = state.start.elapsed().as_secs_f64();
    let line = if detail.is_empty() || detail == what {
        format!("{who} {what}")
    } else {
        format!("{who} {what} {detail}")
    };
    let _ = t;
    state.log.push_back(line);
    while state.log.len() > 80 {
        state.log.pop_front();
    }
}

fn dispatch(state: &mut Agent, op: &str, v: &serde_json::Value, win: u32, _actor: &str) -> String {
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    match op {
        "windows" => format!(
            "{{\"ok\":true,\"active\":{},\"windows\":{}}}",
            state.active,
            windows_json(state)
        ),
        "screenshot" => {
            let want_b64 = v.get("b64").and_then(|x| x.as_bool()).unwrap_or(false);
            match state.render_window(win) {
                Some((data, w, h)) => {
                    let rev = state
                        .windows
                        .iter()
                        .find(|x| x.id == win)
                        .map(|x| x.revision)
                        .unwrap_or(0);
                    if want_b64 {
                        match encode_png(&data, w, h, false) {
                            Ok(png) => format!(
                                "{{\"ok\":true,\"id\":{win},\"w\":{w},\"h\":{h},\"revision\":{rev},\"png\":\"{}\"}}",
                                b64(&png)
                            ),
                            Err(e) => format!("{{\"ok\":false,\"error\":\"png: {e}\"}}"),
                        }
                    } else {
                        let path = v
                            .get("path")
                            .and_then(|x| x.as_str())
                            .unwrap_or("/tmp/agentc-shot.png")
                            .to_string();
                        match write_png(&path, &data, w, h) {
                            Ok(_) => format!(
                                "{{\"ok\":true,\"id\":{win},\"path\":\"{path}\",\"w\":{w},\"h\":{h}}}"
                            ),
                            Err(e) => format!("{{\"ok\":false,\"error\":\"png: {e}\"}}"),
                        }
                    }
                }
                None => "{\"ok\":false,\"error\":\"no image for window (has it committed a buffer yet?)\"}".into(),
            }
        }

        "move" => {
            state.move_to(win, num("x"), num("y"));
            "{\"ok\":true}".into()
        }
        "scroll" => {
            state.scroll(win, num("x"), num("y"), num("dy"));
            "{\"ok\":true}".into()
        }
        "press" | "release" => {
            let btn = match v.get("button").and_then(|b| b.as_str()).unwrap_or("left") {
                "right" => 0x111,
                "middle" => 0x112,
                _ => 0x110,
            };
            state.focus(win);
            state.button_evt(win, num("x"), num("y"), btn, op == "press");
            "{\"ok\":true}".into()
        }
        "resize" | "set_size" => {
            let w = v.get("width").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
            let h = v.get("height").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
            match state.reconfigure(win, Some((w, h)), None) {
                Some(rev) => {
                    let (rw, rh) = state
                        .windows
                        .iter()
                        .find(|x| x.id == win)
                        .map(|x| x.size)
                        .unwrap_or((0, 0));
                    format!("{{\"ok\":true,\"revision\":{rev},\"w\":{rw},\"h\":{rh}}}")
                }
                None => "{\"ok\":false,\"error\":\"no such window\"}".into(),
            }
        }
        "set_mode" => {
            let m = match v.get("mode").and_then(|x| x.as_str()).unwrap_or("") {
                "windowed" => WinMode::Windowed,
                "maximized" | "maximize" => WinMode::Maximized,
                "fullscreen" | "full" => WinMode::Fullscreen,
                other => return format!("{{\"ok\":false,\"error\":\"bad mode {other:?}\"}}"),
            };
            match state.reconfigure(win, None, Some(m)) {
                Some(rev) => format!(
                    "{{\"ok\":true,\"revision\":{rev},\"mode\":\"{}\"}}",
                    match m {
                        WinMode::Windowed => "windowed",
                        WinMode::Maximized => "maximized",
                        WinMode::Fullscreen => "fullscreen",
                    }
                ),
                None => "{\"ok\":false,\"error\":\"no such window\"}".into(),
            }
        }
        "focus" => {
            if state.focus(win) {
                state.active = win;
                "{\"ok\":true}".into()
            } else {
                "{\"ok\":false,\"error\":\"no such window\"}".into()
            }
        }

        "key" => {
            let combo = v.get("combo").and_then(|x| x.as_str()).unwrap_or("").to_string();
            state.focus(win);
            state.active = win;
            match state.parse_combo(&combo) {
                Some((mods, code, shift)) => {
                    state.send_combo(win, &mods, code, shift);
                    "{\"ok\":true}".into()
                }
                None => format!("{{\"ok\":false,\"error\":\"bad combo {combo:?}\"}}"),
            }
        }
        "popups" => {
            let Some(t) = state.window_surface(win) else {
                return "{\"ok\":false,\"error\":\"no such window\"}".into();
            };
            let list: Vec<String> = PopupManager::popups_for_surface(&t)
                .map(|(k, p)| {
                    let s = k.wl_surface().clone();
                    let img = get_image(&s).map(|i| (i.w as i32, i.h as i32));
                    format!("{{\"at\":[{},{}],\"img\":{:?}}}", p.x, p.y, img)
                })
                .collect();
            format!("{{\"ok\":true,\"popups\":[{}]}}", list.join(","))
        }
        _ => format!("{{\"ok\":false,\"error\":\"unknown cmd {op:?}\"}}"),
    }
}

fn write_png(path: &str, rgba: &[u8], w: u32, h: u32) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let wtr = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(wtr, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}
