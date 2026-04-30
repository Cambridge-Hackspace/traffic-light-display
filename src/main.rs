mod display;

use display::DisplayDriver;
use embedded_svc::http::Headers;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::{PinDriver, Pull};
use esp_idf_hal::io::{Read, Write};
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::http::server::{Configuration as HttpConfiguration, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use esp_idf_svc::wifi::{
    AccessPointConfiguration, ClientConfiguration, Configuration as WifiConfiguration, EspWifi,
};
use smart_leds::{SmartLedsWrite, RGB8};
use std::net::UdpSocket;
use std::time::{Duration, Instant};
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

fn main() {
    esp_idf_svc::sys::link_patches();

    println!("> hello, world");

    // set up LED strip -- pin D13 on the ESP32
    let peripherals = Peripherals::take().unwrap();
    let led_pin = peripherals.pins.gpio13;
    let channel = peripherals.rmt.channel0;
    let mut ws2812 = Ws2812Esp32Rmt::new(channel, led_pin).unwrap();

    // boot test
    let pixels = [RGB8::new(50, 0, 0); 20];
    match ws2812.write(pixels.iter().cloned()) {
        Ok(_) => println!("> signal sent successfully"),
        Err(e) => println!("> ws2812 write error: {:?}", e),
    }
    FreeRtos::delay_ms(3000);

    // WS2811 string -- pin D12 on the ESP32
    let string_pin = peripherals.pins.gpio12;
    let string_channel = peripherals.rmt.channel1;
    let mut ws2811 = Ws2812Esp32Rmt::new(string_channel, string_pin).unwrap();

    let _ = std::thread::Builder::new().stack_size(4096).spawn(move || {
        let mut toggle = false;
        loop {
            let color = if toggle {
                RGB8::new(100, 100, 100)
            } else {
                RGB8::new(0, 0, 0)
            };
            let pixels = [color; 100];
            let _ = ws2811.write(pixels.iter().cloned());
            toggle = !toggle;
            FreeRtos::delay_ms(500);
        }
    });

    // set up non-volatile storage on the ESP32
    let nvs_partition = EspDefaultNvsPartition::take().unwrap();
    let sysloop = EspSystemEventLoop::take().unwrap();

    // set up the BOOT button on the ESP32
    let mut boot_btn = PinDriver::input(peripherals.pins.gpio0).unwrap();
    boot_btn.set_pull(Pull::Up).unwrap();

    // if the BOOT button is pressed for 5+ seconds, request wireless setup
    let nvs_part_clone = nvs_partition.clone();
    let _ = std::thread::Builder::new().stack_size(8192).spawn(move || {
        let mut pressed_time = 0;
        loop {
            if boot_btn.is_low() {
                pressed_time += 1;
                if pressed_time >= 50 {
                    if let Ok(nvs) = EspNvs::new(nvs_part_clone.clone(), "wifi_cfg", true) {
                        let _ = nvs.set_u8("wap_mode", 1);
                    }
                    println!("> reboot into wireless setup");
                    unsafe {
                        esp_idf_svc::sys::esp_restart();
                    }
                }
            } else {
                pressed_time = 0;
            }
            FreeRtos::delay_ms(100);
        }
    });

    // wifi capabilities
    let mut wifi = EspWifi::new(
        peripherals.modem,
        sysloop.clone(),
        Some(nvs_partition.clone()),
    )
    .unwrap();

    let nvs = EspNvs::new(nvs_partition.clone(), "wifi_cfg", true).unwrap();

    // check if we booted with a wireless setup request
    let wap_mode = nvs.get_u8("wap_mode").unwrap_or(Some(0)).unwrap_or(0);

    // wifi config
    let mut ssid_buf = [0u8; 64];
    let mut pass_buf = [0u8; 64];
    let ssid = nvs
        .get_str("ssid", &mut ssid_buf)
        .unwrap_or(None)
        .map(|s| s.to_string());
    let pass = nvs
        .get_str("pass", &mut pass_buf)
        .unwrap_or(None)
        .map(|s| s.to_string());

    let colors = [
        RGB8::new(195, 78, 75),  // red
        RGB8::new(61, 132, 175), // blue
        RGB8::new(216, 163, 0),  // yellow
        RGB8::new(137, 177, 8),  // green
    ];

    // display driver
    let display = DisplayDriver::new(ws2812);

    // enter wireless setup if requested or if we're missing the wireless config
    if wap_mode == 1 || ssid.is_none() {
        println!("> activating access point");
        display.set_status_color(RGB8::new(0, 0, 50));
        display.set_image(&[100; 192]);
        run_ap_mode(&mut wifi, nvs_partition.clone());
    } else {
        // connect to the wifi
        let s = ssid.unwrap();
        let p = pass.unwrap_or_default();

        wifi.set_configuration(&WifiConfiguration::Client(ClientConfiguration {
            ssid: s.as_str().try_into().unwrap(),
            password: p.as_str().try_into().unwrap(),
            ..Default::default()
        }))
        .unwrap();

        println!("> attempting to connect to wifi");

        wifi.start().unwrap();
        wifi.connect().unwrap();

        let mut connected = false;
        for _ in 0..100 {
            // 10 second timeout
            if wifi.is_connected().unwrap_or(false) {
                connected = true;
                break;
            }
            FreeRtos::delay_ms(100);
        }

        if !connected {
            println!("> failed to connect; re-entering ap mode");
            display.set_status_color(RGB8::new(50, 0, 0));
            display.set_image(&[100; 192]);
            run_ap_mode(&mut wifi, nvs_partition.clone());
        } else {
            println!("> connected successfully");
            display.set_status_color(RGB8::new(0, 50, 0));
            display.set_image(&[100; 192]);
            FreeRtos::delay_ms(2000);
        }
    }

    println!("> listening for DDP on UDP via port 4048");

    let socket = UdpSocket::bind("0.0.0.0:4048").unwrap();
    socket.set_nonblocking(true).unwrap();

    let mut buf = [0u8; 1500];
    let timeout = Duration::from_secs(5);
    let mut last_packet = Instant::now() - timeout;
    let mut marquee_offset = 0;
    let mut last_marquee_update = Instant::now();
    let marquee_text = "CAMBRIDGE HACKSPACE :)     ";

    // main animation loop
    let mut color_idx = 0;
    let mut last_color_update = Instant::now();

    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, _src)) => {
                // DDP: 10 byte header + 192 byte payload
                if len >= 202 {
                    let mut img = [0u8; 192];
                    img.copy_from_slice(&buf[10..202]);
                    display.set_image(&img);
                    last_packet = Instant::now();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {} // expected on non-blocking
            Err(_) => {}                                               // expected on read timeout
        }

        let now = Instant::now();

        if now.duration_since(last_packet) > timeout {
            if now.duration_since(last_marquee_update) > Duration::from_millis(100) {
                let mut img = [0u8; 192];
                render_marquee(marquee_text, marquee_offset, &mut img);
                display.set_image(&img);
                marquee_offset = (marquee_offset + 1) % (marquee_text.len() * 6);
                last_marquee_update = now;
            }
        }

        if now.duration_since(last_color_update) > Duration::from_millis(1000) {
            display.set_status_color(colors[color_idx]);
            color_idx = (color_idx + 1) % colors.len();
            last_color_update = now;
        }

        FreeRtos::delay_ms(10);
    }
}

