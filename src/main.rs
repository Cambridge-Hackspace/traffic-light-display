mod display;

// Cross-platform config helpers/constants used by the animation loop and the
// marquee/orientation renderers (compiled on both the ESP32 and native sim).
use display::{
    clamp_direction, orientation_is_vertical, truncate_chars, virtual_major,
    virtual_to_physical_major, DisplayDriver, DIR_BOTTOM_TO_TOP, DIR_LEFT_TO_RIGHT,
    MARQUEE_TEXT_MAX, ORIENT_0, ORIENT_180, ORIENT_270, ORIENT_90,
};
use smart_leds::RGB8;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

// Symbols only referenced by the ESP32 captive-portal / wifi code, and by the
// host tests of the portal page renderer.
#[cfg(target_os = "espidf")]
use display::clamp_speed_ms;
#[cfg(any(target_os = "espidf", test))]
use display::{
    orientation_parts, WifiTestResult, DIR_RIGHT_TO_LEFT, DIR_TOP_TO_BOTTOM, MARQUEE_SPEED_MS_MAX,
    MARQUEE_SPEED_MS_MIN, PANEL_MARGIN_MAX,
};

#[cfg(target_os = "espidf")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "espidf")]
use {
    embedded_svc::http::server::Request,
    embedded_svc::http::Headers,
    esp_idf_hal::delay::FreeRtos,
    esp_idf_hal::gpio::{PinDriver, Pull},
    esp_idf_hal::io::{Read, Write},
    esp_idf_hal::peripherals::Peripherals,
    esp_idf_hal::rmt::{config::TransmitConfig, TxRmtDriver},
    esp_idf_svc::eventloop::EspSystemEventLoop,
    esp_idf_svc::handle::RawHandle,
    esp_idf_svc::http::server::{
        Configuration as HttpConfiguration, EspHttpConnection, EspHttpServer,
    },
    esp_idf_svc::http::Method,
    esp_idf_svc::io::EspIOError,
    esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs},
    esp_idf_svc::ota::{EspOta, SlotState},
    esp_idf_svc::wifi::{
        AccessPointConfiguration, ClientConfiguration, Configuration as WifiConfiguration, EspWifi,
    },
    ws2812_esp32_rmt_driver::Ws2812Esp32Rmt,
};

// Default UDP port for the DDP listener. Overridable via NVS ("listen_port").
const DEFAULT_LISTEN_PORT: u16 = 4048;

// How many 10-second association attempts a boot gets before it gives up and
// falls back to setup mode.
#[cfg(target_os = "espidf")]
const WIFI_CONNECT_ATTEMPTS: u32 = 3;

// Where the setup portal is being served from. It decides what a Wi-Fi
// credential change does: in setup mode the access point is torn down for a
// test-in-place and restored afterwards; once we are a client on a real
// network we only store the credentials and let the reboot try them, relying
// on the boot-time fallback into setup mode if they do not work.
pub enum PortalMode {
    Setup,
    Connected { ip: String },
}

// Credentials handed from the portal's /save handler to whoever runs the
// test-in-place (the setup-mode loop). Absent when there is no one to run it.
#[cfg(target_os = "espidf")]
type WifiTestSlot = Arc<Mutex<Option<(String, String)>>>;

// The portal credentials, shared by every handler. Behind a mutex rather than
// read fresh from NVS per request: NVS reads are flash reads, but a password
// set through the portal should still take effect immediately rather than at
// the next reboot.
#[cfg(target_os = "espidf")]
type AuthSlot = Arc<Mutex<AuthPolicy>>;

// --------------------------------------------------------
// ESP32 ENTRY POINT
// --------------------------------------------------------
#[cfg(target_os = "espidf")]
fn main() {
    esp_idf_svc::sys::link_patches();

    println!("> hello, world");
    println!(
        "> strip: {} sacrificial + {} matrix = {} leds",
        display::SACRIFICIAL_LEDS,
        display::STRIP_LEN - display::SACRIFICIAL_LEDS,
        display::STRIP_LEN
    );

    let peripherals = match Peripherals::take() {
        Ok(p) => p,
        Err(e) => {
            // Nothing we can do without peripherals; log and idle rather than
            // panic-looping the boot.
            println!("> fatal: could not take peripherals: {:?}", e);
            loop {
                FreeRtos::delay_ms(1000);
            }
        }
    };

    // set up combined LED strip (status + screen) - pin D13 on the ESP32
    let led_pin = peripherals.pins.gpio13;
    let led_channel = peripherals.rmt.channel0;
    // The WS2812 crate's default RMT config uses a single 64-symbol memory
    // block, so the RMT interrupt must refill the hardware every 32 symbols
    // (~40us). Any interrupt latency on the core (wifi driver critical
    // sections, in particular) longer than that starves the peripheral, which
    // the strip sees as a reset gap: the rest of the frame is latched into the
    // wrong LEDs and the display flickers. Channel 0 on the ESP32 can own all
    // eight blocks (512 symbols), which stretches the refill deadline to ~320us.
    const LED_RMT_MEM_BLOCKS: u8 = 8;
    let led_cfg = TransmitConfig::new()
        .clock_divider(1)
        .mem_block_num(LED_RMT_MEM_BLOCKS);
    println!(
        "> rmt: mem_block_num={} ({} symbols)",
        LED_RMT_MEM_BLOCKS,
        LED_RMT_MEM_BLOCKS as u32 * 64
    );
    let led_strip = match TxRmtDriver::new(led_channel, led_pin, &led_cfg)
        .map_err(|e| format!("{:?}", e))
        .and_then(|tx| Ws2812Esp32Rmt::new_with_rmt_driver(tx).map_err(|e| format!("{:?}", e)))
    {
        Ok(s) => s,
        Err(e) => {
            println!("> fatal: could not init led strip: {}", e);
            loop {
                FreeRtos::delay_ms(1000);
            }
        }
    };

    // display driver (spawns the render thread)
    let display = DisplayDriver::new(led_strip);

    // set up non-volatile storage on the ESP32
    let nvs_partition = match EspDefaultNvsPartition::take() {
        Ok(p) => p,
        Err(e) => {
            // Without NVS we can still run, just with defaults and no persistence.
            println!("> warning: could not take nvs partition: {:?}", e);
            display.set_status_color(RGB8::new(50, 0, 0));
            run_animation_loop(display, DEFAULT_LISTEN_PORT);
            return;
        }
    };

    // What this image has to do to earn its place, if it arrived over the air.
    // Read here rather than further down so that the bail-outs between this
    // point and the wifi connect are covered by the deadline below; anything
    // that fails before NVS is available cannot be judged at all, and stays
    // provisional so the bootloader reverts it at the next reboot.
    let ota_expect = read_ota_expectation(&nvs_partition);
    println!(
        "> ota: running from {} ({:?} to prove)",
        running_slot_label(),
        ota_expect
    );
    arm_health_deadline(ota_expect, nvs_partition.clone());

    let sysloop = match EspSystemEventLoop::take() {
        Ok(s) => s,
        Err(e) => {
            println!("> fatal: could not take system event loop: {:?}", e);
            display.set_status_color(RGB8::new(50, 0, 0));
            run_animation_loop(display, DEFAULT_LISTEN_PORT);
            return;
        }
    };

    // set up the BOOT button on the ESP32 (also wired to the external tactile
    // button in parallel). Held for 5+ seconds -> request wireless setup.
    let boot_btn = PinDriver::input(peripherals.pins.gpio0).and_then(|mut btn| {
        btn.set_pull(Pull::Up)?;
        Ok(btn)
    });

    if let Ok(boot_btn) = boot_btn {
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
                        // Holding the button is the documented way back in for
                        // someone who has lost the portal password. Whoever can
                        // reach the button can already rewrite the wifi config,
                        // so this concedes nothing that was being protected.
                        clear_auth(&nvs_part_clone);
                        println!("> reboot into wireless setup (portal password cleared)");
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
    } else {
        println!("> warning: could not init boot button; setup entry disabled");
    }

    // wifi capabilities
    let mut wifi = match EspWifi::new(
        peripherals.modem,
        sysloop.clone(),
        Some(nvs_partition.clone()),
    ) {
        Ok(w) => w,
        Err(e) => {
            // No wifi means no config portal and no streaming clients, but the
            // marquee can still run from stored/default config.
            println!("> warning: could not init wifi: {:?}", e);
            display.set_status_color(RGB8::new(50, 0, 0));
            seed_marquee_from_nvs(&display, &nvs_partition);
            {
                // Dump the effective (post-NVS) display config. Anything non-default
                // here silently transforms every DDP frame before it reaches the
                // physical pixel mapper, so it must be visible when debugging.
                let c = display.marquee_config();
                println!(
            "> cfg: orientation={} direction={} margin={} margin_streams={} speed={}ms text={:?}",
            c.orientation, c.direction, c.panel_margin, c.margin_applies_streams, c.speed_ms, c.text
        );
            }
            let port = read_listen_port(&nvs_partition);
            run_animation_loop(display, port);
            return;
        }
    };

    let nvs = match EspNvs::new(nvs_partition.clone(), "wifi_cfg", true) {
        Ok(n) => n,
        Err(e) => {
            println!("> warning: could not open nvs namespace: {:?}", e);
            display.set_status_color(RGB8::new(50, 0, 0));
            run_animation_loop(display, DEFAULT_LISTEN_PORT);
            return;
        }
    };

    // seed the live marquee config + listen port from NVS before anything runs
    seed_marquee_from_nvs(&display, &nvs_partition);
    {
        // Dump the effective (post-NVS) display config. Anything non-default
        // here silently transforms every DDP frame before it reaches the
        // physical pixel mapper, so it must be visible when debugging.
        let c = display.marquee_config();
        println!(
            "> cfg: orientation={} direction={} margin={} margin_streams={} speed={}ms text={:?}",
            c.orientation,
            c.direction,
            c.panel_margin,
            c.margin_applies_streams,
            c.speed_ms,
            c.text
        );
    }
    let listen_port = read_listen_port(&nvs_partition);

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

    // The portal server, when we are serving it from the network we joined.
    // It has to outlive this match: the handle owns the server task.
    let mut portal: Option<EspHttpServer<'static>> = None;

    // enter wireless setup if requested or if we're missing the wireless config
    match ssid {
        Some(s) if wap_mode != 1 => {
            // connect to the wifi
            let p = pass.unwrap_or_default();

            let configured = wifi
                .set_configuration(&WifiConfiguration::Client(ClientConfiguration {
                    ssid: s.as_str().try_into().unwrap_or_default(),
                    password: p.as_str().try_into().unwrap_or_default(),
                    ..Default::default()
                }))
                .is_ok();

            // set hostname
            unsafe {
                esp_idf_svc::sys::esp_netif_set_hostname(
                    wifi.sta_netif().handle() as *mut _,
                    c"traffic-light".as_ptr() as _,
                );
            }

            println!("> attempting to connect to wifi");

            let started = configured && wifi.start().is_ok() && wifi.connect().is_ok();

            if started {
                unsafe {
                    esp_idf_svc::sys::esp_wifi_set_ps(
                        esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_NONE,
                    );
                }
                // Read the setting back rather than trusting the call. Modem
                // sleep waking the radio periodically is a known source of
                // RMT timing disruption, so this must be observable.
                unsafe {
                    let mut ps: esp_idf_svc::sys::wifi_ps_type_t = 0;
                    let rc = esp_idf_svc::sys::esp_wifi_get_ps(&mut ps);
                    println!(
                        "> wifi power save: rc={} ps={} (0=NONE 1=MIN_MODEM 2=MAX_MODEM)",
                        rc, ps
                    );
                }
            }

            // Three 10-second attempts rather than one. A single transient
            // auth timeout used to strand the light in setup mode until
            // somebody power-cycled it, and now that a failure to connect can
            // also mean rolling firmware back, one attempt is not enough
            // evidence to act on.
            let mut connected = false;
            if started {
                'attempts: for attempt in 1..=WIFI_CONNECT_ATTEMPTS {
                    for _ in 0..100 {
                        if wifi.is_connected().unwrap_or(false) {
                            connected = true;
                            break 'attempts;
                        }
                        FreeRtos::delay_ms(100);
                    }
                    if attempt < WIFI_CONNECT_ATTEMPTS {
                        println!(
                            "> wifi attempt {}/{} timed out; retrying",
                            attempt, WIFI_CONNECT_ATTEMPTS
                        );
                        let _ = wifi.connect();
                    }
                }
            }

            if !connected {
                // Boot-time safety net: a bad/unreachable saved network drops
                // us straight into the captive portal rather than stranding.
                // If this image was pushed over the network, though, failing to
                // get back on it is exactly the failure worth reverting -- and
                // this fallback is what would otherwise hide it.
                println!("> failed to connect; re-entering ap mode");
                settle_health(ota_expect, BootOutcome::ApFallback, &nvs_partition);
                display.set_status_color(RGB8::new(50, 0, 0));
                run_ap_mode(
                    &mut wifi,
                    nvs_partition.clone(),
                    display.clone(),
                    OtaExpect::Nothing,
                );
            } else {
                println!("> connected successfully");
                display.set_status_color(RGB8::new(0, 50, 0));
                display.set_image(&[100; 300]);
                FreeRtos::delay_ms(2000);

                // Serve the same portal on the network so the marquee can be
                // changed without walking over to the light. Scan once now,
                // while nothing else needs the radio, so the network picker
                // is populated; a scan while connected costs a moment of
                // radio time but keeps the association.
                let ip = wifi
                    .sta_netif()
                    .get_ip_info()
                    .map(|i| i.ip.to_string())
                    .unwrap_or_else(|_| "?".to_string());
                let ssids = scan_ssids(&mut wifi);
                portal = start_portal(
                    PortalMode::Connected { ip: ip.clone() },
                    ssids,
                    nvs_partition.clone(),
                    display.clone(),
                    None,
                );
                if portal.is_some() {
                    println!(
                        "> portal available at http://{}/ (marquee changes apply live, wifi changes after reboot)",
                        ip
                    );
                }
                // Being on the network is not enough on its own: an image that
                // cannot serve the portal is one nobody could replace over the
                // air, which is the thing rollback exists to avoid.
                settle_health(
                    ota_expect,
                    if portal.is_some() {
                        BootOutcome::Station
                    } else {
                        BootOutcome::StationNoPortal
                    },
                    &nvs_partition,
                );
            }
        }
        _ => {
            println!("> activating access point");
            display.set_status_color(RGB8::new(0, 0, 50));
            // Setup mode entered on purpose, either because someone held BOOT
            // or because there are no credentials to try. Either way it is a
            // decision, not the image failing, so an image that expected the
            // network still counts as healthy here.
            run_ap_mode(
                &mut wifi,
                nvs_partition.clone(),
                display.clone(),
                ota_expect,
            );
        }
    }

    // keep the network-mode portal alive for as long as the animation loop runs
    let _portal = portal;
    run_animation_loop(display, listen_port);
}

