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
const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const PUBLISH_TOPIC: &str = env!("PUBLISH_TOPIC");
const RECEIVE_TOPIC: &str = env!("RECEIVE_TOPIC");

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
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut rng = Rng::new(peripherals.RNG);

    let esp_wifi_ctrl = &*mk_static!(
        EspWifiController<'static>,
        esp_wifi::init(timg0.timer0, rng, peripherals.RADIO_CLK).unwrap()
    );
    let (controller, interfaces) = esp_wifi::wifi::new(esp_wifi_ctrl, peripherals.WIFI).unwrap();
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

    stack.wait_config_up().await;

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let mut led = Output::new(peripherals.GPIO8, Level::Low, OutputConfig::default());
    let mut button = Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    );

    // Flash the onboard led to show that we have the pin right
    // and to indicate network connection
    for _ in 0..10 {
        led.toggle();
        Timer::after(Duration::from_millis(100)).await;
    }

    // On my ESP32C3, the onboard LED is active low
    led.set_high();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
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
    config.add_client_id("clientId-8rhWgBODCl");
    config.max_packet_size = 100;

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
    client
        .subscribe_to_topic(RECEIVE_TOPIC)
        .await
        .expect("Error subscribing to topic: {e:?}");

    // embassy_futures::select::select(client.send_ping(), client.send_ping()).await;

    loop {
        match select3(
            client.receive_message(),
            button.wait_for_low(),
            sleep(3_000),
        )
        .await
        {
            Either3::First(result) => {
                match result {
                    // match client.receive_message_if_ready().await {
                    Ok((_topic, message)) => {
                        let c: Option<char> = message.iter().next().map(|num| char::from(*num));
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
                    Err(ReasonCode::NetworkError) => (),

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

pub async fn sleep(millis: u32) {
    Timer::after(Duration::from_millis(u64::from(millis))).await;
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        if let WifiState::StaConnected = esp_wifi::wifi::wifi_state() {
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
            controller.start_async().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(()) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await;
}
