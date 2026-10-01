//! BLE on the dongle: starts the BLE host on the ESP32's radio, and carries
//! out the commands from the serial console and the wake-up sessions using
//! `hue-ble`. Results are logged. The dongle is the BLE central and the
//! lights are peripherals.

use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use esp_radio::ble::controller::BleConnector;
use heapless::Vec;
use hue_ble::{Addr, Address, Finder, MAX_LIGHTS};
use log::{info, warn};
use static_cell::StaticCell;
use trouble_host::prelude::{DefaultPacketPool, ExternalController, HostResources, IoCapabilities};

use crate::light::LIGHT;
use crate::settings;

type Ctrl = ExternalController<BleConnector<'static>, 20>;
type Stack<'a> = hue_ble::Stack<'a, Ctrl>;

/// One connection per light in a session.
const CONNECTIONS: usize = MAX_LIGHTS;
const L2CAP_CHANNELS: usize = 2;
const AUTOPAIR_TIME: Duration = Duration::from_secs(60);
const AUTOPAIR_SCAN_TIME: Duration = Duration::from_secs(4);
/// Pause between two pairing attempts in `/autopair`, so a light that
/// refuses isn't hammered.
const AUTOPAIR_PAUSE: Duration = Duration::from_secs(3);
/// What the dongle says it can do in the Pairing Request. `NoInputNoOutput`
/// gives "Just Works" without MITM, the same as the Windows pairing that the
/// lights accepted. `KeyboardDisplay` (as the Python HueBLE library) asks
/// for MITM; `hue_ble::pair` confirms the passkey by itself in that case.
const IO_CAPABILITIES: IoCapabilities = IoCapabilities::NoInputNoOutput;

/// What the serial console and the user interface can ask of the BLE task.
pub enum Command {
    /// Looks for lights for this many seconds, listing them as they appear.
    Scan(u32),
    /// Scans for this many seconds, logging each change in what a light
    /// advertises as it happens.
    Watch(u32),
    /// Pairs with light number `n` from the last scan.
    Pair(usize),
    /// Stays connected to the paired light, sending it the levels signalled
    /// on `LIGHT`, until `LIGHT.stop`.
    Session,
    /// Pairs with every light as soon as it accepts pairing; also the ones
    /// already paired, if `all`.
    AutoPair { all: bool },
    /// Lists the paired lights.
    Lights,
    /// Forgets paired light number `n` (from `Lights`), or all if `None`.
    Forget(Option<usize>),
    /// Switches paired light number `light` (as listed by `Lights`).
    Power { light: usize, on: bool },
    /// Brightness, 1 to 254, of paired light number `light`.
    Brightness { light: usize, level: u8 },
}

pub static COMMANDS: Channel<CriticalSectionRawMutex, Command, 4> = Channel::new();

/// The lights heard while scanning.
static FINDER: Finder = Finder::new();

/// Runs the BLE host and carries out commands from `COMMANDS`.
pub async fn run(connector: BleConnector<'static>, store: &'static settings::Shared) {
    static RESOURCES: StaticCell<HostResources<Ctrl, DefaultPacketPool, CONNECTIONS, L2CAP_CHANNELS>> =
        StaticCell::new();
    let resources = RESOURCES.init(HostResources::new());
    // The dongle uses the chip's public address (logged by the host as
    // "Device Address"); the lights remember it with the bond.
    let stack = trouble_host::new(ExternalController::<_, 20>::new(connector), resources)
        .set_io_capabilities(IO_CAPABILITIES)
        .build();

    let bonds = store.lock().await.load_bonds();
    hue_ble::add_bonds(&stack, &bonds);
    info!("BLE: {} paired light(s); commands: scan, watch, pair, lights, forget, power, bright", bonds.len());
    info!("BLE: IO capability {IO_CAPABILITIES:?}; the pairing request is logged when pairing");

    let mut runner = stack.runner();
    let commands = async {
        loop {
            let _ = match COMMANDS.receive().await {
                Command::Scan(secs) => scan(&stack, secs).await,
                Command::Watch(secs) => watch(&stack, secs).await,
                Command::Pair(n) => pair(&stack, store, n).await,
                Command::Session => session(&stack, store).await,
                Command::AutoPair { all } => autopair(&stack, store, all).await,
                Command::Lights => lights(store).await,
                Command::Forget(n) => forget(&stack, store, n).await,
                Command::Power { light, on } => match light_address(store, light).await {
                    Ok(address) => hue_ble::set_power(&stack, address, on).await,
                    Err(()) => Err(()),
                },
                Command::Brightness { light, level } => match light_address(store, light).await {
                    Ok(address) => hue_ble::set_brightness(&stack, address, level).await,
                    Err(()) => Err(()),
                },
            };
        }
    };
    match select(runner.run_with_handler(&FINDER), commands).await {
        Either::First(result) => warn!("BLE: host stopped: {result:?}"),
        Either::Second(()) => {}
    }
}