// --------------------------------------------------------
// NATIVE *NIX ENTRY POINT
// --------------------------------------------------------
#[cfg(not(target_os = "espidf"))]
fn main() {
    println!("> hello, world");
    let display = DisplayDriver::new_simulated();
    run_animation_loop(display, DEFAULT_LISTEN_PORT);
}

// --------------------------------------------------------
// SHARED APPLICATION LOGIC
// --------------------------------------------------------
fn run_animation_loop(display: DisplayDriver, listen_port: u16) {
    println!("> listening for DDP on UDP via port {}", listen_port);

    let socket = match UdpSocket::bind(("0.0.0.0", listen_port)) {
        Ok(s) => s,
        Err(e) => {
            println!("> fatal: could not bind udp port {}: {:?}", listen_port, e);
            // Fall back to marquee-only operation without a socket.
            run_marquee_only(display);
            return;
        }
    };
    let _ = socket.set_nonblocking(true);

    let mut buf = [0u8; 1500];
    let timeout = Duration::from_secs(5);
    let mut last_packet = Instant::now() - timeout;
    let mut marquee_offset: usize = 0;
    let mut last_marquee_update = Instant::now();

    let colors = [
        RGB8::new(195, 78, 75),  // red
        RGB8::new(61, 132, 175), // blue
        RGB8::new(216, 163, 0),  // yellow
        RGB8::new(137, 177, 8),  // green
    ];

    // main animation loop
    let mut color_idx = 0;
    let mut last_color_update = Instant::now();

    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, _src)) => {
                // DDP: 10 byte header + payload. We do not parse the header;
                // we take up to 300 payload bytes, padding short frames with
                // black and truncating long ones. Keeps processing minimal on
                // this older chip and can never overflow the image buffer.
                if len > 10 {
                    let payload = &buf[10..len];
                    let cfg = display.marquee_config();

                    // Build the physical 30x10 (pre-rotation) buffer from the
                    // incoming frame. When the margin applies to streams, the
                    // sender authored into the larger virtual canvas that
                    // includes the inter-panel gaps, so we drop the gap slabs;
                    // otherwise the frame is treated as one contiguous space.
                    let mut img = if cfg.margin_applies_streams && cfg.panel_margin > 0 {
                        stream_frame_with_gaps(payload, cfg.orientation, cfg.panel_margin)
                    } else {
                        let mut b = [0u8; 300];
                        let n = payload.len().min(300);
                        b[..n].copy_from_slice(&payload[..n]);
                        b
                    };

                    apply_orientation(&mut img, cfg.orientation);

                    display.set_image(&img);
                    last_packet = Instant::now();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {} // expected on non-blocking
            Err(_) => {}                                               // expected on read timeout
        }

        let now = Instant::now();

        // pull the live config each tick so preview changes take effect
        let cfg = display.marquee_config();

        if now.duration_since(last_packet) > timeout
            && now.duration_since(last_marquee_update) > Duration::from_millis(cfg.speed_ms as u64)
        {
            let mut img = [0u8; 300];
            let span = marquee_span(&cfg.text, cfg.orientation);
            render_marquee(
                &cfg.text,
                marquee_offset,
                cfg.orientation,
                cfg.direction,
                cfg.panel_margin,
                &mut img,
            );
            display.set_image(&img);
            if span > 0 {
                marquee_offset = (marquee_offset + 1) % span;
            } else {
                marquee_offset = 0;
            }
            last_marquee_update = now;
        }

        if now.duration_since(last_color_update) > Duration::from_millis(1000) {
            display.set_status_color(colors[color_idx]);
            color_idx = (color_idx + 1) % colors.len();
            last_color_update = now;
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

// Marquee-only fallback used when no UDP socket is available. Identical scroll
// behavior to the main loop minus DDP ingest.
fn run_marquee_only(display: DisplayDriver) {
    let mut marquee_offset: usize = 0;
    let mut last_marquee_update = Instant::now();
    let colors = [
        RGB8::new(195, 78, 75),
        RGB8::new(61, 132, 175),
        RGB8::new(216, 163, 0),
        RGB8::new(137, 177, 8),
    ];
    let mut color_idx = 0;
    let mut last_color_update = Instant::now();

    loop {
        let now = Instant::now();
        let cfg = display.marquee_config();

        if now.duration_since(last_marquee_update) > Duration::from_millis(cfg.speed_ms as u64) {
            let mut img = [0u8; 300];
            let span = marquee_span(&cfg.text, cfg.orientation);
            render_marquee(
                &cfg.text,
                marquee_offset,
                cfg.orientation,
                cfg.direction,
                cfg.panel_margin,
                &mut img,
            );
            display.set_image(&img);
            marquee_offset = if span > 0 {
                (marquee_offset + 1) % span
            } else {
                0
            };
            last_marquee_update = now;
        }

        if now.duration_since(last_color_update) > Duration::from_millis(1000) {
            display.set_status_color(colors[color_idx]);
            color_idx = (color_idx + 1) % colors.len();
            last_color_update = now;
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

// Advance the marquee one animation tick if enough time has elapsed. Shared by
// the WAP-mode setup screen so the live preview scrolls there too. Returns the
// updated (offset, last_update) pair.
#[cfg(target_os = "espidf")]
fn marquee_tick(display: &DisplayDriver, offset: usize, last_update: Instant) -> (usize, Instant) {
    let now = Instant::now();
    let cfg = display.marquee_config();
    if now.duration_since(last_update) <= Duration::from_millis(cfg.speed_ms as u64) {
        return (offset, last_update);
    }
    let mut img = [0u8; 300];
    let span = marquee_span(&cfg.text, cfg.orientation);
    render_marquee(
        &cfg.text,
        offset,
        cfg.orientation,
        cfg.direction,
        cfg.panel_margin,
        &mut img,
    );
    display.set_image(&img);
    let next = if span > 0 { (offset + 1) % span } else { 0 };
    (next, now)
}

// --------------------------------------------------------
// NVS CONFIG HELPERS (ESP32 ONLY)
// --------------------------------------------------------
// Seed the live display config from NVS at boot. Missing keys leave the
// defaults already present in DisplayState untouched.
#[cfg(target_os = "espidf")]
fn seed_marquee_from_nvs(display: &DisplayDriver, nvs_partition: &EspDefaultNvsPartition) {
    let nvs = match EspNvs::new(nvs_partition.clone(), "disp_cfg", true) {
        Ok(n) => n,
        Err(_) => return,
    };

    let mut txt_buf = [0u8; MARQUEE_TEXT_MAX * 4 + 1]; // worst-case UTF-8 bytes + nul
    if let Ok(Some(s)) = nvs.get_str("mq_text", &mut txt_buf) {
        display.set_marquee_text(s);
    }
    if let Ok(Some(v)) = nvs.get_u16("mq_speed_ms") {
        display.set_marquee_speed_ms(v);
    }
    if let Ok(Some(v)) = nvs.get_u8("mq_orient") {
        display.set_orientation(v);
    }
    if let Ok(Some(v)) = nvs.get_u8("mq_dir") {
        display.set_direction(v);
    }
    if let Ok(Some(v)) = nvs.get_u8("panel_margin") {
        display.set_panel_margin(v);
    }
    if let Ok(Some(v)) = nvs.get_u8("margin_strm") {
        display.set_margin_applies_streams(v != 0);
    }
}

#[cfg(target_os = "espidf")]
fn read_listen_port(nvs_partition: &EspDefaultNvsPartition) -> u16 {
    let nvs = match EspNvs::new(nvs_partition.clone(), "disp_cfg", true) {
        Ok(n) => n,
        Err(_) => return DEFAULT_LISTEN_PORT,
    };
    match nvs.get_u16("listen_port") {
        Ok(Some(p)) if p != 0 => p,
        _ => DEFAULT_LISTEN_PORT,
    }
}

// --------------------------------------------------------
// ORIENTATION TRANSFORM
// --------------------------------------------------------
// Rotate the logical 30x10 image in place to match the physical mounting.
//
//   ORIENT_0   (Horizontal, upright)  : identity
//   ORIENT_90  (Vertical,   upright)  : the source is authored as a 10-wide x
//                                       30-tall frame; its top-left lands at the
//                                       30x10 buffer's top-right. (x=29-y', y=x')
//   ORIENT_180 (Horizontal, inverted) : full 180 (x = 29 - x, y = 9 - y)
//   ORIENT_270 (Vertical,   inverted) : inverse of 90 (x = y', y = 9 - x')
//
// For 90/270 the source is treated as a 10x30 grid (src = y'*10 + x',
// x' in 0..10, y' in 0..30). For 0/180 it is the native 30x10 grid.
fn apply_orientation(img: &mut [u8; 300], orientation: u8) {
    match orientation {
        ORIENT_0 => {}
        ORIENT_180 => {
            let src = *img;
            for y in 0..10 {
                for x in 0..30 {
                    img[y * 30 + x] = src[(9 - y) * 30 + (29 - x)];
                }
            }
        }
        ORIENT_90 => {
            let src = *img; // interpreted as 10 wide x 30 tall
            for yp in 0..30 {
                for xp in 0..10 {
                    let val = src[yp * 10 + xp];
                    let x = 29 - yp; // 0..30
                    let y = xp; // 0..10
                    img[y * 30 + x] = val;
                }
            }
        }
        ORIENT_270 => {
            let src = *img; // interpreted as 10 wide x 30 tall
            for yp in 0..30 {
                for xp in 0..10 {
                    let val = src[yp * 10 + xp];
                    let x = yp; // 0..30
                    let y = 9 - xp; // 0..10
                    img[y * 30 + x] = val;
                }
            }
        }
        _ => {}
    }
}

// --------------------------------------------------------
// MARQUEE RENDERING
// --------------------------------------------------------
// The marquee always draws upright 5x7 glyphs. On horizontal mounts the text
// train scrolls along the 30-wide x-axis; on vertical mounts it scrolls along
// the 30-tall y-axis. We render into a visual canvas sized for the orientation
// and then reuse apply_orientation to map it into the physical 30x10 buffer,
// so the marquee and DDP paths share one rotation and can never disagree.

// Total scroll span (in steps) for the current text + orientation. One step is
// one column (horizontal) or one row (vertical) of travel.
fn marquee_span(text: &str, orientation: u8) -> usize {
    // The renderer indexes the text by BYTE (the font is a byte-per-glyph
    // table), so the scroll span must be byte-based to stay in phase. Text is
    // already truncated to MARQUEE_TEXT_MAX chars at storage time.
    let len = truncate_chars(text, MARQUEE_TEXT_MAX).len();
    if len == 0 {
        return 0;
    }
    // Pitch differs by scroll axis: horizontal scrolls along glyph width
    // (5 + 1 gap = 6), vertical scrolls along glyph height (7 + 1 gap = 8).
    if orientation_is_vertical(orientation) {
        len * 8
    } else {
        len * 6
    }
}

fn render_marquee(
    text: &str,
    offset: usize,
    orientation: u8,
    direction: u8,
    margin: u8,
    img: &mut [u8; 300],
) {
    let text = truncate_chars(text, MARQUEE_TEXT_MAX);
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len == 0 {
        *img = [0u8; 300];
        return;
    }

    let direction = clamp_direction(orientation, direction);

    // The panel major axis is the "30" dimension of the pre-rotation buffer.
    // We render the glyph train across the VIRTUAL major axis (30 + 2*margin)
    // so that content landing on an inter-panel gap falls into a dead zone, then
    // compress the surviving columns/rows down to the physical 30. A letter that
    // straddles a seam vanishes into the gap and re-emerges on the next panel,
    // matching the physically separated lamps.
    let vmajor = virtual_major(margin); // 30 + 2*margin

    let mut phys = [0u8; 300];

    if orientation_is_vertical(orientation) {
        // Physical (pre-rotation) buffer is 10 wide x 30 tall. Major axis = rows
        // (30), minor = cols (10). Glyphs are upright, 7 tall, pitch 8 along the
        // scroll (major) axis, centered in the 10-wide minor axis.
        let pitch = 8usize;
        let total = len * pitch;
        let x_pad = 2usize;
        let eff_offset = match direction {
            DIR_BOTTOM_TO_TOP => (total - (offset % total)) % total,
            _ => offset % total, // top-to-bottom
        };

        for vrow in 0..vmajor {
            // where does this virtual row land physically? (None = dead gap)
            let prow = match virtual_to_physical_major(vrow, margin) {
                Some(p) => p,
                None => continue,
            };
            let text_pos = (vrow + eff_offset) % total;
            let row_in_glyph = text_pos % pitch; // 0..7 body, 7 = inter-char gap
            if row_in_glyph >= 7 {
                continue;
            }
            let char_idx = text_pos / pitch;
            let c = normalize_char(bytes[char_idx]);
            let base = ((c - 32) as usize) * 5;
            for fx in 0..5 {
                let col_data = FONT[base + fx];
                if (col_data & (1 << row_in_glyph)) != 0 {
                    let vx = x_pad + fx;
                    if vx < 10 {
                        phys[prow * 10 + vx] = 100;
                    }
                }
            }
        }
    } else {
        // Physical buffer is 30 wide x 10 tall. Major axis = cols (30), minor =
        // rows (10). Glyphs upright, 7 tall (vertically centered +1), pitch 6
        // along the scroll (major) axis.
        let pitch = 6usize;
        let total = len * pitch;
        let eff_offset = match direction {
            DIR_LEFT_TO_RIGHT => (total - (offset % total)) % total,
            _ => offset % total, // right-to-left
        };

        for vx in 0..vmajor {
            let px = match virtual_to_physical_major(vx, margin) {
                Some(p) => p,
                None => continue,
            };
            let text_x = (vx + eff_offset) % total;
            let pixel_x = text_x % pitch;
            if pixel_x >= 5 {
                continue; // inter-character gap column
            }
            let char_idx = text_x / pitch;
            let c = normalize_char(bytes[char_idx]);
            let col_data = FONT[((c - 32) as usize) * 5 + pixel_x];
            for y in 0..7 {
                if (col_data & (1 << y)) != 0 {
                    phys[(y + 1) * 30 + px] = 100; // +1 to center vertically
                }
            }
        }
    }

    *img = phys;
    apply_orientation(img, orientation); // ORIENT_0/90/180/270
}

// Build a physical 30x10 (pre-rotation) buffer from an incoming DDP frame that
// was authored into the gapped VIRTUAL canvas. The sender draws into the full
// virtual space (major axis = 30 + 2*margin) including the dead zones; we drop
// the gap slabs and keep the three panel blocks. Frame layout depends on
// orientation, matching the pre-rotation source the marquee uses:
//   horizontal: (30+2*margin) wide x 10 tall, row-major  vy*vmajor + vx
//   vertical:   10 wide x (30+2*margin) tall, row-major   vrow*10   + x
// Bytes beyond the frame (short frames) read as black; extra bytes are ignored.
fn stream_frame_with_gaps(payload: &[u8], orientation: u8, margin: u8) -> [u8; 300] {
    let vmajor = virtual_major(margin);
    let mut phys = [0u8; 300];
    let get = |idx: usize| -> u8 { payload.get(idx).copied().unwrap_or(0) };

    if orientation_is_vertical(orientation) {
        // source minor axis = 10 columns; major axis = vmajor rows
        for vrow in 0..vmajor {
            let prow = match virtual_to_physical_major(vrow, margin) {
                Some(p) => p,
                None => continue,
            };
            for x in 0..10 {
                phys[prow * 10 + x] = get(vrow * 10 + x);
            }
        }
    } else {
        // source minor axis = 10 rows; major axis = vmajor columns
        for vx in 0..vmajor {
            let px = match virtual_to_physical_major(vx, margin) {
                Some(p) => p,
                None => continue,
            };
            for y in 0..10 {
                phys[y * 30 + px] = get(y * vmajor + vx);
            }
        }
    }
    phys
}

// Map a byte to a printable font index range [32, 96], upper-casing lowercase
// letters and substituting space for anything out of range.
fn normalize_char(mut c: u8) -> u8 {
    if (97..=122).contains(&c) {
        c -= 32; // basic uppercase conversion
    }
    if !(32..97).contains(&c) {
        c = 32;
    }
    c
}

#[cfg(target_os = "espidf")]
fn run_ap_mode(
    wifi: &mut EspWifi,
    nvs_partition: EspDefaultNvsPartition,
    display: DisplayDriver,
    ota_expect: OtaExpect,
) {
    println!("> scanning for wifi networks...");

    // temporarily switch to client mode to scan
    let _ = wifi.stop();
    let _ = wifi.set_configuration(&WifiConfiguration::Client(ClientConfiguration::default()));
    let _ = wifi.start();
    FreeRtos::delay_ms(2000);

    let ssids = scan_ssids(wifi);

    // pivot to access point mode to host the setup server
    start_ap(wifi);
    display.set_status_color(RGB8::new(0, 0, 50));

    // Shared handle for the pending wifi test request. The /save handler drops
    // the requested credentials here; the loop below picks them up, runs the
    // test-in-place, and restores AP mode. Kept off the HTTP thread so the
    // response returns before the AP is torn down.
    let pending: WifiTestSlot = Arc::new(Mutex::new(None));

    let _server = match start_portal(
        PortalMode::Setup,
        ssids,
        nvs_partition.clone(),
        display.clone(),
        Some(pending.clone()),
    ) {
        Some(s) => {
            // Setup mode is up and serving, which is all an image delivered
            // through the access point ever promised to do.
            settle_health(ota_expect, BootOutcome::ApRequested, &nvs_partition);
            s
        }
        None => {
            // without the portal there is no way to finish setup
            println!("> fatal: setup portal unavailable");
            settle_health(ota_expect, BootOutcome::ApNoPortal, &nvs_partition);
            loop {
                FreeRtos::delay_ms(1000);
            }
        }
    };

    println!(
        "> ready for wireless setup: connect to 'Traffic-Light' and visit http://192.168.71.1"
    );

    // Setup-mode loop: drive the live marquee (so preview works here) and, when
    // credentials are queued, run the test-in-place then restore AP mode. The
    // test lives outside the HTTP handlers so the /save response returns before
    // the AP briefly drops during association.
    let mut marquee_offset: usize = 0;
    let mut last_marquee_update = Instant::now();

    loop {
        // advance the live preview marquee, unless firmware is being written,
        // in which case the panels are showing the progress bar instead
        if display.update_progress().is_none() {
            let (off, last) = marquee_tick(&display, marquee_offset, last_marquee_update);
            marquee_offset = off;
            last_marquee_update = last;
        }

        // handle any queued wifi test
        let job = match pending.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        };

        if let Some((ssid, pass)) = job {
            println!("> testing wifi credentials for '{}'", ssid);
            let ok = test_credentials(wifi, &ssid, &pass);
            display.set_wifi_test(if ok {
                WifiTestResult::Success
            } else {
                WifiTestResult::Failed
            });
            // restore AP so the user's device can rejoin and read the result
            start_ap(wifi);
            display.set_status_color(RGB8::new(0, 0, 50));
            println!(
                "> wifi test complete: {}",
                if ok { "success" } else { "failed" }
            );
            last_marquee_update = Instant::now();
        }

        FreeRtos::delay_ms(15);
    }
}

// Build the HTTP setup portal and register its handlers. Used from both the
// setup access point and, once connected, from the real network. The returned
// handle owns the server task; drop it and the portal goes away.
//
// `wifi_test` is the slot a credential change is queued into for a
// test-in-place. Only the setup access point provides one: when we are already
// a client on a network, dropping it to test a new one would cut off the very
// connection the portal is being served over.
#[cfg(target_os = "espidf")]
fn start_portal(
    mode: PortalMode,
    ssids: Vec<String>,
    nvs_partition: EspDefaultNvsPartition,
    display: DisplayDriver,
    wifi_test: Option<WifiTestSlot>,
) -> Option<EspHttpServer<'static>> {
    let server_config = HttpConfiguration {
        // The OTA handler decodes a header, holds a 4K chunk buffer and calls
        // down into the flash driver. esp-idf-svc's default of 6K is not much
        // room for that; the cost of the extra few kilobytes is one task.
        stack_size: 10240,
        ..Default::default()
    };
    let mut server = match EspHttpServer::new(&server_config) {
        Ok(s) => s,
        Err(e) => {
            println!("> warning: could not start http server: {:?}", e);
            return None;
        }
    };
    let mode = Arc::new(mode);
    // Read once per portal rather than per request: NVS reads are flash reads,
    // and a credential change reboots anyway.
    let auth: AuthSlot = Arc::new(Mutex::new(load_auth_policy(&nvs_partition)));
    if !auth.lock().map(|a| a.is_configured()).unwrap_or(false) {
        println!("> warning: no portal password set; anyone on this network can reconfigure or reflash this light");
    }

    // ---- GET / : the setup page ----
    {
        let display = display.clone();
        let ssids = ssids.clone();
        let nvs_partition = nvs_partition.clone();
        let mode = mode.clone();
        let page_auth = auth.clone();
        guarded(&mut server, "/", Method::Get, auth.clone(), move |req| {
            let port = read_listen_port(&nvs_partition);
            let html = match page_auth.lock() {
                Ok(policy) => {
                    let firmware = RunningFirmware {
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        slot: running_slot_label(),
                    };
                    render_portal(&ssids, &display, port, &mode, &policy, &firmware)
                }
                // Unreachable in practice: the guard this handler sits behind
                // has already refused a poisoned lock. Serve something anyway
                // rather than panic inside an http handler.
                Err(_) => "<html><body>portal state unavailable</body></html>".to_string(),
            };
            let mut res = req.into_ok_response()?;
            res.write_all(html.as_bytes())?;
            Ok::<(), EspIOError>(())
        });
    }

    // ---- POST /save : persist changed marquee/wifi keys, queue wifi test ----
    // This is invoked by the page just before a reboot when something changed.
    {
        let nvs_partition = nvs_partition.clone();
        let display = display.clone();
        let wifi_test = wifi_test.clone();
        let save_auth = auth.clone();
        guarded(
            &mut server,
            "/save",
            Method::Post,
            auth.clone(),
            move |mut req| {
                let body = read_body(&mut req);
                let fields = parse_form(&body);

                // --- marquee + port config (disp_cfg namespace) ---
                if let Ok(mut cfg) = EspNvs::new(nvs_partition.clone(), "disp_cfg", true) {
                    if let Some(v) = fields.get("mq_text") {
                        let v = truncate_chars(v, MARQUEE_TEXT_MAX);
                        write_str_if_changed(&mut cfg, "mq_text", &v);
                        display.set_marquee_text(&v);
                    }
                    if let Some(v) = fields.get("mq_speed_ms") {
                        if let Ok(ms) = v.parse::<u16>() {
                            let ms = clamp_speed_ms(ms);
                            write_u16_if_changed(&mut cfg, "mq_speed_ms", ms);
                            display.set_marquee_speed_ms(ms);
                        }
                    }
                    if let Some(v) = fields.get("mq_orient") {
                        if let Ok(o) = v.parse::<u8>() {
                            let o = match o {
                                ORIENT_0 | ORIENT_90 | ORIENT_180 | ORIENT_270 => o,
                                _ => ORIENT_0,
                            };
                            write_u8_if_changed(&mut cfg, "mq_orient", o);
                            display.set_orientation(o);
                        }
                    }
                    if let Some(v) = fields.get("mq_dir") {
                        if let Ok(d) = v.parse::<u8>() {
                            // clamp against whatever orientation we just stored
                            let cur_orient = display.marquee_config().orientation;
                            let d = clamp_direction(cur_orient, d);
                            write_u8_if_changed(&mut cfg, "mq_dir", d);
                            display.set_direction(d);
                        }
                    }
                    if let Some(v) = fields.get("panel_margin") {
                        if let Ok(m) = v.parse::<u8>() {
                            let m = m.min(PANEL_MARGIN_MAX);
                            write_u8_if_changed(&mut cfg, "panel_margin", m);
                            display.set_panel_margin(m);
                        }
                    }
                    // checkbox state sent explicitly as 0/1 by the page
                    if let Some(v) = fields.get("margin_streams") {
                        let on = v == "1";
                        write_u8_if_changed(&mut cfg, "margin_strm", on as u8);
                        display.set_margin_applies_streams(on);
                    }
                    if let Some(v) = fields.get("listen_port") {
                        if let Ok(p) = v.parse::<u16>() {
                            if p != 0 {
                                write_u16_if_changed(&mut cfg, "listen_port", p);
                            }
                        }
                    }
                }

                // --- wifi credentials (wifi_cfg namespace) ---
                // ONLY touched when a non-empty SSID is supplied. A blank SSID means
                // "keep the current network unchanged", so we never clobber stored
                // credentials on a marquee-only save.
                let mut queued_test = false;
                if let Some(ssid) = fields.get("ssid") {
                    let ssid = ssid.trim();
                    if !ssid.is_empty() {
                        let pass = fields.get("password").cloned().unwrap_or_default();
                        if let Ok(mut wc) = EspNvs::new(nvs_partition.clone(), "wifi_cfg", true) {
                            write_str_if_changed(&mut wc, "ssid", ssid);
                            write_str_if_changed(&mut wc, "pass", &pass);
                            // ensure the next boot attempts client mode
                            write_u8_if_changed(&mut wc, "wap_mode", 0);
                        }
                        // In setup mode, queue a test-in-place for the AP loop to
                        // run. On a live network there is no slot: the credentials
                        // are simply stored and tried at the reboot that follows.
                        if let Some(pending) = &wifi_test {
                            if let Ok(mut slot) = pending.lock() {
                                *slot = Some((ssid.to_string(), pass));
                                queued_test = true;
                            }
                        }
                    }
                }

                // --- portal password (sec_cfg namespace) ---
                // Same "blank means leave it alone" convention as the wifi password
                // above, so the page never has to echo the stored secret back. The
                // new credentials take effect on the next portal start, i.e. after
                // the reboot the page performs.
                if fields
                    .get("portal_clear")
                    .map(|v| v == "1")
                    .unwrap_or(false)
                {
                    clear_auth(&nvs_partition);
                    if let Ok(mut policy) = save_auth.lock() {
                        policy.user = AUTH_USER_DEFAULT.to_string();
                        policy.pass = String::new();
                    }
                } else if let Some(new_pass) = fields.get("portal_pass") {
                    if !new_pass.is_empty() {
                        let user = fields
                            .get("portal_user")
                            .map(|u| u.trim())
                            .filter(|u| !u.is_empty())
                            .unwrap_or(AUTH_USER_DEFAULT)
                            .to_string();
                        if let Ok(mut sec) =
                            EspNvs::new(nvs_partition.clone(), AUTH_NAMESPACE, true)
                        {
                            write_str_if_changed(&mut sec, "user", &user);
                            write_str_if_changed(&mut sec, "pass", new_pass);
                        }
                        // Apply it to the running portal too, so the next request
                        // is already challenged rather than waiting for a reboot.
                        if let Ok(mut policy) = save_auth.lock() {
                            policy.user = user;
                            policy.pass = new_pass.clone();
                        }
                    }
                }

                if queued_test {
                    display.set_wifi_test(WifiTestResult::Testing);
                }

                let mut res = req.into_ok_response()?;
                res.write_all(if queued_test {
                    b"saved-testing"
                } else {
                    b"saved"
                })?;
                Ok::<(), EspIOError>(())
            },
        );
    }

    // ---- POST /preview : push marquee config to RAM only (no NVS) ----
    {
        let display = display.clone();
        guarded(
            &mut server,
            "/preview",
            Method::Post,
            auth.clone(),
            move |mut req| {
                let body = read_body(&mut req);
                let fields = parse_form(&body);
                if let Some(v) = fields.get("mq_text") {
                    display.set_marquee_text(v);
                }
                if let Some(v) = fields.get("mq_speed_ms") {
                    if let Ok(ms) = v.parse::<u16>() {
                        display.set_marquee_speed_ms(ms);
                    }
                }
                if let Some(v) = fields.get("mq_orient") {
                    if let Ok(o) = v.parse::<u8>() {
                        display.set_orientation(o);
                    }
                }
                if let Some(v) = fields.get("mq_dir") {
                    if let Ok(d) = v.parse::<u8>() {
                        display.set_direction(d);
                    }
                }
                if let Some(v) = fields.get("panel_margin") {
                    if let Ok(m) = v.parse::<u8>() {
                        display.set_panel_margin(m);
                    }
                }
                // preview always sends the checkbox state explicitly as 0/1
                if let Some(v) = fields.get("margin_streams") {
                    display.set_margin_applies_streams(v == "1");
                }
                let mut res = req.into_ok_response()?;
                res.write_all(b"ok")?;
                Ok::<(), EspIOError>(())
            },
        );
    }

    // ---- GET /status : current wifi test result as plain text for polling ----
    {
        let display = display.clone();
        guarded(
            &mut server,
            "/status",
            Method::Get,
            auth.clone(),
            move |req| {
                let s = match display.wifi_test() {
                    WifiTestResult::Idle => "idle",
                    WifiTestResult::Testing => "testing",
                    WifiTestResult::Success => "success",
                    WifiTestResult::Failed => "failed",
                };
                let mut res = req.into_ok_response()?;
                res.write_all(s.as_bytes())?;
                Ok::<(), EspIOError>(())
            },
        );
    }

    // ---- POST /reboot : explicit, user-initiated reboot ----
    {
        guarded(
            &mut server,
            "/reboot",
            Method::Post,
            auth.clone(),
            move |req| {
                let mut res = req.into_ok_response()?;
                res.write_all(b"rebooting")?;
                std::thread::spawn(|| {
                    FreeRtos::delay_ms(1500);
                    unsafe {
                        esp_idf_svc::sys::esp_restart();
                    }
                });
                Ok::<(), EspIOError>(())
            },
        );
    }

    // ---- GET /version : what is running, for the push script to check ----
    // Deliberately a new endpoint rather than an extension of /status, whose
    // bare-word body the portal's own javascript string-compares.
    {
        guarded(
            &mut server,
            "/version",
            Method::Get,
            auth.clone(),
            move |req| {
                let uptime_ms = unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000;
                let state = EspOta::new()
                    .and_then(|ota| ota.get_running_slot())
                    .map(|slot| format!("{:?}", slot.state).to_lowercase())
                    .unwrap_or_else(|_| "unknown".to_string());
                let body = format!(
                    concat!(
                        "{{\"name\":\"{}\",\"version\":\"{}\",\"idf\":\"{}\",",
                        "\"partition\":\"{}\",\"elf_sha256\":\"{}\",",
                        "\"ota_state\":\"{}\",\"uptime_ms\":{}}}"
                    ),
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION"),
                    running_idf_version(),
                    running_slot_label(),
                    running_elf_sha256(),
                    state,
                    uptime_ms,
                );
                let mut res =
                    req.into_response(200, None, &[("Content-Type", "application/json")])?;
                res.write_all(body.as_bytes())?;
                Ok::<(), EspIOError>(())
            },
        );
    }

    // ---- POST /ota : a raw application image, straight into the spare slot ----
    // The body is the bare .bin, so one endpoint serves both
    // `curl --data-binary @app.bin` and a browser sending a File through XHR,
    // and nothing here has to parse multipart.
    {
        let display = display.clone();
        let nvs_partition = nvs_partition.clone();
        let mode = mode.clone();
        let ota_auth = auth.clone();
        guarded(
            &mut server,
            "/ota",
            Method::Post,
            auth.clone(),
            move |mut req| {
                use std::sync::atomic::Ordering;

                // An open portal will serve the settings page, but it will not
                // run arbitrary code. This is the one place where "no password
                // set" is refused rather than tolerated.
                let configured = ota_auth
                    .lock()
                    .map(|policy| policy.is_configured())
                    .unwrap_or(false);
                if !configured {
                    return plain_response(
                        req,
                        403,
                        "set a portal password before uploading firmware",
                    );
                }

                if OTA_IN_FLIGHT.swap(true, Ordering::SeqCst) {
                    return plain_response(req, 409, "another update is already running");
                }
                let _in_flight = OtaInFlight;

                let slot = ota_slot_size();
                let total = match check_upload_size(req.content_len(), slot) {
                    Ok(n) => n,
                    Err(problem) => {
                        return plain_response(req, problem.status(), &problem.message())
                    }
                };

                let mut ota = match EspOta::new() {
                    Ok(ota) => ota,
                    Err(e) => {
                        println!("> ota: cannot reach the ota slots: {:?}", e);
                        return plain_response(req, 500, "no ota slots on this device");
                    }
                };

                // Someone authenticated and reached this endpoint, which is
                // proof that the running image works well enough to be talked
                // to. Confirm it, or esp_ota_begin refuses while the image is
                // still provisional and the only way out would be a cable.
                if let Ok(running) = ota.get_running_slot() {
                    if running.state == SlotState::Unverified {
                        println!("> ota: confirming the running image so it can be replaced");
                        let _ = ota.mark_running_slot_valid();
                        HEALTH_SETTLED.store(true, Ordering::SeqCst);
                        clear_ota_expectation(&nvs_partition);
                    }
                }

                let mut update = match ota.initiate_update() {
                    Ok(update) => update,
                    Err(e) => {
                        println!("> ota: could not open the spare slot: {:?}", e);
                        return plain_response(req, 500, "could not open the spare slot");
                    }
                };

                if let Some(who) = req.header("X-OTA-Pusher") {
                    println!("> ota: {} bytes incoming from {}", total, who);
                } else {
                    println!("> ota: {} bytes incoming", total);
                }

                display.begin_update();
                let mut buf = vec![0u8; OTA_CHUNK];
                let mut head: Vec<u8> = Vec::with_capacity(OTA_HEADER_PEEK);
                let mut written = 0usize;
                let mut last_percent = u8::MAX;
                let mut failure: Option<(u16, String)> = None;

                while written < total {
                    let want = OTA_CHUNK.min(total - written);
                    let n = match req.read(&mut buf[..want]) {
                        Ok(0) => {
                            failure = Some((400, "the body ended early".to_string()));
                            break;
                        }
                        Ok(n) => n,
                        Err(e) => {
                            failure = Some((408, format!("upload stalled: {:?}", e)));
                            break;
                        }
                    };
                    if head.len() < OTA_HEADER_PEEK {
                        let take = n.min(OTA_HEADER_PEEK - head.len());
                        head.extend_from_slice(&buf[..take]);
                        if let Err(problem) = validate_image_prefix(&head) {
                            failure = Some((400, problem.message().to_string()));
                            break;
                        }
                    }
                    if let Err(e) = update.write(&buf[..n]) {
                        failure = Some((500, format!("flash write failed: {:?}", e)));
                        break;
                    }
                    written += n;
                    let percent = ota_progress_percent(written, total);
                    if percent != last_percent {
                        display.set_update_progress(percent);
                        last_percent = percent;
                    }
                }

                if let Some((status, why)) = failure {
                    let _ = update.abort();
                    display.end_update();
                    println!("> ota: gave up after {}/{} bytes: {}", written, total, why);
                    return plain_response(req, status, &why);
                }

                // Record what this image has to prove before the boot pointer
                // moves, so a power cut between the two cannot lose it.
                let expect = match mode.as_ref() {
                    PortalMode::Connected { .. } => OtaExpect::Station,
                    PortalMode::Setup => OtaExpect::Ap,
                };
                write_ota_expectation(&nvs_partition, expect);

                if let Err(e) = update.complete() {
                    clear_ota_expectation(&nvs_partition);
                    display.end_update();
                    println!("> ota: esp-idf rejected the image: {:?}", e);
                    return plain_response(req, 400, "the image failed esp-idf's own checks");
                }

                display.set_update_progress(100);
                println!(
                    "> ota: written, rebooting into it (must reach {:?})",
                    expect
                );

                // Answer before rebooting, the same way POST /reboot does, or
                // every successful push looks like a dropped connection.
                plain_response(req, 200, "ok")?;
                std::thread::spawn(|| {
                    FreeRtos::delay_ms(1500);
                    unsafe {
                        esp_idf_svc::sys::esp_restart();
                    }
                });
                Ok::<(), EspIOError>(())
            },
        );
    }

    Some(server)
}

// Scan for nearby SSIDs, de-duplicated and sorted. Assumes wifi is already
// started in client mode.
#[cfg(target_os = "espidf")]
fn scan_ssids(wifi: &mut EspWifi) -> Vec<String> {
    let mut ssids: Vec<String> = Vec::new();
    match wifi.scan() {
        Ok(scans) => {
            for ap in scans {
                let ssid = ap.ssid.to_string();
                if !ssid.is_empty() && !ssids.contains(&ssid) {
                    ssids.push(ssid);
                }
            }
        }
        Err(e) => println!("> wifi scan failed: {:?}", e),
    }
    ssids.sort();
    ssids
}

// Bring up the "Traffic-Light" access point that hosts the setup server.
#[cfg(target_os = "espidf")]
fn start_ap(wifi: &mut EspWifi) {
    let _ = wifi.stop();
    let _ = wifi.set_configuration(&WifiConfiguration::AccessPoint(AccessPointConfiguration {
        ssid: "Traffic-Light".try_into().unwrap_or_default(),
        channel: 1,
        ..Default::default()
    }));
    let _ = wifi.start();
    FreeRtos::delay_ms(1000);
}

// Test-in-place: switch to client mode, try to associate within a short
// timeout, and report whether it worked. Does NOT persist anything. The caller
// is responsible for restoring AP mode afterward.
#[cfg(target_os = "espidf")]
fn test_credentials(wifi: &mut EspWifi, ssid: &str, pass: &str) -> bool {
    let _ = wifi.stop();
    if wifi
        .set_configuration(&WifiConfiguration::Client(ClientConfiguration {
            ssid: ssid.try_into().unwrap_or_default(),
            password: pass.try_into().unwrap_or_default(),
            ..Default::default()
        }))
        .is_err()
    {
        return false;
    }
    if wifi.start().is_err() || wifi.connect().is_err() {
        return false;
    }
    // Fast timeout: ~8s is enough to associate on a reachable network without
    // keeping the user's device off the AP for too long.
    for _ in 0..80 {
        if wifi.is_connected().unwrap_or(false) {
            return true;
        }
        FreeRtos::delay_ms(100);
    }
    false
}

// --------------------------------------------------------
// PORTAL AUTHENTICATION
// --------------------------------------------------------
// The portal can change the wifi credentials, reboot the light and (since the
// OTA work) replace its firmware, so it sits behind HTTP Basic auth. The parts
// that decide whether a request is allowed are pure functions, compiled on the
// host too, because they are the parts worth testing.

// Where the portal credentials live. Its own namespace rather than wifi_cfg or
// disp_cfg so that clearing either of those cannot lock anyone out.
#[cfg(target_os = "espidf")]
const AUTH_NAMESPACE: &str = "sec_cfg";

// Used when nobody has set a username. The password has no default: an unset
// password means the portal is open (see AuthPolicy::is_configured).
#[cfg(any(target_os = "espidf", test))]
const AUTH_USER_DEFAULT: &str = "admin";

// What the portal page says about the image it is being served by. Passed in
// rather than read inside the renderer, so the renderer stays pure and testable.
#[cfg(any(target_os = "espidf", test))]
pub struct RunningFirmware {
    pub version: String,
    pub slot: String,
}

#[cfg(any(target_os = "espidf", test))]
pub struct AuthPolicy {
    pub user: String,
    pub pass: String,
}

#[cfg(any(target_os = "espidf", test))]
impl AuthPolicy {
    // No stored password means nobody has set one yet, and the portal serves
    // everyone. That keeps a firmware update from locking an existing light out
    // of its own portal; the page nags about it instead, and the firmware
    // upload endpoint refuses to run until a password exists.
    pub fn is_configured(&self) -> bool {
        !self.pass.is_empty()
    }
}

// Decode standard base64 (RFC 4648, no line breaks, optional '=' padding).
//
// Hand-rolled because the generated esp-idf bindings do not expose mbedtls's
// base64 -- and because a pure decoder can be tested on the host, which an FFI
// call could not be. Output is capped: the only thing being decoded here is a
// "user:password" pair, and an attacker should not be able to make the portal
// allocate by sending a long header.
#[cfg(any(target_os = "espidf", test))]
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const MAX_OUT: usize = 192;

    fn sextet(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a') as u32 + 26),
            b'0'..=b'9' => Some((b - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes = s.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut i = 0;
    while i < bytes.len() {
        let chunk = &bytes[i..i + 4];
        let is_last = i + 4 == bytes.len();
        let mut acc: u32 = 0;
        let mut pad = 0usize;
        for (j, &b) in chunk.iter().enumerate() {
            if b == b'=' {
                // Padding is legal only in the last two slots of the last chunk.
                if !is_last || j < 2 {
                    return None;
                }
                pad += 1;
                acc <<= 6;
            } else {
                // ...and nothing may follow it.
                if pad > 0 {
                    return None;
                }
                acc = (acc << 6) | sextet(b)?;
            }
        }
        for k in 0..(3 - pad) {
            out.push(((acc >> (16 - 8 * k)) & 0xff) as u8);
        }
        if out.len() > MAX_OUT {
            return None;
        }
        i += 4;
    }
    Some(out)
}

// Split an `Authorization: Basic <base64>` header into its user and password.
// The scheme token is case-insensitive per RFC 7235, and only the first colon
// separates the two, so a password may itself contain colons.
#[cfg(any(target_os = "espidf", test))]
fn parse_basic_auth(header: &str) -> Option<(String, String)> {
    let (scheme, rest) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let text = String::from_utf8(base64_decode(rest.trim())?).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

// Compare without letting the time taken depend on how much of the secret
// matched. The lengths are already observable from the wire, so only the
// content has to be hidden; black_box keeps the optimiser from unrolling the
// accumulation back into an early exit.
#[cfg(any(target_os = "espidf", test))]
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u32;
    for i in 0..a.len().min(b.len()) {
        diff |= (a[i] ^ b[i]) as u32;
    }
    std::hint::black_box(diff) == 0
}

// The whole access decision, in one testable place.
#[cfg(any(target_os = "espidf", test))]
fn auth_ok(header: Option<&str>, policy: &AuthPolicy) -> bool {
    if !policy.is_configured() {
        return true;
    }
    let Some(header) = header else {
        return false;
    };
    let Some((user, pass)) = parse_basic_auth(header) else {
        return false;
    };
    // Check both halves unconditionally so a wrong username does not answer
    // faster than a wrong password.
    let user_ok = ct_eq(user.as_bytes(), policy.user.as_bytes());
    let pass_ok = ct_eq(pass.as_bytes(), policy.pass.as_bytes());
    user_ok & pass_ok
}

#[cfg(target_os = "espidf")]
fn load_auth_policy(nvs_partition: &EspDefaultNvsPartition) -> AuthPolicy {
    let mut user = AUTH_USER_DEFAULT.to_string();
    let mut pass = String::new();
    if let Ok(nvs) = EspNvs::new(nvs_partition.clone(), AUTH_NAMESPACE, true) {
        let mut user_buf = [0u8; 96];
        let mut pass_buf = [0u8; 160];
        if let Ok(Some(stored)) = nvs.get_str("user", &mut user_buf) {
            if !stored.is_empty() {
                user = stored.to_string();
            }
        }
        if let Ok(Some(stored)) = nvs.get_str("pass", &mut pass_buf) {
            pass = stored.to_string();
        }
    }
    AuthPolicy { user, pass }
}

// Forget the portal credentials. Reached by holding BOOT, which is the way back
// in for someone who has lost the password: physical access to the button
// already implies control of the device.
#[cfg(target_os = "espidf")]
fn clear_auth(nvs_partition: &EspDefaultNvsPartition) {
    if let Ok(mut nvs) = EspNvs::new(nvs_partition.clone(), AUTH_NAMESPACE, true) {
        let _ = nvs.remove("user");
        let _ = nvs.remove("pass");
    }
}

// Register a handler behind the auth check, so no individual handler carries a
// copy of it and none can be added without one.
#[cfg(target_os = "espidf")]
fn guarded<F>(
    server: &mut EspHttpServer<'static>,
    uri: &str,
    method: Method,
    policy: AuthSlot,
    handler: F,
) where
    F: for<'r> Fn(Request<&mut EspHttpConnection<'r>>) -> Result<(), EspIOError> + Send + 'static,
{
    let _ = server.fn_handler(uri, method, move |req| {
        let allowed = match policy.lock() {
            Ok(policy) => auth_ok(req.header("Authorization"), &policy),
            // A poisoned lock means a handler panicked while holding it. Refuse
            // rather than guess which way to fail.
            Err(_) => false,
        };
        if !allowed {
            let mut res = req.into_response(
                401,
                Some("Unauthorized"),
                &[
                    ("WWW-Authenticate", "Basic realm=\"traffic light\""),
                    ("Content-Type", "text/plain"),
                ],
            )?;
            res.write_all(b"unauthorized\n")?;
            return Ok::<(), EspIOError>(());
        }
        handler(req)
    });
}

// --------------------------------------------------------
// FIRMWARE UPDATES
// --------------------------------------------------------
// The device accepts a raw esp-idf application image as the body of a POST and
// writes it into the spare app slot. The decisions -- is this plausibly our
// firmware, is it going to fit, did the image that just booted actually work --
// are pure functions so they can be tested off the device, which matters
// because being wrong about any of them means a walk to the light with a cable.

// Streamed in 4K pieces: one flash sector, and small enough that the buffer can
// live on the heap without mattering. It must not be a stack array -- the httpd
// task's stack is measured in single-digit kilobytes.
#[cfg(target_os = "espidf")]
const OTA_CHUNK: usize = 4096;

// Enough of the head of the image to cover the esp_image_header_t (24 bytes),
// the first segment header (8) and the start of the esp_app_desc_t that follows.
#[cfg(any(target_os = "espidf", test))]
const OTA_HEADER_PEEK: usize = 96;

// The real firmware is ~1MB. Anything this small is a wrong file, not a build.
#[cfg(any(target_os = "espidf", test))]
const OTA_MIN_IMAGE: u64 = 256 * 1024;

// Used only if the running slot cannot be interrogated; the real value comes
// from the partition table (see partitions.csv).
#[cfg(any(target_os = "espidf", test))]
const OTA_SLOT_FALLBACK: usize = 0x1F_0000;

// First byte of any esp-idf application image.
#[cfg(any(target_os = "espidf", test))]
const ESP_IMAGE_MAGIC: u8 = 0xE9;

// esp_app_desc_t.magic_word, at offset 0x20 of the image.
#[cfg(any(target_os = "espidf", test))]
const ESP_APP_DESC_MAGIC: u32 = 0xABCD_5432;

#[cfg(any(target_os = "espidf", test))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageProblem {
    BadMagic,
    WrongChip,
    NoAppDescriptor,
}

#[cfg(any(target_os = "espidf", test))]
impl ImageProblem {
    fn message(self) -> &'static str {
        match self {
            ImageProblem::BadMagic => "not an esp32 application image (bad magic byte)",
            ImageProblem::WrongChip => "that image was built for a different chip",
            ImageProblem::NoAppDescriptor => {
                "no application descriptor -- is that a bootloader or a merged image?"
            }
        }
    }
}

/// Check as much of the image header as has arrived so far. Called repeatedly
/// as the first chunks come in, so a short prefix must be treated as "not yet
/// known to be bad" rather than as an error.
#[cfg(any(target_os = "espidf", test))]
fn validate_image_prefix(head: &[u8]) -> Result<(), ImageProblem> {
    if !head.is_empty() && head[0] != ESP_IMAGE_MAGIC {
        return Err(ImageProblem::BadMagic);
    }
    if head.len() >= 14 {
        // esp_image_header_t.chip_id, u16 LE at offset 12. ESP32 is 0.
        if u16::from_le_bytes([head[12], head[13]]) != 0 {
            return Err(ImageProblem::WrongChip);
        }
    }
    if head.len() >= 36 {
        let magic = u32::from_le_bytes([head[32], head[33], head[34], head[35]]);
        if magic != ESP_APP_DESC_MAGIC {
            return Err(ImageProblem::NoAppDescriptor);
        }
    }
    Ok(())
}

#[cfg(any(target_os = "espidf", test))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UploadProblem {
    LengthRequired,
    TooSmall(u64),
    TooLarge(u64, usize),
}

#[cfg(any(target_os = "espidf", test))]
impl UploadProblem {
    fn status(self) -> u16 {
        match self {
            UploadProblem::LengthRequired => 411,
            UploadProblem::TooSmall(_) => 400,
            UploadProblem::TooLarge(_, _) => 413,
        }
    }

    fn message(self) -> String {
        match self {
            UploadProblem::LengthRequired => {
                "a Content-Length is required; this server cannot take a chunked body".to_string()
            }
            UploadProblem::TooSmall(n) => {
                format!("{} bytes is far too small to be this firmware", n)
            }
            UploadProblem::TooLarge(n, slot) => format!(
                "{} bytes will not fit the {} byte slot (a padded or merged image, perhaps?)",
                n, slot
            ),
        }
    }
}

/// Decide whether a body is worth starting an update for, before any of it is
/// read. The most likely real mistake is a padded or merged image, which is
/// flash-sized rather than app-sized, so that one gets its own hint.
#[cfg(any(target_os = "espidf", test))]
fn check_upload_size(content_len: Option<u64>, slot: usize) -> Result<usize, UploadProblem> {
    let len = content_len.ok_or(UploadProblem::LengthRequired)?;
    if len < OTA_MIN_IMAGE {
        return Err(UploadProblem::TooSmall(len));
    }
    if len > slot as u64 {
        return Err(UploadProblem::TooLarge(len, slot));
    }
    Ok(len as usize)
}

#[cfg(any(target_os = "espidf", test))]
fn ota_progress_percent(written: usize, total: usize) -> u8 {
    if total == 0 {
        return 0;
    }
    ((written as u64 * 100 / total as u64).min(100)) as u8
}

// --------------------------------------------------------
// ROLLBACK
// --------------------------------------------------------
// esp-idf boots a freshly written image in a provisional state and reverts to
// the previous one unless the new image says it is working. What counts as
// "working" here is whatever the light was already doing when the update was
// accepted: an update delivered over the network has to get back on the
// network, and one delivered through the setup access point has to bring that
// access point back.

#[cfg(target_os = "espidf")]
const OTA_NAMESPACE: &str = "ota_cfg";

#[cfg(any(target_os = "espidf", test))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OtaExpect {
    /// Nothing to prove -- flashed over serial, or already confirmed.
    Nothing,
    Station,
    Ap,
}

