#[cfg(target_os = "espidf")]
use esp_idf_hal::delay::FreeRtos;
#[cfg(target_os = "espidf")]
use esp_idf_hal::{cpu::Core, task::thread::ThreadSpawnConfiguration};
#[cfg(not(target_os = "espidf"))]
use smart_leds::RGB8;
#[cfg(target_os = "espidf")]
use smart_leds::{SmartLedsWrite, RGB8};
use std::sync::{Arc, Mutex};

// --------------------------------------------------------
// CONFIG DEFAULTS / BOUNDS
// --------------------------------------------------------
// These are the authoritative defaults and limits for the runtime-
// configurable marquee. main.rs seeds the shared state from NVS at
// startup, falling back to these values when a key is missing.

/// Maximum marquee text length, in characters. Enforced here and in the
/// portal save handler so a crafted POST cannot grow the string without
/// bound. The renderer treats the stored text as a fixed logical source.
pub const MARQUEE_TEXT_MAX: usize = 64;

/// Default marquee text used when NVS has no stored value.
pub const MARQUEE_TEXT_DEFAULT: &str = "CAMBRIDGE HACKSPACE :)";

/// Marquee speed is an OBJECTIVE delay in milliseconds between scroll steps
/// (one step = one pixel of travel). Lower = faster. The bounds keep the
/// fastest setting from pegging the render thread and the slowest from looking
/// frozen. This is the real unit stored in NVS and shown in the portal.
pub const MARQUEE_SPEED_MS_MIN: u16 = 20;
pub const MARQUEE_SPEED_MS_MAX: u16 = 500;
pub const MARQUEE_SPEED_MS_DEFAULT: u16 = 100; // the historical rate

/// The physical display is three 10x10 panels seated in the three lamps of a
/// traffic light, so they are NOT physically continuous. The panel margin is
/// the number of invisible "dead" units (columns when horizontal, rows when
/// vertical) inserted between adjacent panels to model those gaps. With three
/// panels there are two gaps, so the virtual major axis is 30 + 2*margin. A
/// margin of 0 restores the old fully-contiguous behavior.
pub const PANEL_COUNT: usize = 3;
pub const PANEL_SIZE: usize = 10; // each panel is 10 along the major axis
pub const PANEL_GAPS: usize = PANEL_COUNT - 1; // gaps between panels
/// Number of "sacrificial" LEDs at the head of the string, ahead of the display
/// matrix. They sit between the ESP32's 3.3V data output and the panel array,
/// acting as a level-shifting repeater, and double as the status indicator.
/// The hardware was reworked from a run of 8 down to a single LED; this is the
/// one place that count is defined.
pub const SACRIFICIAL_LEDS: usize = 1;

/// Total physical LEDs on the string: the sacrificial head plus the 30x10 matrix.
pub const STRIP_LEN: usize = SACRIFICIAL_LEDS + 300;

pub const PANEL_MARGIN_MAX: u8 = 20;
pub const PANEL_MARGIN_DEFAULT: u8 = 0;

/// Virtual major-axis length for a given margin: the three panels plus the two
/// inter-panel gaps.
pub fn virtual_major(margin: u8) -> usize {
    PANEL_COUNT * PANEL_SIZE + PANEL_GAPS * margin as usize
}

/// Map a virtual major-axis index (into a `virtual_major(margin)`-length axis)
/// to its physical index in 0..30, or None if it lands in an inter-panel gap
/// (a dead zone that is never shown). Shared by the marquee and stream paths so
/// the two can never disagree about where the seams are.
pub fn virtual_to_physical_major(vi: usize, margin: u8) -> Option<usize> {
    let m = margin as usize;
    let mut v = vi;
    for panel in 0..PANEL_COUNT {
        if v < PANEL_SIZE {
            return Some(panel * PANEL_SIZE + v);
        }
        v -= PANEL_SIZE;
        // after the last panel there is no trailing gap
        if panel < PANEL_COUNT - 1 {
            if v < m {
                return None; // inside an inter-panel gap
            }
            v -= m;
        }
    }
    None
}

pub const PANEL_MARGIN_STREAMS_DEFAULT: bool = false;

