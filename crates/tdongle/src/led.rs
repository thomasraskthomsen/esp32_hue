//! The dongle's APA102 RGB LED, on two GPIOs driven by hand: the LED needs
//! only a clock and data line, and a few bytes per update.

use esp_hal::gpio::{Level, Output};

/// The LED's orange at full level.
const ORANGE: [u8; 3] = [255, 100, 0];

pub struct Led<'d> {
    clock: Output<'d>,
    data: Output<'d>,
}

impl<'d> Led<'d> {
    pub fn new(clock: Output<'d>, data: Output<'d>) -> Self {
        Self { clock, data }
    }

    /// Lights the LED orange at `level` out of `full`; 0 turns it off.
    pub fn set_level(&mut self, level: u8, full: u8) {
        let scale = |value: u8| (u16::from(value) * u16::from(level) / u16::from(full)) as u8;
        let [red, green, blue] = ORANGE.map(scale);
        // Start frame, one LED frame (full global brightness, colour carries
        // the level; blue, green, red), then an end frame to clock it out.
        for byte in [0, 0, 0, 0, 0xFF, blue, green, red, 0xFF, 0xFF, 0xFF, 0xFF] {
            self.send(byte);
        }
    }

    fn send(&mut self, byte: u8) {
        for bit in (0..8).rev() {
            self.data.set_level(Level::from(byte >> bit & 1 != 0));
            self.clock.set_high();
            self.clock.set_low();
        }
    }
}
