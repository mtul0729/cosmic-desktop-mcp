//! Minimal `/dev/uinput` client.
//!
//! COSMIC does not advertise `zwp_virtual_pointer_manager_v1`, so pointer
//! buttons and wheel scrolling have no protocol path — they have to be
//! injected below the compositor. Keyboard input could go through
//! `zwp_virtual_keyboard_manager_v1`, but that protocol is not exposed by the
//! `wayland-protocols` crate, and mixing two input backends buys nothing.
//! Everything goes through one absolute-pointer + keyboard uinput device.
//!
//! The ioctl numbers are stable kernel ABI (`linux/uinput.h`), so this is
//! hand-rolled rather than pulling in a bindings crate.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

use anyhow::{Context, Result, bail};

const UINPUT_MAX_NAME_SIZE: usize = 80;
const UINPUT_IOCTL_BASE: u32 = b'U' as u32;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0;
const REL_WHEEL: u16 = 8;
const REL_HWHEEL: u16 = 6;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const BUS_VIRTUAL: u16 = 0x06;

// _IOC direction bits
const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u64 = 1;

const fn iow(nr: u64, size: u64) -> libc::c_ulong {
    ((IOC_WRITE << IOC_DIRSHIFT)
        | ((UINPUT_IOCTL_BASE as u64) << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT)) as libc::c_ulong
}

// UI_DEV_CREATE / UI_DEV_DESTROY are _IO, not _IOW: no direction bit and no
// size. Encoding them as _IOW makes the kernel reject them with EINVAL.
const fn io(nr: u64) -> libc::c_ulong {
    (((UINPUT_IOCTL_BASE as u64) << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT)) as libc::c_ulong
}

const UI_DEV_CREATE: libc::c_ulong = io(1);
const UI_DEV_DESTROY: libc::c_ulong = io(2);
const UI_DEV_SETUP: libc::c_ulong = iow(3, std::mem::size_of::<UinputSetup>() as u64);
const UI_ABS_SETUP: libc::c_ulong = iow(4, std::mem::size_of::<UinputAbsSetup>() as u64);
const UI_SET_EVBIT: libc::c_ulong = iow(100, std::mem::size_of::<libc::c_int>() as u64);
const UI_SET_KEYBIT: libc::c_ulong = iow(101, std::mem::size_of::<libc::c_int>() as u64);
const UI_SET_RELBIT: libc::c_ulong = iow(102, std::mem::size_of::<libc::c_int>() as u64);
const UI_SET_ABSBIT: libc::c_ulong = iow(103, std::mem::size_of::<libc::c_int>() as u64);

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [libc::c_char; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TimeVal {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: TimeVal,
    type_: u16,
    code: u16,
    value: i32,
}

pub struct Device {
    // Owning the `File` matters: dropping it closes the fd, and the number
    // gets reused by the next `open(2)` in this process (often the clipboard
    // child), which would make event writes land somewhere random.
    file: File,
    width: i32,
    height: i32,
}

impl Device {
    /// Creates an absolute-pointer + keyboard device covering a
    /// `width` x `height` coordinate space.
    pub fn open(name: &str, width: i32, height: i32) -> Result<Self> {
        let path = "/dev/uinput";
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .with_context(|| {
                format!(
                    "open {path}: needs write access (udev rule or `uinput` group membership)"
                )
            })?;
        let fd = file.as_raw_fd();

        let mut setup = UinputSetup {
            id: InputId {
                bustype: BUS_VIRTUAL,
                vendor: 0x1,
                product: 0x1,
                version: 1,
            },
            name: [0; UINPUT_MAX_NAME_SIZE],
            ff_effects_max: 0,
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(UINPUT_MAX_NAME_SIZE - 1);
        for (slot, byte) in setup.name.iter_mut().zip(bytes[..n].iter()) {
            *slot = *byte as libc::c_char;
        }

        ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int)?;
        ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_int)?;
        ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_int)?;
        ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int)?;

        for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
            ioctl(fd, UI_SET_KEYBIT, code as libc::c_int)?;
        }
        // Every keycode we can emit from the layout table below.
        for code in 1..=248u16 {
            ioctl(fd, UI_SET_KEYBIT, code as libc::c_int)?;
        }
        ioctl(fd, UI_SET_RELBIT, REL_WHEEL as libc::c_int)?;
        ioctl(fd, UI_SET_RELBIT, REL_HWHEEL as libc::c_int)?;

        for (code, max) in [(ABS_X, width - 1), (ABS_Y, height - 1)] {
            ioctl(fd, UI_SET_ABSBIT, code as libc::c_int)?;
            let abs = UinputAbsSetup {
                code,
                absinfo: InputAbsInfo {
                    value: 0,
                    minimum: 0,
                    maximum: max,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            };
            let rc = unsafe {
                libc::ioctl(fd, UI_ABS_SETUP as _, &abs as *const UinputAbsSetup)
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("UI_ABS_SETUP for abs code {code}"));
            }
        }

        let rc = unsafe { libc::ioctl(fd, UI_DEV_SETUP as _, &setup as *const UinputSetup) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).context("UI_DEV_SETUP");
        }
        ioctl(fd, UI_DEV_CREATE, 0)?;

        // The compositor needs a moment to notice the new device; events sent
        // before it is picked up are dropped silently.
        std::thread::sleep(Duration::from_millis(300));

        Ok(Self {
            file,
            width,
            height,
        })
    }

    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    fn emit(&self, events: &[InputEvent]) -> Result<()> {
        let mut buf = Vec::with_capacity(std::mem::size_of_val(events));
        for event in events {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    event as *const InputEvent as *const u8,
                    std::mem::size_of::<InputEvent>(),
                )
            };
            buf.extend_from_slice(bytes);
        }
        let written = unsafe {
            libc::write(self.file.as_raw_fd(), buf.as_ptr() as *const _, buf.len())
        };
        if written < 0 {
            return Err(std::io::Error::last_os_error()).context("write input_event");
        }
        Ok(())
    }

    fn ev(type_: u16, code: u16, value: i32) -> InputEvent {
        InputEvent {
            time: TimeVal {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        }
    }

    /// Moves the pointer. Coordinates are in the device's coordinate space,
    /// i.e. screenshot pixels when the screenshot is not downscaled.
    pub fn move_to(&self, x: i32, y: i32) -> Result<()> {
        let x = x.clamp(0, self.width - 1);
        let y = y.clamp(0, self.height - 1);
        self.emit(&[
            Self::ev(EV_ABS, ABS_X, x),
            Self::ev(EV_ABS, ABS_Y, y),
            Self::ev(EV_SYN, SYN_REPORT, 0),
        ])
    }

    pub fn button(&self, button: Button, pressed: bool) -> Result<()> {
        let value = i32::from(pressed);
        self.emit(&[
            Self::ev(EV_KEY, button.code(), value),
            Self::ev(EV_SYN, SYN_REPORT, 0),
        ])
    }

    pub fn scroll(&self, vertical: i32, horizontal: i32) -> Result<()> {
        let mut events = Vec::new();
        if vertical != 0 {
            events.push(Self::ev(EV_REL, REL_WHEEL, vertical));
        }
        if horizontal != 0 {
            events.push(Self::ev(EV_REL, REL_HWHEEL, horizontal));
        }
        if events.is_empty() {
            return Ok(());
        }
        events.push(Self::ev(EV_SYN, SYN_REPORT, 0));
        self.emit(&events)
    }

    pub fn key(&self, code: u16, pressed: bool) -> Result<()> {
        self.emit(&[
            Self::ev(EV_KEY, code, i32::from(pressed)),
            Self::ev(EV_SYN, SYN_REPORT, 0),
        ])
    }

}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY as _);
        }
    }
}

