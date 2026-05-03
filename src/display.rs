#[cfg(target_os = "espidf")]
use esp_idf_hal::delay::FreeRtos;
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
                FreeRtos::delay_ms(125);
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

    fn get_pixel_index(lx: usize, ly: usize) -> Option<usize> {
        // translate logical coordinates to the physical matrix orientation
        let (px, py) = match lx {
            0..=9 => (lx, ly),        // panel 0: normal
            10..=19 => (29 - lx, ly), // panel 1: mirrored horizontally
            20..=29 => (49 - lx, ly), // panel 2: rotated by 180 degrees
            _ => return None,
        };

        // transform x-coordinates where x = 29 is the first physical column (c=0)
        let c = 29 - px;

        // serpentine vertical mapping
        let offset = if c % 2 == 0 { py } else { 9 - py };
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
        out.push_str("\n+------------------------------------------------------------+\n");
        print!("{}", out);
    }
}
