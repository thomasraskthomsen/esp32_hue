use core::cell::Cell;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::join::join_array;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use embassy_time::{Duration, Timer};
use log::{info, warn};
use trouble_host::prelude::{
    Address, Characteristic, CharacteristicProp, Connection, DefaultPacketPool, GattClient, NotificationListener,
};

use crate::client::open;
use crate::protocol::{
    MAX_BRIGHTNESS, MIN_BRIGHTNESS, brightness_byte, brightness_characteristic, control_characteristic, control_level,
    light_service, power_characteristic,
};
use crate::{ATTEMPTS, Addr, HueController, Stack, fail};

/// Most lights a session controls at once, one BLE connection each. The
/// controller and the stack must allow that many connections.
pub const MAX_LIGHTS: usize = 3;

/// How a session goes.
#[derive(Clone, Copy)]
pub enum Link {
    /// Looking for the lights and connecting to them.
    Connecting,
    /// The first light is connected, and is at this level (0 is off, 1 to
    /// 254 is on at that brightness). The others follow as they connect.
    Connected(u8),
    /// Not connected: the session ended, or never got going.
    Disconnected,
}

/// The channels of a session, which the caller keeps (usually in a static):
/// it sends a new `level` for the lights, and `stop` to end the session,
/// and follows `link`.
pub struct Session {
    /// The level the lights should have, 0 for off. Every light follows it,
    /// and only the newest matters, so a slow light just skips the levels in
    /// between.
    pub level: Watch<CriticalSectionRawMutex, u8, MAX_LIGHTS>,
    pub stop: Signal<CriticalSectionRawMutex, ()>,
    pub link: Signal<CriticalSectionRawMutex, Link>,
    /// The level of the first light (the first of the addresses given to
    /// `run_session`) whenever it changes, from anywhere (the Hue app, say) and
    /// also from this session's own `level`. The other lights aren't followed.
    pub observed: Signal<CriticalSectionRawMutex, u8>,
    /// Whether to log everything a light offers and sends. Read when a light
    /// connects, so it takes effect the next time it does.
    pub log_all: AtomicBool,
}

impl Session {
    pub const fn new() -> Self {
        Self {
            level: Watch::new(),
            stop: Signal::new(),
            link: Signal::new(),
            observed: Signal::new(),
            log_all: AtomicBool::new(false),
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// How long to wait before connecting to a light again after it dropped or
/// couldn't be reached.
const RECONNECT_PAUSE: Duration = Duration::from_secs(5);

/// Disconnects when dropped, however the session ends.
struct Connected<'a>(Connection<'a, DefaultPacketPool>);

impl Drop for Connected<'_> {
    fn drop(&mut self) {
        self.0.disconnect();
    }
}

/// Keeps connections to the paired lights at `addresses` (the first
/// `MAX_LIGHTS` of them) open, sending them each level sent on
/// `session.level`, until `session.stop` is signalled. A light that drops or
/// can't be reached is tried again every few seconds. `session.link` tells how
/// it goes: it reports the first light once it is connected (only the first
/// time), and the rest join in as they connect.
pub async fn run_session<C: HueController>(
    stack: &Stack<'_, C>,
    addresses: &[Address],
    session: &Session,
) -> Result<(), ()> {
    session.stop.reset();
    session.level.sender().clear();
    session.link.signal(Link::Connecting);

    // Connecting one light at a time keeps each attempt's time limit from
    // running while it waits for another light's attempt.
    let connecting = Mutex::<NoopRawMutex, ()>::new(());
    let first = Cell::new(true);
    // The first light is the one followed, see `Session::observed`.
    let light = |n: usize| keep_light(stack, addresses.get(n).copied(), session, &connecting, &first, n == 0);
    let lights = join_array([light(0), light(1), light(2)]);

    let result = match select(session.stop.wait(), lights).await {
        Either::First(()) => Ok(()),
        Either::Second(results) if results.iter().any(Result::is_ok) => Ok(()),
        Either::Second(_) => Err(()),
    };
    session.link.signal(Link::Disconnected);
    result
}

/// Keeps one light of the session connected: when it can't be reached or the
/// link drops, tries again after a pause, until the session is stopped. Fails
/// at once if there's no light `address`.
async fn keep_light<C: HueController>(
    stack: &Stack<'_, C>,
    address: Option<Address>,
    session: &Session,
    connecting: &Mutex<NoopRawMutex, ()>,
    first: &Cell<bool>,
    follow: bool,
) -> Result<(), ()> {
    let address = address.ok_or(())?;
    loop {
        let _ = light(stack, Some(address), session, connecting, first, follow).await;
        info!("BLE: connecting to {} again in {} s", Addr(address.addr.raw()), RECONNECT_PAUSE.as_secs());
        Timer::after(RECONNECT_PAUSE).await;
    }
}

/// Logs every service and characteristic the light has, with its properties
/// and (if readable) its value, and subscribes to every characteristic that
/// notifies or indicates. Returns a listener for all that arrives.
async fn log_everything<'a, C: HueController>(
    client: &'a GattClient<'_, C, DefaultPacketPool, 10>,
    address: Address,
) -> Option<NotificationListener<'a, 512>> {
    let name = Addr(address.addr.raw());
    let services = client.services().await.map_err(fail("list services")).ok()?;
    for service in &services {
        info!("BLE: {name}: service {:?}", service.uuid());
        let Ok(characteristics) = client
            .characteristics::<16>(service)
            .await
            .map_err(fail("list characteristics"))
        else {
            continue;
        };
        for characteristic in &characteristics {
            info!(
                "BLE: {name}:   characteristic {:?} handle {} properties {:?}",
                characteristic.uuid, characteristic.handle, characteristic.props
            );
            if characteristic.props.any(&[CharacteristicProp::Read]) {
                let mut value = [0u8; 32];
                match client.read_characteristic(characteristic, &mut value).await {
                    Ok(len) => info!("BLE: {name}:     value {:02x?}", &value[..len]),
                    Err(err) => warn!("BLE: {name}:     read failed: {err:?}"),
                }
            }
            if characteristic.props.any(&[CharacteristicProp::Notify, CharacteristicProp::Indicate]) {
                let indicate = !characteristic.props.any(&[CharacteristicProp::Notify]);
                match client.subscribe(characteristic, indicate).await {
                    Ok(_) => info!("BLE: {name}:     subscribed"),
                    Err(err) => warn!("BLE: {name}:     subscribing failed: {err:?}"),
                }
            }
        }
    }
    client.listen_all().map_err(fail("listen")).ok()
}