fn run_ap_mode(wifi: &mut EspWifi, nvs_partition: EspDefaultNvsPartition) {
    println!("> scanning for wifi networks...");

    // temporarily switch to client mode
    let _ = wifi.stop();
    wifi.set_configuration(&WifiConfiguration::Client(ClientConfiguration::default()))
        .unwrap();
    wifi.start().unwrap();
    FreeRtos::delay_ms(2000);

    let mut ssids = Vec::new();
    match wifi.scan() {
        Ok(scans) => {
            for ap in scans {
                let ssid = ap.ssid.to_string();
                if !ssid.is_empty() && !ssids.contains(&ssid) {
                    ssids.push(ssid);
                }
            }
        }
        Err(e) => {
            println!("> wifi scan failed: {:?}", e);
        }
    }
    ssids.sort();

    let mut options = String::new();
    if ssids.is_empty() {
        options.push_str("<option value=\"\" disabled selected>no networks found</option>\n");
    } else {
        options.push_str("<option value=\"\" disabled selected>select a network...</option>\n");
        for ssid in ssids {
            let safe_ssid = ssid
                .replace("&", "&amp;")
                .replace("<", "&lt;")
                .replace(">", "&gt;")
                .replace("\"", "&quot;");
            options.push_str(&format!("<option value=\"{0}\">{0}</option>\n", safe_ssid));
        }
    }

    // pivot back to access point mode to host the server
    let _ = wifi.stop();
    wifi.set_configuration(&WifiConfiguration::AccessPoint(AccessPointConfiguration {
        ssid: "Traffic-Light".try_into().unwrap(),
        channel: 1,
        ..Default::default()
    }))
    .unwrap();
    let _ = wifi.start();
    wifi.start().unwrap();
    FreeRtos::delay_ms(1000);

    let server_config = HttpConfiguration::default();
    let mut server = EspHttpServer::new(&server_config).unwrap();

    server.fn_handler("/", Method::Get, move |req| {
      let html = format!(r#"<!DOCTYPE html>
<html>
  <head>
    <title>Traffic Light Setup</title>
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <style>
      body {{
        font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
        background-color: #f4f4f9;
        display: flex;
        justify-content: center;
        align-items: center;
        height: 100vh;
        margin: 0;
      }}
      .container {{
        background: white;
        padding: 30px;
        border-radius: 10px;
        box-shadow: 0 4px 6px rgba(0, 0, 0, 0.1);
        width: 100%;
        max-width: 400px;
        box-sizing: border-box;
        text-align: center;
      }}
      h2 {{
        margin-top: 0;
        color: #333;
        margin-bottom:
        20px
      }}
      .form-group {{
        text-align: left;
        margin-bottom: 15px;
      }}
      label {{
        display: block;
        margin-bottom: 5px;
        color: #666;
        font-weight: bold;
      }}
      select, input[type="password"] {{
        width: 100%;
        padding: 10px;
        border: 1px solid #ccc;
        border-radius: 5px;
        box-sizing: border-box;
        font-size: 16px;
      }}
      input[type="submit"] {{
        background-color: #007bff;
        color: white;
        border: none;
        padding: 12px 20px;
        font-size: 16px;
        border-radius: 5px;
        cursor: pointer;
        width: 100%;
        margin-top: 10px;
        transition: background-color 0.3s;
      }}
      input[type="submit"]:hover {{
        background-color: #0056b3;
      }}
    </style>
    <script>
      document.cookie.split(";").forEach(function(c) {{
        document.cookie = c.replace(/^ +/, "").replace(/=.*/, "=;expires=" + new Date().toUTCString() + ";path=/");
      }})
    </script>
  </head>
  <body>
    <div class="container">
      <h2>Traffic Light - Wireless Setup</h2>
      <form action="/save" method="post">
        <div class="form-group">
          <label>SSID:</label>
          <select name="ssid" required>
            {}
          </select>
        </div>
        <div class="form-group">
          <label>Password:</label>
          <input type="password" name="password" placeholder="Leave blank if open">
        </div>
        <input type="submit" value="Connect" />
      </form>
    </div>
  </body>
</html>"#, options);
        let mut res = req.into_ok_response().unwrap();
        res.write_all(html.as_bytes()).unwrap();
        Ok::<(), std::io::Error>(())
    }).unwrap();

    server
        .fn_handler("/save", Method::Post, move |mut req| {
            let len = req.content_len().unwrap_or(0) as usize;
            let mut buf = vec![0; len];
            if len > 0 {
                req.read_exact(&mut buf).unwrap_or(());
            }
            let body = String::from_utf8_lossy(&buf);

            let mut ssid = String::new();
            let mut pass = String::new();

            for pair in body.split('&') {
                let mut kv = pair.split('=');
                if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
                    if k == "ssid" {
                        ssid = urldecode(v);
                    }
                    if k == "password" {
                        pass = urldecode(v);
                    }
                }
            }

            let mut nvs = EspNvs::new(nvs_partition.clone(), "wifi_cfg", true).unwrap();
            nvs.set_str("ssid", &ssid).unwrap();
            nvs.set_str("pass", &pass).unwrap();
            nvs.set_u8("wap_mode", 0).unwrap();

            let mut res = req.into_ok_response().unwrap();
            res.write_all(b"credentials saved; rebooting").unwrap();

            std::thread::spawn(|| {
                FreeRtos::delay_ms(2000);
                unsafe {
                    esp_idf_svc::sys::esp_restart();
                }
            });

            Ok::<(), std::io::Error>(())
        })
        .unwrap();

    println!(
        "> ready for wireless setup: connect to 'Traffic-Light' and visit http://192.168.71.1"
    );
    loop {
        FreeRtos::delay_ms(1000);
    }
}