/// Scans for `secs` seconds, logging each light with its number as soon as
/// it is heard.
async fn scan(stack: &Stack<'_>, secs: u32) -> Result<(), ()> {
    info!("BLE: scanning for {secs} s");
    let mut seen = 0;
    hue_ble::watch(stack, &FINDER, Duration::from_secs(u64::from(secs)), |at, light| {
        // `watch` also reports changed data; only new lights get a line.
        let n = FINDER.lights().iter().position(|l| l.address == light.address).unwrap_or(0);
        if n >= seen {
            seen = n + 1;
            info!(
                "BLE: {n}: {} rssi {} data {:02x?} ({:.1} s)",
                Addr(light.address.addr.raw()),
                light.rssi,
                light.data.as_slice(),
                at.as_millis() as f32 / 1000.0
            );
        }
    })
    .await?;
    if seen == 0 {
        info!("BLE: no lights found");
    }
    Ok(())
}

/// Scans for `secs` seconds and logs every change in a light's service
/// data as it happens, with the time since the start, to see whether (and
/// when) putting the lights in pairing mode changes what they advertise.
async fn watch(stack: &Stack<'_>, secs: u32) -> Result<(), ()> {
    info!("BLE: watching advertisements for {secs} s; a line per change");
    hue_ble::watch(stack, &FINDER, Duration::from_secs(u64::from(secs)), |at, light| {
        info!(
            "BLE: {:6.1} s {} rssi {} data {:02x?}",
            at.as_millis() as f32 / 1000.0,
            Addr(light.address.addr.raw()),
            light.rssi,
            light.data.as_slice()
        );
    })
    .await?;
    info!("BLE: watch over");
    Ok(())
}

async fn pair(stack: &Stack<'_>, store: &settings::Shared, n: usize) -> Result<(), ()> {
    let Some(light) = FINDER.get(n) else {
        warn!("BLE: no light {n}; /scan first");
        return Err(());
    };
    pair_and_store(stack, store, light.address).await
}

/// Pairs with the light at `address` and stores the bond.
async fn pair_and_store(stack: &Stack<'_>, store: &settings::Shared, address: Address) -> Result<(), ()> {
    let bond = hue_ble::pair(stack, address).await?;
    let mut store = store.lock().await;
    let mut bonds = store.load_bonds();
    bonds.retain(|b| b.address != bond.address);
    if bonds.push(bond).is_err() {
        warn!("BLE: too many paired lights; /forget <n> one first");
        return Err(());
    }
    if let Err(err) = store.save_bonds(&bonds) {
        warn!("BLE: saving the bond failed: {err:?}");
        return Err(());
    }
    info!("BLE: bond stored; {} paired light(s)", bonds.len());
    Ok(())
}

