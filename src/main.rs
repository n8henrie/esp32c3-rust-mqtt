#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_net::{Config, DhcpConfig, Runner, StackResources, dns::DnsQueryType, tcp::TcpSocket};
use embassy_time::{Duration, Timer};

use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_wifi::{
    EspWifiController,
    wifi::{ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiState},
};

use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::{publish_packet::QualityOfService::QoS1, reason_codes::ReasonCode},
    utils::rng_generator::CountingRng,
};

use esp_alloc as _;

use thiserror::Error;

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
const PUBLISH_TOPIC: &str = env!("PUBLISH_TOPIC");
const RECEIVE_TOPIC: &str = env!("RECEIVE_TOPIC");
const KEEP_ALIVE_SECS: u16 = 12;
const SOCKET_TIMEOUT_SECS: u64 = 60;

const MQTT_CLIENT_ID: &str = env!("MQTT_CLIENT_ID");
const MQTT_USERNAME: &str = env!("MQTT_USERNAME");
const MQTT_PASSWORD: &str = env!("MQTT_PASSWORD");

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

impl From<rust_mqtt::packet::v5::reason_codes::ReasonCode> for Error {
    fn from(reason_code: rust_mqtt::packet::v5::reason_codes::ReasonCode) -> Self {
        Error::Mqtt(reason_code)
    }
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut rng = Rng::new(peripherals.RNG);

    let esp_wifi_ctrl = &*mk_static!(
        EspWifiController<'static>,
        esp_wifi::init(timg0.timer0, rng, peripherals.RADIO_CLK).expect("couldn't init esp_wifi")
    );
    let (controller, interfaces) = esp_wifi::wifi::new(esp_wifi_ctrl, peripherals.WIFI)
        .expect("couldn't create wifi controller");
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

    'main: loop {
        stack.wait_link_up().await;
        stack.wait_config_up().await;

        println!("Waiting to get IP address...");
        loop {
            if let Some(config) = stack.config_v4() {
                println!("Got IP: {}", config.address);
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
        println!("Setting timeout");
        socket.set_timeout(Some(embassy_time::Duration::from_secs(SOCKET_TIMEOUT_SECS)));

        println!("Getting address");
        loop {
            let address = match stack
                .dns_query(MQTT_HOST, DnsQueryType::A)
                .await
                .map(|a| a[0])
            {
                Ok(address) => address,
                Err(e) => {
                    println!("DNS lookup error: {e:?}");
                    continue;
                }
            };

            let port: u16 = MQTT_PORT.parse().expect("Couldn't parse MQTT_PORT as u16");
            let remote_endpoint = (address, port);
            println!("connecting to {remote_endpoint:?}...");

            if let Err(e) = socket.connect(remote_endpoint).await {
                println!("connect error: {:?}", e);
                continue;
            }
            println!("connected");
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

        let mut writebuf = [0; 1024];
        let mut readbuf = [0; 1024];
        let mut client =
            MqttClient::<_, 5, _>::new(socket, &mut writebuf, 80, &mut readbuf, 80, config);

        match client.connect_to_broker().await {
            Ok(()) => {
                println!("Connected to broker");
            }
            Err(mqtt_error) => {
                if let ReasonCode::NetworkError = mqtt_error {
                    println!("MQTT Network Error");
                } else {
                    println!("Other MQTT Error: {:?}", mqtt_error);
                }
            }
        }

        println!("Subscribing to topic {RECEIVE_TOPIC:?}");
        if let Err(e) = client.subscribe_to_topic(RECEIVE_TOPIC).await {
            println!("Error subscribing to topic: {e:?}");
            continue 'main;
        }
        println!("Subscribed");

        loop {
            match select3(
                client.receive_message(),
                button.wait_for_low(),
                sleep(u64::from(KEEP_ALIVE_SECS) * 1_000),
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
                                    println!("Invalid message: {message:?}");
                                }
                            }
                        }

                        // reasons include:
                        // - no mqtt broker
                        Err(ReasonCode::NetworkError) => {
                            println!("Network error! restarting stack after a brief delay");
                            sleep(5_000).await;
                            continue 'main;
                        }

                        Err(e) => {
                            println!("Error receiving message: {e:?}");
                        }
                    }
                }

                Either3::Second(()) => {
                    // debounce
                    sleep(100).await;
                    button.wait_for_high().await;

                    println!("Publishing message to topic {PUBLISH_TOPIC:?}");
                    match client.send_message(PUBLISH_TOPIC, b"42", QoS1, false).await {
                        Ok(()) => {
                            println!("Message sent");
                        }
                        Err(e) => {
                            println!("Error sending message: {e} ({e:?})");
                        }
                    }
                }

                Either3::Third(()) => match client.send_ping().await {
                    Ok(()) => (),
                    Err(e) => {
                        println!("Error sending message: {e} ({e:?})");
                    }
                },
            }
        }
    }
}

pub async fn sleep(millis: u64) {
    Timer::after(Duration::from_millis(millis)).await;
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        if let WifiState::StaConnected = esp_wifi::wifi::wifi_state() {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            sleep(5_000).await;
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
            println!("Starting wifi");
            controller
                .start_async()
                .await
                .expect("couldn't start controller");
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(()) => {
                if let Ok(rssi) = controller.rssi() {
                    println!("Wifi connected! rssi: {rssi}");
                } else {
                    println!("Wifi connected!");
                }
            }
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                sleep(5_000).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await;
}
