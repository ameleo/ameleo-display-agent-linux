// SPDX-License-Identifier: GPL-3.0-or-later

//! Absolute pointer through uinput, over the whole desktop.
//!
//! The device is an absolute mouse, like QEMU's USB tablet: `ABS_X`/`ABS_Y`
//! and mouse buttons, no pen and no touch. libinput gives it to the
//! compositor as an absolute pointer, which KWin, Mutter and wlroots map to
//! the bounding box of all monitors, in logical coordinates.
//!
//! The host addresses a head (virtio-gpu scanout index) in pixels. The
//! session agent reports where the compositor put each head and at which
//! scale ([`crate::ipc::Head`]). Without a session agent (greeter), the
//! layout comes from DRM: the virtio-gpu connectors `Virtual-N` side by
//! side, at scale 1.

use std::sync::mpsc;

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AbsoluteAxisEvent, AttributeSet, BusType, InputEvent, InputId,
    KeyCode, KeyEvent, RelativeAxisCode, RelativeAxisEvent, UinputAbsSetup,
};
use proto::Button;

use crate::ipc::Head;

/// Largest absolute value: libinput scales `[0, ABS_MAX]` to the desktop.
const ABS_MAX: i32 = 32767;
const WHEEL_DELTA: i32 = 120;

pub enum Cmd {
    Move {
        head: u32,
        x: u32,
        y: u32,
    },
    Button {
        button: u32,
        down: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    /// The layout of the active session; empty: none known.
    Layout(Vec<Head>),
    /// The host is gone: release the buttons it left down.
    ReleaseAll,
}

#[derive(Clone)]
pub struct MouseHandle(mpsc::Sender<Cmd>);

impl MouseHandle {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.0.send(cmd);
    }

    /// A handle whose commands go nowhere (tests, without uinput).
    #[cfg(test)]
    pub fn detached() -> Self {
        Self(mpsc::channel().0)
    }
}

/// Starts the pointer thread. Fails when uinput is not available.
pub fn spawn() -> std::io::Result<MouseHandle> {
    let device = create_device()?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("mouse".into())
        .spawn(move || run(Mouse::new(device), rx))?;
    Ok(MouseHandle(tx))
}

fn create_device() -> std::io::Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for k in [
        KeyCode::BTN_LEFT,
        KeyCode::BTN_RIGHT,
        KeyCode::BTN_MIDDLE,
        KeyCode::BTN_SIDE,
        KeyCode::BTN_EXTRA,
    ] {
        keys.insert(k);
    }
    let mut rel = AttributeSet::<RelativeAxisCode>::new();
    for r in [
        RelativeAxisCode::REL_WHEEL,
        RelativeAxisCode::REL_HWHEEL,
        RelativeAxisCode::REL_WHEEL_HI_RES,
        RelativeAxisCode::REL_HWHEEL_HI_RES,
    ] {
        rel.insert(r);
    }
    let abs = |code| UinputAbsSetup::new(code, AbsInfo::new(0, 0, ABS_MAX, 0, 0, 0));
    VirtualDevice::builder()?
        .name("ameleo-display-agent pointer")
        .input_id(InputId::new(BusType::BUS_VIRTUAL, 0, 0, 1))
        .with_keys(&keys)?
        .with_relative_axes(&rel)?
        .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_X))?
        .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_Y))?
        .build()
}

fn run(mut mouse: Mouse, rx: mpsc::Receiver<Cmd>) {
    while let Ok(first) = rx.recv() {
        // Moves queued behind a newer move are skipped: the pointer goes
        // straight to the last position. A button between two moves keeps
        // both, so a click lands where the host clicked.
        let mut cmds = vec![first];
        cmds.extend(rx.try_iter());
        for i in 0..cmds.len() {
            if move_superseded(&cmds, i) {
                continue;
            }
            match &cmds[i] {
                Cmd::Move { head, x, y } => mouse.move_to(*head, *x, *y),
                Cmd::Button { button, down } => mouse.button(*button, *down),
                Cmd::Scroll { dx, dy } => mouse.scroll(*dx, *dy),
                Cmd::Layout(heads) => mouse.set_layout(heads.clone()),
                Cmd::ReleaseAll => mouse.release_all(),
            }
        }
    }
}

