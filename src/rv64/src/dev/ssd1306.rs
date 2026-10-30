//! Solomon Systech SSD1306, a 128x64 monochrome OLED controller, as an I2C
//! device (address 0x3c), following its datasheet (rev 1.1):
//! - each transfer starts with a control byte: D/C# (bit 6) selects
//!   commands or display data, Co (bit 7) says one byte follows before the
//!   next control byte
//! - display RAM (GDDRAM): 8 pages of 128 columns, a byte per column with the
//!   page's 8 rows, LSB at the top
//! - the commands for addressing (page, horizontal and vertical modes),
//!   segment and COM remapping, start line, display offset, multiplex ratio,
//!   contrast, inversion, entire display on and display on/off
//!
//! Scrolling, fading and the hardware configuration commands (clock, charge
//! pump, precharge, COM pins, VCOMH) are accepted and ignored.

pub const ADDR: u8 = 0x3c;
pub const WIDTH: usize = 128;
pub const HEIGHT: usize = 64;
const PAGES: usize = HEIGHT / 8;

pub struct Ssd1306 {
    pub ram: [u8; WIDTH * PAGES],
    /// Changes whenever what's shown may have changed
    pub frame: u32,
    col: u8,
    page: u8,
    mode: u8,
    col_start: u8,
    col_end: u8,
    page_start: u8,
    page_end: u8,
    seg_remap: bool,
    com_reverse: bool,
    start_line: u8,
    offset: u8,
    mux: u8,
    pub contrast: u8,
    invert: bool,
    entire_on: bool,
    on: bool,
    /// The command being received (with its arguments)
    cmd: Vec<u8>,
    /// Waiting for a control byte; the last one's D/C# and Co
    want_control: bool,
    data: bool,
    single: bool,
}

impl Default for Ssd1306 {
    fn default() -> Ssd1306 {
        // Reset state (datasheet section 10)
        Ssd1306 {
            ram: [0; WIDTH * PAGES],
            frame: 0,
            col: 0,
            page: 0,
            mode: 2,
            col_start: 0,
            col_end: 127,
            page_start: 0,
            page_end: 7,
            seg_remap: false,
            com_reverse: false,
            start_line: 0,
            offset: 0,
            mux: 63,
            contrast: 0x7f,
            invert: false,
            entire_on: false,
            on: false,
            cmd: Vec::new(),
            want_control: true,
            data: false,
            single: false,
        }
    }
}

/// The number of argument bytes a command takes.
fn arguments(c: u8) -> usize {
    match c {
        0x81 | 0x20 | 0xa8 | 0xd3 | 0xd5 | 0xd9 | 0xda | 0xdb | 0x8d | 0xad | 0x23 => 1,
        0x21 | 0x22 | 0xa3 => 2,
        0x29 | 0x2a => 5,
        0x26 | 0x27 => 6,
        _ => 0,
    }
}

impl Ssd1306 {
    /// A transfer to the display starts (after its address).
    pub fn start(&mut self) {
        self.want_control = true;
    }

    /// A byte of a transfer.
    pub fn byte(&mut self, b: u8) {
        if self.want_control {
            self.single = b & 0x80 != 0;
            self.data = b & 0x40 != 0;
            self.want_control = false;
            return;
        }
        if self.data {
            self.write_data(b);
        } else {
            self.command_byte(b);
        }
        if self.single {
            self.want_control = true;
        }
    }

    fn write_data(&mut self, b: u8) {
        let i = self.page as usize % PAGES * WIDTH + self.col as usize % WIDTH;
        if self.ram[i] != b {
            self.ram[i] = b;
            self.frame = self.frame.wrapping_add(1);
        }
        match self.mode {
            // Horizontal: along the column range, then the next page
            0 => {
                if self.col >= self.col_end {
                    self.col = self.col_start;
                    self.page = if self.page >= self.page_end { self.page_start } else { self.page + 1 };
                } else {
                    self.col += 1;
                }
            }
            // Vertical: down the page range, then the next column
            1 => {
                if self.page >= self.page_end {
                    self.page = self.page_start;
                    self.col = if self.col >= self.col_end { self.col_start } else { self.col + 1 };
                } else {
                    self.page += 1;
                }
            }
            // Page: along the page, wrapping to its start
            _ => self.col = (self.col + 1) % WIDTH as u8,
        }
    }

