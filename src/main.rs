use anyhow::Result;
use esp_idf_hal::io::{EspIOError, Write};
use log::*;
use rgb::RGB8;
use rgb_led::WS2812RMT;
use std::sync::{Arc, Mutex};
use toml_cfg::toml_config;
use url::form_urlencoded;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::{delay::FreeRtos, peripherals::Peripherals};
use esp_idf_svc::http::server::{EspHttpConnection, EspHttpServer, Method, Request};
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::sys::link_patches;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use esp_idf_svc::mdns::EspMdns;
#[toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
}
const CORS_HEADERS: &[(&str, &str)] = &[
    ("Access-Control-Allow-Origin",  "*"),
    ("Access-Control-Allow-Methods", "GET, OPTIONS"),
    ("Access-Control-Allow-Headers", "Content-Type"),
    ("Access-Control-Allow-Private-Network", "true"),

];
fn main() -> Result<()> {
    // Patch syscalls & init logger
    link_patches();
    EspLogger::initialize_default();

    // Load Wi‑Fi config from `Config.toml`
    let cfg = CONFIG;

    // Grab peripherals
    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    // Bring up Wi‑Fi in STA mode
    let sysloop = EspSystemEventLoop::take()?;
    let mut esp_wifi = EspWifi::new(peripherals.modem, sysloop.clone(), None)?;
    let mut wifi = BlockingWifi::wrap(&mut esp_wifi, sysloop)?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: cfg.wifi_ssid.try_into().unwrap(),
        password: cfg.wifi_psk.try_into().unwrap(),
        ..Default::default()
    }))?;
    info!("Starting Wi‑Fi...");
    wifi.start()?;
    wifi.connect()?;
    wifi.wait_netif_up()?;
    info!("Wi‑Fi connected!");
    
    let mut mdns =EspMdns::take()?;
    mdns.set_hostname("still-device")?;
    mdns.set_instance_name("Still LED Controller")?;

    mdns.add_service(
        Some("Initial Setup"),
        "_http",
        "_tcp",
        80,
        &[("path", "/led/color")],
    )?;



    // Initialize the NeoPixel driver on IO40 + RMT channel0
    let ws = WS2812RMT::new(pins.gpio40, peripherals.rmt.channel0)?;
    let ws = Arc::new(Mutex::new(ws));

    // Start the HTTP server
    let mut server = EspHttpServer::new(&Default::default())?;


    // OPTIONS preflight handler
    server.fn_handler(
        "/led/color",
        Method::Options,
        move |mut req: Request<&mut EspHttpConnection<'_>>| -> Result<(), EspIOError> {
            // 204 No Content is typical for preflight
            req.into_response(204, None, CORS_HEADERS)?;
            Ok(())
        },
    )?;

    // Handler for GET /led/color?r=...&g=...&b=...
    let ws_clone = ws.clone();
    server.fn_handler(
        "/led/color",
        Method::Get,
        move |mut req: Request<&mut EspHttpConnection<'_>>| -> Result<(), anyhow::Error> {
            // parse query stringf
            let uri = req.uri();
            let query = uri.splitn(2, '?').nth(1).unwrap_or("");
            let mut r = 0u8;
            let mut g = 0u8;
            let mut b = 0u8;

            for (k, v) in form_urlencoded::parse(query.as_bytes()) {
                let val = v.parse::<u8>().unwrap_or(0);
                match &*k {
                    "r" => r = val,
                    "g" => g = val,
                    "b" => b = val,
                    _ => {}
                }
            }

            // drive the LED
            {
                let mut drv = ws_clone.lock().unwrap();
                drv.set_pixel(RGB8::new(r, g, b))?;
            }

            // send response
            let mut resp = req.into_response(
                200,               // HTTP status code 200
                Some("OK"),        // reason phrase
                CORS_HEADERS,               // extra headers
            )?;
            resp.write_all(
                format!("OK: r={} g={} b={}", r, g, b).as_bytes()
            )?;
            Ok(())
        },
        
    )?;

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    info!("Our IP is {}", ip_info.ip);
    info!("HTTP server running; point your browser at http://{}:80/led/color?r=255&g=0&b=0", ip_info.ip);

    // never exit
    loop {
        FreeRtos::delay_ms(1_000);
    }
}
