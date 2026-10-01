#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_io_async::Read;
use esp_backtrace as _;
use esp_bootloader_esp_idf::partitions::FlashStorage;
use esp_hal::Async;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::gpio::{DriveMode, Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::ledc::channel::{self, ChannelIFace};
use esp_hal::ledc::timer::{self, TimerIFace};
use esp_hal::ledc::{LSGlobalClkSource, Ledc, LowSpeed};
use esp_hal::rng::Rng;
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx};
use esp_radio::ble::controller::BleConnector;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{AuthenticationMethodConfig, Config as WifiConfig, ControllerConfig, Interface, WifiController};
use log::{info, warn};
use mipidsi::interface::SpiInterface;
use mipidsi::models::ST7735s;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation, Rotation};
use static_cell::StaticCell;
use embassy_sync::mutex::Mutex;
use hue_ble::Link;
use tdongle::led::Led;
use core::sync::atomic::Ordering;
use tdongle::light::{AUTOMATE, LIGHT, Press};
use tdongle::screen::{Auto, GRAY, ORANGE, draw_clock, draw_progress};
use tdongle::{ble, clock, light, schedule, settings};

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

/// WiFi credentials from the environment at build time, used when none are
/// stored in the settings partition.
const BUILD_SSID: Option<&str> = option_env!("SSID");
const BUILD_PASSWORD: Option<&str> = option_env!("PASSWORD");

/// Name sent with the DHCP request.
const HOSTNAME: &str = "esp32-hue";


