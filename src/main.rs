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
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

fn main() {
    esp_idf_svc::sys::link_patches();

    println!("> hello, world");

    // set up LED strip -- pin D13 on the ESP32
    let peripherals = Peripherals::take().unwrap();
    let led_pin = peripherals.pins.gpio13;
    let channel = peripherals.rmt.channel0;
    let mut ws2812 = Ws2812Esp32Rmt::new(channel, led_pin).unwrap();

    // set up non-volatile storage on the ESP32
    let nvs_partition = EspDefaultNvsPartition::take().unwrap();
    let sysloop = EspSystemEventLoop::take().unwrap();

    // set up the BOOT button on the ESP32
    let mut boot_btn = PinDriver::input(peripherals.pins.gpio0).unwrap();
    boot_btn.set_pull(Pull::Up).unwrap();

    // if the BOOT button is pressed for 5+ seconds, request wireless setup
    let nvs_part_clone = nvs_partition.clone();
    std::thread::spawn(move || {
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
        RGB8::new(50, 0, 0),   // red
        RGB8::new(0, 50, 0),   // green
        RGB8::new(0, 0, 50),   // blue
        RGB8::new(50, 50, 0),  // yellow
        RGB8::new(50, 0, 50),  // magenta
        RGB8::new(0, 50, 50),  // cyan
        RGB8::new(50, 50, 50), // white
    ];

    let mut pixels = [RGB8::default(); 199];

    // enter wireless setup if requested or if we're missing the wireless config
    if wap_mode == 1 || ssid.is_none() {
        println!("> activating access point");
        for i in 0..8 {
            pixels[i] = RGB8::new(0, 0, 50);
        }
        for i in 8..199 {
            pixels[i] = RGB8::new(100, 100, 100);
        }
        ws2812.write(pixels.iter().cloned()).unwrap();
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
            for i in 0..8 {
                pixels[i] = RGB8::new(50, 0, 0);
            }
            ws2812.write(pixels.iter().cloned()).unwrap();
            run_ap_mode(&mut wifi, nvs_partition.clone());
        } else {
            println!("> connected successfully");
            for i in 0..199 {
                pixels[i] = RGB8::new(0, 50, 0);
            }
            ws2812.write(pixels.iter().cloned()).unwrap();
            FreeRtos::delay_ms(2000);
        }
    }

    println!("> launching animation");

    for i in 0..199 {
        pixels[i] = RGB8::new(100, 100, 100); // bright white
        ws2812.write(pixels.iter().cloned()).unwrap();
    }

    // main animation loop
    loop {
        // for each of the first 8 LEDs
        for i in 0..8 {
            // for each color
            for color in colors.iter() {
                // shut off all LEDs
                for j in 0..8 {
                    pixels[j] = RGB8::default();
                }
                // activate ONLY the current LED
                pixels[i] = *color;
                // push the data array to the physical strip
                ws2812.write(pixels.iter().cloned()).unwrap();
                // wait 142 ms per color (about 1 second per LED)
                FreeRtos::delay_ms(142);
            }

            // turn off current LED before outer loop moves to next one
            pixels[i] = RGB8::default();
            ws2812.write(pixels.iter().cloned()).unwrap();
        }
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