#[cfg(any(target_os = "espidf", test))]
impl OtaExpect {
    /// Anything unrecognised degrades to "nothing to prove". A value written by
    /// some future firmware, or a half-written byte, must never be read as a
    /// reason to roll back -- rolling back on a guess is worse than not
    /// rolling back at all.
    pub fn from_nvs(stored: Option<u8>) -> Self {
        match stored {
            Some(1) => OtaExpect::Station,
            Some(2) => OtaExpect::Ap,
            _ => OtaExpect::Nothing,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            OtaExpect::Nothing => 0,
            OtaExpect::Station => 1,
            OtaExpect::Ap => 2,
        }
    }
}

#[cfg(any(target_os = "espidf", test))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootOutcome {
    /// Joined the stored network and the portal is serving on it.
    Station,
    /// Joined, but the http server would not start.
    StationNoPortal,
    /// In setup mode because someone held BOOT, not because anything failed.
    ApRequested,
    /// In setup mode because the stored network could not be reached.
    ApFallback,
    /// Setup mode is up but its portal would not start.
    ApNoPortal,
}

#[cfg(any(target_os = "espidf", test))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HealthVerdict {
    MarkValid,
    RollBack,
}

/// The whole rollback decision.
#[cfg(any(target_os = "espidf", test))]
fn health_verdict(expect: OtaExpect, outcome: BootOutcome) -> HealthVerdict {
    use BootOutcome::*;
    use HealthVerdict::*;
    match (expect, outcome) {
        // Nothing was promised, so nothing can be broken.
        (OtaExpect::Nothing, _) => MarkValid,

        (OtaExpect::Station, Station) => MarkValid,
        // Someone held the button; that is a person choosing setup mode, not
        // the new image failing to reach the network.
        (OtaExpect::Station, ApRequested) => MarkValid,
        // This is the case the whole feature exists for. The boot-time
        // fallback into setup mode would otherwise make an image that cannot
        // reach the network look perfectly healthy, forever.
        (OtaExpect::Station, ApFallback) => RollBack,
        // On the network but unreachable: no way to replace it over the air,
        // which is the definition of needing a rollback.
        (OtaExpect::Station, StationNoPortal) => RollBack,
        (OtaExpect::Station, ApNoPortal) => RollBack,

        // An update delivered over the setup access point has no network to
        // rejoin, so only a portal that will not start counts as failure.
        (OtaExpect::Ap, ApNoPortal) => RollBack,
        (OtaExpect::Ap, _) => MarkValid,
    }
}