#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    // Wi-Fi needs more than the reclaimed region.
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let mut delay = Delay::new();

    // LilyGO T-Dongle-S3: 0.96" ST7735 (80x160) on SPI.
    let c = OutputConfig::default();
    // Backlight is PWM-dimmed and active low, so the duty cycle is inverted.
    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
    let mut backlight_timer = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    backlight_timer
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_khz(20),
        })
        .unwrap();
    let mut backlight = ledc.channel(channel::Number::Channel0, peripherals.GPIO38);
    backlight
        .configure(channel::config::Config {
            timer: &backlight_timer,
            // On, always; the backlight is active low.
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        })
        .unwrap();

    let spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(27))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO5)
    .with_mosi(peripherals.GPIO3);
    let cs = Output::new(peripherals.GPIO4, Level::High, c);
    let spi_device = ExclusiveDevice::new_no_delay(spi, cs).unwrap();

    let dc = Output::new(peripherals.GPIO2, Level::Low, c);
    let rst = Output::new(peripherals.GPIO1, Level::High, c);

    let mut buffer = [0u8; 512];
    let di = SpiInterface::new(spi_device, dc, &mut buffer);

    let mut display = mipidsi::Builder::new(ST7735s, di)
        .display_size(80, 160)
        .display_offset(26, 1)
        .invert_colors(ColorInversion::Inverted)
        .color_order(ColorOrder::Bgr)
        .orientation(Orientation::new().rotate(Rotation::Deg90))
        .reset_pin(rst)
        .init(&mut delay)
        .unwrap();
    display.clear(Rgb565::BLACK).unwrap();
    info!("Display initialised");

    // BOOT button: switches the lights, see `light`.
    let button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));

    // WiFi credentials: stored ones first, then the ones from the build.
    let mut store = settings::Store::new(FlashStorage::new(peripherals.FLASH))
        .expect("No settings partition; flash with partitions.csv");
    let mut wifi = store.load();
    AUTOMATE.store(store.load_automate(), Ordering::Relaxed);
    static STORE: StaticCell<settings::Shared> = StaticCell::new();
    let store: &'static settings::Shared = STORE.init(Mutex::new(store));
    if wifi.ssid.is_empty() {
        wifi.ssid = BUILD_SSID.unwrap_or("").try_into().unwrap_or_default();
        wifi.password = BUILD_PASSWORD.unwrap_or("").try_into().unwrap_or_default();
    }

    // Commands from the serial monitor. Logging keeps using the TX side.
    let (usb_rx, _usb_tx) = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split();
    spawner.spawn(serial_input(usb_rx, store).unwrap());

    let connector = BleConnector::new(peripherals.BT, Default::default()).expect("Failed to initialize BLE");
    spawner.spawn(ble_task(connector, store).unwrap());

    // WiFi station + DHCP.
    let controller_config = if wifi.ssid.is_empty() {
        warn!("No WiFi network set; use /ssid and /password on the serial console");
        ControllerConfig::default()
    } else {
        let auth = if wifi.password.is_empty() {
            AuthenticationMethodConfig::Open
        } else {
            AuthenticationMethodConfig::Wpa2Personal(wifi.password.as_str().try_into().unwrap())
        };
        let station = StationConfig::default()
            .with_ssid(wifi.ssid.as_str().try_into().unwrap())
            .with_authentication(auth);
        ControllerConfig::default().with_initial_config(WifiConfig::Station(station))
    };
    let controller =
        WifiController::new(peripherals.WIFI, controller_config).expect("Failed to initialize WiFi controller");

    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    // DHCP sends our name, which most home routers then register in their
    // DNS, so the dongle can be reached as `esp32-hue`.
    let mut dhcp = embassy_net::DhcpConfig::default();
    dhcp.hostname = Some(HOSTNAME.try_into().unwrap());
    // Sockets: DHCP, DNS, SNTP and the web server.
    static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        Interface::station(),
        embassy_net::Config::dhcpv4(dhcp),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    if !wifi.ssid.is_empty() {
        spawner.spawn(connection(controller, wifi.ssid.clone()).unwrap());
    }
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(clock::sntp(stack).unwrap());
    spawner.spawn(web_server(stack, store).unwrap());

    // The button only counts once it has been seen released, so a press held
    // since boot is ignored.
    let mut button_armed = false;
    let mut dimmer = light::Dimmer::new();
    // The dongle's APA102 LED (clock on GPIO39, data on GPIO40) isn't used,
    // but it may power up lit, so switch it off once.
    Led::new(
        Output::new(peripherals.GPIO39, Level::Low, c),
        Output::new(peripherals.GPIO40, Level::Low, c),
    )
    .set_level(0, light::FULL);
    // The progress bar shows the light level, or is empty and gray until the
    // lights have answered (`None`); `shown_bar` is what is drawn.
    let mut bar_level: Option<u8> = None;
    let mut shown_bar = None;
    let mut shown_up = None;
    let mut shown_clock = None;
    // The session with the lights is started at boot and kept open; the
    // screen is always on and shows how it goes. `lights_up`: the first light
    // has connected, and `dimmer` has its level, which it follows from then
    // on. `next_session`: when to start the session if it isn't running (it
    // ends if no light can be reached). `stopping`: the session was stopped
    // to restart it, so its end is expected.
    let mut session_running = false;
    let mut lights_up = false;
    let mut next_session = Instant::now();
    let mut stopping = false;
    // The first light also reports our own changes back, late; they are
    // ignored until `mute_until`, so they don't set the level back.
    let mut mute_until = Instant::now();
    // Automation: the last minute checked against the schedule.
    let mut last_minute: Option<u16> = None;
    // Automation was just switched on: set the level the schedule has now.
    let mut was_automate = AUTOMATE.load(Ordering::Relaxed);
    let mut apply_now = false;

    loop {
        if !session_running && Instant::now() >= next_session {
            info!("Connecting to the lights");
            session_running = ble::COMMANDS.try_send(ble::Command::Session).is_ok();
        }

        // Network state, logged when it changes.
        let up = stack.is_link_up() && stack.is_config_up();
        if shown_up != Some(up) {
            match stack.config_v4() {
                Some(cfg) if up => info!("Got IP address {}", cfg.address.address()),
                _ if shown_up == Some(true) => warn!("Lost network connection"),
                _ => {}
            }
            shown_up = Some(up);
        }

        // The bar, with AUTO over it, redrawn when the level, automation or
        // the WiFi state changes.
        let auto = Auto::new(AUTOMATE.load(Ordering::Relaxed), up);
        if shown_bar != Some((bar_level, auto)) {
            draw_bar(&mut display, bar_level, auto);
            shown_bar = Some((bar_level, auto));
        }

        // The clock, redrawn when the minute changes.
        let clock = clock::now();
        if shown_clock != Some(clock) {
            draw_clock(&mut display, clock);
            shown_clock = Some(clock);
        }

        // Automation: carry out the schedule entry whose minute has come, if
        // the lights are as the previous entry left them (or it is forced).
        let minute = clock::now().map(|(hour, minute)| u16::from(hour) * 60 + u16::from(minute));
        let automate = AUTOMATE.load(Ordering::Relaxed);
        if automate != was_automate {
            was_automate = automate;
            apply_now = automate;
        }
        if let (true, Some(minute)) = (apply_now, minute) {
            apply_now = false;
            let entries = store.lock().await.load_schedule();
            if let Some(at) = schedule::current(&entries, minute).filter(|_| entries.len() >= schedule::MIN_ENTRIES) {
                let entry = entries[at];
                if !lights_up {
                    warn!("Automation: not connected to the lights; current level not set");
                } else {
                    info!("Automation: on; light level {} -> {}", dimmer.level(), entry.level());
                    dimmer.set_level(entry.level(), |level| bar_level = Some(level));
                    mute_until = Instant::now() + MUTE;
                }
            }
        }
        if minute.is_some() && minute != last_minute {
            last_minute = minute;
            if let (Some(minute), true) = (minute, AUTOMATE.load(Ordering::Relaxed)) {
                let entries = store.lock().await.load_schedule();
                if entries.len() >= schedule::MIN_ENTRIES {
                    if let Some(at) = schedule::due(&entries, minute) {
                        let (entry, previous) = (entries[at], schedule::previous(&entries, at));
                        let current = dimmer.level();
                        if !lights_up {
                            warn!("Automation: not connected to the lights; skipped");
                        } else if entry.applies(current, &previous) {
                            info!("Automation: light level {current} -> {}", entry.level());
                            dimmer.set_level(entry.level(), |level| bar_level = Some(level));
                            mute_until = Instant::now() + MUTE;
                        } else {
                            info!("Automation: light level {current} isn't what it set before; skipped");
                        }
                    }
                }
            }
        }

        // Stop the session and start it again, for `/log`.
        if light::RECONNECT.swap(false, Ordering::Relaxed) {
            if ble::COMMANDS.try_send(ble::Command::Session).is_err() {
                warn!("BLE is busy; not reconnecting");
            } else {
                if session_running {
                    LIGHT.stop.signal(());
                    stopping = true;
                }
                session_running = true;
                lights_up = false;
                bar_level = None;
            }
        }

        // How the connection to the lights goes.
        if let Some(link) = LIGHT.link.try_take() {
            match link {
                Link::Connected(level) => {
                    info!("Light connected, at {level}");
                    lights_up = true;
                    dimmer.set_initial(level);
                    bar_level = Some(level);
                }
                Link::Disconnected if stopping => stopping = false,
                Link::Disconnected => {
                    warn!("No connection to the lights; trying again in {} s", SESSION_RETRY.as_secs());
                    session_running = false;
                    lights_up = false;
                    next_session = Instant::now() + SESSION_RETRY;
                    bar_level = None;
                }
                Link::Connecting => {}
            }
        }

        // The first light's level, when it changes from elsewhere (the Hue
        // app, say).
        if let Some(level) = LIGHT.observed.try_take() {
            if lights_up && Instant::now() >= mute_until && level != dimmer.level() {
                info!("The light was changed elsewhere, to {level}");
                dimmer.set_initial(level);
                bar_level = Some(level);
            }
        }

        if button.is_high() {
            button_armed = true;
        } else if button_armed {
            if lights_up {
                // The bar follows the level as it changes, even while the
                // button is held.
                let mut show = |level| {
                    info!("Light level {level}");
                    bar_level = Some(level);
                    shown_bar = Some((bar_level, auto));
                    draw_bar(&mut display, bar_level, auto);
                };
                let pressed = dimmer.press(&button, &mut show).await;
                if pressed == Press::Click {
                    if light::second_press(&button).await {
                        // Double-click: automation, only with WiFi up (the
                        // lights are, as we're here).
                        if up {
                            toggle_automate(&mut *store.lock().await);
                        }
                    } else {
                        dimmer.toggle(&mut show);
                    }
                }
                mute_until = Instant::now() + MUTE;
            } else {
                // Not connected yet; the press does nothing.
                while button.is_low() {
                    Timer::after(Duration::from_millis(10)).await;
                }
            }
        }

        Timer::after(Duration::from_millis(20)).await;
    }
}

