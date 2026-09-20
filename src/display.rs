#[cfg(target_os = "espidf")]
use esp_idf_hal::delay::FreeRtos;
#[cfg(target_os = "espidf")]
use esp_idf_hal::{cpu::Core, task::thread::ThreadSpawnConfiguration};
#[cfg(not(target_os = "espidf"))]
use smart_leds::RGB8;
#[cfg(target_os = "espidf")]
use smart_leds::{SmartLedsWrite, RGB8};
use std::sync::{Arc, Mutex};

pub struct DisplayState {
    status_color: RGB8,
    image: [u8; 300],
}

#[derive(Clone)]
pub struct DisplayDriver {
    state: Arc<Mutex<DisplayState>>,
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
        let state = Arc::new(Mutex::new(DisplayState {
            status_color: RGB8::default(),
            image: [0; 300],
        }));

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

            let mut pixels = [RGB8::default(); 308];

            loop {
                let (status_color, image) = {
                    let lock = state_clone.lock().unwrap();
                    (lock.status_color, lock.image)
                };

                // sliding gradient on status LEDs
                for i in 0..8 {
                    let b = brightnesses[(8 + i - (tick % 8)) % 8];
                    pixels[i] = Self::scale_color(status_color, b);
                }

                // map 30x10 grayscale logical image to physical serpentine grid
                for y in 0..10 {
                    for x in 0..30 {
                        if let Some(idx) = Self::get_pixel_index(x, y) {
                            let val = image[y * 30 + x];
                            pixels[8 + idx] = RGB8::new(val, val, val); // shift matrix to skip the status strip
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
            state: Arc::new(Mutex::new(DisplayState {
                status_color: RGB8::default(),
                image: [0; 300],
            })),
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
                    6..=25 => "░░",
                    26..=50 => "▒▒",
                    51..=75 => "▓▓",
                    _ => "██",
                };
                out.push_str(c);
            }
            out.push_str("|\n");
        }
        out.push_str("+------------------------------------------------------------+\n");
        print!("{}", out);
    }
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