fn move_superseded(cmds: &[Cmd], i: usize) -> bool {
    matches!(cmds[i], Cmd::Move { .. }) && matches!(cmds.get(i + 1), Some(Cmd::Move { .. }))
}

struct Mouse {
    device: VirtualDevice,
    /// From the session agent; empty: use DRM.
    session_layout: Vec<Head>,
    layout: Layout,
    /// Buttons held down, by `Button` number.
    pressed: u16,
    /// High resolution scroll not yet worth a wheel click.
    rest: (i32, i32),
}

impl Mouse {
    fn new(device: VirtualDevice) -> Self {
        let mut m = Self {
            device,
            session_layout: Vec::new(),
            layout: Layout::default(),
            pressed: 0,
            rest: (0, 0),
        };
        m.refresh();
        m
    }

    fn set_layout(&mut self, heads: Vec<Head>) {
        self.session_layout = heads;
        self.refresh();
    }

    fn refresh(&mut self) {
        let heads = if self.session_layout.is_empty() {
            match drm_layout() {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("cannot read the monitors from DRM: {e}");
                    Vec::new()
                }
            }
        } else {
            self.session_layout.clone()
        };
        tracing::debug!(?heads, "pointer layout");
        self.layout = Layout::new(heads);
    }

    fn emit(&mut self, events: &[InputEvent]) {
        if let Err(e) = self.device.emit(events) {
            tracing::warn!("uinput write failed: {e}");
        }
    }

    fn move_to(&mut self, head: u32, x: u32, y: u32) {
        if self.layout.head(head).is_none() {
            // A head that appeared since: the layout may be stale.
            self.refresh();
        }
        let Some((ax, ay)) = self.layout.to_abs(head, x, y) else {
            tracing::debug!(head, "pointer move on an unknown head");
            return;
        };
        self.emit(&[
            *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_X, ax),
            *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_Y, ay),
        ]);
    }

    fn button(&mut self, button: u32, down: bool) {
        let Ok(button) = Button::try_from(button) else {
            return;
        };
        let key = match button {
            Button::Left => KeyCode::BTN_LEFT,
            Button::Middle => KeyCode::BTN_MIDDLE,
            Button::Right => KeyCode::BTN_RIGHT,
            Button::Side => KeyCode::BTN_SIDE,
            Button::Extra => KeyCode::BTN_EXTRA,
            // Wheel "buttons" fire on press only, one notch each.
            Button::WheelUp if down => return self.scroll(0, WHEEL_DELTA),
            Button::WheelDown if down => return self.scroll(0, -WHEEL_DELTA),
            Button::WheelLeft if down => return self.scroll(-WHEEL_DELTA, 0),
            Button::WheelRight if down => return self.scroll(WHEEL_DELTA, 0),
            _ => return,
        };
        let bit = 1u16 << button as u32;
        if down {
            self.pressed |= bit;
        } else if self.pressed & bit == 0 {
            return; // never pressed: nothing to release
        } else {
            self.pressed &= !bit;
        }
        self.emit(&[*KeyEvent::new(key, down as i32)]);
    }

    fn release_all(&mut self) {
        for n in 0..16u32 {
            if self.pressed & (1 << n) != 0 {
                tracing::debug!(button = n, "releasing button left down");
                self.button(n, false);
            }
        }
        self.rest = (0, 0);
    }

    /// Windows convention, as on the wire: `dy > 0` scrolls up, `dx > 0`
    /// right, 120 per notch. Linux has the same signs.
    fn scroll(&mut self, dx: i32, dy: i32) {
        let mut events = Vec::with_capacity(4);
        if dy != 0 {
            events.push(*RelativeAxisEvent::new(
                RelativeAxisCode::REL_WHEEL_HI_RES,
                dy,
            ));
            let (clicks, rest) = notches(self.rest.1, dy);
            self.rest.1 = rest;
            if clicks != 0 {
                events.push(*RelativeAxisEvent::new(RelativeAxisCode::REL_WHEEL, clicks));
            }
        }
        if dx != 0 {
            events.push(*RelativeAxisEvent::new(
                RelativeAxisCode::REL_HWHEEL_HI_RES,
                dx,
            ));
            let (clicks, rest) = notches(self.rest.0, dx);
            self.rest.0 = rest;
            if clicks != 0 {
                events.push(*RelativeAxisEvent::new(
                    RelativeAxisCode::REL_HWHEEL,
                    clicks,
                ));
            }
        }
        if !events.is_empty() {
            self.emit(&events);
        }
    }
}