/// Physical mounting orientation. The panel's native layout is a 30-wide
/// by 10-tall horizontal grid; these values rotate the logical image so a
/// display mounted at 90/180/270 degrees still reads upright.
///
/// The portal exposes this as two independent choices, Layout x Flip:
///   ORIENT_0   = Horizontal, upright
///   ORIENT_180 = Horizontal, inverted
///   ORIENT_90  = Vertical,   upright
///   ORIENT_270 = Vertical,   inverted
pub const ORIENT_0: u8 = 0;
pub const ORIENT_90: u8 = 1;
pub const ORIENT_180: u8 = 2;
pub const ORIENT_270: u8 = 3;
pub const ORIENT_DEFAULT: u8 = ORIENT_0;

/// Marquee scroll direction. Which values are valid depends on orientation:
/// horizontal mounts (0/180) allow the left/right pair, vertical mounts
/// (90/270) allow the up/down pair. `clamp_direction` enforces this.
pub const DIR_LEFT_TO_RIGHT: u8 = 0;
pub const DIR_RIGHT_TO_LEFT: u8 = 1;
pub const DIR_TOP_TO_BOTTOM: u8 = 2;
pub const DIR_BOTTOM_TO_TOP: u8 = 3;
pub const DIR_DEFAULT: u8 = DIR_RIGHT_TO_LEFT; // scrolls text leftward, classic marquee feel

/// True when the given orientation places the display in a vertical mount.
pub fn orientation_is_vertical(orientation: u8) -> bool {
    orientation == ORIENT_90 || orientation == ORIENT_270
}

/// Clamp a raw millisecond delay into the supported range.
pub fn clamp_speed_ms(ms: u16) -> u16 {
    ms.clamp(MARQUEE_SPEED_MS_MIN, MARQUEE_SPEED_MS_MAX)
}

/// Decompose an orientation code into (vertical, inverted) for the portal.
#[cfg(any(target_os = "espidf", test))]
pub fn orientation_parts(orientation: u8) -> (bool, bool) {
    match orientation {
        ORIENT_180 => (false, true),
        ORIENT_90 => (true, false),
        ORIENT_270 => (true, true),
        _ => (false, false),
    }
}

/// Clamp a stored/received direction to one that is valid for the given
/// orientation. If the direction belongs to the wrong axis, fall back to a
/// sane default for that axis. This guarantees the renderer never sees an
/// impossible orientation/direction pair regardless of what NVS or a crafted
/// POST contains.
pub fn clamp_direction(orientation: u8, direction: u8) -> u8 {
    if orientation_is_vertical(orientation) {
        match direction {
            DIR_TOP_TO_BOTTOM | DIR_BOTTOM_TO_TOP => direction,
            _ => DIR_TOP_TO_BOTTOM,
        }
    } else {
        match direction {
            DIR_LEFT_TO_RIGHT | DIR_RIGHT_TO_LEFT => direction,
            _ => DIR_RIGHT_TO_LEFT,
        }
    }
}

// --------------------------------------------------------
// WIFI CONNECTION TEST RESULT (RAM ONLY)
// --------------------------------------------------------
// The captive portal's test-in-place flow records its outcome here so the
// page can display success/failure after the user's device rejoins the AP.
// This is deliberately never persisted to NVS.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WifiTestResult {
    /// No test has been run since boot.
    Idle,
    /// A test is currently in progress (AP may be briefly unavailable).
    Testing,
    /// The most recent test associated successfully.
    Success,
    /// The most recent test failed to associate before timeout.
    Failed,
}

/// A snapshot of the live, user-configurable display settings. Returned by
/// `DisplayDriver::marquee_config` so callers read a consistent set in one lock.
#[derive(Clone)]
pub struct MarqueeConfig {
    pub text: String,
    pub speed_ms: u16,
    pub orientation: u8,
    pub direction: u8,
    pub panel_margin: u8,
    pub margin_applies_streams: bool,
}

pub struct DisplayState {
    status_color: RGB8,
    image: [u8; 300],

    // runtime marquee configuration (live-previewable, seeded from NVS)
    marquee_text: String,
    marquee_speed_ms: u16, // objective delay between scroll steps
    orientation: u8,       // ORIENT_*
    direction: u8,         // DIR_*

    // physical panel gap modeling
    panel_margin: u8,             // dead units between adjacent panels
    margin_applies_streams: bool, // whether DDP frames also account for gaps

