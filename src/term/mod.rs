//! Terminal setup, teardown and geometry.

pub mod caps;
pub mod events;
pub mod input;
pub mod keysym;
pub mod kitty;
pub mod shm;
pub mod writer;

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use crossterm::terminal;

static RAW: AtomicBool = AtomicBool::new(false);
static FULL_SCREEN: AtomicBool = AtomicBool::new(false);

/// Everything the full-screen session turns on.
///
/// Autowrap is off because a write that reaches the last column would scroll the
/// screen, and scrolling moves every graphics placement with it. Mouse reporting
/// is `1003` (all motion, not just while dragging) plus `1006` for the SGR
/// framing and `1016` to get that position in pixels rather than cells. The
/// keyboard flags request every key, its event type and the layout's shifted
/// character. RFB needs both the symbol to type and a matching release.
///
/// Focus reporting (`1004`) matters more than it looks: without it the terminal
/// never says the window lost focus, so a modifier held while switching away stays
/// held as far as the remote is concerned, and a background window with mode
/// `1003` still armed can keep tracking the pointer behind another application.
const SETUP: &str = concat!(
    "\x1b[?1049h", // alternate screen
    "\x1b[?25l",   // hide the text cursor
    "\x1b[?7l",    // no autowrap
    "\x1b[?1003h", // report all mouse motion
    "\x1b[?1006h", // SGR mouse framing
    "\x1b[?1016h", // ... in pixels
    "\x1b[?2004h", // bracketed paste
    "\x1b[?1004h", // focus in/out, so held input releases and motion pauses on focus loss
    "\x1b[>15u",   // kitty keyboard: disambiguate | event types | alternate keys | all keys
    "\x1b[0m",     // erase with the default background so desktop tiles show through
    "\x1b[2J",
);

/// Toggle all-motion mouse reporting as the terminal gains and loses focus, so
/// an unfocused terminal window does not keep tracking the pointer while the
/// user is working in another application.
pub const ENABLE_MOUSE_MOTION: &[u8] = b"\x1b[?1003h";
pub const DISABLE_MOUSE_MOTION: &[u8] = b"\x1b[?1003l";

/// The inverse of [`SETUP`], plus a delete-all-images so no placements are left
/// in the terminal's image store.
const TEARDOWN: &str = concat!(
    "\x1b_Ga=d,d=A,q=2\x1b\\",
    "\x1b[<u",
    "\x1b[?1004l",
    "\x1b[?2004l",
    "\x1b[?1016l",
    "\x1b[?1006l",
    "\x1b[?1003l",
    "\x1b[?7h",
    "\x1b[?25h",
    "\x1b[?1049l",
);

/// Owns every mode change we make, and undoes them on drop and on panic.
///
/// Construction only enables raw mode: capability probing has to happen in raw
/// mode but before the alternate screen, so that a terminal which cannot do
/// graphics can be refused without a screen flash.
pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        if !io::stdout().is_terminal() {
            anyhow::bail!("stdout is not a terminal");
        }
        install_panic_hook();
        terminal::enable_raw_mode().context("failed to enter raw mode")?;
        RAW.store(true, Ordering::SeqCst);
        Ok(Self { _private: () })
    }

    /// Switch to the alternate screen and turn on input reporting.
    pub fn begin_full_screen(&self) -> Result<()> {
        let mut out = io::stdout();
        out.write_all(SETUP.as_bytes())?;
        out.flush()?;
        FULL_SCREEN.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Restore the terminal. Idempotent, and safe from a panic or signal path.
    pub fn leave() {
        if FULL_SCREEN.swap(false, Ordering::SeqCst) {
            let mut out = io::stdout();
            let _ = out.write_all(TEARDOWN.as_bytes());
            let _ = out.flush();
        }
        if RAW.swap(false, Ordering::SeqCst) {
            let _ = terminal::disable_raw_mode();
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::leave();
    }
}

/// Restore the terminal before the default hook prints anything, so the panic
/// message lands on a usable screen rather than the alternate one.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            TerminalGuard::leave();
            default(info);
        }));
    });
}