/// How long after a change of ours the light's reports of its level are
/// ignored.
const MUTE: Duration = Duration::from_secs(1);

/// How long to wait before starting the session with the lights again after it
/// ended because no light could be reached.
const SESSION_RETRY: Duration = Duration::from_secs(60);

/// Draws the bar: orange at the light `level`, or empty and gray if there is
/// none yet; with the clock icon over it as `auto` says.
fn draw_bar<D: DrawTarget<Color = Rgb565>>(display: &mut D, level: Option<u8>, auto: Auto) {
    match level {
        Some(level) => draw_progress(display, u32::from(level) * 100 / u32::from(light::FULL), ORANGE, auto),
        None => draw_progress(display, 0, GRAY, auto),
    }
}

/// Keeps the station connected, reconnecting after a drop.
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>, ssid: heapless::String<32>) {
    loop {
        info!("Connecting to {ssid}");
        match controller.connect_async().await {
            Ok(_) => {
                info!("WiFi connected");
                let _ = controller.wait_for_disconnect_async().await;
                warn!("WiFi disconnected");
            }
            Err(err) => warn!("Failed to connect to WiFi: {err:?}"),
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

/// Runs BLE, which the serial console drives (see `tdongle::ble`).
#[embassy_executor::task]
async fn ble_task(connector: BleConnector<'static>, store: &'static settings::Shared) {
    ble::run(connector, store).await;
}

/// The time in front of every log line, for `esp-println`'s `timestamp`
/// feature: milliseconds since boot.
#[unsafe(no_mangle)]
extern "Rust" fn _esp_println_timestamp() -> u64 {
    esp_hal::time::Instant::now().duration_since_epoch().as_millis()
}

/// Reads commands from the serial monitor, one per line (see `command`).
#[embassy_executor::task]
async fn serial_input(mut rx: UsbSerialJtagRx<'static, Async>, store: &'static settings::Shared) {
    let mut buf = [0u8; 64];
    let mut line = heapless::String::<128>::new();
    let mut too_long = false;
    loop {
        let n = match rx.read(&mut buf).await {
            Ok(n) => n,
            Err(err) => {
                warn!("USB serial read failed: {err:?}");
                continue;
            }
        };
        for &byte in &buf[..n] {
            if byte == b'\r' || byte == b'\n' {
                if too_long {
                    warn!("Command too long");
                } else {
                    // The `/` before a command is optional.
                    let text = line.trim();
                    let text = text.strip_prefix('/').unwrap_or(text);
                    if !text.is_empty() {
                        info!("> /{text}");
                        command(text, store).await;
                    }
                }
                line.clear();
                too_long = false;
            } else if line.push(char::from(byte)).is_err() {
                too_long = true;
            }
        }
    }
}

/// Carries out a serial console command.
///
/// - `ssid <name>`: the WiFi network to join
/// - `password <password>`: its password; empty for an open network
/// - `status`: shows the stored settings, without the password
/// - `restart`: restarts, connecting with the stored settings
///
/// and for the lights, whose results are logged by the BLE task:
///
/// - `scan [secs]`: looks for lights for `secs` (8) seconds, numbering them
///   as they appear
/// - `watch [secs]`: scans for `secs` (120) seconds, logging each change in
///   what a light advertises as it happens
/// - `pair <n>`: pairs with light `n` from the scan, which must be in pairing mode
/// - `autopair [all]`: for a minute, pairs with every light not paired yet
///   (`all`: every light) as soon as it accepts pairing (Make Discoverable
///   in the Hue app)
/// - `lights`: lists the paired lights
/// - `forget [light]`: forgets paired light number `light`, or all of them
/// - `power on|off [light]`, `bright <1-254> [light]`: controls paired light
///   number `light` (from `lights`; the first if left out)
async fn command(line: &str, store: &settings::Shared) {
    let (name, value) = line.split_once(' ').unwrap_or((line, ""));
    let light_command = match (name, value.trim()) {
        ("scan", secs) => Some(ble::Command::Scan(secs.parse().unwrap_or(8))),
        ("watch", secs) => Some(ble::Command::Watch(secs.parse().unwrap_or(120))),
        ("pair", n) => n.parse().ok().map(ble::Command::Pair),
        ("autopair", all) => Some(ble::Command::AutoPair { all: all == "all" }),
        ("lights", _) => Some(ble::Command::Lights),
        ("forget", "") => Some(ble::Command::Forget(None)),
        ("forget", n) => n.parse().ok().map(|n| ble::Command::Forget(Some(n))),
        ("power", args) => {
            let (on, light) = args.split_once(' ').unwrap_or((args, "0"));
            let light = light.trim().parse().ok();
            match (on, light) {
                ("on", Some(light)) => Some(ble::Command::Power { light, on: true }),
                ("off", Some(light)) => Some(ble::Command::Power { light, on: false }),
                _ => None,
            }
        }
        ("bright", args) => {
            let (level, light) = args.split_once(' ').unwrap_or((args, "0"));
            match (level.parse().ok(), light.trim().parse().ok()) {
                (Some(level), Some(light)) => Some(ble::Command::Brightness { light, level }),
                _ => None,
            }
        }
        _ => None,
    };
    if let Some(light_command) = light_command {
        if ble::COMMANDS.try_send(light_command).is_err() {
            warn!("BLE is busy; try again in a moment");
        }
        return;
    }
    let mut store = store.lock().await;
    match name {
        "ssid" | "password" => {
            let mut stored = store.load();
            let saved = if name == "ssid" {
                value.try_into().map(|v| stored.ssid = v).is_ok()
            } else {
                value.try_into().map(|v| stored.password = v).is_ok()
            };
            if !saved {
                warn!("Too long");
            } else if let Err(err) = store.save(&stored) {
                warn!("Saving settings failed: {err:?}");
            } else {
                info!("Saved {name}; /restart to connect with it");
            }
        }
        "status" => {
            let stored = store.load();
            if stored.ssid.is_empty() {
                info!("No WiFi network stored");
            } else {
                let password = if stored.password.is_empty() { "no password" } else { "password set" };
                info!("WiFi network: {} ({password})", stored.ssid);
            }
        }
        "log" => {
            let on = match value.trim() {
                "" => !LIGHT.log_all.load(Ordering::Relaxed),
                "on" => true,
                "off" => false,
                _ => {
                    warn!("Use /log on, /log off, or /log to toggle");
                    return;
                }
            };
            LIGHT.log_all.store(on, Ordering::Relaxed);
            // Read when a light connects, so connect again.
            light::RECONNECT.store(true, Ordering::Relaxed);
            info!("Logging everything from the lights: {}; reconnecting", if on { "on" } else { "off" });
        }
        "schedule" => schedule_command(&mut store, value.trim()),
        "automate" => match value.trim() {
            "" => toggle_automate(&mut store),
            "on" => set_automate(&mut store, true),
            "off" => set_automate(&mut store, false),
            _ => warn!("Use /automate on, /automate off, or /automate to toggle"),
        },
        "restart" => esp_hal::system::software_reset(),
        _ => info!(
            "Commands: /ssid <name>, /password <password>, /status, /log [on|off], /automate [on|off], /schedule [list|set HH:MM <1-254|off> [force]|remove HH:MM|clear], /restart, /scan [secs], /watch [secs], /pair <n>, \
             /autopair [all], /lights, /forget [light], /power on|off [light], /bright <1-254> [light]"
        ),
    }
}

/// Switches automation on or off and stores the new setting. The main loop
/// redraws the badge.
fn toggle_automate(store: &mut settings::Store) {
    set_automate(store, !AUTOMATE.load(Ordering::Relaxed));
}

/// Carries out `/schedule [list]`, `/schedule set HH:MM <1-254|off> [force]`,
/// `/schedule remove HH:MM` and `/schedule clear`, then lists the schedule.
/// Automation goes off if the schedule is left with too few times.
fn schedule_command(store: &mut settings::Store, args: &str) {
    let (sub, rest) = args.split_once(' ').unwrap_or((args, ""));
    let mut entries = store.load_schedule();
    let mut changed = true;
    match sub {
        "" | "list" => changed = false,
        "set" => {
            let mut words = rest.split_whitespace();
            let minute = words.next().and_then(schedule::parse_time);
            let dim = match words.next() {
                Some("off") => Some(None),
                Some(n) => n.parse::<u8>().ok().filter(|n| (1..=254).contains(n)).map(Some),
                None => None,
            };
            let forced = match words.next() {
                None => Some(false),
                Some("force") => Some(true),
                Some(_) => None,
            };
            let (Some(minute), Some(dim), Some(forced)) = (minute, dim, forced) else {
                warn!("Use /schedule set HH:MM <1-254|off> [force]");
                return;
            };
            if schedule::set(&mut entries, schedule::Entry { minute, dim, forced }).is_err() {
                warn!("The schedule is full ({} times); remove one first", schedule::MAX_ENTRIES);
                return;
            }
        }
        "remove" => match rest.trim().split_once(':').and(schedule::parse_time(rest.trim())) {
            Some(minute) if schedule::remove(&mut entries, minute) => {}
            _ => {
                warn!("No schedule time like that; use /schedule remove HH:MM");
                return;
            }
        },
        "clear" => entries.clear(),
        _ => {
            warn!("Use /schedule [list], set HH:MM <1-254|off> [force], remove HH:MM, or clear");
            return;
        }
    }
    if changed {
        if let Err(err) = store.save_schedule(&entries) {
            warn!("Saving the schedule failed: {err:?}");
            return;
        }
        if entries.len() < schedule::MIN_ENTRIES && AUTOMATE.load(Ordering::Relaxed) {
            warn!("Fewer than {} times left; switching automation off", schedule::MIN_ENTRIES);
            set_automate(store, false);
        }
    }
    if entries.is_empty() {
        info!("Schedule: empty");
    }
    for entry in &entries {
        let (hour, minute) = (entry.minute / 60, entry.minute % 60);
        let forced = if entry.forced { " (forced)" } else { "" };
        match entry.dim {
            Some(dim) => info!("Schedule: {hour:02}:{minute:02} dim {dim}{forced}"),
            None => info!("Schedule: {hour:02}:{minute:02} off{forced}"),
        }
    }
}

/// Switches automation on or off and stores the setting.
fn set_automate(store: &mut settings::Store, on: bool) {
    if on && store.load_schedule().len() < schedule::MIN_ENTRIES {
        warn!("Automation needs at least {} schedule times; see /schedule set", schedule::MIN_ENTRIES);
        return;
    }
    AUTOMATE.store(on, Ordering::Relaxed);
    info!("Automation {}", if on { "on" } else { "off" });
    if let Err(err) = store.save_automate(on) {
        warn!("Saving the automation setting failed: {err:?}");
    }
}

#[embassy_executor::task]
async fn web_server(stack: embassy_net::Stack<'static>, store: &'static settings::Shared) {
    tdongle::web::serve(stack, store).await
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}