/// Whole notches in `rest + delta`, and what is left. A change of direction
/// drops the rest.
fn notches(rest: i32, delta: i32) -> (i32, i32) {
    let rest = if rest.signum() * delta.signum() < 0 {
        0
    } else {
        rest
    };
    let total = rest + delta;
    (total / WHEEL_DELTA, total % WHEEL_DELTA)
}

/// The heads and the desktop's bounding box, in logical coordinates.
#[derive(Debug, Default)]
struct Layout {
    heads: Vec<Head>,
    origin: (i32, i32),
    size: (u32, u32),
}

impl Layout {
    fn new(heads: Vec<Head>) -> Self {
        let left = heads.iter().map(|h| h.1).min().unwrap_or(0);
        let top = heads.iter().map(|h| h.2).min().unwrap_or(0);
        let right = heads.iter().map(|h| h.1 + h.3 as i32).max().unwrap_or(1);
        let bottom = heads.iter().map(|h| h.2 + h.4 as i32).max().unwrap_or(1);
        Self {
            heads,
            origin: (left, top),
            size: ((right - left).max(1) as u32, (bottom - top).max(1) as u32),
        }
    }

    fn head(&self, head: u32) -> Option<&Head> {
        self.heads.iter().find(|h| h.0 == head)
    }

    /// The absolute axis values for pixel `(x, y)` of `head`.
    fn to_abs(&self, head: u32, x: u32, y: u32) -> Option<(i32, i32)> {
        let &(_, hx, hy, w, h, mw, mh) = self.head(head)?;
        // Pixel centers, scaled from the mode to the logical size.
        let lx = (2 * x.min(mw.saturating_sub(1)) as u64 + 1) * w as u64;
        let ly = (2 * y.min(mh.saturating_sub(1)) as u64 + 1) * h as u64;
        let px = (hx - self.origin.0) as u64 * 2 * mw.max(1) as u64 + lx; // x2 x mode
        let py = (hy - self.origin.1) as u64 * 2 * mh.max(1) as u64 + ly;
        // libinput: logical = value * size / (ABS_MAX + 1).
        let range = ABS_MAX as u64 + 1;
        let ax = px * range / (2 * mw.max(1) as u64 * self.size.0 as u64);
        let ay = py * range / (2 * mh.max(1) as u64 * self.size.1 as u64);
        Some((ax.min(ABS_MAX as u64) as i32, ay.min(ABS_MAX as u64) as i32))
    }
}

