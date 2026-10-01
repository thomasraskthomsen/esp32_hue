use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer, with_timeout};
use log::{info, warn};
use trouble_host::prelude::{
    Address, Characteristic, ConnectConfig, Connection, ConnectionEvent, DefaultPacketPool, GattClient, ScanConfig,
    Uuid,
};

use crate::protocol::{
    brightness_byte, brightness_characteristic, light_service, power_characteristic,
};
use crate::{ATTEMPTS, Addr, Bond, CONNECT_TIME, HueController, SECURITY_TIME, Stack, fail};

/// Connects to `address`, giving up after `CONNECT_TIME`.
pub(crate) async fn connect<'a, C: HueController>(
    stack: &'a Stack<'_, C>,
    address: Address,
) -> Result<Connection<'a, DefaultPacketPool>, ()> {
    let mut central = stack.central();
    info!("BLE: connecting to {}", Addr(address.addr.raw()));
    let config = ConnectConfig {
        scan_config: ScanConfig {
            filter_accept_list: &[address],
            ..Default::default()
        },
        connect_params: Default::default(),
    };
    with_timeout(CONNECT_TIME, central.connect(&config))
        .await
        .map_err(fail("connect"))?
        .map_err(fail("connect"))
}

/// Pairs with the light at `address`, which must be in pairing mode (see
/// [`crate::protocol::pairing_open`]), and returns what to keep of it. The
/// pairing is "Just Works": there's no way to show or enter a passkey, so the
/// stack should be built with `IoCapabilities::NoInputNoOutput`.
pub async fn pair<C: HueController>(stack: &Stack<'_, C>, address: Address) -> Result<Bond, ()> {
    let connection = connect(stack, address).await?;
    let result = pair_on(&connection).await;
    connection.disconnect();
    result
}

async fn pair_on(connection: &Connection<'_, DefaultPacketPool>) -> Result<Bond, ()> {
    connection.set_bondable(true).map_err(fail("set bondable"))?;
    connection.request_security().map_err(fail("request security"))?;
    info!("BLE: pairing; the light must be in pairing mode");

    let outcome = with_timeout(SECURITY_TIME, async {
        loop {
            match connection.next().await {
                ConnectionEvent::PassKeyConfirm(key) => {
                    info!("BLE: confirming passkey {key:?}");
                    connection.pass_key_confirm().map_err(fail("confirm passkey"))?;
                }
                ConnectionEvent::PassKeyDisplay(key) => info!("BLE: passkey {key:?}"),
                ConnectionEvent::PassKeyInput => {
                    warn!("BLE: the light wants a passkey typed in; not supported");
                    return Err(());
                }
                ConnectionEvent::PairingComplete { security_level, bond } => {
                    info!("BLE: paired, security {security_level:?}");
                    return Ok(bond);
                }
                ConnectionEvent::PairingFailed(err) => {
                    warn!("BLE: pairing failed: {err:?}");
                    return Err(());
                }
                ConnectionEvent::Disconnected { reason } => {
                    warn!("BLE: disconnected while pairing: {reason:?}");
                    return Err(());
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(fail("pairing timed out"))??;

    let Some(bond) = outcome else {
        warn!("BLE: the light did not bond; nothing to keep");
        return Err(());
    };
    // `PairingComplete` arrives the moment the stack queues the dongle's own
    // identity keys for the light. Disconnecting at once can cut them off,
    // and the light then drops the bond. Give them time to go out.
    Timer::after(Duration::from_secs(2)).await;
    Ok(Bond::from_information(&bond))
}

/// Connects to a paired light and encrypts the link with the stored key.
pub(crate) async fn open<'a, C: HueController>(
    stack: &'a Stack<'_, C>,
    address: Address,
) -> Result<Connection<'a, DefaultPacketPool>, ()> {
    let connection = connect(stack, address).await?;
    connection.request_security().map_err(fail("request security"))?;
    with_timeout(SECURITY_TIME, async {
        loop {
            match connection.next().await {
                ConnectionEvent::Encrypted { .. } | ConnectionEvent::PairingComplete { .. } => return Ok(()),
                ConnectionEvent::PairingFailed(err) => {
                    warn!("BLE: encrypting failed: {err:?}");
                    return Err(());
                }
                ConnectionEvent::Disconnected { reason } => {
                    warn!("BLE: disconnected: {reason:?}");
                    return Err(());
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(fail("encrypting timed out"))??;
    Ok(connection)
}

/// Switches a paired light on or off, trying again if that fails.
pub async fn set_power<C: HueController>(stack: &Stack<'_, C>, address: Address, on: bool) -> Result<(), ()> {
    write(stack, address, power_characteristic(), u8::from(on)).await
}

/// Sets the brightness of a paired light (clamped to what lights take),
/// trying again if that fails. A light that is off stays off.
pub async fn set_brightness<C: HueController>(stack: &Stack<'_, C>, address: Address, level: u8) -> Result<(), ()> {
    write(stack, address, brightness_characteristic(), brightness_byte(level)).await
}

async fn write<C: HueController>(
    stack: &Stack<'_, C>,
    address: Address,
    characteristic: Uuid,
    value: u8,
) -> Result<(), ()> {
    for attempt in 1..=ATTEMPTS {
        if write_once(stack, address, &characteristic, value).await.is_ok() {
            return Ok(());
        }
        if attempt < ATTEMPTS {
            info!("BLE: trying again ({attempt}/{ATTEMPTS} failed)");
            Timer::after(Duration::from_secs(1)).await;
        }
    }
    warn!("BLE: giving up after {ATTEMPTS} attempts");
    Err(())
}

async fn write_once<C: HueController>(
    stack: &Stack<'_, C>,
    address: Address,
    characteristic: &Uuid,
    value: u8,
) -> Result<(), ()> {
    let connection = open(stack, address).await?;
    let result = write_on(stack, &connection, characteristic, value).await;
    connection.disconnect();
    result
}

/// Writes one byte to a characteristic of the light control service over an
/// encrypted connection.
async fn write_on<C: HueController>(
    stack: &Stack<'_, C>,
    connection: &Connection<'_, DefaultPacketPool>,
    characteristic: &Uuid,
    value: u8,
) -> Result<(), ()> {
    let client = GattClient::<C, DefaultPacketPool, 10>::new(stack, connection)
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
        let characteristic: Characteristic<u8> = client
            .characteristic_by_uuid(&service, characteristic)
            .await
            .map_err(fail("find characteristic"))?;
        client
            .write_characteristic(&characteristic, &[value])
            .await
            .map_err(fail("write"))?;
        info!("BLE: wrote {value}");
        Ok(())
    };
    match select(client.task(), work).await {
        Either::First(result) => {
            warn!("BLE: GATT client stopped: {result:?}");
            Err(())
        }
        Either::Second(result) => result,
    }
}
