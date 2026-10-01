//! Anti-aliased bitmap icons, as generated into src/fonts.rs by
//! tools/fontgen.

/// An icon: coverage, 4 bits a pixel, high nibble first; each row starts on a
/// byte.
pub struct Icon {
    pub width: u16,
    pub height: u16,
    pub data: &'static [u8],
}

impl Icon {
    /// Calls `pixel(x, y, coverage)` for each pixel with coverage, with the
    /// icon's top left at the origin; coverage is 1 to 15.
    pub fn pixels(&self, mut pixel: impl FnMut(i32, i32, u8)) {
        let stride = usize::from(self.width).div_ceil(2);
        for row in 0..usize::from(self.height) {
            for col in 0..usize::from(self.width) {
                let byte = self.data[row * stride + col / 2];
                let coverage = if col % 2 == 0 { byte >> 4 } else { byte & 0xF };
                if coverage > 0 {
                    pixel(col as i32, row as i32, coverage);
                }
            }
        }
    }
}