fn ioctl(fd: RawFd, request: libc::c_ulong, value: libc::c_int) -> Result<()> {
    let rc = unsafe { libc::ioctl(fd, request as _, value) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("ioctl {request:#x}"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "middle" => Ok(Self::Middle),
            other => bail!("unknown button {other:?} (expected left, right or middle)"),
        }
    }

    pub fn code(self) -> u16 {
        match self {
            Self::Left => BTN_LEFT,
            Self::Right => BTN_RIGHT,
            Self::Middle => BTN_MIDDLE,
        }
    }
}

pub const KEY_LEFTSHIFT: u16 = 42;

/// US layout only: the compositor's layout is irrelevant here because the
/// events go in below it.
pub fn keycode_for(ch: char) -> Option<(u16, bool)> {
    let (code, shift) = match ch {
        'a'..='z' => (letter(ch)?, false),
        'A'..='Z' => (letter(ch.to_ascii_lowercase())?, true),
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
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        '\\' => (43, false),
        '|' => (43, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        '`' => (41, false),
        '~' => (41, true),
        _ => return None,
    };
    Some((code, shift))
}

fn letter(lower: char) -> Option<u16> {
    let code = match lower {
        'q' => 16,
        'w' => 17,
        'e' => 18,
        'r' => 19,
        't' => 20,
        'y' => 21,
        'u' => 22,
        'i' => 23,
        'o' => 24,
        'p' => 25,
        'a' => 30,
        's' => 31,
        'd' => 32,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'z' => 44,
        'x' => 45,
        'c' => 46,
        'v' => 47,
        'b' => 48,
        'n' => 49,
        'm' => 50,
        _ => return None,
    };
    Some(code)
}

/// Named keys accepted by the `key` tool, as `ctrl+s` style chords.
pub fn parse_chord(chord: &str) -> Result<Vec<u16>> {
    let mut codes = Vec::new();
    for part in chord.split('+') {
        let part = part.trim().to_ascii_lowercase();
        let code = match part.as_str() {
            "ctrl" | "control" => 29,
            "shift" => KEY_LEFTSHIFT,
            "alt" => 56,
            "super" | "meta" => 125,
            "enter" | "return" => 28,
            "tab" => 15,
            "esc" | "escape" => 1,
            "backspace" => 14,
            "delete" => 111,
            "space" => 57,
            "up" => 103,
            "down" => 108,
            "left" => 105,
            "right" => 106,
            "home" => 102,
            "end" => 107,
            "pageup" => 104,
            "pagedown" => 109,
            single if single.chars().count() == 1 => keycode_for(single.chars().next().unwrap())
                .with_context(|| format!("unsupported key {part:?}"))?
                .0,
            other => bail!("unknown key {other:?}"),
        };
        codes.push(code);
    }
    if codes.is_empty() {
        bail!("empty key chord");
    }
    Ok(codes)
}
