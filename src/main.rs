#![warn(clippy::pedantic)]
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{dns::DnsQueryType, tcp::TcpSocket, Config, DhcpConfig, Stack, StackResources};
use embassy_time::{Duration, Timer};

use esp_backtrace as _;
use esp_hal::{
    clock::ClockControl,
    gpio::{AnyOutput, Io, Level},
    peripherals::Peripherals,
    prelude::*,
    rng::Rng,
    system::SystemControl,
    timer::{ErasedTimer, OneShotTimer, PeriodicTimer},
};
use esp_println::println;
use esp_wifi::{
    initialize,
    wifi::{
        ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
        WifiState,
    },
    EspWifiInitFor,
};

use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::{publish_packet::QualityOfService::QoS1, reason_codes::ReasonCode},
    utils::rng_generator::CountingRng,
};

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

// const MQTT_HOST: &str = "test.mosquitto.org";
const MQTT_HOST: &str = "natepro.home.arpa";
const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const RECEIVE_TOPIC: &str = env!("RECEIVE_TOPIC");
const PUBLISH_TOPIC: &str = env!("PUBLISH_TOPIC");

// #TODO: consider thiserror once no_std compatible
// https://github.com/dtolnay/thiserror/pull/304
enum Error {
    MqttNetwork,
    Mqtt(rust_mqtt::packet::v5::reason_codes::ReasonCode),
    Dns,
}

impl From<rust_mqtt::packet::v5::reason_codes::ReasonCode> for Error {
    fn from(reason_code: rust_mqtt::packet::v5::reason_codes::ReasonCode) -> Self {
        Error::Mqtt(reason_code)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::MqttNetwork => write!(f, "MQTT Network Error"),
            Error::Mqtt(reason_code) => write!(f, "Other MQTT Error: {reason_code:?}"),
            Error::Dns => write!(f, "DNS lookup error"),
        }
    }
}

type Result<T> = core::result::Result<T, Error>;

#[embassy_executor::task]
async fn receive_message(
    client: &'static mut MqttClient<'static, TcpSocket<'static>, 5, CountingRng>,
) {
    let (topic, message) = client.receive_message().await.expect("something broke");
    println!("topic: {topic:?}");
    println!("message: {message:?}");
}

fn set_onboard_led(led: &mut AnyOutput<'static>, level: Level) {
    println!("Setting onboard led to {level:?}");
    led.set_level(level);
}

#[main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();

    let peripherals = Peripherals::take();

    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = ClockControl::max(system.clock_control).freeze();

    let timer = PeriodicTimer::new(
        esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0, &clocks, None)
            .timer0
            .into(),
    );
    let init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
        &clocks,
    )
    .unwrap();

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    let timg1 = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG1, &clocks, None);
    esp_hal_embassy::init(
        &clocks,
        mk_static!(
            [OneShotTimer<ErasedTimer>; 1],
            [OneShotTimer::new(timg1.timer0.into())]
        ),
    );

    let config = Config::dhcpv4(DhcpConfig::default());

    let seed = 1234;

    // Init network stack
    let stack = &*mk_static!(
        Stack<WifiDevice<'_, WifiStaDevice>>,
        Stack::new(
            wifi_interface,
            config,
            mk_static!(StackResources<3>, StackResources::<3>::new()),
            seed
        )
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(stack)).ok();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address); //dhcp IP address
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

    socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

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

        let remote_endpoint = (address, 1883);
        println!("connecting to {remote_endpoint:?}...");
        let connection = socket.connect(remote_endpoint).await;
        if let Err(e) = connection {
            println!("connect error: {:?}", e);
            continue;
        }
        println!("connected");
        break;
    }
    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);
    let mut led = AnyOutput::new(io.pins.gpio8, Level::Low);

    // Flash the onboard led to show that we have the pin right
    // and to indicate network connection
    for _ in 0..10 {
        led.toggle();
        Timer::after(Duration::from_millis(100)).await;
    }

    // On my ESP32C3, the onboard LED is active low
    led.set_high();

    let mut config = ClientConfig::new(
        rust_mqtt::client::client_config::MqttVersion::MQTTv5,
        CountingRng(20000),
    );
    config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
    config.add_client_id("clientId-8rhWgBODCl");
    config.max_packet_size = 100;
    let mut recv_buffer = [0; 80];
    let mut write_buffer = [0; 80];

    // MqttClient<'a, T: Read + Write, const MAX_PROPERTIES: usize, R: RngCore>
    let mut client =
        MqttClient::<_, 5, _>::new(socket, &mut write_buffer, 80, &mut recv_buffer, 80, config);

    match client.connect_to_broker().await {
        Ok(()) => {
            println!("Connected to broker");
            // break;
        }
        Err(mqtt_error) => {
            if let ReasonCode::NetworkError = mqtt_error {
                println!("MQTT Network Error");
            } else {
                println!("Other MQTT Error: {:?}", mqtt_error);
            }
        }
    };

    println!("Subscribing to topic {RECEIVE_TOPIC:?}");
    client
        .subscribe_to_topic(RECEIVE_TOPIC)
        .await
        .expect("Error subscribing to topic: {e:?}");

    loop {
        Timer::after(Duration::from_millis(100)).await;

        println!("Publishing message to topic {PUBLISH_TOPIC:?}");
        match client.send_message(PUBLISH_TOPIC, b"42", QoS1, false).await {
            Ok(()) => {
                println!("Message sent");
            }
            Err(e) => {
                println!("Error sending message: {e:?}");
            }
        }

        let (_topic, message) = match client.receive_message().await {
            Ok((topic, message)) => (topic, message),
            Err(ReasonCode::NetworkError) => {
                // no message to receive?
                continue;
            }
            Err(e) => {
                println!("Error receiving message: {e:?}");
                continue;
            }
        };

        let c: Option<char> = message.iter().next().map(|num| char::from(*num));
        match c {
            Some('1') => set_onboard_led(&mut led, Level::Low),
            Some('0') => set_onboard_led(&mut led, Level::High),
            _ => {
                println!("Invalid message: {message:?}");
            }
        }
    }
}

pub async fn sleep(millis: u32) {
    Timer::after(Duration::from_millis(u64::from(millis))).await;
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        if let WifiState::StaConnected = esp_wifi::wifi::get_wifi_state() {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await;
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi");
            controller.start().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect().await {
            Ok(()) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await;
}