/// The virtio-gpu heads from DRM: connector `Virtual-N` is scanout `N-1`.
/// Put side by side in head order at scale 1, the compositors' default.
fn drm_layout() -> std::io::Result<Vec<Head>> {
    use drm::control::{Device as _, connector};
    use std::os::fd::{AsFd, BorrowedFd};

    struct Card(std::fs::File);
    impl AsFd for Card {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }
    impl drm::Device for Card {}
    impl drm::control::Device for Card {}

    let mut found = Vec::new();
    for entry in std::fs::read_dir("/dev/dri")? {
        let path = entry?.path();
        if !path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("card"))
        {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            continue;
        };
        let card = Card(file);
        let Ok(res) = card.resource_handles() else {
            continue;
        };
        for &c in res.connectors() {
            let Ok(info) = card.get_connector(c, false) else {
                continue;
            };
            if info.interface() != connector::Interface::Virtual
                || info.state() != connector::State::Connected
            {
                continue;
            }
            let mode = info
                .current_encoder()
                .and_then(|e| card.get_encoder(e).ok())
                .and_then(|e| e.crtc())
                .and_then(|c| card.get_crtc(c).ok())
                .and_then(|c| c.mode());
            if let Some(mode) = mode {
                let (w, h) = mode.size();
                found.push((info.interface_id().saturating_sub(1), w as u32, h as u32));
            }
        }
    }
    found.sort();
    let mut x = 0i32;
    Ok(found
        .into_iter()
        .map(|(head, w, h)| {
            let head = (head, x, 0, w, h, w, h);
            x += w as i32;
            head
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mv(x: u32) -> Cmd {
        Cmd::Move { head: 0, x, y: 0 }
    }

    #[test]
    fn moves_coalesce_only_between_other_events() {
        let press = || Cmd::Button {
            button: 0,
            down: true,
        };
        let cmds = vec![mv(1), mv(2), press(), mv(3), mv(4)];
        let kept: Vec<u32> = (0..cmds.len())
            .filter(|&i| !move_superseded(&cmds, i))
            .filter_map(|i| match cmds[i] {
                Cmd::Move { x, .. } => Some(x),
                _ => None,
            })
            .collect();
        assert_eq!(kept, vec![2, 4]);
    }

    #[test]
    fn single_head_corners() {
        let l = Layout::new(vec![(0, 0, 0, 1920, 1080, 1920, 1080)]);
        let (x0, y0) = l.to_abs(0, 0, 0).unwrap();
        let (x1, y1) = l.to_abs(0, 1919, 1079).unwrap();
        // libinput: value * size / 32768 lands on the same pixel.
        assert_eq!(x0 as u64 * 1920 / 32768, 0);
        assert_eq!(y0 as u64 * 1080 / 32768, 0);
        assert_eq!(x1 as u64 * 1920 / 32768, 1919);
        assert_eq!(y1 as u64 * 1080 / 32768, 1079);
        // Out of range clamps to the last pixel.
        assert_eq!(l.to_abs(0, 5000, 5000).unwrap(), (x1, y1));
        assert!(l.to_abs(1, 0, 0).is_none());
    }

    #[test]
    fn second_head_scaled() {
        // Head 1 at the right of head 0, 2560x1440 at scale 2.
        let l = Layout::new(vec![
            (0, 0, 0, 1920, 1080, 1920, 1080),
            (1, 1920, 0, 1280, 720, 2560, 1440),
        ]);
        assert_eq!(l.size, (3200, 1080));
        for (px, py, lx, ly) in [
            (0, 0, 1920, 0),
            (2559, 1439, 3199, 719),
            (1280, 720, 2560, 360),
        ] {
            let (ax, ay) = l.to_abs(1, px, py).unwrap();
            assert_eq!(ax as u64 * 3200 / 32768, lx, "x of {px}");
            assert_eq!(ay as u64 * 1080 / 32768, ly, "y of {py}");
        }
    }

    #[test]
    fn negative_origin() {
        // Head 1 left of head 0.
        let l = Layout::new(vec![
            (0, 0, 0, 1024, 768, 1024, 768),
            (1, -1024, 0, 1024, 768, 1024, 768),
        ]);
        let (ax, _) = l.to_abs(1, 0, 0).unwrap();
        assert_eq!(ax as u64 * 2048 / 32768, 0);
        let (ax, _) = l.to_abs(0, 0, 0).unwrap();
        assert_eq!(ax as u64 * 2048 / 32768, 1024);
    }

    #[test]
    fn wheel_notches() {
        assert_eq!(notches(0, 120), (1, 0));
        assert_eq!(notches(0, 60), (0, 60));
        assert_eq!(notches(60, 60), (1, 0));
        assert_eq!(notches(60, -30), (0, -30));
        assert_eq!(notches(0, -240), (-2, 0));
    }
}
