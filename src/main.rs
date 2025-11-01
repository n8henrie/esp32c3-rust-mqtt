#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select, select3};
use embassy_net::{Config, DhcpConfig, Runner, StackResources, dns::DnsQueryType, tcp::TcpSocket};
use embassy_time::{Duration, Timer};

use defmt::{Debug2Format, Display2Format, info};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    rng::Rng,
    timer::timg::TimerGroup,
    tsens::{self, TemperatureSensor},
};
use esp_println as _;
use esp_wifi::{
    EspWifiController,
    config::PowerSaveMode,
    wifi::{ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiState},
};

use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::{publish_packet::QualityOfService::QoS1, reason_codes::ReasonCode},
    utils::rng_generator::CountingRng,
};

use esp_alloc as _;
#[macro_use]
extern crate alloc;

use thiserror::Error;

pub static CURRENT_RSSI: AtomicI32 = AtomicI32::new(0);

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

const MQTT_HOST: &str = env!("MQTT_HOST");
const MQTT_PORT: &str = env!("MQTT_PORT");

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

const KEEP_ALIVE_SECS: u16 = 12;
const SOCKET_TIMEOUT_SECS: u64 = 60;

const MQTT_CLIENT_ID: &str = env!("MQTT_CLIENT_ID");
const MQTT_USERNAME: &str = env!("MQTT_USERNAME");
const MQTT_PASSWORD: &str = env!("MQTT_PASSWORD");

const PUBLISH_TOPIC: &str = env!("PUBLISH_TOPIC");
const RECEIVE_TOPIC: &str = env!("RECEIVE_TOPIC");
const WILL_TOPIC: &str = env!("WILL_TOPIC");
const TEMP_TOPIC: &str = env!("TEMP_TOPIC");
const RSSI_TOPIC: &str = env!("RSSI_TOPIC");

#[allow(unused)]
#[derive(Debug, Error)]
enum Error {
    #[error("MQTT Network Error")]
    MqttNetwork,

    #[error("MQTT Error, reason code: `{0}`")]
    Mqtt(rust_mqtt::packet::v5::reason_codes::ReasonCode),

    #[error("DNS lookup error")]
    Dns,
}

impl defmt::Format for Error {
    fn format(&self, f: defmt::Formatter) {
        match self {
            Error::MqttNetwork | Error::Dns => self.format(f),
            Error::Mqtt(reasoncode) => defmt::write!(f, "{}", Display2Format(reasoncode)),
        }
    }
}

