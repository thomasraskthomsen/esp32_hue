//! The time of day, from SNTP over WiFi.
//!
//! The `sntp` task asks `pool.ntp.org` once the network is up, and again every
//! hour. Between answers the time runs on `embassy_time`. Until the first
//! answer there is no time; see [`now`].

use core::cell::Cell;

use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use log::{info, warn};

use crate::localtime;

const SERVER: &str = "pool.ntp.org";
const PORT: u16 = 123;
/// Seconds from 1900-01-01, the start of NTP time, to 1970-01-01.
const NTP_TO_UNIX: u32 = 2_208_988_800;
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const RESYNC: Duration = Duration::from_secs(3600);
const RETRY: Duration = Duration::from_secs(10);

/// The last answer: Unix seconds, and when it arrived.
static SYNC: Mutex<CriticalSectionRawMutex, Cell<Option<(u32, Instant)>>> = Mutex::new(Cell::new(None));

/// The hour and minute in Copenhagen, or `None` before the first answer.
pub fn now() -> Option<(u8, u8)> {
    let (unix, at) = SYNC.lock(Cell::get)?;
    Some(localtime::copenhagen(i64::from(unix) + at.elapsed().as_secs() as i64))
}

/// Keeps the time up to date.
#[embassy_executor::task]
pub async fn sntp(stack: Stack<'static>) {
    loop {
        stack.wait_config_up().await;
        match query(stack).await {
            Ok(unix) => {
                SYNC.lock(|sync| sync.set(Some((unix, Instant::now()))));
                info!("Time: {:?}", now());
                Timer::after(RESYNC).await;
            }
            Err(why) => {
                warn!("Time: {why}");
                Timer::after(RETRY).await;
            }
        }
    }
}

/// Asks the server for the time; Unix seconds.
async fn query(stack: Stack<'static>) -> Result<u32, &'static str> {
    let addresses = stack.dns_query(SERVER, DnsQueryType::A).await.map_err(|_| "DNS lookup failed")?;
    let address = *addresses.first().ok_or("no address for the server")?;

    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut rx = [0u8; 64];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx = [0u8; 64];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket.bind(0).map_err(|_| "bind failed")?;

    // Version 4, client mode; the rest of the request is zero.
    let mut packet = [0u8; 48];
    packet[0] = 0x23;
    socket.send_to(&packet, (address, PORT)).await.map_err(|_| "send failed")?;

    let mut reply = [0u8; 64];
    let (len, _) = with_timeout(REPLY_TIMEOUT, socket.recv_from(&mut reply))
        .await
        .map_err(|_| "no reply")?
        .map_err(|_| "receive failed")?;
    if len < 48 {
        return Err("short reply");
    }
    // The server's transmit time, seconds part.
    let seconds = u32::from_be_bytes([reply[40], reply[41], reply[42], reply[43]]);
    if seconds == 0 {
        return Err("server isn't synchronised");
    }
    Ok(seconds.wrapping_sub(NTP_TO_UNIX))
}
