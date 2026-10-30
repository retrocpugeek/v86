//! Sitronix ST7735S, a 132x162 colour TFT controller on SPI, with an 80x160
//! IPS panel (as on the common 0.96" modules), following its datasheet
//! (v1.4). Bytes come with the D/CX pin: low for a command, high for its
//! arguments and for pixel data.
//! - frame memory: 132 columns by 162 rows of RGB565 (16 bits per pixel,
//!   COLMOD 0x05, the most significant byte first)
//! - CASET/RASET set the window RAMWR writes, left to right, then down
//! - MADCTL: MV exchanges rows and columns, then MX and MY mirror them (the
//!   way MCU drivers' offsets for these panels assume); its RGB bit swaps red
//!   and blue
//! - the panel shows columns 26 to 105 and rows 1 to 160 of the frame
//!   memory, is wired BGR (MADCTL's RGB bit set shows colours as sent) and,
//!   being IPS, inverts them unless INVON is set
//!
//! Gamma, power, frame rate and other configuration commands are accepted
//! and ignored; other pixel formats are taken as 16 bits. Reads (RDDID, ...)
//! aren't modelled: the pins are write-only here.
//!
//! The same model as the RISC-V core's (src/rv64/src/dev/st7735.rs), for the
//! card10's display (dev/max32665.rs).

pub const COLS: usize = 132;
pub const ROWS: usize = 162;
/// The panel: frame memory columns PANEL_COL.. and rows PANEL_ROW..
pub const PANEL_COLS: usize = 80;
pub const PANEL_ROWS: usize = 160;
const PANEL_COL: usize = 26;
const PANEL_ROW: usize = 1;
/// The view render() draws: the panel on its side, 160 wide and 80 high
pub const VIEW_WIDTH: usize = PANEL_ROWS;
pub const VIEW_HEIGHT: usize = PANEL_COLS;

const SWRESET: u8 = 0x01;
const SLPIN: u8 = 0x10;
const SLPOUT: u8 = 0x11;
const INVOFF: u8 = 0x20;
const INVON: u8 = 0x21;
const DISPOFF: u8 = 0x28;
const DISPON: u8 = 0x29;
const CASET: u8 = 0x2a;
const RASET: u8 = 0x2b;
const RAMWR: u8 = 0x2c;
const MADCTL: u8 = 0x36;
const MADCTL_MY: u8 = 0x80;
const MADCTL_MX: u8 = 0x40;
const MADCTL_MV: u8 = 0x20;
const MADCTL_BGR: u8 = 0x08;

pub struct St7735 {
    pub ram: Vec<u16>,
    /// Changes whenever what's shown may have changed
    pub frame: u32,
    cmd: u8,
    args: Vec<u8>,
    /// The window, in the addressing MADCTL sets up, and the write position
    xs: u16,
    xe: u16,
    ys: u16,
    ye: u16,
    x: u16,
    y: u16,
    /// The first byte of a pixel
    hi: Option<u8>,
    madctl: u8,
    invert: bool,
    on: bool,
    asleep: bool,
    /// The panel's mounted the other way up (the card10's): the view
    /// rotated 180 degrees
    pub flip: bool,
}

impl Default for St7735 {
    fn default() -> St7735 {
        // Reset state (datasheet section 9.13): asleep, display off, the
        // window the whole frame memory
        St7735 {
            ram: vec![0; COLS * ROWS],
            frame: 0,
            cmd: 0,
            args: Vec::new(),
            xs: 0,
            xe: COLS as u16 - 1,
            ys: 0,
            ye: ROWS as u16 - 1,
            x: 0,
            y: 0,
            hi: None,
            madctl: 0,
            invert: false,
            on: false,
            asleep: true,
            flip: false,
        }
    }
}

impl St7735 {
    /// A hardware reset (RESX low); the frame memory keeps what it had.
    pub fn reset(&mut self) {
        let ram = std::mem::take(&mut self.ram);
        *self = St7735 { ram, frame: self.frame.wrapping_add(1), flip: self.flip, ..St7735::default() };
    }