fn urldecode(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '+' {
            out.push(' ');
        } else if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(b) = u8::from_str_radix(&hex, 16) {
                    out.push(b as char);
                    continue;
                }
            }
            out.push('%');
            out.push_str(&hex);
        } else {
            out.push(c);
        }
    }
    out
}

fn render_marquee(text: &str, offset: usize, img: &mut [u8; 192]) {
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return;
    }

    let total_width = len * 6;
    for x in 0..24 {
        let text_x = (x + offset) % total_width;
        let char_idx = text_x / 6;
        let pixel_x = text_x % 6;

        if pixel_x < 5 {
            let mut c = bytes[char_idx];
            if c >= 97 && c <= 122 {
                c -= 32; // basic uppercase conversion
            }
            if c < 32 || c >= 97 {
                c = 32;
            }

            let font_idx = ((c - 32) as usize) * 5 + pixel_x;
            let col_data = FONT[font_idx];

            for y in 0..7 {
                if (col_data & (1 << y)) != 0 {
                    img[y * 24 + x] = 100; // max brightness
                }
            }
        }
    }
}

const FONT: [u8; 325] = [
    0x00, 0x00, 0x00, 0x00, 0x00, // 32 space
    0x00, 0x00, 0x4f, 0x00, 0x00, // 33 !
    0x00, 0x07, 0x00, 0x07, 0x00, // 34 "
    0x14, 0x7f, 0x14, 0x7f, 0x14, // 35 #
    0x24, 0x2a, 0x7f, 0x2a, 0x12, // 36 $
    0x23, 0x13, 0x08, 0x64, 0x62, // 37 %
    0x36, 0x49, 0x55, 0x22, 0x50, // 38 &
    0x00, 0x05, 0x03, 0x00, 0x00, // 39 '
    0x00, 0x1c, 0x22, 0x41, 0x00, // 40 (
    0x00, 0x41, 0x22, 0x1c, 0x00, // 41 )
    0x14, 0x08, 0x3e, 0x08, 0x14, // 42 *
    0x08, 0x08, 0x3e, 0x08, 0x08, // 43 +
    0x00, 0x50, 0x30, 0x00, 0x00, // 44 ,
    0x08, 0x08, 0x08, 0x08, 0x08, // 45 -
    0x00, 0x60, 0x60, 0x00, 0x00, // 46 .
    0x20, 0x10, 0x08, 0x04, 0x02, // 47 /
    0x3e, 0x51, 0x49, 0x45, 0x3e, // 48 0
    0x00, 0x42, 0x7f, 0x40, 0x00, // 49 1
    0x42, 0x61, 0x51, 0x49, 0x46, // 50 2
    0x21, 0x41, 0x45, 0x4b, 0x31, // 51 3
    0x18, 0x14, 0x12, 0x7f, 0x10, // 52 4
    0x27, 0x45, 0x45, 0x45, 0x39, // 53 5
    0x3c, 0x4a, 0x49, 0x49, 0x30, // 54 6
    0x01, 0x71, 0x09, 0x05, 0x03, // 55 7
    0x36, 0x49, 0x49, 0x49, 0x36, // 56 8
    0x06, 0x49, 0x49, 0x29, 0x1e, // 57 9
    0x00, 0x36, 0x36, 0x00, 0x00, // 58 :
    0x00, 0x56, 0x36, 0x00, 0x00, // 59 ;
    0x08, 0x14, 0x22, 0x41, 0x00, // 60 <
    0x14, 0x14, 0x14, 0x14, 0x14, // 61 =
    0x00, 0x41, 0x22, 0x14, 0x08, // 62 >
    0x02, 0x01, 0x51, 0x09, 0x06, // 63 ?
    0x32, 0x49, 0x79, 0x41, 0x3e, // 64 @
    0x7e, 0x11, 0x11, 0x11, 0x7e, // 65 A
    0x7f, 0x49, 0x49, 0x49, 0x36, // 66 B
    0x3e, 0x41, 0x41, 0x41, 0x22, // 67 C
    0x7f, 0x41, 0x41, 0x22, 0x1c, // 68 D
    0x7f, 0x49, 0x49, 0x49, 0x41, // 69 E
    0x7f, 0x09, 0x09, 0x09, 0x01, // 70 F
    0x3e, 0x41, 0x49, 0x49, 0x7a, // 71 G
    0x7f, 0x08, 0x08, 0x08, 0x7f, // 72 H
    0x00, 0x41, 0x7f, 0x41, 0x00, // 73 I
    0x20, 0x40, 0x41, 0x3f, 0x01, // 74 J
    0x7f, 0x08, 0x14, 0x22, 0x41, // 75 K
    0x7f, 0x40, 0x40, 0x40, 0x40, // 76 L
    0x7f, 0x02, 0x0c, 0x02, 0x7f, // 77 M
    0x7f, 0x04, 0x08, 0x10, 0x7f, // 78 N
    0x3e, 0x41, 0x41, 0x41, 0x3e, // 79 O
    0x7f, 0x09, 0x09, 0x09, 0x06, // 80 P
    0x3e, 0x41, 0x51, 0x21, 0x5e, // 81 Q
    0x7f, 0x09, 0x19, 0x29, 0x46, // 82 R
    0x46, 0x49, 0x49, 0x49, 0x31, // 83 S
    0x01, 0x01, 0x7f, 0x01, 0x01, // 84 T
    0x3f, 0x40, 0x40, 0x40, 0x3f, // 85 U
    0x1f, 0x20, 0x40, 0x20, 0x1f, // 86 V
    0x3f, 0x40, 0x38, 0x40, 0x3f, // 87 W
    0x63, 0x14, 0x08, 0x14, 0x63, // 88 X
    0x07, 0x08, 0x70, 0x08, 0x07, // 89 Y
    0x61, 0x51, 0x49, 0x45, 0x43, // 90 Z
    0x00, 0x7f, 0x41, 0x41, 0x00, // 91 [
    0x02, 0x04, 0x08, 0x10, 0x20, // 92 \
    0x00, 0x41, 0x41, 0x7f, 0x00, // 93 ]
    0x04, 0x02, 0x01, 0x02, 0x04, // 94 ^
    0x40, 0x40, 0x40, 0x40, 0x40, // 95 _
    0x00, 0x01, 0x02, 0x04, 0x00, // 96 `
];
