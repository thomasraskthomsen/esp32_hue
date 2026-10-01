use core::cell::RefCell;

use bt_hci::param::LeAdvReportsIter;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant, Timer};
use heapless::Vec;
use trouble_host::prelude::{Address, EventHandler, ScanConfig, Scanner};

use crate::protocol::hue_service_data;
use crate::{HueController, Stack, fail};

/// Most lights remembered from a scan.
pub const MAX_FOUND: usize = 8;
/// Bytes of a light's service data kept.
const DATA_LEN: usize = 12;

/// A light heard while scanning.
#[derive(Clone)]
pub struct Found {
    pub address: Address,
    pub rssi: i8,
    /// The light's service data; it ends with part of its Zigbee address.
    pub data: Vec<u8, DATA_LEN>,
}

/// Collects the lights heard while scanning. Pass it to
/// `Runner::run_with_handler`, and scan with [`scan`].
pub struct Finder {
    found: BlockingMutex<CriticalSectionRawMutex, RefCell<Vec<Found, MAX_FOUND>>>,
}

impl Finder {
    pub const fn new() -> Self {
        Self {
            found: BlockingMutex::new(RefCell::new(Vec::new())),
        }
    }

    pub fn clear(&self) {
        self.found.lock(|found| found.borrow_mut().clear());
    }

    /// The lights found so far, in the order they were first heard.
    pub fn lights(&self) -> Vec<Found, MAX_FOUND> {
        self.found.lock(|found| found.borrow().clone())
    }

    /// Light number `n` of those found.
    pub fn get(&self, n: usize) -> Option<Found> {
        self.found.lock(|found| found.borrow().get(n).cloned())
    }
}

impl Default for Finder {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHandler for Finder {
    fn on_adv_reports(&self, reports: LeAdvReportsIter) {
        for report in reports.flatten() {
            let Some(data) = hue_service_data(report.data) else {
                continue;
            };
            let data: Vec<u8, DATA_LEN> = Vec::from_slice(data.get(..DATA_LEN).unwrap_or(data)).unwrap_or_default();
            let address = Address {
                kind: report.addr_kind,
                addr: report.addr,
            };
            self.found.lock(|found| {
                let mut found = found.borrow_mut();
                if let Some(known) = found.iter_mut().find(|f| f.address == address) {
                    known.rssi = report.rssi;
                    if !data.is_empty() {
                        known.data = data;
                    }
                } else {
                    let _ = found.push(Found {
                        address,
                        rssi: report.rssi,
                        data,
                    });
                }
            });
        }
    }
}

/// Clears the finder, then scans for `time`, leaving the lights heard in the
/// finder. The stack's runner must be running with the finder as its event
/// handler.
pub async fn scan<C: HueController>(stack: &Stack<'_, C>, finder: &Finder, time: Duration) -> Result<(), ()> {
    watch(stack, finder, time, |_, _| {}).await
}

/// Like [`scan`], but calls `changed` with the elapsed time and the light
/// each time a light is first heard or its service data changes, so what a
/// light advertises can be watched over time (for example while it is put
/// in pairing mode). The finder is checked every 100 ms.
pub async fn watch<C: HueController>(
    stack: &Stack<'_, C>,
    finder: &Finder,
    time: Duration,
    mut changed: impl FnMut(Duration, &Found),
) -> Result<(), ()> {
    finder.clear();
    let mut scanner = Scanner::new(stack.central());
    let config = ScanConfig {
        active: true,
        ..Default::default()
    };
    let session = scanner.scan(&config).await.map_err(fail("scan"))?;
    let start = Instant::now();
    let mut seen: Vec<Found, MAX_FOUND> = Vec::new();
    while start.elapsed() < time {
        Timer::after(Duration::from_millis(100)).await;
        for light in finder.lights().iter() {
            match seen.iter_mut().find(|s| s.address == light.address) {
                Some(known) if known.data == light.data => {}
                Some(known) => {
                    known.data = light.data.clone();
                    changed(start.elapsed(), light);
                }
                None => {
                    let _ = seen.push(light.clone());
                    changed(start.elapsed(), light);
                }
            }
        }
    }
    drop(session);
    Ok(())
}