    fn command_byte(&mut self, b: u8) {
        self.cmd.push(b);
        if self.cmd.len() < 1 + arguments(self.cmd[0]) {
            return;
        }
        let cmd = std::mem::take(&mut self.cmd);
        self.command(&cmd);
        self.frame = self.frame.wrapping_add(1);
    }

    fn command(&mut self, c: &[u8]) {
        let arg = |i: usize| c[i];
        match c[0] {
            // Page mode column, low and high nibbles; page (the pointers are
            // shared, so these work in every mode, as drivers rely on)
            0x00..=0x0f => self.col = (self.col & 0xf0) | (c[0] & 0x0f),
            0x10..=0x1f => self.col = (self.col & 0x0f) | (c[0] & 0x07) << 4,
            0xb0..=0xb7 => self.page = c[0] & 7,
            0x20 => self.mode = arg(1) & 3,
            0x21 => {
                self.col_start = arg(1) & 0x7f;
                self.col_end = arg(2) & 0x7f;
                self.col = self.col_start;
            }
            0x22 => {
                self.page_start = arg(1) & 7;
                self.page_end = arg(2) & 7;
                self.page = self.page_start;
            }
            0x40..=0x7f => self.start_line = c[0] & 0x3f,
            0x81 => self.contrast = arg(1),
            0xa0 | 0xa1 => self.seg_remap = c[0] & 1 != 0,
            0xa4 | 0xa5 => self.entire_on = c[0] & 1 != 0,
            0xa6 | 0xa7 => self.invert = c[0] & 1 != 0,
            0xa8 => self.mux = (arg(1) & 0x3f).max(15),
            0xae | 0xaf => self.on = c[0] & 1 != 0,
            0xc0 | 0xc8 => self.com_reverse = c[0] & 8 != 0,
            0xd3 => self.offset = arg(1) & 0x3f,
            _ => {}
        }
    }

    /// The panel's pixels, a byte per pixel (0 off, 1 on), row by row, as
    /// it's seen on the usual 0.96" modules: upright with the segment remap
    /// and reversed COM scan (A1, C8) that drivers set for them, rotated by
    /// 180 degrees at the reset settings.
    pub fn render(&self, out: &mut [u8]) {
        let rows = self.mux as usize + 1;
        for r in 0..HEIGHT {
            for x in 0..WIDTH {
                let on = if !self.on || r >= rows {
                    false
                } else if self.entire_on {
                    true
                } else {
                    // The row's COM line, and the RAM row shown on it
                    let com = if self.com_reverse { r } else { rows - 1 - r };
                    let y = (com + self.start_line as usize + self.offset as usize) % HEIGHT;
                    let col = if self.seg_remap { x } else { WIDTH - 1 - x };
                    (self.ram[y / 8 * WIDTH + col] >> (y % 8) & 1 != 0) != self.invert
                };
                out[r * WIDTH + x] = on as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(d: &mut Ssd1306, bytes: &[u8]) {
        d.start();
        for &b in bytes {
            d.byte(b);
        }
    }

    #[test]
    fn horizontal_mode_wraps_into_the_next_page() {
        let mut d = Ssd1306::default();
        send(&mut d, &[0x00, 0x20, 0x00, 0x21, 126, 127, 0x22, 0, 7, 0xaf]);
        send(&mut d, &[0x40, 1, 2, 3]);
        assert_eq!((d.ram[126], d.ram[127], d.ram[128 + 126]), (1, 2, 3));
    }

    #[test]
    fn page_pointer_commands_and_flips() {
        let mut d = Ssd1306::default();
        // Co set: one command per control byte
        send(&mut d, &[0x80, 0xaf, 0x80, 0xb2, 0x80, 0x05, 0x80, 0x10, 0x40, 0x81]);
        assert_eq!(d.ram[2 * 128 + 5], 0x81);
        let mut px = vec![0; WIDTH * HEIGHT];
        // At reset, rotated by 180 degrees
        d.render(&mut px);
        assert_eq!((px[(63 - 16) * 128 + 122], px[(63 - 23) * 128 + 122], px[(63 - 17) * 128 + 122]), (1, 1, 0));
        // With A1 and C8, upright
        send(&mut d, &[0x00, 0xa1, 0xc8]);
        d.render(&mut px);
        assert_eq!((px[16 * 128 + 5], px[23 * 128 + 5], px[17 * 128 + 5]), (1, 1, 0));
    }
}
