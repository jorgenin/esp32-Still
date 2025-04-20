use anyhow::Result;
use esp_idf_sys as _; // must be first for esp-idf-sys
use esp_idf_svc::sys::link_patches;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::wifi::{EspWifi, BlockingWifi, Configuration, ClientConfiguration};
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::http::server::{EspHttpServer, EspHttpConnection, Method, Request};
use esp_idf_hal::{
    peripherals::Peripherals,
    gpio::{PinDriver, Output},
    delay::Ets,
};
use esp_idf_svc::hal::adc::{
    AdcDriver, AdcChannelDriver, config::Config as AdcConfig, // Use AdcDriver and AdcChannelDriver for oneshot
    attenuation
};
use esp_idf_sys::adc_atten_t; // Import the attenuation type
use log::*;
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use toml_cfg::toml_config;
use url::form_urlencoded;


#[toml_config]
pub struct Config {
    #[default("")] wifi_ssid: &'static str,
    #[default("")] wifi_psk: &'static str,
}

const CORS: &[(&str, &str)] = &[
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Methods", "GET, OPTIONS"),
    ("Access-Control-Allow-Headers", "Content-Type"),
    ("Access-Control-Allow-Private-Network", "true"),
];

struct Shared {
    setpoint: f32,    // 0.0..1.0
    rms_current: f32, // in A
}

use esp_idf_svc::io::Write;

fn main() -> Result<()> {
    // 1) syscalls & logging
    link_patches();
    EspLogger::initialize_default();
    let cfg = CONFIG;

    // 2) Wi‑Fi
    let peripherals = Peripherals::take().unwrap();
    let sysloop = EspSystemEventLoop::take()?;
    let mut esp_wifi = EspWifi::new(peripherals.modem, sysloop.clone(), None)?;
    let mut wifi = BlockingWifi::wrap(&mut esp_wifi, sysloop.clone())?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: cfg.wifi_ssid.try_into().unwrap(),
        password: cfg.wifi_psk.try_into().unwrap(),
        ..Default::default()
    }))?;
    info!("Starting Wi‑Fi...");
    wifi.start()?;
    wifi.connect()?;
    wifi.wait_netif_up()?;
    info!("Wi‑Fi up");

    let mut mdns = EspMdns::take()?;
    mdns.set_hostname("heater")?;
    mdns.set_instance_name("Heater Ctrl")?;
    mdns.add_service(Some("ctrl"), "_http", "_tcp", 80, &[])?;

    // 3) Shared state
    let shared = Arc::new(Mutex::new(Shared {
        setpoint: 0.0,
        rms_current: 0.0,
    }));

    // 4) ADC oneshot setup on a chosen GPIO
    // Onespawn works on both ADC1 and ADC2.
    // Let's use GPIO2, which is ADC1_CH2 on ESP32-S3 for example.
    // If you need to use GPIO17 (ADC2_CH6 on ESP32-S3), change `peripherals.adc1` to `peripherals.adc2`
    // and `peripherals.pins.gpio2` to `peripherals.pins.gpio17`.
    // We use DB_11 attenuation as in your original continuous setup.
    const ATTENUATION: adc_atten_t = attenuation::DB_11;

    // Initialize ADC driver. We enable calibration for better accuracy.
    let adc_driver = AdcDriver::new(peripherals.adc1, &AdcConfig::new().calibration(true))?;

    // Initialize ADC channel driver for the specific pin.
    // This needs to be mutable because reading consumes the channel ownership temporarily.
    let adc_pin: AdcChannelDriver<'static, { ATTENUATION }, _> =
        AdcChannelDriver::new(peripherals.pins.gpio2)?; // <--- Use the appropriate pin here

    // 5) Spawn measurement "task" as a thread
    {
        let shared = shared.clone();
        // move ownership of `adc_driver` and `adc_pin` into the thread
        let builder = thread::Builder::new().stack_size(8 * 1024);
        builder
            .spawn(move || measurement_task(adc_driver, adc_pin, shared))
            .unwrap();
    }

    // 6) Spawn SSR control "task" as a thread
    {
        let shared = shared.clone();
        let ssr = PinDriver::output(peripherals.pins.gpio5)?;
        let builder = thread::Builder::new().stack_size(4 * 1024);
        builder
            .spawn(move || ssr_control_task(ssr, shared))
            .unwrap();
    }

    // 7) HTTP server
    let mut server = EspHttpServer::new(&Default::default())?;
    // OPTIONS handlers
    for path in &["/heater/power", "/heater/current"] {
        server.fn_handler(
            path,
            Method::Options,
            move |req: Request<&mut EspHttpConnection>| -> Result<_, anyhow::Error> {
                req.into_response(204, None, CORS)?;
                Ok(())
            },
        )?;
    }
    // GET /heater/power?p=0.5 | p=off
    {
        let shared = shared.clone();
        server.fn_handler(
            "/heater/power",
            Method::Get,
            move |req: Request<&mut EspHttpConnection>| -> Result<_, anyhow::Error> {
                let query = req.uri().splitn(2, '?').nth(1).unwrap_or("");
                let mut new_sp = None;
                for (k, v) in form_urlencoded::parse(query.as_bytes()) {
                    if &*k == "p" {
                        new_sp = if &*v == "off" {
                            Some(0.0)
                        } else {
                            v.parse::<f32>().ok().map(|x| x.clamp(0.0, 1.0))
                        };
                    }
                }
                if let Some(sp) = new_sp {
                    shared.lock().unwrap().setpoint = sp;
                }
                let sp = shared.lock().unwrap().setpoint;
                let mut res = req.into_response(200, Some("OK"), CORS)?;
                res.write_all(format!("setpoint={:.2}", sp).as_bytes())?;
                Ok(())
            },
        )?;
    }
    // GET /heater/current
    {
        let shared = shared.clone();
        server.fn_handler(
            "/heater/current",
            Method::Get,
            move |req: Request<&mut EspHttpConnection>| -> Result<_, anyhow::Error> {
                let rms = shared.lock().unwrap().rms_current;
                let mut res = req.into_response(200, Some("OK"), CORS)?;
                res.write_all(format!("{{\"rms_current\":{:.3}}}", rms).as_bytes())?;
                Ok(())
            },
        )?;
    }

    let ip = wifi.wifi().sta_netif().get_ip_info()?.ip;
    info!("Server at http://{}:80", ip);

    // keep main alive
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