/// Scans for up to `AUTOPAIR_TIME` and pairs with each light not paired yet
/// as soon as it advertises that it accepts pairing (see
/// `hue_ble::protocol::pairing_open`), which the Hue app's "Make
/// Discoverable" brings about. Lights that don't say so are left alone:
/// they refuse anyway. Stops once every light seen is paired.
///
/// With `all`, lights already paired are paired anew too (their stored
/// bond is replaced).
async fn autopair(stack: &Stack<'_>, store: &settings::Shared, all: bool) -> Result<(), ()> {
    info!(
        "BLE: auto-pairing for up to {} s; press Make Discoverable in the Hue app",
        AUTOPAIR_TIME.as_secs()
    );
    let deadline = Instant::now() + AUTOPAIR_TIME;
    let mut newly_paired = 0;
    while Instant::now() < deadline {
        hue_ble::scan(stack, &FINDER, AUTOPAIR_SCAN_TIME).await?;
        let lights = FINDER.lights();
        let mut waiting = 0;
        for light in &lights {
            if !all && is_paired(store, &light.address).await {
                continue;
            }
            if !hue_ble::protocol::pairing_open(&light.data) {
                waiting += 1;
                continue;
            }
            if all {
                // A new pairing, not an encryption with the old key.
                hue_ble::remove_bonds(stack, &store.lock().await.load_bonds());
            }
            if pair_and_store(stack, store, light.address).await.is_ok() {
                newly_paired += 1;
            } else {
                Timer::after(AUTOPAIR_PAUSE).await;
            }
        }
        if waiting == 0 && (newly_paired > 0 || !lights.is_empty()) {
            break;
        }
    }
    if newly_paired == 0 {
        warn!("BLE: no light accepted pairing");
        return Err(());
    }
    info!("BLE: paired {newly_paired} new light(s)");
    Ok(())
}

/// Whether the light at `address` has a stored bond.
async fn is_paired(store: &settings::Shared, address: &Address) -> bool {
    store.lock().await.load_bonds().iter().any(|b| b.address == address.addr.raw())
}

async fn lights(store: &settings::Shared) -> Result<(), ()> {
    let bonds = store.lock().await.load_bonds();
    if bonds.is_empty() {
        info!("BLE: no paired lights");
    }
    for (n, bond) in bonds.iter().enumerate() {
        info!("BLE: {n}: paired {}", Addr(&bond.address));
    }
    Ok(())
}

/// Forgets paired light `n`, or all of them (the lights still remember us).
async fn forget(stack: &Stack<'_>, store: &settings::Shared, n: Option<usize>) -> Result<(), ()> {
    let mut store = store.lock().await;
    let mut bonds = store.load_bonds();
    let forgotten: Vec<_, { settings::MAX_BONDS }> = match n {
        Some(n) if n < bonds.len() => Vec::from_slice(&[bonds.remove(n)]).unwrap_or_default(),
        Some(n) => {
            warn!("BLE: no paired light {n}; see /lights");
            return Err(());
        }
        None => core::mem::take(&mut bonds),
    };
    hue_ble::remove_bonds(stack, &forgotten);
    if let Err(err) = store.save_bonds(&bonds) {
        warn!("BLE: saving the bonds failed: {err:?}");
        return Err(());
    }
    for bond in &forgotten {
        info!("BLE: forgot {} (the light still remembers us)", Addr(&bond.address));
    }
    info!("BLE: {} paired light(s) left", bonds.len());
    Ok(())
}

/// The address of paired light number `n`, as listed by `/lights`.
async fn light_address(store: &settings::Shared, n: usize) -> Result<Address, ()> {
    let Some(bond) = store.lock().await.load_bonds().get(n).cloned() else {
        warn!("BLE: no paired light {n}; see /lights");
        return Err(());
    };
    Ok(bond.light_address())
}

/// Keeps connections to the paired lights open, for the user interface (see
/// `hue_ble::run_session`).
async fn session(stack: &Stack<'_>, store: &settings::Shared) -> Result<(), ()> {
    let addresses: Vec<Address, MAX_LIGHTS> = store
        .lock()
        .await
        .load_bonds()
        .iter()
        .take(MAX_LIGHTS)
        .map(|bond| bond.light_address())
        .collect();
    if addresses.is_empty() {
        warn!("BLE: no paired light; /scan and /pair first");
        // Let the interface know there's nothing to connect to.
        LIGHT.link.signal(hue_ble::Link::Disconnected);
        return Err(());
    }
    hue_ble::run_session(stack, &addresses, &LIGHT).await
}
