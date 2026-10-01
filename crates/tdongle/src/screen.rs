//! The 160x80 display: the time at the top in seven-segment digits, and the
//! light's progress bar in a box below it, with a clock icon over the bar
//! while automation is on.

use core::fmt::Write as _;

use crate::aafont::Icon;
use crate::fonts::CLOCK_OUTLINE;

use eg_seven_segment::SevenSegmentStyleBuilder;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, StrokeAlignment};
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};

/// Orange, in RGB565: the light level once connected.
pub const ORANGE: Rgb565 = Rgb565::new(31, 33, 0);
/// Dark gray: the bar while still connecting.
pub const GRAY: Rgb565 = Rgb565::new(8, 16, 8);
/// Green, in RGB565: the icon while WiFi is up.
const GREEN: Rgb565 = Rgb565::new(4, 56, 4);
/// Light gray: the icon without WiFi.
const LIGHT_GRAY: Rgb565 = Rgb565::new(20, 40, 20);

/// The progress bar's box, at the bottom of the display.
const BOX: Rectangle = Rectangle::new(Point::new(0, 52), Size::new(160, 28));
const BORDER: u32 = 2;
/// Space between the border and the bar.
const GAP: u32 = 2;

/// Whether to draw the clock icon over the progress bar, and how WiFi is.
#[derive(Clone, Copy, PartialEq)]
pub enum Auto {
    /// Automation is off: no icon.
    Off,
    /// Automation is on, WiFi is down: gray.
    NoWifi,
    /// Automation is on, WiFi is up: green.
    Wifi,
}

impl Auto {
    pub fn new(on: bool, wifi_up: bool) -> Self {
        match (on, wifi_up) {
            (false, _) => Self::Off,
            (true, false) => Self::NoWifi,
            (true, true) => Self::Wifi,
        }
    }

    fn color(self) -> Option<Rgb565> {
        match self {
            Self::Off => None,
            Self::NoWifi => Some(LIGHT_GRAY),
            Self::Wifi => Some(GREEN),
        }
    }
}

/// Draws the progress bar: a box with a border in `color`, and inside it a
/// bar in the same color, filled `percent` percent from the left. With
/// `auto`, the clock icon is drawn over it, in the middle.
pub fn draw_progress<T: DrawTarget<Color = Rgb565>>(display: &mut T, percent: u32, color: Rgb565, auto: Auto) {
    let _ = BOX
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(color)
                .stroke_width(BORDER)
                .stroke_alignment(StrokeAlignment::Inside)
                .build(),
        )
        .draw(display);

    let inset = BORDER + GAP;
    let inner = Rectangle::new(
        BOX.top_left + Point::new(inset as i32, inset as i32),
        Size::new(BOX.size.width - 2 * inset, BOX.size.height - 2 * inset),
    );
    let filled = inner.size.width * percent.min(100) / 100;
    let _ = Rectangle::new(inner.top_left, Size::new(filled, inner.size.height))
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(display);
    let _ = Rectangle::new(
        inner.top_left + Point::new(filled as i32, 0),
        Size::new(inner.size.width - filled, inner.size.height),
    )
    .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
    .draw(display);

    if let Some(icon_color) = auto.color() {
        draw_icon(display, &CLOCK_OUTLINE, percent, color, icon_color);
    }
}

/// Room for the icon: the largest it is, and the buffer's size.
const ICON_AREA: Size = Size::new(24, 24);

/// Draws `icon` in `icon_color` over the bar just drawn, in the middle. The
/// icon blends into what is under it, which is worked out from `percent` and
/// `color`, as the display can't be read back.
fn draw_icon<T: DrawTarget<Color = Rgb565>>(
    display: &mut T,
    icon: &Icon,
    percent: u32,
    color: Rgb565,
    icon_color: Rgb565,
) {
    let area = Rectangle::with_center(BOX.center(), ICON_AREA);
    let mut pixels = [Rgb565::BLACK; (ICON_AREA.width * ICON_AREA.height) as usize];
    for (i, pixel) in pixels.iter_mut().enumerate() {
        let at = area.top_left + Point::new((i as u32 % ICON_AREA.width) as i32, (i as u32 / ICON_AREA.width) as i32);
        *pixel = bar_pixel(at - BOX.top_left, percent, color);
    }
    let (x0, y0) = (
        (ICON_AREA.width as i32 - i32::from(icon.width)) / 2,
        (ICON_AREA.height as i32 - i32::from(icon.height)) / 2,
    );
    icon.pixels(|x, y, coverage| {
        let (px, py) = (x0 + x, y0 + y);
        if (0..ICON_AREA.width as i32).contains(&px) && (0..ICON_AREA.height as i32).contains(&py) {
            let pixel = &mut pixels[(py * ICON_AREA.width as i32 + px) as usize];
            let (a, b) = (u16::from(coverage), 15 - u16::from(coverage));
            let mix = |fg: u8, bg: u8| ((u16::from(fg) * a + u16::from(bg) * b) / 15) as u8;
            *pixel = Rgb565::new(
                mix(icon_color.r(), pixel.r()),
                mix(icon_color.g(), pixel.g()),
                mix(icon_color.b(), pixel.b()),
            );
        }
    });
    let _ = display.fill_contiguous(&area, pixels);
}

/// The color of the pixel at `at`, relative to the box's corner, in the
/// progress bar drawn by `draw_progress`.
fn bar_pixel(at: Point, percent: u32, color: Rgb565) -> Rgb565 {
    let (width, height) = (BOX.size.width as i32, BOX.size.height as i32);
    let border = BORDER as i32;
    let inset = (BORDER + GAP) as i32;
    let filled = ((BOX.size.width - 2 * (BORDER + GAP)) * percent.min(100) / 100) as i32;
    let on_border = at.x < border || at.x >= width - border || at.y < border || at.y >= height - border;
    let on_bar = (inset..height - inset).contains(&at.y) && (inset..inset + filled).contains(&at.x);
    if on_border || on_bar { color } else { Rgb565::BLACK }
}

/// The area of the time, above the bar.
const CLOCK: Rectangle = Rectangle::new(Point::new(0, 0), Size::new(160, 48));

/// Draws the time, hour and minute, in white seven-segment digits, in the
/// middle of the top of the display; `--:--` while the time isn't known.
pub fn draw_clock<T: DrawTarget<Color = Rgb565>>(display: &mut T, time: Option<(u8, u8)>) {
    let _ = CLOCK.into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);
    let mut text = heapless::String::<5>::new();
    match time {
        Some((hour, minute)) => {
            let _ = write!(text, "{hour:02}:{minute:02}");
        }
        None => {
            let _ = text.push_str("--:--");
        }
    }
    // Four digits of 30 px, the colon, and 6 px between: 150 px of the 160.
    let style = SevenSegmentStyleBuilder::new()
        .digit_size(Size::new(30, CLOCK.size.height))
        .digit_spacing(6)
        .segment_width(6)
        .segment_color(Rgb565::WHITE)
        .build();
    let _ = Text::with_text_style(
        &text,
        Point::new(CLOCK.center().x, CLOCK.top_left.y),
        style,
        TextStyleBuilder::new()
            .alignment(Alignment::Center)
            .baseline(Baseline::Top)
            .build(),
    )
    .draw(display);
}