    // last wifi connection test outcome (RAM only, shown in portal)
    wifi_test: WifiTestResult,
}

#[derive(Clone)]
pub struct DisplayDriver {
    state: Arc<Mutex<DisplayState>>,
}

impl DisplayState {
    fn new_default() -> Self {
        DisplayState {
            status_color: RGB8::default(),
            image: [0; 300],
            marquee_text: MARQUEE_TEXT_DEFAULT.to_string(),
            marquee_speed_ms: MARQUEE_SPEED_MS_DEFAULT,
            orientation: ORIENT_DEFAULT,
            direction: DIR_DEFAULT,
            panel_margin: PANEL_MARGIN_DEFAULT,
            margin_applies_streams: PANEL_MARGIN_STREAMS_DEFAULT,
            wifi_test: WifiTestResult::Idle,
        }
    }
}

impl DisplayDriver {
    // --------------------------------------------------------
    // HARDWARE INITIALIZER (ESP32 ONLY)
    // --------------------------------------------------------
    #[cfg(target_os = "espidf")]
    pub fn new<W>(mut leds: W) -> Self
    where
        W: SmartLedsWrite<Color = RGB8> + Send + 'static,
        W::Error: std::fmt::Debug,
    {
        let state = Arc::new(Mutex::new(DisplayState::new_default()));

        let state_clone = state.clone();

        #[cfg(target_os = "espidf")]
        ThreadSpawnConfiguration {
            pin_to_core: Some(Core::Core1),
            priority: 15,
            ..Default::default()
        }
        .set()
        .ok();

        std::thread::spawn(move || {
            let mut tick: usize = 0;
            let brightnesses: [u8; 8] = [12, 18, 24, 31, 42, 56, 75, 100];

            let mut pixels = [RGB8::default(); STRIP_LEN];

            loop {
                let (status_color, image) = {
                    let lock = state_clone.lock().unwrap();
                    (lock.status_color, lock.image)
                };

                // Sliding gradient over the sacrificial LEDs. With a run of
                // them this reads as a chase; with a single LED the phase still
                // walks the whole brightness table, so it breathes instead.
                let steps = brightnesses.len();
                for i in 0..SACRIFICIAL_LEDS {
                    let b = brightnesses[(steps + i - (tick % steps)) % steps];
                    pixels[i] = Self::scale_color(status_color, b);
                }

                // map 30x10 grayscale logical image to physical serpentine grid
                for y in 0..10 {
                    for x in 0..30 {
                        if let Some(idx) = Self::get_pixel_index(x, y) {
                            let val = image[y * 30 + x];
                            // offset past the sacrificial head of the string
                            pixels[SACRIFICIAL_LEDS + idx] = RGB8::new(val, val, val);
                        }
                    }
                }

                // write mapped array to physical strip
                if let Err(e) = leds.write(pixels.iter().cloned()) {
                    println!("> led strip write error: {:?}", e);
                }

                tick = tick.wrapping_add(1);
                FreeRtos::delay_ms(33);
            }
        });

        Self { state }
    }

    // --------------------------------------------------------
    // SIMULATOR INITIALIZER (*NIX ONLY)
    // --------------------------------------------------------
    #[cfg(not(target_os = "espidf"))]
    pub fn new_simulated() -> Self {
        Self {
            state: Arc::new(Mutex::new(DisplayState::new_default())),
        }
    }

    pub fn set_status_color(&self, color: RGB8) {
        if let Ok(mut lock) = self.state.lock() {
            lock.status_color = color;
        }
    }

    pub fn set_image(&self, image: &[u8; 300]) {
        if let Ok(mut lock) = self.state.lock() {
            #[cfg(feature = "console-sim")]
            if lock.image != *image {
                Self::print_sim(image);
            }
            lock.image.copy_from_slice(image);
        }
    }

    // --------------------------------------------------------
    // MARQUEE CONFIG ACCESSORS
    // --------------------------------------------------------
    // These let main.rs seed the config from NVS at startup and let the
    // captive portal push live-preview updates without touching NVS or the
    // render pipeline directly.