/// One connection to a light of the session, until it ends. Fails at once if
/// there's no light `address`.
async fn light<C: HueController>(
    stack: &Stack<'_, C>,
    address: Option<Address>,
    session: &Session,
    connecting: &Mutex<NoopRawMutex, ()>,
    first: &Cell<bool>,
    follow: bool,
) -> Result<(), ()> {
    let address = address.ok_or(())?;
    let mut level = session.level.receiver().ok_or(())?;

    let connection = 'connected: {
        for attempt in 1..=ATTEMPTS {
            let opened = {
                let _turn = connecting.lock().await;
                open(stack, address).await
            };
            match opened {
                Ok(connection) => break 'connected Connected(connection),
                Err(()) if attempt < ATTEMPTS => {
                    info!("BLE: trying again ({attempt}/{ATTEMPTS} failed)");
                    Timer::after(Duration::from_secs(1)).await;
                }
                Err(()) => {}
            }
        }
        warn!("BLE: giving up on {} after {ATTEMPTS} attempts", Addr(address.addr.raw()));
        return Err(());
    };

    let client = GattClient::<C, DefaultPacketPool, 10>::new(stack, &connection.0)
        .await
        .map_err(fail("GATT client"))?;
    let work = async {
        let service = client
            .services_by_uuid(&light_service())
            .await
            .map_err(fail("find service"))?
            .first()
            .cloned()
            .ok_or_else(|| warn!("BLE: the light has no control service"))?;
        let power: Characteristic<u8> = client
            .characteristic_by_uuid(&service, &power_characteristic())
            .await
            .map_err(fail("find power"))?;
        let brightness: Characteristic<u8> = client
            .characteristic_by_uuid(&service, &brightness_characteristic())
            .await
            .map_err(fail("find brightness"))?;

        // What the light is doing now.
        let mut value = [0u8; 1];
        client.read_characteristic(&power, &mut value).await.map_err(fail("read power"))?;
        let mut on = value[0] != 0;
        client
            .read_characteristic(&brightness, &mut value)
            .await
            .map_err(fail("read brightness"))?;
        let own = if on { value[0].clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS) } else { 0 };
        info!("BLE: {} is at {own}", Addr(address.addr.raw()));

        // The first light sets where the session starts; the others join the
        // level the session has reached since.
        let mut wanted = if first.replace(false) {
            session.link.signal(Link::Connected(own));
            None
        } else {
            level.try_get().filter(|&wanted| wanted != own)
        };

        // The first light is followed: it notifies its state whenever it
        // changes, whoever changed it, and that is passed on as `observed`.
        let control: Option<Characteristic<[u8]>> = if follow {
            client
                .characteristic_by_uuid(&service, &control_characteristic())
                .await
                .map_err(fail("find control"))
                .ok()
        } else {
            None
        };
        // With logging on, everything the light offers is listed, and what it
        // notifies is logged as it arrives (and subscribed to, the control
        // characteristic with the rest).
        let log_all = session.log_all.load(Ordering::Relaxed);
        let mut notifications = if log_all {
            log_everything(&client, address).await
        } else if let Some(control) = &control {
            client.subscribe(control, false).await.map_err(fail("subscribe to control")).ok()
        } else {
            None
        };

        loop {
            if let Some(wanted) = wanted {
                if wanted == 0 {
                    client.write_characteristic(&power, &[0]).await.map_err(fail("write power"))?;
                    on = false;
                } else {
                    // Set the brightness before switching on, so the light
                    // doesn't flash at its old one.
                    client
                        .write_characteristic(&brightness, &[brightness_byte(wanted)])
                        .await
                        .map_err(fail("write brightness"))?;
                    if !on {
                        client.write_characteristic(&power, &[1]).await.map_err(fail("write power"))?;
                        on = true;
                    }
                }
            }
            wanted = match &mut notifications {
                Some(listener) => match select(level.changed(), listener.next()).await {
                    Either::First(level) => Some(level),
                    Either::Second(notification) => {
                        if log_all {
                            info!(
                                "BLE: {}: {} handle {}: {:02x?}",
                                Addr(address.addr.raw()),
                                if notification.is_indication() { "indication" } else { "notification" },
                                notification.handle(),
                                notification.as_ref()
                            );
                        }
                        if control.as_ref().is_some_and(|control| control.handle == notification.handle()) {
                            if let Some(level) = control_level(notification.as_ref()) {
                                session.observed.signal(level);
                                on = level != 0;
                            }
                        }
                        None
                    }
                },
                None => Some(level.changed().await),
            };
        }
    };
    match select(client.task(), work).await {
        Either::First(result) => {
            warn!("BLE: a light disconnected: {result:?}");
            Err(())
        }
        Either::Second(result) => result,
    }
}