/// The terminal's geometry, in both cells and pixels.
///
/// When the terminal supplies a pixel area, derive cell size from the same grid
/// the viewer addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    pub cols: u16,
    pub rows: u16,
    pub px_w: u32,
    pub px_h: u32,
    pub cell_w: u32,
    pub cell_h: u32,
}

/// Read the current grid and any pixel dimensions the kernel provides.
pub fn window_size() -> Result<terminal::WindowSize> {
    if let Ok(size) = terminal::window_size() {
        return Ok(size);
    }
    let (columns, rows) = terminal::size().context("failed to read terminal size")?;
    Ok(terminal::WindowSize {
        columns,
        rows,
        width: 0,
        height: 0,
    })
}

pub fn ioctl_pixels(size: &terminal::WindowSize) -> Option<(u32, u32)> {
    (size.width != 0 && size.height != 0).then(|| (u32::from(size.width), u32::from(size.height)))
}

fn area_from_cells(size: &terminal::WindowSize, cell: (u32, u32)) -> Result<(u32, u32)> {
    let w = cell.0.checked_mul(u32::from(size.columns));
    let h = cell.1.checked_mul(u32::from(size.rows));
    w.zip(h).context("terminal pixel dimensions overflow")
}

impl Metrics {
    /// Use the startup probe before the input reader and frame writer take ownership.
    pub fn from_probe(size: &terminal::WindowSize, caps: &caps::Caps) -> Result<Self> {
        Self::from_window(size, || {
            if let Some(area) = caps.text_area_px.filter(|&(w, h)| w > 0 && h > 0) {
                return Ok(area);
            }
            if let Some(cell) = caps.cell_px.filter(|&(w, h)| w > 0 && h > 0) {
                return area_from_cells(size, cell);
            }
            // A terminal that missed the initial query still gets one startup retry.
            caps::query_pixel_geometry()
        })
    }

    /// A resize must not query the terminal over the input or output stream. If
    /// the kernel omits pixels, apply the last measured cell size to the new grid.
    pub fn refreshed(&self) -> Result<Self> {
        let size = window_size()?;
        Self::from_window(&size, || {
            if (size.columns, size.rows) == (self.cols, self.rows) {
                return Ok((self.px_w, self.px_h));
            }
            area_from_cells(&size, (self.cell_w, self.cell_h))
        })
    }

    fn from_window(
        size: &terminal::WindowSize,
        fallback: impl FnOnce() -> Result<(u32, u32)>,
    ) -> Result<Self> {
        let (cols, rows) = (size.columns, size.rows);
        anyhow::ensure!(cols > 0 && rows > 0, "terminal reports a zero-sized grid");
        let (px_w, px_h) = match ioctl_pixels(size) {
            Some(area) => area,
            None => fallback()?,
        };
        anyhow::ensure!(
            px_w > 0 && px_h > 0,
            "terminal did not report its size in pixels, which a pixel-exact \
             client cannot work without"
        );

        Ok(Self {
            cols,
            rows,
            px_w,
            px_h,
            cell_w: (px_w / u32::from(cols)).max(1),
            cell_h: (px_h / u32::from(rows)).max(1),
        })
    }

    /// Rows available to the remote framebuffer: everything but the status line.
    pub fn image_rows(&self) -> u16 {
        self.rows.saturating_sub(1)
    }

    /// Pixel area available to the remote framebuffer.
    ///
    /// Derived from the cell size rather than from `px_w` directly: when the
    /// window is not an exact multiple of the cell size, the reported text area
    /// is larger than the cells actually cover, and the excess is not
    /// addressable.
    pub fn image_area(&self) -> (u32, u32) {
        (
            self.cell_w * u32::from(self.cols),
            self.cell_h * u32::from(self.image_rows()),
        )
    }
}