    /// Snapshot the full live config in one lock acquisition. Direction is
    /// guaranteed valid for the orientation.
    pub fn marquee_config(&self) -> MarqueeConfig {
        match self.state.lock() {
            Ok(lock) => MarqueeConfig {
                text: lock.marquee_text.clone(),
                speed_ms: lock.marquee_speed_ms,
                orientation: lock.orientation,
                direction: clamp_direction(lock.orientation, lock.direction),
                panel_margin: lock.panel_margin,
                margin_applies_streams: lock.margin_applies_streams,
            },
            Err(_) => MarqueeConfig {
                text: MARQUEE_TEXT_DEFAULT.to_string(),
                speed_ms: MARQUEE_SPEED_MS_DEFAULT,
                orientation: ORIENT_DEFAULT,
                direction: DIR_DEFAULT,
                panel_margin: PANEL_MARGIN_DEFAULT,
                margin_applies_streams: PANEL_MARGIN_STREAMS_DEFAULT,
            },
        }
    }

    /// Overwrite the marquee text, truncated to MARQUEE_TEXT_MAX characters.
    pub fn set_marquee_text(&self, text: &str) {
        if let Ok(mut lock) = self.state.lock() {
            lock.marquee_text = truncate_chars(text, MARQUEE_TEXT_MAX);
        }
    }

    /// Set the marquee speed as a millisecond delay, clamped to the valid range.
    pub fn set_marquee_speed_ms(&self, ms: u16) {
        if let Ok(mut lock) = self.state.lock() {
            lock.marquee_speed_ms = clamp_speed_ms(ms);
        }
    }

    /// Set the panel margin (dead units between panels), clamped to the max.
    pub fn set_panel_margin(&self, margin: u8) {
        if let Ok(mut lock) = self.state.lock() {
            lock.panel_margin = margin.min(PANEL_MARGIN_MAX);
        }
    }

    /// Set whether the panel margin also applies to incoming DDP streams.
    pub fn set_margin_applies_streams(&self, applies: bool) {
        if let Ok(mut lock) = self.state.lock() {
            lock.margin_applies_streams = applies;
        }
    }

    /// Set orientation and (re)clamp the stored direction to remain valid.
    pub fn set_orientation(&self, orientation: u8) {
        if let Ok(mut lock) = self.state.lock() {
            let orientation = match orientation {
                ORIENT_0 | ORIENT_90 | ORIENT_180 | ORIENT_270 => orientation,
                _ => ORIENT_DEFAULT,
            };
            lock.orientation = orientation;
            lock.direction = clamp_direction(orientation, lock.direction);
        }
    }

    /// Set the scroll direction, clamped to the current orientation's axis.
    pub fn set_direction(&self, direction: u8) {
        if let Ok(mut lock) = self.state.lock() {
            lock.direction = clamp_direction(lock.orientation, direction);
        }
    }

    // --------------------------------------------------------
    // WIFI TEST RESULT ACCESSORS (RAM ONLY)
    // --------------------------------------------------------
    pub fn set_wifi_test(&self, result: WifiTestResult) {
        if let Ok(mut lock) = self.state.lock() {
            lock.wifi_test = result;
        }
    }

    pub fn wifi_test(&self) -> WifiTestResult {
        match self.state.lock() {
            Ok(lock) => lock.wifi_test,
            Err(_) => WifiTestResult::Idle,
        }
    }

    fn scale_color(color: RGB8, percent: u8) -> RGB8 {
        RGB8::new(
            ((color.r as u16 * percent as u16) / 100) as u8,
            ((color.g as u16 * percent as u16) / 100) as u8,
            ((color.b as u16 * percent as u16) / 100) as u8,
        )
    }

    #[cfg(any(target_os = "espidf", test))]
    /// Maps a logical pixel of the 30x10 image to its index on the matrix
    /// part of the string (0..299, before the sacrificial LEDs are added).
    ///
    /// Physical layout, seen from the front with the red lamp on the left and
    /// the blue lamp on the right (measured with the camera, 2026-09-20):
    /// every panel is ten vertical serpentine columns of ten LEDs. The data
    /// line enters the string at the blue lamp and runs left-to-right across
    /// it, then left-to-right across amber, then across red. Physical column
    /// 0 is therefore the left edge of the blue lamp and column 29 the right
    /// edge of the red lamp. Even columns are wired bottom-to-top, odd
    /// columns top-to-bottom.
    fn get_pixel_index(lx: usize, ly: usize) -> Option<usize> {
        if lx >= 30 || ly >= 10 {
            return None;
        }
        // the string visits the logical panels in reverse order (2, 1, 0)
        // but runs in the logical x direction within each one
        let panel = lx / 10;
        let c = (2 - panel) * 10 + lx % 10;
        // serpentine: even columns start at the bottom, odd ones at the top
        let offset = if c % 2 == 0 { 9 - ly } else { ly };
        Some(c * 10 + offset)
    }

