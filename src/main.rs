#![warn(clippy::pedantic)]
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{dns::DnsQueryType, tcp::TcpSocket, Config, DhcpConfig, Stack, StackResources};
use embassy_time::{Duration, Timer};

use esp_backtrace as _;
use esp_hal::{
    gpio::{Io, Level, Output},
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_wifi::{
    wifi::{
        ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
        WifiState,
    },
    EspWifiInitFor,
};

use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::{
        publish_packet::QualityOfService::{self, QoS1},
        reason_codes::ReasonCode,
    },
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
const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const PUBLISH_TOPIC: &str = env!("PUBLISH_TOPIC");
const RECEIVE_TOPIC: &str = env!("RECEIVE_TOPIC");

// #TODO: consider thiserror once no_std compatible
// https://github.com/dtolnay/thiserror/pull/304

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

type Result<T> = core::result::Result<T, Error>;

// async fn mkclient<'a, T: embassy_net::driver::Driver>(
//     stack: &'static embassy_net::Stack<T>,
//     rx_buffer: &'a mut [u8],
//     tx_buffer: &'a mut [u8],
//     recv_buffer: &'a mut [u8],
//     write_buffer: &'a mut [u8],
// ) -> MqttClient<'a, embassy_net::tcp::TcpSocket<'a>, 5, rust_mqtt::utils::rng_generator::CountingRng>
// {
// }

struct Buffers {
    rx: [u8; 4096],
    tx: [u8; 4096],
    recv: [u8; 80],
    write: [u8; 80],
}

impl Buffers {
    fn new() -> Self {
        Self {
            rx: [0; 4096],
            tx: [0; 4096],
            recv: [0; 80],
            write: [0; 80],
        }
    }
}
struct Client<'a> {
    client: MqttClient<
        'a,
        embassy_net::tcp::TcpSocket<'a>,
        5,
        rust_mqtt::utils::rng_generator::CountingRng,
    >,
}

impl<'a> Client<'a> {
    async fn new<T>(stack: &'static embassy_net::Stack<T>, buf: &'a mut Buffers) -> Client<'a>
    where
        T: embassy_net::driver::Driver,
    {
        println!("Creating client");

        // Crashes here
        let mut socket = TcpSocket::new(stack, &mut buf.rx, &mut buf.tx);
        println!("Setting timeout");
        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

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

        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );
        config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
        config.add_client_id("clientId-8rhWgBODCl");
        config.max_packet_size = 100;

        // MqttClient<'a, T: Read + Write, const MAX_PROPERTIES: usize, R: RngCore>
        let mut client =
            MqttClient::<_, 5, _>::new(socket, &mut buf.write, 80, &mut buf.recv, 80, config);

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

        Self { client }
    }

    async fn subscribe_to_topic(&mut self, topic: &str) -> Result<()> {
        self.client
            .subscribe_to_topic(topic)
            .await
            .map_err(Into::into)
    }

    async fn receive_message(&mut self) -> Result<(&str, &[u8])> {
        self.client.receive_message().await.map_err(Into::into)
    }

    async fn send_message(
        &mut self,
        topic_name: &str,
        message: &[u8],
        qos: QualityOfService,
        retain: bool,
    ) -> Result<()> {
        self.client
            .send_message(topic_name, message, qos, retain)
            .await
            .map_err(Into::into)
    }
}

#[embassy_executor::task]
async fn receive(
    // stack: &'static embassy_net::Stack<impl embassy_net::driver::Driver>,
    stack: &'static embassy_net::Stack<WifiDevice<'static, esp_wifi::wifi::WifiStaDevice>>,
    // mut led: AnyOutput<'static>,
    mut led: Output<'static>,
) {
    let mut buf = Buffers::new();
    let mut client = Client::new(stack, &mut buf).await;

    println!("Subscribing to topic {RECEIVE_TOPIC:?}");
    client
        .subscribe_to_topic(RECEIVE_TOPIC)
        .await
        .expect("Error subscribing to topic: {e:?}");

    loop {
        let (_topic, message) = match client.receive_message().await {
            Ok((topic, message)) => (topic, message),
            Err(Error::Mqtt(ReasonCode::NetworkError)) => {
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
            Some('1') => led.set_level(Level::Low),
            Some('0') => led.set_level(Level::High),
            _ => {
                println!("Invalid message: {message:?}");
            }
        }
    }
}

#[embassy_executor::task]
// async fn send(stack: &'static embassy_net::Stack<impl embassy_net::driver::Driver>) {
async fn send(
    stack: &'static embassy_net::Stack<WifiDevice<'static, esp_wifi::wifi::WifiStaDevice>>,
) {
    let mut buf = Buffers::new();
    let mut client = Client::new(stack, &mut buf).await;

    println!("Subscribing to topic {RECEIVE_TOPIC:?}");

    loop {
        println!("Publishing message to topic {PUBLISH_TOPIC:?}");
        match client.send_message(PUBLISH_TOPIC, b"42", QoS1, false).await {
            Ok(()) => {
                println!("Message sent");
            }
            Err(e) => {
                println!("Error sending message: {e:?}");
            }
        }
    }
}

// #[main]
#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();

    let peripherals = esp_hal::init(esp_hal::Config::default());

    esp_alloc::heap_allocator!(72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let init = esp_wifi::init(
        EspWifiInitFor::Wifi,
        timg0.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
        // &clocks,
    )
    .unwrap();

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    let timg1 = TimerGroup::new(peripherals.TIMG1);
    esp_hal_embassy::init(timg1.timer0);

    let config = Config::dhcpv4(DhcpConfig::default());

    let seed = 1234;

    let stack = &*mk_static!(
        Stack<WifiDevice<'_, WifiStaDevice>>,
        Stack::new(
            wifi_interface,
            config,
            mk_static!(StackResources<4>, StackResources::<4>::new()),
            seed
        )
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(stack)).ok();

    stack.wait_config_up().await;

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);
    let mut led = Output::new(io.pins.gpio8, Level::Low);

    // Flash the onboard led to show that we have the pin right
    // and to indicate network connection
    for _ in 0..10 {
        led.toggle();
        Timer::after(Duration::from_millis(100)).await;
    }

    // On my ESP32C3, the onboard LED is active low
    led.set_high();

    spawner.spawn(receive(stack, led)).ok();
    spawner.spawn(send(stack)).ok();
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
