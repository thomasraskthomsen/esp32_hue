//! The BOOT button as a light switch.
//!
//! A click toggles between off and full, once it is clear it isn't the first
//! half of a double-click, which toggles automation instead. Holding the button for [`HOLD`]
//! starts dimming the light for as long as it stays held, so a setting in the
//! middle can be picked. The direction alternates from one hold to the next;
//! from off, a hold always brightens, and from full it always dims.
//!
//! The level (0 is off, 1..=[`FULL`] is Hue's brightness scale) is published
//! on [`LIGHT`], the channels of the session that sends it to the light.

use core::sync::atomic::AtomicBool;

use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Input;
use hue_ble::protocol::{MAX_BRIGHTNESS, MIN_BRIGHTNESS};

/// How long the button must be held to count as a hold rather than a click.
pub const HOLD: Duration = Duration::from_millis(500);
/// Brightest level.
pub const FULL: u8 = MAX_BRIGHTNESS;
/// Dimmest level a hold goes down to. Holding never turns the light off.
const DIMMEST: u8 = MIN_BRIGHTNESS;
/// Time between steps while dimming, and the size of a step: about four
/// seconds from dimmest to full.
const RAMP_PERIOD: Duration = Duration::from_millis(30);
const RAMP_STEP: u8 = 2;
/// How often the button is sampled.
const POLL: Duration = Duration::from_millis(10);

/// The session with the light: the dimmer signals levels on it, and the BLE
/// task sends them to the light.
pub static LIGHT: hue_ble::Session = hue_ble::Session::new();

/// Whether automation is enabled. Shown as a badge on the display; set from
/// the stored setting at boot, then by `/automate` and a double-click.
pub static AUTOMATE: AtomicBool = AtomicBool::new(false);

/// Set to have the main loop stop the session with the lights and start it
/// again, for a change that is only read when a light connects.
pub static RECONNECT: AtomicBool = AtomicBool::new(false);

/// How long after a click a second press still makes a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(300);

/// How a press of the button ended.
#[derive(PartialEq)]
pub enum Press {
    Click,
    Hold,
}

/// Waits up to [`DOUBLE_CLICK`] for the button to be pressed again, and if it
/// is, until it is released.
pub async fn second_press(button: &Input<'_>) -> bool {
    let start = Instant::now();
    while start.elapsed() < DOUBLE_CLICK {
        if button.is_low() {
            while button.is_low() {
                Timer::after(POLL).await;
            }
            return true;
        }
        Timer::after(POLL).await;
    }
    false
}

pub struct Dimmer {
    level: u8,
    /// Whether the last hold brightened.
    rose: bool,
}

impl Dimmer {
    /// A dimmer that starts with the light off.
    pub const fn new() -> Self {
        Self { level: 0, rose: false }
    }

    pub fn level(&self) -> u8 {
        self.level
    }

    /// Takes the level the light turned out to have, without sending it
    /// back to the light.
    pub fn set_initial(&mut self, level: u8) {
        self.level = level;
        self.rose = false;
    }

    /// Sets the level, like a click does. `changed` is called with the new
    /// level, if it differs from the current one.
    pub fn set_level(&mut self, level: u8, mut changed: impl FnMut(u8)) {
        self.set(level, &mut changed);
    }

    /// Toggles between off and full. `changed` is called with the new level.
    pub fn toggle(&mut self, mut changed: impl FnMut(u8)) {
        self.set(if self.level == 0 { FULL } else { 0 }, &mut changed);
    }

    /// Runs one press of the button, from the moment it goes down until it
    /// is released. A click is left to the caller, which may find it is the
    /// first half of a double-click; a hold is carried out here, calling
    /// `changed` with each new level.
    pub async fn press(&mut self, button: &Input<'_>, mut changed: impl FnMut(u8)) -> Press {
        let pressed = Instant::now();
        while button.is_low() && pressed.elapsed() < HOLD {
            Timer::after(POLL).await;
        }
        if button.is_high() {
            return Press::Click;
        }

        // Brighten from off, dim from full, otherwise go the other way from
        // last time.
        let rising = match self.level {
            0 => true,
            FULL => false,
            _ => !self.rose,
        };
        self.rose = rising;
        while button.is_low() {
            let next = if rising {
                self.level.saturating_add(RAMP_STEP).min(FULL)
            } else {
                self.level.saturating_sub(RAMP_STEP).max(DIMMEST)
            };
            self.set(next, &mut changed);
            Timer::after(RAMP_PERIOD).await;
        }
        Press::Hold
    }

    fn set(&mut self, level: u8, changed: &mut impl FnMut(u8)) {
        if level != self.level {
            self.level = level;
            LIGHT.level.sender().send(level);
            changed(level);
        }
    }
}

impl Default for Dimmer {
    fn default() -> Self {
        Self::new()
    }
}
