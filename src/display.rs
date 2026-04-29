use esp_idf_hal::delay::FreeRtos;
use smart_leds::{SmartLedsWrite, RGB8};
use std::sync::{Arc, Mutex};

pub struct DisplayState {
    status_color: RGB8,
    image: [u8; 192],
}

#[derive(Clone)]
pub struct DisplayDriver {
    state: Arc<Mutex<DisplayState>>,
}

impl DisplayDriver {
    pub fn new<W>(mut ws2812: W) -> Self
    where
        W: SmartLedsWrite<Color = RGB8> + Send + 'static,
        W::Error: std::fmt::Debug,
    {
        let state = Arc::new(Mutex::new(DisplayState {
            status_color: RGB8::default(),
            image: [0; 192],
        }));

        let state_clone = state.clone();

        std::thread::spawn(move || {
            let mut tick: usize = 0;
            let brightnesses: [u8; 8] = [12, 18, 24, 31, 42, 56, 75, 100];

            let mut pixels = [RGB8::default(); 199];

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

                // map 24x8 grayscale logical image to physical serpentine grid
                for y in 0..8 {
                    for x in 0..24 {
                        if let Some(idx) = Self::get_pixel_index(x, y) {
                            let val = image[y * 24 + x];
                            pixels[idx] = RGB8::new(val, val, val);
                        }
                    }
                }

                // write mapped array to physical strip
                let _ = ws2812.write(pixels.iter().cloned());

                tick = tick.wrapping_add(1);
                FreeRtos::delay_ms(125);
            }
        });

        Self { state }
    }

    pub fn set_status_color(&self, color: RGB8) {
        if let Ok(mut lock) = self.state.lock() {
            lock.status_color = color;
        }
    }

    pub fn set_image(&self, image: &[u8; 192]) {
        if let Ok(mut lock) = self.state.lock() {
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

    fn get_pixel_index(x: usize, y: usize) -> Option<usize> {
        // transform x-coordinates where x=23 is the first physical column (c=0)
        let c = 23 - x;

        // first physical column on the right (missing top pixel)
        if c == 0 {
            if y == 0 {
                return None; // missing pixel
            }
            return Some(8 + (y - 1));
        }

        // remaining columns
        let base = 15 + (c - 1) * 8;
        let offset = if c % 2 == 0 { y } else { 7 - y }; // serpentine

        Some(base + offset)
    }
}