impl From<rust_mqtt::packet::v5::reason_codes::ReasonCode> for Error {
    fn from(reason_code: rust_mqtt::packet::v5::reason_codes::ReasonCode) -> Self {
        Error::Mqtt(reason_code)
    }
}

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz);
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut rng = Rng::new(peripherals.RNG);

    let esp_wifi_ctrl = &*mk_static!(
        EspWifiController<'static>,
        esp_wifi::init(timg0.timer0, rng).expect("couldn't init esp_wifi")
    );
    let (mut controller, interfaces) = esp_wifi::wifi::new(esp_wifi_ctrl, peripherals.WIFI)
        .expect("couldn't create wifi controller");
    controller
        .set_power_saving(PowerSaveMode::Maximum)
        .expect("couldn't set power save mode");
    let wifi_interface = interfaces.sta;

    let timg1 = TimerGroup::new(peripherals.TIMG1);
    esp_hal_embassy::init(timg1.timer0);

    let config = Config::dhcpv4(DhcpConfig::default());
    let seed = (u64::from(rng.random())) << 32 | u64::from(rng.random());

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    let mut led = Output::new(peripherals.GPIO8, Level::Low, OutputConfig::default());
    let mut button = Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    );
    let temperature_sensor =
        TemperatureSensor::new(peripherals.TSENS, tsens::Config::default()).unwrap();

    stack.wait_link_up().await;
    stack.wait_config_up().await;

    info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            break;
        }
    }

    // Flash the onboard led to show that we have the pin right
    // and to indicate network connection
    for _ in 0..10 {
        led.toggle();
        sleep(100).await;
    }

    // On my ESP32C3, the onboard LED is active low
    led.set_high();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    info!("Setting timeout");
    socket.set_timeout(Some(embassy_time::Duration::from_secs(SOCKET_TIMEOUT_SECS)));

    info!("Getting address");
    loop {
        let address = match stack
            .dns_query(MQTT_HOST, DnsQueryType::A)
            .await
            .map(|a| a[0])
        {
            Ok(address) => address,
            Err(e) => {
                info!("DNS lookup error: {}", e);
                continue;
            }
        };

        let port: u16 = MQTT_PORT.parse().expect("Couldn't parse MQTT_PORT as u16");
        let remote_endpoint = (address, port);
        info!("connecting to {}...", Debug2Format(&remote_endpoint));

        if let Err(e) = socket.connect(remote_endpoint).await {
            info!("connect error: {:?}", Debug2Format(&e));
            continue;
        }
        info!("connected");
        break;
    }

    let mut config = ClientConfig::new(
        rust_mqtt::client::client_config::MqttVersion::MQTTv5,
        CountingRng(20000),
    );
    config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
    config.add_client_id(MQTT_CLIENT_ID);
    config.max_packet_size = 100;
    config.keep_alive = KEEP_ALIVE_SECS;

    config.add_username(MQTT_USERNAME);
    config.add_password(MQTT_PASSWORD);

    config.add_will(WILL_TOPIC, b"death", false);

    let mut writebuf = [0; 128];
    let mut readbuf = [0; 128];
    let mut client = {
        let writebuf_len = writebuf.len();
        let readbuf_len = readbuf.len();
        MqttClient::<_, 5, _>::new(
            socket,
            &mut writebuf,
            writebuf_len,
            &mut readbuf,
            readbuf_len,
            config,
        )
    };

    match client.connect_to_broker().await {
        Ok(()) => {
            info!("Connected to broker");
        }
        Err(mqtt_error) => {
            if let ReasonCode::NetworkError = mqtt_error {
                info!("MQTT Network Error");
            } else {
                info!("Other MQTT Error: {:?}", Debug2Format(&mqtt_error));
            }
        }
    }

    info!("Publishing birth message to will topic {}", WILL_TOPIC);
    match client.send_message(WILL_TOPIC, b"birth", QoS1, false).await {
        Ok(()) => {
            info!("Message sent");
        }
        Err(e) => {
            info!(
                "Error sending message: {} ({:?})",
                Display2Format(&e),
                Debug2Format(&e)
            );
        }
    }

    info!("Subscribing to topic {}", RECEIVE_TOPIC);
    if let Err(e) = client.subscribe_to_topic(RECEIVE_TOPIC).await {
        info!(
            "Error subscribing to topic: {} ({})",
            Display2Format(&e),
            Debug2Format(&e)
        );
        // continue 'main;
    }
    info!("Subscribed");

    loop {
        match select3(
            client.receive_message(),
            button.wait_for_low(),
            sleep(4_000),
        )
        .await
        {
            Either3::First(result) => {
                match result {
                    Ok((_topic, message)) => {
                        let c: Option<char> = message.iter().next().copied().map(char::from);
                        match c {
                            Some('1') => led.set_level(Level::Low),
                            Some('0') => led.set_level(Level::High),
                            _ => {
                                info!("Invalid message: {}", message);
                            }
                        }
                    }

                    // reasons include:
                    // - no mqtt broker
                    Err(ReasonCode::NetworkError) => {
                        info!("Network error!");
                    }

                    Err(e) => {
                        info!(
                            "Error receiving message: {} ({})",
                            Display2Format(&e),
                            Debug2Format(&e)
                        );
                    }
                }
            }

            Either3::Second(()) => {
                // debounce
                sleep(100).await;
                button.wait_for_high().await;

                info!("Publishing message to topic {}", PUBLISH_TOPIC);
                match client.send_message(PUBLISH_TOPIC, b"42", QoS1, false).await {
                    Ok(()) => {
                        info!("Message sent");
                    }
                    Err(e) => {
                        info!(
                            "Error sending message: {} ({:?})",
                            Display2Format(&e),
                            Debug2Format(&e)
                        );
                    }
                }
            }

            Either3::Third(()) => {
                let rssi = format!("{}", CURRENT_RSSI.load(Ordering::Relaxed));
                info!("Publishing RSSI {} to {}", &*rssi, RSSI_TOPIC);

                match client
                    .send_message(RSSI_TOPIC, rssi.as_bytes(), QoS1, false)
                    .await
                {
                    Ok(()) => {
                        info!("Message sent");
                    }
                    Err(e) => {
                        info!(
                            "Error sending message: {} ({:?})",
                            Display2Format(&e),
                            Debug2Format(&e)
                        );
                    }
                }

                let fahrenheit = format!(
                    "{:.2}",
                    temperature_sensor.get_temperature().to_fahrenheit()
                );
                info!(
                    "Publishing temperature {}°C to {}",
                    &*fahrenheit, TEMP_TOPIC
                );

                match client
                    .send_message(TEMP_TOPIC, fahrenheit.as_bytes(), QoS1, false)
                    .await
                {
                    Ok(()) => {
                        info!("Message sent");
                    }
                    Err(e) => {
                        info!(
                            "Error sending message: {} ({:?})",
                            Display2Format(&e),
                            Debug2Format(&e)
                        );
                    }
                }
            }
        }
    }
}

pub async fn sleep(millis: u64) {
    Timer::after(Duration::from_millis(millis)).await;
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    info!(
        "Device capabilities: {}",
        Debug2Format(
            &controller
                .capabilities()
                .expect("Unable to get capabilities")
        )
    );

    loop {
        if let WifiState::StaConnected = esp_wifi::wifi::wifi_state() {
            if let Ok(rssi) = controller.rssi() {
                CURRENT_RSSI.store(rssi, Ordering::Relaxed);
            }

            select(
                controller.wait_for_event(WifiEvent::StaDisconnected),
                sleep(4_000),
            )
            .await;
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.into(),
                password: PASSWORD.into(),
                ..Default::default()
            });
            controller
                .set_configuration(&client_config)
                .expect("couldn't set controller configuration");
            info!("Starting wifi");
            controller
                .start_async()
                .await
                .expect("couldn't start controller");
            info!("Wifi started!");
        }
        info!("About to connect...");

        match controller.connect_async().await {
            Ok(()) => {
                info!("Wifi connected!");
            }
            Err(e) => {
                info!("Failed to connect to wifi: {:?}", e);
                sleep(5_000).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await;
}