// Where the running image's promise is kept across the reboot that follows an
// update. Written just before the boot pointer moves, read on the next boot,
// and cleared as soon as a verdict is reached.
#[cfg(target_os = "espidf")]
fn read_ota_expectation(nvs_partition: &EspDefaultNvsPartition) -> OtaExpect {
    match EspNvs::new(nvs_partition.clone(), OTA_NAMESPACE, true) {
        Ok(nvs) => OtaExpect::from_nvs(nvs.get_u8("expect").ok().flatten()),
        Err(_) => OtaExpect::Nothing,
    }
}

#[cfg(target_os = "espidf")]
fn write_ota_expectation(nvs_partition: &EspDefaultNvsPartition, expect: OtaExpect) {
    if let Ok(nvs) = EspNvs::new(nvs_partition.clone(), OTA_NAMESPACE, true) {
        let _ = nvs.set_u8("expect", expect.as_u8());
    }
}

#[cfg(target_os = "espidf")]
fn clear_ota_expectation(nvs_partition: &EspDefaultNvsPartition) {
    if let Ok(mut nvs) = EspNvs::new(nvs_partition.clone(), OTA_NAMESPACE, true) {
        let _ = nvs.remove("expect");
    }
}

// Set once a verdict has been reached, so the watchdog below knows not to fire.
#[cfg(target_os = "espidf")]
static HEALTH_SETTLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Only one update at a time. Two interleaved writes into the same slot produce
// something shaped like firmware that is not firmware.
#[cfg(target_os = "espidf")]
static OTA_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Deliver the verdict on the running image and act on it. Does not return in
/// the rollback case -- esp-idf reboots into the previous image.
#[cfg(target_os = "espidf")]
fn settle_health(expect: OtaExpect, outcome: BootOutcome, nvs_partition: &EspDefaultNvsPartition) {
    use std::sync::atomic::Ordering;
    if HEALTH_SETTLED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Clear first, in both branches. After a rollback the *previous* image
    // boots this same code; a surviving expectation would make it roll back
    // again the moment the network happened to be down, and the two slots
    // would take turns rejecting each other forever.
    clear_ota_expectation(nvs_partition);

    match health_verdict(expect, outcome) {
        HealthVerdict::MarkValid => {
            if let Ok(mut ota) = EspOta::new() {
                match ota.mark_running_slot_valid() {
                    Ok(()) => println!("> ota: image confirmed ({:?} after {:?})", expect, outcome),
                    Err(e) => println!("> ota: could not confirm image: {:?}", e),
                }
            }
        }
        HealthVerdict::RollBack => {
            println!(
                "> ota: this image was supposed to reach {:?} and got {:?}; rolling back",
                expect, outcome
            );
            if let Ok(mut ota) = EspOta::new() {
                // Returns only on failure, e.g. when there is no previous
                // image to go back to.
                let e = ota.mark_running_slot_invalid_and_reboot();
                println!("> ota: rollback failed: {:?}; carrying on", e);
            }
        }
    }
}