    /// A byte, with D/CX low (a command) or high (data).
    pub fn byte(&mut self, b: u8, data: bool) {
        if !data {
            self.command(b);
            return;
        }
        if self.cmd == RAMWR {
            match self.hi.take() {
                None => self.hi = Some(b),
                Some(hi) => self.pixel(u16::from_be_bytes([hi, b])),
            }
            return;
        }
        self.args.push(b);
        let a = &self.args;
        let be = |i: usize| u16::from_be_bytes([a[i], a[i + 1]]);
        match (self.cmd, a.len()) {
            (CASET, 4) => (self.xs, self.xe) = (be(0), be(2)),
            (RASET, 4) => (self.ys, self.ye) = (be(0), be(2)),
            (MADCTL, 1) => {
                self.madctl = a[0];
                self.frame = self.frame.wrapping_add(1);
            }
            _ => {}
        }
    }

    fn command(&mut self, c: u8) {
        self.cmd = c;
        self.args.clear();
        self.hi = None;
        match c {
            SWRESET => self.reset(),
            SLPIN => self.asleep = true,
            SLPOUT => self.asleep = false,
            INVOFF => self.invert = false,
            INVON => self.invert = true,
            DISPOFF => self.on = false,
            DISPON => self.on = true,
            RAMWR => (self.x, self.y) = (self.xs, self.ys),
            _ => return,
        }
        self.frame = self.frame.wrapping_add(1);
    }

    /// Write a pixel at the write position, and advance it.
    fn pixel(&mut self, v: u16) {
        let (mut c, mut r) = (self.x as usize, self.y as usize);
        if self.madctl & MADCTL_MV != 0 {
            (c, r) = (r, c);
        }
        if self.madctl & MADCTL_MX != 0 {
            c = (COLS - 1).wrapping_sub(c);
        }
        if self.madctl & MADCTL_MY != 0 {
            r = (ROWS - 1).wrapping_sub(r);
        }
        if c < COLS && r < ROWS {
            self.ram[r * COLS + c] = v;
            self.frame = self.frame.wrapping_add(1);
        }
        if self.x < self.xe {
            self.x += 1;
        } else {
            self.x = self.xs;
            self.y = if self.y < self.ye { self.y + 1 } else { self.ys };
        }
    }

    /// The panel on its side (VIEW_WIDTH x VIEW_HEIGHT, its row 160 at the
    /// left), as 0xRRGGBB. Off or asleep, it's black.
    pub fn render(&self, out: &mut [u32]) {
        let lit = self.on && !self.asleep;
        let swap = self.madctl & MADCTL_BGR == 0;
        for (i, o) in out.iter_mut().enumerate().take(VIEW_WIDTH * VIEW_HEIGHT) {
            let (mut vx, mut vy) = (i % VIEW_WIDTH, i / VIEW_WIDTH);
            if self.flip {
                (vx, vy) = (VIEW_WIDTH - 1 - vx, VIEW_HEIGHT - 1 - vy);
            }
            let (c, r) = (PANEL_COL + vy, PANEL_ROW + PANEL_ROWS - 1 - vx);
            let mut v = self.ram[r * COLS + c];
            // IPS: inverted unless INVON
            if !self.invert {
                v = !v;
            }
            let (mut red, green, mut blue) = ((v >> 11) as u32, (v >> 5 & 63) as u32, (v & 31) as u32);
            if swap {
                std::mem::swap(&mut red, &mut blue);
            }
            *o = if lit { (red * 255 / 31) << 16 | (green * 255 / 63) << 8 | blue * 255 / 31 } else { 0 };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(d: &mut St7735, cmd: u8, args: &[u8]) {
        d.byte(cmd, false);
        for &a in args {
            d.byte(a, true);
        }
    }

    /// As the mini game console's driver draws in its rotation 3 (MY, MV,
    /// BGR; columns offset 1, rows 26): its (0, 0) is the view's top left.
    #[test]
    fn rotated_window_lands_top_left() {
        let mut d = St7735::default();
        send(&mut d, SLPOUT, &[]);
        send(&mut d, INVON, &[]);
        send(&mut d, MADCTL, &[MADCTL_MY | MADCTL_MV | MADCTL_BGR]);
        send(&mut d, DISPON, &[]);
        send(&mut d, CASET, &[0, 1, 0, 2]);
        send(&mut d, RASET, &[0, 26, 0, 26]);
        send(&mut d, RAMWR, &[0xf8, 0x00, 0x07, 0xe0]);
        let mut out = vec![0; VIEW_WIDTH * VIEW_HEIGHT];
        d.render(&mut out);
        assert_eq!(out[0], 0xff0000);
        assert_eq!(out[1], 0x00ff00);
        assert_eq!(out[2], 0);
    }
}