use esp_idf_hal::gpio::Gpio5;

/// measurement_task reads samples one by one using oneshot ADC,
/// accumulates a batch, computes RMS, stores in shared.
fn measurement_task(
    // AdcDriver and AdcChannelDriver need to be moved into the thread
    mut adc: AdcDriver<'static, impl esp_idf_hal::adc::config::Resolution>,
    mut adc_pin: AdcChannelDriver<'static, { ATTENUATION }, impl esp_idf_hal::gpio::ADCPin>,
    shared: Arc<Mutex<Shared>>,
) {
    const VREF: f32 = 3.3; // Or calibrate this if needed
    // ADC_MAX depends on resolution, with DB_11 attenuation default is 12-bit (4095)
    // Assuming 12-bit resolution here
    const ADC_MAX: f32 = 4095.0;
    const V_ZERO: f32 = 1.5;    // Your zero-current voltage bias
    const SENSE: f32 = 0.066;   // 66 mV/A sensor sensitivity
    const BATCH: usize = 100;   // Number of samples per RMS calculation batch

    let mut sum_sq = 0.0f32;
    let mut cnt = 0usize;

    info!("Measurement task started (oneshot mode)");

    loop {
        // In oneshot mode, we explicitly read samples one by one
        // We'll gather BATCH samples before calculating RMS
        for _ in 0..BATCH {
             match adc.read(&mut adc_pin) {
                Ok(raw_data) => {
                    // Convert raw u16 data to voltage then current
                    let v = raw_data as f32 / ADC_MAX * VREF; // Simple linear conversion, calibration helps accuracy
                    let i = (v - V_ZERO) / SENSE;
                    sum_sq += i * i;
                    cnt += 1;
                }
                Err(e) => {
                    error!("Error reading ADC: {:?}", e);
                    // Optionally break or add delay on error
                    Ets::delay_ms(10); // Wait a bit before trying again
                    continue; // Skip to the next sample attempt
                }
            }
        }

        // After collecting BATCH samples, calculate RMS and update shared state
        if cnt > 0 { // Ensure we collected at least one sample
            let rms = (sum_sq / cnt as f32).sqrt();
            shared.lock().unwrap().rms_current = rms;
            // Reset for the next batch
            sum_sq = 0.0;
            cnt = 0;
        } else {
             // Handle case where BATCH attempts resulted in 0 successful reads
             shared.lock().unwrap().rms_current = 0.0; // Or keep the last value, or indicate error
        }

        // Add a small delay between batches to not overload the CPU completely
        Ets::delay_ms(10); // Adjust this delay as needed
    }
}


/// SSR “slice” controller at 100 ms period
fn ssr_control_task(
    mut ssr: PinDriver<'_, Gpio5, Output>,
    shared: Arc<Mutex<Shared>>,
) {
    const PERIOD: u32 = 100; // ms
    info!("SSR control task started");
    loop {
        let sp = shared.lock().unwrap().setpoint;
        let on_ms = (sp * PERIOD as f32).round() as u32;
        if on_ms == 0 {
            let _ = ssr.set_low();
            Ets::delay_ms(PERIOD);
        } else if on_ms >= PERIOD {
            let _ = ssr.set_high();
            Ets::delay_ms(PERIOD);
        } else {
            let _ = ssr.set_high();
            Ets::delay_ms(on_ms);
            let _ = ssr.set_low();
            Ets::delay_ms(PERIOD - on_ms);
        }
    }
}
