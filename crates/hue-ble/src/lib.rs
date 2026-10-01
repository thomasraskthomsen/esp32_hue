//! Control of Philips Hue Bluetooth lights from a BLE central, on
//! [`trouble-host`](trouble_host).
//!
//! This is unofficial: the protocol was worked out by others looking at the
//! lights, and may change with a firmware update.
//!
//! Lights advertise the Philips service `0xFE0F` and expose their controls in
//! a vendor service. They need a bonded (paired) link before those controls
//! can be used, and a light only accepts a new pairing while it is in pairing
//! mode (in the Hue app: Settings, Voice Assistants, Amazon Alexa or Google
//! Home, Make Discoverable).
//!
//! The caller builds and runs the `trouble-host` stack, passing the
//! [`Finder`] as the event handler to `Runner::run_with_handler` so that
//! scanning finds lights. Then:
//!
//! - [`scan`] fills the [`Finder`] with the lights in range;
//! - [`pair`] pairs with one and returns its [`Bond`], for the caller to
//!   store; [`add_bonds`] gives the stored ones back to the stack at startup;
//! - [`set_power`] and [`set_brightness`] do one-off changes;
//! - [`run_session`] keeps a connection open, for changing a light in real
//!   time, reporting through a [`Session`].

#![no_std]
// Failures are logged where they happen; a real error type can come later.
#![allow(clippy::result_unit_err)]

mod bond;
mod client;
mod finder;
pub mod protocol;
mod session;

use core::fmt;

use bt_hci::cmd::le::{
    LeAddDeviceToFilterAcceptList, LeClearFilterAcceptList, LeCreateConn, LeSetScanEnable, LeSetScanParams,
};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_time::Duration;
use trouble_host::prelude::{Controller, DefaultPacketPool};

pub use bond::{Bond, add_bonds, remove_bonds};
pub use client::{pair, set_brightness, set_power};
pub use finder::{Finder, Found, MAX_FOUND, scan, watch};
pub use session::{Link, MAX_LIGHTS, Session, run_session};
pub use trouble_host::prelude::Address;

/// A `trouble-host` stack for lights, on any controller that can scan and
/// connect.
pub type Stack<'a, C> = trouble_host::Stack<'a, C, DefaultPacketPool>;

/// What the controller needs to be able to do: scan, connect, and handle the
/// security the lights require. Implemented for every controller that can.
pub trait HueController:
    Controller
    + ControllerCmdSync<LeSetScanParams>
    + ControllerCmdSync<LeSetScanEnable>
    + ControllerCmdSync<LeClearFilterAcceptList>
    + ControllerCmdSync<LeAddDeviceToFilterAcceptList>
    + ControllerCmdAsync<LeCreateConn>
{
}

impl<C> HueController for C where
    C: Controller
        + ControllerCmdSync<LeSetScanParams>
        + ControllerCmdSync<LeSetScanEnable>
        + ControllerCmdSync<LeClearFilterAcceptList>
        + ControllerCmdSync<LeAddDeviceToFilterAcceptList>
        + ControllerCmdAsync<LeCreateConn>
{
}

/// How long to try connecting to a light at a time.
pub(crate) const CONNECT_TIME: Duration = Duration::from_secs(20);
/// How long pairing, or encrypting with a stored key, may take.
pub(crate) const SECURITY_TIME: Duration = Duration::from_secs(30);
/// Connection attempts before giving up on a light. They take a while to
/// wake up, and often drop the first connection.
pub const ATTEMPTS: usize = 2;

/// A Bluetooth address as usually written, most significant byte first.
pub struct Addr<'a>(pub &'a [u8]);

impl fmt::Display for Addr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for (i, byte) in self.0.iter().rev().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

/// Logs a failure, to be returned as `Err(())`.
pub(crate) fn fail<E: fmt::Debug>(what: &'static str) -> impl FnOnce(E) {
    move |err| log::warn!("BLE: {what} failed: {err:?}")
}
