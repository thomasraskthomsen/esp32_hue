//! A small web server for editing the automation schedule from a phone.
//!
//! `GET /` is the page (`web/index.html`), `GET /api/schedule` reads the
//! schedule and `PUT /api/schedule` replaces it:
//!
//! ```text
//! {"automate":true,"entries":[{"minute":390,"dim":60,"forced":false},
//!                             {"minute":1380,"dim":null,"forced":true}]}
//! ```
//!
//! `minute` is minutes since midnight, `dim` is 1 to 254 or `null` for off.
//! `PUT` only reads `entries`; automation itself is switched with the BOOT
//! button. Connections are served one at a time, each closed after its
//! response.

use core::fmt::Write as _;
use core::sync::atomic::Ordering;

use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::Duration;
use embedded_io_async::Write;
use heapless::String;
use log::{info, warn};

use crate::light::AUTOMATE;
use crate::schedule::{self, Entries, Entry, MIN_ENTRIES};
use crate::settings::Shared;

const PAGE: &str = include_str!("../../../web/index.html");
const PORT: u16 = 80;
/// Largest request, headers and body, that is accepted.
const REQUEST_MAX: usize = 1536;
const DAY_MINUTES: u16 = 24 * 60;

/// Serves the page and the API forever.
pub async fn serve(stack: Stack<'static>, store: &'static Shared) -> ! {
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 2048];
    let mut request = [0u8; REQUEST_MAX];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(Duration::from_secs(5)));
        if socket.accept(PORT).await.is_err() {
            continue;
        }
        if handle(&mut socket, &mut request, store).await.is_err() {
            warn!("Web: connection failed");
        }
        socket.close();
        let _ = socket.flush().await;
        socket.abort();
    }
}

async fn handle(socket: &mut TcpSocket<'_>, request: &mut [u8], store: &Shared) -> Result<(), ()> {
    let mut len = 0;
    let (head_end, body_len) = loop {
        if len == request.len() {
            return respond(socket, "431 Request Header Fields Too Large", "text/plain", b"").await;
        }
        let n = socket.read(&mut request[len..]).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        len += n;
        if let Some(end) = find(&request[..len], b"\r\n\r\n") {
            break (end + 4, content_length(&request[..end]));
        }
    };
    if head_end + body_len > request.len() {
        return respond(socket, "413 Payload Too Large", "text/plain", b"").await;
    }
    while len < head_end + body_len {
        let n = socket.read(&mut request[len..]).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        len += n;
    }

    let head = core::str::from_utf8(&request[..head_end]).map_err(|_| ())?;
    let mut line = head.lines().next().unwrap_or("").split(' ');
    let (method, path) = (line.next().unwrap_or(""), line.next().unwrap_or(""));
    let body = &request[head_end..head_end + body_len];

    match (method, path) {
        ("GET", "/" | "/index.html") => respond(socket, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()).await,
        ("GET", "/api/schedule") => {
            let entries = store.lock().await.load_schedule();
            let mut json = String::<640>::new();
            if write_json(&mut json, AUTOMATE.load(Ordering::Relaxed), &entries).is_err() {
                return respond(socket, "500 Internal Server Error", "text/plain", b"").await;
            }
            respond(socket, "200 OK", "application/json", json.as_bytes()).await
        }
        ("PUT", "/api/schedule") => match parse_entries(body) {
            Ok(entries) => {
                if let Err(err) = store.lock().await.save_schedule(&entries) {
                    warn!("Web: saving the schedule failed: {err:?}");
                    return respond(socket, "500 Internal Server Error", "text/plain", b"Saving failed").await;
                }
                info!("Web: schedule saved, {} times", entries.len());
                respond(socket, "204 No Content", "text/plain", b"").await
            }
            Err(why) => respond(socket, "400 Bad Request", "text/plain", why.as_bytes()).await,
        },
        _ => respond(socket, "404 Not Found", "text/plain", b"").await,
    }
}