    #[cfg(feature = "console-sim")]
    fn print_sim(image: &[u8; 300]) {
        let mut out = String::with_capacity(30 * 10 * 3 + 100);
        out.push_str("\n+------------------------------------------------------------+\n");
        for y in 0..10 {
            out.push('|');
            for x in 0..30 {
                let val = image[y * 30 + x];
                let c = match val {
                    0..=5 => "  ",
                    6..=25 => "¦¦",
                    26..=50 => "¦¦",
                    51..=75 => "¦¦",
                    _ => "¦¦",
                };
                out.push_str(c);
            }
            out.push_str("|\n");
        }
        out.push_str("+------------------------------------------------------------+\n");
        print!("{}", out);
    }
}

/// Truncate a string to at most `max` characters (not bytes), preserving
/// whole UTF-8 characters. Used to bound marquee text everywhere it enters.
pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::DisplayDriver;

    fn idx(lx: usize, ly: usize) -> usize {
        DisplayDriver::get_pixel_index(lx, ly).unwrap()
    }

    #[test]
    fn corners_land_where_the_camera_saw_them() {
        // red lamp (logical panel 0) is the tail of the string: columns 20..29
        assert_eq!(
            idx(0, 0),
            209,
            "red top-left: column 20 (even, bottom-up), top LED"
        );
        assert_eq!(idx(0, 9), 200, "red bottom-left");
        assert_eq!(
            idx(9, 0),
            290,
            "red top-right: column 29 (odd, top-down), first LED"
        );
        assert_eq!(idx(9, 9), 299, "red bottom-right");
        // amber lamp (panel 1): columns 10..19
        assert_eq!(idx(10, 0), 109, "amber top-left");
        assert_eq!(idx(19, 9), 199, "amber bottom-right");
        // blue lamp (panel 2) is the head of the string: columns 0..9
        assert_eq!(
            idx(20, 0),
            9,
            "blue top-left is the top of the first column"
        );
        assert_eq!(
            idx(20, 9),
            0,
            "blue bottom-left is the very first matrix LED"
        );
        assert_eq!(idx(29, 0), 90, "blue top-right");
        assert_eq!(idx(29, 9), 99, "blue bottom-right");
    }

    #[test]
    fn every_panel_is_a_contiguous_block_with_x_running_left_to_right() {
        for panel in 0..3 {
            let base = (2 - panel) * 100;
            for lx in panel * 10..(panel + 1) * 10 {
                for ly in 0..10 {
                    let i = idx(lx, ly);
                    assert!(
                        i >= base && i < base + 100,
                        "({lx},{ly}) -> {i} outside panel block"
                    );
                    assert_eq!(i / 10, base / 10 + lx % 10, "column of ({lx},{ly})");
                }
            }
        }
    }

    #[test]
    fn rows_are_horizontal_and_y_runs_top_to_bottom() {
        // moving one LED down a column must move one step along the string in
        // the direction that column is wired
        for lx in 0..30 {
            for ly in 0..9 {
                let step = idx(lx, ly + 1) as isize - idx(lx, ly) as isize;
                let expected = if (idx(lx, ly) / 10) % 2 == 0 { -1 } else { 1 };
                assert_eq!(step, expected, "({lx},{ly})");
            }
        }
    }

    #[test]
    fn mapping_is_a_bijection_over_the_matrix() {
        let mut seen = [false; 300];
        for ly in 0..10 {
            for lx in 0..30 {
                let i = idx(lx, ly);
                assert!(!seen[i], "index {i} hit twice");
                seen[i] = true;
            }
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn out_of_range_is_none() {
        assert_eq!(DisplayDriver::get_pixel_index(30, 0), None);
        assert_eq!(DisplayDriver::get_pixel_index(0, 10), None);
    }
}
