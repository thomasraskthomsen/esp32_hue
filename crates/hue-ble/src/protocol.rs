//! What the lights speak: identifiers, and the bytes of their values.

use trouble_host::prelude::Uuid;

/// Little-endian bytes of the Philips service UUID `0xFE0F`, which Hue lights
/// advertise.
pub const HUE_SERVICE_UUID16: [u8; 2] = [0x0F, 0xFE];

/// Dimmest level that is still on.
pub const MIN_BRIGHTNESS: u8 = 1;
/// Brightest level.
pub const MAX_BRIGHTNESS: u8 = 254;

/// The service holding the light's controls.
pub fn light_service() -> Uuid {
    Uuid::from(0x932c32bd_0000_47a2_835a_a8d455b859dd_u128)
}

/// Power: one byte, 0 off and 1 on.
pub fn power_characteristic() -> Uuid {
    Uuid::from(0x932c32bd_0002_47a2_835a_a8d455b859dd_u128)
}

/// Brightness: one byte, `MIN_BRIGHTNESS` to `MAX_BRIGHTNESS`.
pub fn brightness_characteristic() -> Uuid {
    Uuid::from(0x932c32bd_0003_47a2_835a_a8d455b859dd_u128)
}

/// Control: notifies with the light's state every time it changes, whoever
/// changed it. Seen on `LWO007` lights: a notification is a list of
/// (tag, length, value) items, among them tag 1 for power (`00` or `01`) and
/// tag 2 for brightness, one byte each, like `01 01 01 02 01 36`.
pub fn control_characteristic() -> Uuid {
    Uuid::from(0x932c32bd_0007_47a2_835a_a8d455b859dd_u128)
}

/// The light level (0 for off, else the brightness) in a notification from
/// the control characteristic, if it has the power and brightness items.
pub fn control_level(mut data: &[u8]) -> Option<u8> {
    let (mut power, mut brightness) = (None, None);
    while let [tag, len, rest @ ..] = data {
        let value = rest.get(..usize::from(*len))?;
        match (tag, value) {
            (1, [on]) => power = Some(*on),
            (2, [level]) => brightness = Some(*level),
            _ => {}
        }
        data = &rest[value.len()..];
    }
    match (power, brightness) {
        (Some(0), _) => Some(0),
        (Some(_), Some(level)) => Some(brightness_byte(level)),
        _ => None,
    }
}

/// The brightness byte for `level`, clamped to what lights take.
pub fn brightness_byte(level: u8) -> u8 {
    level.clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS)
}

/// Whether a light's service data (see [`hue_service_data`]) says it
/// accepts pairing right now. Seen on `LWO007` lights: byte 4 is a flag
/// byte whose bit `0x01` the Hue app's "Make Discoverable" sets on the
/// lights it reaches, for a limited time; bit `0x02` comes and goes for
/// reasons not known. A light with bit `0x01` clear answers every Pairing
/// Request with "Pairing Not Supported".
pub fn pairing_open(service_data: &[u8]) -> bool {
    service_data.get(4).is_some_and(|flags| flags & 0x01 != 0)
}

/// The Philips service data of an advertisement, if it's from a Hue light
/// (empty if the light advertises the service without data). The data ends
/// with part of the light's Zigbee address.
pub fn hue_service_data(mut rest: &[u8]) -> Option<&[u8]> {
    let mut hue = false;
    let mut service_data: &[u8] = &[];
    while let [len, tail @ ..] = rest {
        let len = usize::from(*len);
        if len == 0 || tail.len() < len {
            break;
        }
        let (structure, next) = tail.split_at(len);
        match structure {
            // 16-bit service UUIDs, incomplete or complete list.
            [0x02 | 0x03, uuids @ ..] => hue |= uuids.chunks_exact(2).any(|uuid| uuid == HUE_SERVICE_UUID16),
            // Service data for a 16-bit UUID.
            [0x16, a, b, value @ ..] if [*a, *b] == HUE_SERVICE_UUID16 => {
                hue = true;
                service_data = value;
            }
            _ => {}
        }
        rest = next;
    }
    hue.then_some(service_data)
}