async fn respond(socket: &mut TcpSocket<'_>, status: &str, content_type: &str, body: &[u8]) -> Result<(), ()> {
    let mut head = String::<192>::new();
    write!(
        head,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .map_err(|_| ())?;
    socket.write_all(head.as_bytes()).await.map_err(|_| ())?;
    socket.write_all(body).await.map_err(|_| ())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The `Content-Length` in the request headers; 0 if there is none.
fn content_length(head: &[u8]) -> usize {
    let Ok(head) = core::str::from_utf8(head) else { return 0 };
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

/// Writes the schedule as the JSON the page reads.
pub fn write_json(out: &mut impl core::fmt::Write, automate: bool, entries: &[Entry]) -> core::fmt::Result {
    write!(out, "{{\"automate\":{automate},\"entries\":[")?;
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            out.write_char(',')?;
        }
        write!(out, "{{\"minute\":{},\"dim\":", entry.minute)?;
        match entry.dim {
            Some(dim) => write!(out, "{dim}")?,
            None => out.write_str("null")?,
        }
        write!(out, ",\"forced\":{}}}", entry.forced)?;
    }
    out.write_str("]}")
}

/// The raw text after `"key":` in a JSON object body, up to the next comma.
fn value<'a>(object: &'a str, key: &str) -> Option<&'a str> {
    let at = object.find(key)? + key.len();
    let rest = object[at..].trim_start().strip_prefix(':')?;
    Some(rest.split(',').next()?.trim())
}

/// Reads the `entries` of a `PUT` body and checks them: between
/// [`MIN_ENTRIES`] and the most that fit, times within the day and not
/// repeated, brightness 1 to 254 or `null`.
pub fn parse_entries(body: &[u8]) -> Result<Entries, &'static str> {
    let text = core::str::from_utf8(body).map_err(|_| "Not text")?;
    let mut rest = &text[text.find("\"entries\"").ok_or("No entries")?..];
    let mut entries = Entries::new();
    while let Some(open) = rest.find('{') {
        let close = open + rest[open..].find('}').ok_or("Bad JSON")?;
        let object = &rest[open + 1..close];
        rest = &rest[close + 1..];

        let minute: u16 = value(object, "\"minute\"").and_then(|v| v.parse().ok()).ok_or("Bad minute")?;
        if minute >= DAY_MINUTES {
            return Err("Time is outside the day");
        }
        let dim = match value(object, "\"dim\"").ok_or("Bad dim")? {
            "null" => None,
            n => Some(n.parse::<u8>().ok().filter(|n| (1..=254).contains(n)).ok_or("Dim must be 1-254 or null")?),
        };
        let forced = value(object, "\"forced\"") == Some("true");

        if schedule::due(&entries, minute).is_some() {
            return Err("Two entries share a time");
        }
        schedule::set(&mut entries, Entry { minute, dim, forced }).map_err(|_| "Too many entries")?;
    }
    if entries.len() < MIN_ENTRIES {
        return Err("Need at least 2 entries");
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sorts() {
        let body = br#"{"automate":true,"entries":[{"minute":1380,"dim":null,"forced":true},{"minute":390,"dim":60,"forced":false}]}"#;
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], Entry { minute: 390, dim: Some(60), forced: false });
        assert_eq!(entries[1], Entry { minute: 1380, dim: None, forced: true });
    }

    #[test]
    fn rejects_bad_schedules() {
        assert!(parse_entries(br#"{"entries":[{"minute":10,"dim":5,"forced":false}]}"#).is_err());
        assert!(parse_entries(br#"{"entries":[{"minute":10,"dim":0,"forced":false},{"minute":20,"dim":5,"forced":false}]}"#).is_err());
        assert!(parse_entries(br#"{"entries":[{"minute":10,"dim":5,"forced":false},{"minute":10,"dim":6,"forced":false}]}"#).is_err());
        assert!(parse_entries(br#"{"entries":[{"minute":1440,"dim":5,"forced":false},{"minute":20,"dim":5,"forced":false}]}"#).is_err());
    }
}