/// Backstop for the paths that never reach a verdict -- no peripherals, no NVS,
/// no wifi driver -- which would otherwise sit provisionally-booted forever,
/// neither confirmed nor rolled back.
#[cfg(target_os = "espidf")]
fn arm_health_deadline(expect: OtaExpect, nvs_partition: EspDefaultNvsPartition) {
    use std::sync::atomic::Ordering;
    if expect == OtaExpect::Nothing {
        return;
    }
    let _ = std::thread::Builder::new().stack_size(4096).spawn(move || {
        FreeRtos::delay_ms(180_000);
        if HEALTH_SETTLED.load(Ordering::SeqCst) {
            return;
        }
        println!("> ota: three minutes with no verdict; treating that as a failure");
        settle_health(expect, BootOutcome::ApNoPortal, &nvs_partition);
    });
}

/// Size of the slot an update would be written into.
#[cfg(target_os = "espidf")]
fn ota_slot_size() -> usize {
    let part = unsafe { esp_idf_svc::sys::esp_ota_get_next_update_partition(core::ptr::null()) };
    if part.is_null() {
        OTA_SLOT_FALLBACK
    } else {
        unsafe { (*part).size as usize }
    }
}

/// Label of the app partition currently running, for the version endpoint.
#[cfg(target_os = "espidf")]
fn running_slot_label() -> String {
    let part = unsafe { esp_idf_svc::sys::esp_ota_get_running_partition() };
    if part.is_null() {
        return "?".to_string();
    }
    let label = unsafe { core::ffi::CStr::from_ptr((*part).label.as_ptr()) };
    label.to_string_lossy().into_owned()
}

/// The esp-idf version this image was built against, from the descriptor
/// esp-idf itself writes into the image.
#[cfg(target_os = "espidf")]
fn running_idf_version() -> String {
    let desc = unsafe { esp_idf_svc::sys::esp_app_get_description() };
    if desc.is_null() {
        return String::new();
    }
    let raw = unsafe { core::ffi::CStr::from_ptr((*desc).idf_ver.as_ptr()) };
    raw.to_string_lossy().into_owned()
}

/// The SHA-256 esp-idf stamps into the image, as hex. This is what identifies
/// one build from another: the version string does not change while you are
/// iterating, and the descriptor's own version field is a git-describe of
/// esp-idf-sys's dummy cmake project rather than of this crate.
#[cfg(target_os = "espidf")]
fn running_elf_sha256() -> String {
    let desc = unsafe { esp_idf_svc::sys::esp_app_get_description() };
    if desc.is_null() {
        return String::new();
    }
    let sha = unsafe { (*desc).app_elf_sha256 };
    let mut out = String::with_capacity(64);
    for b in sha {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

// --------------------------------------------------------
// HTTP HELPERS (ESP32 ONLY)
// --------------------------------------------------------
// Plain-text reply for the endpoints a script talks to, where the status code
// carries the meaning and the body just says why.
#[cfg(target_os = "espidf")]
fn plain_response(
    req: Request<&mut EspHttpConnection<'_>>,
    status: u16,
    message: &str,
) -> Result<(), EspIOError> {
    let mut res = req.into_response(status, None, &[("Content-Type", "text/plain")])?;
    res.write_all(message.as_bytes())?;
    res.write_all(b"\n")?;
    Ok(())
}

// Releases the single-flight flag however the handler leaves.
#[cfg(target_os = "espidf")]
struct OtaInFlight;

#[cfg(target_os = "espidf")]
impl Drop for OtaInFlight {
    fn drop(&mut self) {
        OTA_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(target_os = "espidf")]
fn read_body<R>(req: &mut R) -> String
where
    R: Read + Headers,
{
    let len = req.content_len().unwrap_or(0) as usize;
    // Cap the body we will read to something sane for this form.
    let len = len.min(4096);
    let mut buf = vec![0u8; len];
    if len > 0 {
        let _ = req.read_exact(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

// Parse an application/x-www-form-urlencoded body into key/value pairs.
#[cfg(target_os = "espidf")]
fn parse_form(body: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut kv = pair.splitn(2, '=');
        let k = kv.next().unwrap_or("");
        let v = kv.next().unwrap_or("");
        if !k.is_empty() {
            map.insert(urldecode(k), urldecode(v));
        }
    }
    map
}

// Write a string key only if the stored value differs, sparing NVS wear.
#[cfg(target_os = "espidf")]
fn write_str_if_changed(nvs: &mut EspNvs<esp_idf_svc::nvs::NvsDefault>, key: &str, val: &str) {
    // Large enough for the longest value we store: 64 chars of UTF-8 marquee
    // text (up to 256 bytes) plus NVS's nul terminator, with headroom.
    let mut buf = [0u8; 320];
    // Compute whether the value is unchanged into an owned bool so no borrow
    // of `nvs`/`buf` from get_str lingers across the following set_str call.
    let unchanged = {
        let current = nvs.get_str(key, &mut buf).ok().flatten();
        current.map(|s| s == val).unwrap_or(false)
    };
    if unchanged {
        return;
    }
    let _ = nvs.set_str(key, val);
}

#[cfg(target_os = "espidf")]
fn write_u8_if_changed(nvs: &mut EspNvs<esp_idf_svc::nvs::NvsDefault>, key: &str, val: u8) {
    if nvs.get_u8(key).ok().flatten() == Some(val) {
        return;
    }
    let _ = nvs.set_u8(key, val);
}

#[cfg(target_os = "espidf")]
fn write_u16_if_changed(nvs: &mut EspNvs<esp_idf_svc::nvs::NvsDefault>, key: &str, val: u16) {
    if nvs.get_u16(key).ok().flatten() == Some(val) {
        return;
    }
    let _ = nvs.set_u16(key, val);
}

// Build the setup page. Reflects current live config so the form shows what the
// device is actually doing (saved-or-previewed).
#[cfg(any(target_os = "espidf", test))]
fn render_portal(
    ssids: &[String],
    display: &DisplayDriver,
    listen_port: u16,
    mode: &PortalMode,
    auth: &AuthPolicy,
    firmware: &RunningFirmware,
) -> String {
    let cfg = display.marquee_config();
    // An unconfigured portal is open to everyone who can reach it, which is
    // worth saying loudly rather than burying in the form below.
    let auth_note = if auth.is_configured() {
        String::new()
    } else {
        "<div class=\"status failed\">No portal password is set, so anyone on this network \
         can reconfigure this light. Firmware uploads stay disabled until you set one.</div>"
            .to_string()
    };
    let auth_user = html_escape(&auth.user);
    // Uploading firmware runs whatever is uploaded, so unlike the rest of the
    // page it is not offered at all until there is a password to gate it.
    let firmware_control = if auth.is_configured() {
        "<div class=\"form-group\">\n        <input type=\"file\" id=\"fw\" accept=\".bin\" />\n      </div>\n               <div id=\"fw_status\" class=\"status idle\">Choose a .bin built by <code>cargo build --release</code>.</div>\n               <button onclick=\"uploadFirmware()\">Upload firmware</button>"
            .to_string()
    } else {
        "<div class=\"status failed\">Set a portal password above before uploading firmware.</div>"
            .to_string()
    };
    let fw_version = html_escape(&firmware.version);
    let fw_slot = html_escape(&firmware.slot);
    let (mode_note, reboot_note) = match mode {
        PortalMode::Setup => (String::new(), "The setup portal will close.".to_string()),
        PortalMode::Connected { ip } => (
            format!(
                "<div class=\"status success\">Serving from the network at {}. Marquee changes \
                 apply as soon as they are saved. A new Wi-Fi network is tried after the \
                 reboot; if it cannot be reached the light falls back to its own setup \
                 access point.</div>",
                html_escape(ip)
            ),
            "The light will be back on the network in a few seconds.".to_string(),
        ),
    };
    let text = cfg.text;
    let speed_ms = cfg.speed_ms;
    let orientation = cfg.orientation;
    let direction = cfg.direction;
    let panel_margin = cfg.panel_margin;
    let margin_streams = cfg.margin_applies_streams;
    let (vertical, inverted) = orientation_parts(orientation);

    let mut options = String::new();
    options.push_str("<option value=\"\" selected>(keep current network)</option>");
    for ssid in ssids {
        let safe = html_escape(ssid);
        options.push_str(&format!("<option value=\"{0}\">{0}</option>", safe));
    }

    let safe_text = html_escape(&text);
    let dir_options = direction_options_html(orientation, direction);
    let sel_layout = |v: bool| if v == vertical { "selected" } else { "" };
    let sel_flip = |i: bool| if i == inverted { "selected" } else { "" };
    let margin_checked = if margin_streams { "checked" } else { "" };

    format!(
        r#"<!DOCTYPE html>
<html>
  <head>
    <title>Traffic Light Setup</title>
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <style>
      body {{ font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif; background:#f4f4f9; margin:0; padding:20px; }}
      .container {{ background:#fff; padding:24px; border-radius:10px; box-shadow:0 4px 6px rgba(0,0,0,.1); max-width:440px; margin:0 auto 20px; box-sizing:border-box; }}
      h2 {{ margin-top:0; color:#333; }}
      h3 {{ color:#333; border-bottom:1px solid #eee; padding-bottom:6px; }}
      .form-group {{ text-align:left; margin-bottom:15px; }}
      label {{ display:block; margin-bottom:5px; color:#666; font-weight:bold; }}
      select, input[type="text"], input[type="password"], input[type="number"] {{ width:100%; padding:10px; border:1px solid #ccc; border-radius:5px; box-sizing:border-box; font-size:16px; }}
      button {{ background:#007bff; color:#fff; border:none; padding:12px 20px; font-size:16px; border-radius:5px; cursor:pointer; width:100%; margin-top:10px; }}
      button:hover {{ background:#0056b3; }}
      .status {{ padding:10px; border-radius:5px; margin-bottom:15px; font-weight:bold; }}
      .status.idle {{ background:#e9ecef; color:#495057; }}
      .status.testing {{ background:#fff3cd; color:#856404; }}
      .status.success {{ background:#d4edda; color:#155724; }}
      .status.failed {{ background:#f8d7da; color:#721c24; }}
      .hint {{ font-weight:normal; color:#888; font-size:13px; }}
    </style>
    <script>
      document.cookie.split(";").forEach(function(c) {{
        document.cookie = c.replace(/^ +/, "").replace(/=.*/, "=;expires=" + new Date().toUTCString() + ";path=/");
      }});

      // Snapshot of the marquee config as first rendered, so we can tell whether
      // anything changed when the user asks to reboot.
      var SAVED = {{
        mq_text: {saved_text_js},
        mq_speed_ms: "{speed_ms}",
        mq_orient: "{orient}",
        mq_dir: "{dir}",
        panel_margin: "{margin}",
        margin_streams: "{margin_streams_val}",
        listen_port: "{port}"
      }};

      function post(path, data) {{
        var body = Object.keys(data).map(function(k) {{
          return encodeURIComponent(k) + "=" + encodeURIComponent(data[k]);
        }}).join("&");
        return fetch(path, {{ method:"POST", headers:{{"Content-Type":"application/x-www-form-urlencoded"}}, body:body }});
      }}

      // current orientation code from the two selects
      function orientCode() {{
        var vertical = document.getElementById("mq_layout").value === "1";
        var inverted = document.getElementById("mq_flip").value === "1";
        if (!vertical && !inverted) return "0";
        if (!vertical && inverted)  return "2";
        if (vertical && !inverted)  return "1";
        return "3";
      }}

      function marqueeData() {{
        return {{
          mq_text: document.getElementById("mq_text").value,
          mq_speed_ms: document.getElementById("mq_speed_ms").value,
          mq_orient: orientCode(),
          mq_dir: document.getElementById("mq_dir").value,
          panel_margin: document.getElementById("panel_margin").value,
          margin_streams: document.getElementById("margin_streams").checked ? "1" : "0"
        }};
      }}

      // live preview: push marquee config to the device on any change (no save)
      function preview() {{ post("/preview", marqueeData()); }}

      // swap the direction choices to match the chosen orientation
      var DIRS = {{
        horizontal: [["1","Right to left"],["0","Left to right"]],
        vertical:   [["2","Top to bottom"],["3","Bottom to top"]]
      }};
      function refreshDirections() {{
        var vertical = document.getElementById("mq_layout").value === "1";
        var opts = vertical ? DIRS.vertical : DIRS.horizontal;
        var sel = document.getElementById("mq_dir");
        var prev = sel.value;
        sel.innerHTML = "";
        opts.forEach(function(pair) {{
          var opt = document.createElement("option");
          opt.value = pair[0]; opt.textContent = pair[1];
          if (pair[0] === prev) opt.selected = true;
          sel.appendChild(opt);
        }});
        preview();
      }}

      // whether the marquee form differs from the last-saved snapshot
      function marqueeChanged() {{
        var d = marqueeData();
        d.listen_port = document.getElementById("listen_port").value;
        return d.mq_text !== SAVED.mq_text
            || d.mq_speed_ms !== SAVED.mq_speed_ms
            || d.mq_orient !== SAVED.mq_orient
            || d.mq_dir !== SAVED.mq_dir
            || d.panel_margin !== SAVED.panel_margin
            || d.margin_streams !== SAVED.margin_streams
            || d.listen_port !== SAVED.listen_port;
      }}

      // Firmware goes up as the raw file, not multipart: the device reads the
      // body straight into flash, and the same endpoint then works from a
      // shell with `curl --data-binary @app.bin`. XMLHttpRequest rather than
      // fetch() because only XHR reports upload progress, and a megabyte over
      // wifi is long enough that a progress bar is the difference between
      // "working" and "hung".
      function uploadFirmware() {{
        var f = document.getElementById("fw").files[0];
        var box = document.getElementById("fw_status");
        if (!f) {{ alert("Choose a .bin first."); return; }}
        if (!confirm("Upload " + f.name + " (" + Math.round(f.size / 1024) + " KB) and reboot into it?")) return;
        var xhr = new XMLHttpRequest();
        xhr.open("POST", "/ota", true);
        xhr.setRequestHeader("Content-Type", "application/octet-stream");
        xhr.upload.onprogress = function(e) {{
          if (!e.lengthComputable) return;
          box.className = "status testing";
          box.textContent = "Uploading " + Math.floor(100 * e.loaded / e.total) + "% - watch the lamps.";
        }};
        xhr.onload = function() {{
          if (xhr.status === 200) {{
            box.className = "status success";
            box.textContent = "Written. Rebooting into it; this page will come back in a few seconds.";
            setTimeout(function() {{ location.reload(); }}, 15000);
          }} else {{
            box.className = "status failed";
            box.textContent = "Refused (" + xhr.status + "): " + xhr.responseText;
          }}
        }};
        xhr.onerror = function() {{
          box.className = "status failed";
          box.textContent = "The connection dropped during the upload. Nothing was changed.";
        }};
        xhr.send(f);
      }}

      // The portal password is saved on its own rather than through the reboot
      // path: it takes effect on the next request, so there is nothing to
      // reboot for, and bundling a secret into the marquee payload would mean
      // re-sending it on every unrelated save.
      function savePortalAuth() {{
        var pass = document.getElementById("portal_pass").value;
        if (!pass) {{ alert("Enter a new password first."); return; }}
        var user = document.getElementById("portal_user").value;
        post("/save", {{ portal_user: user, portal_pass: pass }}).then(function() {{
          alert("Password set. The browser will ask for it on the next page load.");
          location.reload();
        }});
      }}

      function clearPortalAuth() {{
        if (!confirm("Remove the portal password? Anyone on this network will then be able to reconfigure and reflash this light.")) return;
        post("/save", {{ portal_clear: "1" }}).then(function() {{ location.reload(); }});
      }}

      // Single action: reboot. If the marquee settings or a wifi network changed,
      // offer to save first; otherwise just confirm the reboot.
      function doReboot() {{
        var ssid = document.getElementById("ssid").value;
        var changed = marqueeChanged() || ssid;
        if (changed) {{
          if (!confirm("You have unsaved changes. Save them and reboot now?")) return;
          var d = marqueeData();
          d.listen_port = document.getElementById("listen_port").value;
          if (ssid) {{ d.ssid = ssid; d.password = document.getElementById("password").value; }}
          post("/save", d).then(function(r) {{ return r.text(); }}).then(function(t) {{
            if (t === "saved-testing") {{
              // a wifi test was queued; show progress instead of rebooting
              pollStatus();
            }} else {{
              post("/reboot", {{}});
            }}
          }});
        }} else {{
          if (!confirm("Reboot now? {reboot_note}")) return;
          post("/reboot", {{}});
        }}
      }}

      // after a wifi save, poll /status; the AP drops briefly during the test,
      // so we tolerate fetch errors and keep trying, then reload to show result.
      function pollStatus() {{
        var tries = 0;
        var box = document.getElementById("wifi_status");
        box.className = "status testing";
        box.textContent = "Testing connection... (your device may briefly disconnect)";
        var iv = setInterval(function() {{
          tries++;
          fetch("/status").then(function(r) {{ return r.text(); }}).then(function(t) {{
            if (t === "success" || t === "failed") {{
              clearInterval(iv);
              location.reload();
            }}
          }}).catch(function() {{ /* AP down mid-test; keep waiting */ }});
          if (tries > 40) {{ clearInterval(iv); location.reload(); }} // ~20s cap
        }}, 500);
      }}

      window.addEventListener("DOMContentLoaded", refreshDirections);
    </script>
  </head>
  <body>
    <div class="container">
      <h2>Traffic Light Setup</h2>
      {mode_note}
      {auth_note}

      <h3>Wi-Fi</h3>
      <div id="wifi_status" class="status {status_class}">{status_text}</div>
      <div class="form-group">
        <label>Network (SSID) <span class="hint">(leave as "keep current" to not change it)</span></label>
        <select id="ssid">{options}</select>
      </div>
      <div class="form-group">
        <label>Password <span class="hint">(only used when changing network)</span></label>
        <input type="password" id="password" />
      </div>

      <h3>Marquee</h3>
      <div class="form-group">
        <label>Text <span class="hint">(max {text_max} characters)</span></label>
        <input type="text" id="mq_text" maxlength="{text_max}" value="{safe_text}" oninput="preview()" />
      </div>
      <div class="form-group">
        <label>Step delay <span class="hint">(milliseconds per pixel; {speed_min}&ndash;{speed_max}, lower is faster)</span></label>
        <input type="number" id="mq_speed_ms" min="{speed_min}" max="{speed_max}" value="{speed_ms}" oninput="preview()" />
      </div>
      <div class="form-group">
        <label>Layout</label>
        <select id="mq_layout" onchange="refreshDirections()">
          <option value="0" {sel_h}>Horizontal</option>
          <option value="1" {sel_v}>Vertical</option>
        </select>
      </div>
      <div class="form-group">
        <label>Flip</label>
        <select id="mq_flip" onchange="preview()">
          <option value="0" {sel_up}>Upright</option>
          <option value="1" {sel_inv}>Inverted</option>
        </select>
      </div>
      <div class="form-group">
        <label>Scroll direction</label>
        <select id="mq_dir" onchange="preview()">{dir_options}</select>
      </div>
      <div class="form-group">
        <label>Panel margin <span class="hint">(dead pixels between the 3 lamps; 0&ndash;{margin_max})</span></label>
        <input type="number" id="panel_margin" min="0" max="{margin_max}" value="{margin}" oninput="preview()" />
      </div>
      <div class="form-group">
        <label style="font-weight:normal;">
          <input type="checkbox" id="margin_streams" {margin_checked} onchange="preview()" style="width:auto;" />
          Apply panel margin to incoming streams
        </label>
        <span class="hint">If on, streamed frames must include the gap pixels (they'll be dropped). If off, streams fill all 30&times;10 as one contiguous area.</span>
      </div>
      <div class="form-group">
        <label>Listening port <span class="hint">(applies after reboot)</span></label>
        <input type="number" id="listen_port" min="1" max="65535" value="{port}" />
      </div>

      <h3>Portal access</h3>
      <div class="form-group">
        <label>Username</label>
        <input type="text" id="portal_user" value="{auth_user}" />
      </div>
      <div class="form-group">
        <label>New password <span class="hint">(leave blank to keep the current one)</span></label>
        <input type="password" id="portal_pass" />
      </div>
      <button onclick="savePortalAuth()">Set portal password</button>
      <button onclick="clearPortalAuth()">Remove password</button>

      <h3>Firmware</h3>
      <div class="form-group">
        <label>Running <span class="hint">(version, and which of the two slots it booted from)</span></label>
        <div>{fw_version} from {fw_slot}</div>
      </div>
      {firmware_control}

      <button onclick="doReboot()">Reboot</button>
    </div>
  </body>
</html>"#,
        mode_note = mode_note,
        auth_note = auth_note,
        auth_user = auth_user,
        firmware_control = firmware_control,
        fw_version = fw_version,
        fw_slot = fw_slot,
        reboot_note = reboot_note,
        status_class = wifi_status_class(display),
        status_text = wifi_status_text(display),
        options = options,
        text_max = MARQUEE_TEXT_MAX,
        safe_text = safe_text,
        saved_text_js = js_string(&text),
        speed_ms = speed_ms,
        speed_min = MARQUEE_SPEED_MS_MIN,
        speed_max = MARQUEE_SPEED_MS_MAX,
        orient = orientation,
        dir = direction,
        dir_options = dir_options,
        sel_h = sel_layout(false),
        sel_v = sel_layout(true),
        sel_up = sel_flip(false),
        sel_inv = sel_flip(true),
        margin = panel_margin,
        margin_max = PANEL_MARGIN_MAX,
        margin_checked = margin_checked,
        margin_streams_val = if margin_streams { 1 } else { 0 },
        port = listen_port,
    )
}

// Direction <option> list appropriate for the given orientation, marking the
// current selection.
#[cfg(any(target_os = "espidf", test))]
fn direction_options_html(orientation: u8, current: u8) -> String {
    let pairs: &[(u8, &str)] = if orientation_is_vertical(orientation) {
        &[
            (DIR_TOP_TO_BOTTOM, "Top to bottom"),
            (DIR_BOTTOM_TO_TOP, "Bottom to top"),
        ]
    } else {
        &[
            (DIR_RIGHT_TO_LEFT, "Right to left"),
            (DIR_LEFT_TO_RIGHT, "Left to right"),
        ]
    };
    let mut out = String::new();
    for (val, label) in pairs {
        let sel = if *val == current { "selected" } else { "" };
        out.push_str(&format!(
            "<option value=\"{}\" {}>{}</option>",
            val, sel, label
        ));
    }
    out
}

#[cfg(any(target_os = "espidf", test))]
fn wifi_status_class(display: &DisplayDriver) -> &'static str {
    match display.wifi_test() {
        WifiTestResult::Idle => "idle",
        WifiTestResult::Testing => "testing",
        WifiTestResult::Success => "success",
        WifiTestResult::Failed => "failed",
    }
}

#[cfg(any(target_os = "espidf", test))]
fn wifi_status_text(display: &DisplayDriver) -> &'static str {
    match display.wifi_test() {
        WifiTestResult::Idle => "Connected network unchanged unless you pick a new one.",
        WifiTestResult::Testing => "Testing connection...",
        WifiTestResult::Success => "Last test: connected successfully.",
        WifiTestResult::Failed => "Last test: connection failed.",
    }
}

#[cfg(any(target_os = "espidf", test))]
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// Encode a Rust string as a safe JSON/JS double-quoted string literal for
// embedding in the page script (used for the saved-text snapshot).
#[cfg(any(target_os = "espidf", test))]
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"), // avoid closing the <script> early
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// Percent-decode an application/x-www-form-urlencoded token. Bytes are
// accumulated into a buffer and interpreted as UTF-8 at the end, so multibyte
// characters in SSIDs, passwords, and marquee text survive intact.
#[cfg(target_os = "espidf")]
fn urldecode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(target_os = "espidf")]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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
    0x02, 0x04, 0x08, 0x10, 0x20, // 92 backslash
    0x00, 0x41, 0x41, 0x7f, 0x00, // 93 ]
    0x04, 0x02, 0x01, 0x02, 0x04, // 94 ^
    0x40, 0x40, 0x40, 0x40, 0x40, // 95 _
    0x00, 0x01, 0x02, 0x04, 0x00, // 96 `
];

// The page renderer is pure, but the simulated DisplayDriver it needs only
// exists off-target, so these run on the host only.
#[cfg(all(test, not(target_os = "espidf")))]
mod portal_tests {
    use super::*;

    fn locked() -> AuthPolicy {
        AuthPolicy {
            user: AUTH_USER_DEFAULT.to_string(),
            pass: "hunter2".to_string(),
        }
    }

    fn open() -> AuthPolicy {
        AuthPolicy {
            user: AUTH_USER_DEFAULT.to_string(),
            pass: String::new(),
        }
    }

    fn page(mode: PortalMode) -> String {
        page_with(mode, &locked())
    }

    fn page_with(mode: PortalMode, auth: &AuthPolicy) -> String {
        let display = DisplayDriver::new_simulated();
        let firmware = RunningFirmware {
            version: "9.9.9".to_string(),
            slot: "ota_1".to_string(),
        };
        render_portal(&["lab".to_string()], &display, 4048, &mode, auth, &firmware)
    }

    #[test]
    fn connected_portal_says_where_it_is_served_from_and_what_a_reboot_does() {
        let p = page(PortalMode::Connected {
            ip: "192.168.1.110".to_string(),
        });
        assert!(p.contains("Serving from the network at 192.168.1.110"));
        assert!(p.contains("falls back to its own setup access point"));
        assert!(p.contains("Reboot now? The light will be back on the network"));
        assert!(!p.contains("The setup portal will close."));
    }

    #[test]
    fn setup_portal_keeps_its_original_wording() {
        let p = page(PortalMode::Setup);
        assert!(!p.contains("Serving from the network"));
        assert!(p.contains("Reboot now? The setup portal will close."));
    }

    #[test]
    fn connected_note_escapes_whatever_it_is_given() {
        let p = page(PortalMode::Connected {
            ip: "<script>".to_string(),
        });
        assert!(!p.contains("at <script>"));
        assert!(p.contains("at &lt;script&gt;"));
    }

    #[test]
    fn the_page_shows_what_is_running_and_offers_an_upload() {
        let p = page_with(PortalMode::Setup, &locked());
        assert!(p.contains("9.9.9 from ota_1"));
        assert!(p.contains("id=\"fw\""));
        assert!(p.contains("xhr.open(\"POST\", \"/ota\", true)"));
    }

    #[test]
    fn an_unprotected_portal_does_not_offer_firmware_upload() {
        // The endpoint refuses in this state anyway; not drawing the control
        // is so the page says why rather than failing when it is used.
        let p = page_with(PortalMode::Setup, &open());
        assert!(!p.contains("id=\"fw\""));
        assert!(p.contains("Set a portal password above before uploading firmware"));
    }

    #[test]
    fn a_portal_with_no_password_says_so_loudly() {
        let p = page_with(PortalMode::Setup, &open());
        assert!(p.contains("No portal password is set"));
    }

    #[test]
    fn a_portal_with_a_password_does_not_nag() {
        let p = page_with(PortalMode::Setup, &locked());
        assert!(!p.contains("No portal password is set"));
    }

    #[test]
    fn the_page_never_echoes_the_stored_password_back() {
        // The password field is write-only by design: the page offers somewhere
        // to type a new one and nothing that would leak the current one to a
        // browser cache, a screenshot or a shoulder.
        let p = page_with(PortalMode::Setup, &locked());
        assert!(!p.contains("hunter2"));
        assert!(p.contains("id=\"portal_pass\""));
    }

    #[test]
    fn the_username_is_escaped_like_everything_else() {
        let p = page_with(
            PortalMode::Setup,
            &AuthPolicy {
                user: "a\"><script>".to_string(),
                pass: "x".to_string(),
            },
        );
        assert!(!p.contains("a\"><script>"));
        assert!(p.contains("&lt;script&gt;"));
    }
}

// Authentication is the part of the portal where being wrong is expensive, and
// all of it is pure, so all of it is tested here rather than on the device.
#[cfg(all(test, not(target_os = "espidf")))]
mod auth_tests {
    use super::*;

    fn policy(user: &str, pass: &str) -> AuthPolicy {
        AuthPolicy {
            user: user.to_string(),
            pass: pass.to_string(),
        }
    }

    fn header(user: &str, pass: &str) -> String {
        // Encode with an independent implementation so the test is not just
        // base64_decode agreeing with itself.
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let raw = format!("{}:{}", user, pass).into_bytes();
        let mut out = String::from("Basic ");
        for chunk in raw.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn base64_decodes_the_rfc4648_vectors() {
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("YWRtaW46c2VjcmV0").unwrap(), b"admin:secret");
    }

    #[test]
    fn base64_rejects_what_is_not_base64() {
        assert!(base64_decode("Zm9").is_none(), "length not a multiple of 4");
        assert!(
            base64_decode("Zm9!").is_none(),
            "character outside alphabet"
        );
        assert!(base64_decode("Z=9v").is_none(), "padding in the middle");
        assert!(
            base64_decode("Zm==Zm9v").is_none(),
            "padding before the end"
        );
        assert!(base64_decode("Z===").is_none(), "three padding characters");
        // An over-long header must not make the device allocate for it.
        assert!(base64_decode(&"QUJD".repeat(200)).is_none());
    }

    #[test]
    fn basic_auth_splits_the_header() {
        assert_eq!(
            parse_basic_auth("Basic YWRtaW46c2VjcmV0"),
            Some(("admin".to_string(), "secret".to_string()))
        );
    }

    #[test]
    fn the_scheme_token_is_case_insensitive() {
        // RFC 7235: the auth scheme is a case-insensitive token, and real
        // clients do send "basic".
        assert!(parse_basic_auth("basic YWRtaW46c2VjcmV0").is_some());
        assert!(parse_basic_auth("BASIC YWRtaW46c2VjcmV0").is_some());
        assert!(parse_basic_auth("Bearer YWRtaW46c2VjcmV0").is_none());
    }

    #[test]
    fn a_password_may_contain_colons() {
        assert_eq!(
            parse_basic_auth(&header("admin", "a:b:c")),
            Some(("admin".to_string(), "a:b:c".to_string()))
        );
    }

    #[test]
    fn malformed_headers_are_rejected_rather_than_guessed_at() {
        assert!(parse_basic_auth("Basic").is_none(), "no credential at all");
        assert!(parse_basic_auth("Basic !!!!").is_none(), "not base64");
        assert!(
            parse_basic_auth(&{
                let mut h = String::from("Basic ");
                h.push_str("bm9jb2xvbg==");
                h
            })
            .is_none(),
            "decodes, but has no colon"
        );
    }

    #[test]
    fn ct_eq_agrees_with_ordinary_equality() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secrer"), "differs in the last byte");
        assert!(!ct_eq(b"secret", b"tecret"), "differs in the first byte");
        assert!(!ct_eq(b"secret", b"secretx"), "prefix is not a match");
        assert!(!ct_eq(b"", b"x"));
    }

    #[test]
    fn an_unconfigured_portal_lets_everyone_in() {
        // Deliberate: a firmware update must not lock an existing light out of
        // its own portal. The page nags and /ota refuses instead.
        let p = policy("admin", "");
        assert!(auth_ok(None, &p));
        assert!(auth_ok(Some(&header("nobody", "nothing")), &p));
    }

    #[test]
    fn a_configured_portal_wants_the_right_credentials() {
        let p = policy(AUTH_USER_DEFAULT, "hunter2");
        assert!(auth_ok(Some(&header(AUTH_USER_DEFAULT, "hunter2")), &p));
        assert!(
            !auth_ok(Some(&header("admin", "hunter3")), &p),
            "wrong password"
        );
        assert!(!auth_ok(Some(&header("root", "hunter2")), &p), "wrong user");
        assert!(!auth_ok(None, &p), "no header");
        assert!(!auth_ok(Some("Basic !!!"), &p), "unparseable header");
        assert!(!auth_ok(Some(&header("admin", "")), &p), "empty password");
    }

    #[test]
    fn every_route_goes_through_the_auth_guard() {
        // guarded() exists so that no handler carries its own copy of the auth
        // check and none can be registered without one. That is only true if
        // nothing calls fn_handler directly, which is a property of the source
        // rather than of any value, so it is asserted against the source.
        //
        // Written because the claim was made and then immediately broken: the
        // /version route went in with a bare fn_handler and served unauthenticated
        // on the real device until someone happened to curl it.
        let src = include_str!("main.rs");
        // Split so this test does not match its own source and count itself.
        let needle = concat!("server.", "fn_handler(");
        let direct = src.matches(needle).count();
        assert_eq!(
            direct, 1,
            "expected exactly one fn_handler call (the one inside guarded()); \
             a route registered directly would not be behind the password"
        );
    }

    #[test]
    fn a_password_that_is_a_prefix_of_the_real_one_is_not_enough() {
        // The bug this guards against is comparing only min(len) bytes.
        let p = policy("admin", "hunter2");
        assert!(!auth_ok(Some(&header("admin", "hunter")), &p));
        assert!(!auth_ok(Some(&header("admin", "hunter22")), &p));
    }
}

// Being wrong about any of these means a walk to the light with a usb cable,
// which is the whole reason the decisions were factored out to be testable.
#[cfg(all(test, not(target_os = "espidf")))]
mod ota_tests {
    use super::*;

    const SLOT: usize = 0x1F_0000;

    // A synthetic image header: magic, esp32 chip id, app descriptor magic.
    fn header_bytes() -> Vec<u8> {
        let mut head = vec![0u8; OTA_HEADER_PEEK];
        head[0] = ESP_IMAGE_MAGIC;
        head[12] = 0;
        head[13] = 0;
        head[32..36].copy_from_slice(&ESP_APP_DESC_MAGIC.to_le_bytes());
        head
    }

    #[test]
    fn a_real_looking_header_is_accepted() {
        assert_eq!(validate_image_prefix(&header_bytes()), Ok(()));
    }

    #[test]
    fn a_prefix_too_short_to_judge_is_not_yet_a_failure() {
        // The header arrives a chunk at a time, so "not enough bytes to tell"
        // has to mean "keep going", not "reject".
        let head = header_bytes();
        assert_eq!(validate_image_prefix(&[]), Ok(()));
        assert_eq!(validate_image_prefix(&head[..1]), Ok(()));
        assert_eq!(validate_image_prefix(&head[..13]), Ok(()));
        assert_eq!(validate_image_prefix(&head[..35]), Ok(()));
    }

    #[test]
    fn the_wrong_file_entirely_is_caught_on_the_first_byte() {
        let mut head = header_bytes();
        head[0] = b'#';
        assert_eq!(validate_image_prefix(&head), Err(ImageProblem::BadMagic));
        // ...and from a single byte, before anything is written to flash.
        assert_eq!(
            validate_image_prefix(&head[..1]),
            Err(ImageProblem::BadMagic)
        );
    }

    #[test]
    fn an_image_for_another_chip_is_refused() {
        let mut head = header_bytes();
        head[12] = 9; // esp32-c3 and friends
        assert_eq!(validate_image_prefix(&head), Err(ImageProblem::WrongChip));
    }

    #[test]
    fn a_bootloader_or_merged_image_is_refused() {
        let mut head = header_bytes();
        head[32..36].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(
            validate_image_prefix(&head),
            Err(ImageProblem::NoAppDescriptor)
        );
    }

    #[test]
    fn a_body_of_a_plausible_size_is_accepted() {
        assert_eq!(check_upload_size(Some(1_060_000), SLOT), Ok(1_060_000));
        assert_eq!(check_upload_size(Some(SLOT as u64), SLOT), Ok(SLOT));
    }

    #[test]
    fn a_chunked_body_is_refused_rather_than_read_forever() {
        assert_eq!(
            check_upload_size(None, SLOT),
            Err(UploadProblem::LengthRequired)
        );
        assert_eq!(check_upload_size(None, SLOT).unwrap_err().status(), 411);
    }

    #[test]
    fn something_far_too_small_to_be_firmware_is_refused() {
        assert_eq!(
            check_upload_size(Some(0), SLOT),
            Err(UploadProblem::TooSmall(0))
        );
        assert_eq!(
            check_upload_size(Some(4096), SLOT),
            Err(UploadProblem::TooSmall(4096))
        );
    }

    #[test]
    fn an_image_that_will_not_fit_is_refused_before_a_byte_is_written() {
        // The likely real mistake: a padded or merged image, which comes out
        // flash-sized rather than app-sized.
        let four_mb = 4 * 1024 * 1024;
        let problem = check_upload_size(Some(four_mb), SLOT).unwrap_err();
        assert_eq!(problem, UploadProblem::TooLarge(four_mb, SLOT));
        assert_eq!(problem.status(), 413);
        assert!(problem.message().contains("merged"));
        assert_eq!(
            check_upload_size(Some(SLOT as u64 + 1), SLOT)
                .unwrap_err()
                .status(),
            413
        );
    }

    #[test]
    fn progress_runs_from_nothing_to_everything() {
        assert_eq!(ota_progress_percent(0, 1000), 0);
        assert_eq!(ota_progress_percent(500, 1000), 50);
        assert_eq!(ota_progress_percent(1000, 1000), 100);
    }

    #[test]
    fn progress_survives_a_big_image_and_an_empty_one() {
        // written * 100 overflows a 32-bit multiply at these sizes, which is
        // why the arithmetic is done in u64.
        assert_eq!(ota_progress_percent(1_000_000, 1_063_430), 94);
        assert_eq!(ota_progress_percent(0, 0), 0, "no divide by zero");
        assert_eq!(ota_progress_percent(10, 5), 100, "clamped, not 200");
    }

    #[test]
    fn an_unknown_expectation_never_causes_a_rollback() {
        // A byte from some future firmware, or a half-written one, must not be
        // read as a reason to revert. Rolling back on a guess is worse than
        // not rolling back at all.
        for stored in [None, Some(0), Some(3), Some(7), Some(255)] {
            assert_eq!(OtaExpect::from_nvs(stored), OtaExpect::Nothing);
        }
        assert_eq!(OtaExpect::from_nvs(Some(1)), OtaExpect::Station);
        assert_eq!(OtaExpect::from_nvs(Some(2)), OtaExpect::Ap);
    }

    #[test]
    fn the_expectation_survives_a_round_trip_through_nvs() {
        for expect in [OtaExpect::Nothing, OtaExpect::Station, OtaExpect::Ap] {
            assert_eq!(OtaExpect::from_nvs(Some(expect.as_u8())), expect);
        }
    }

    #[test]
    fn an_image_that_promised_nothing_is_always_kept() {
        // Flashed over serial, or already confirmed once.
        for outcome in [
            BootOutcome::Station,
            BootOutcome::StationNoPortal,
            BootOutcome::ApRequested,
            BootOutcome::ApFallback,
            BootOutcome::ApNoPortal,
        ] {
            assert_eq!(
                health_verdict(OtaExpect::Nothing, outcome),
                HealthVerdict::MarkValid,
                "{:?}",
                outcome
            );
        }
    }

    #[test]
    fn an_image_pushed_over_the_network_must_get_back_on_it() {
        // This is the case the feature exists for: without it the boot-time
        // fallback into setup mode makes an unreachable image look healthy.
        assert_eq!(
            health_verdict(OtaExpect::Station, BootOutcome::ApFallback),
            HealthVerdict::RollBack
        );
        assert_eq!(
            health_verdict(OtaExpect::Station, BootOutcome::Station),
            HealthVerdict::MarkValid
        );
    }

    #[test]
    fn someone_holding_the_button_is_not_the_images_fault() {
        assert_eq!(
            health_verdict(OtaExpect::Station, BootOutcome::ApRequested),
            HealthVerdict::MarkValid
        );
    }

    #[test]
    fn an_image_nobody_could_replace_is_rolled_back() {
        // On the network but not serving: no way in over the air, which is
        // precisely what rollback is for.
        assert_eq!(
            health_verdict(OtaExpect::Station, BootOutcome::StationNoPortal),
            HealthVerdict::RollBack
        );
        assert_eq!(
            health_verdict(OtaExpect::Station, BootOutcome::ApNoPortal),
            HealthVerdict::RollBack
        );
    }

    #[test]
    fn an_image_delivered_through_the_access_point_only_owes_an_access_point() {
        // It has no network to rejoin, so requiring one would revert every
        // update ever delivered in setup mode.
        assert_eq!(
            health_verdict(OtaExpect::Ap, BootOutcome::ApRequested),
            HealthVerdict::MarkValid
        );
        assert_eq!(
            health_verdict(OtaExpect::Ap, BootOutcome::ApFallback),
            HealthVerdict::MarkValid
        );
        assert_eq!(
            health_verdict(OtaExpect::Ap, BootOutcome::Station),
            HealthVerdict::MarkValid
        );
        assert_eq!(
            health_verdict(OtaExpect::Ap, BootOutcome::ApNoPortal),
            HealthVerdict::RollBack
        );
    }
}
