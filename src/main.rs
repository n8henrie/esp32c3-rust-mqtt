#![no_std]
#![no_main]

extern crate alloc;

use alloc::{format, string::String};
use core::{
    fmt::Debug,
    future::pending,
    sync::atomic::{AtomicI32, Ordering},
};

use defmt::{Debug2Format, Display2Format, error, info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_net::{
    Config, DhcpConfig, Runner, Stack, StackResources, dns::DnsQueryType, tcp::TcpSocket,
};
use embassy_time::{Duration, Ticker, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    interrupt::software::SoftwareInterruptControl,
    rng::Rng,
    timer::timg::TimerGroup,
    tsens::{self, TemperatureSensor},
};
use esp_println as _;
use esp_radio::wifi::{
    Config as WifiConfig, ControllerConfig, Interface, PowerSaveMode, WifiController,
};
use minimq::{
    Buffers, ConfigBuilder, ConnectEvent, Connection, Io, Publication, QoS, Session, TopicFilter,
    Will,
};
use static_cell::StaticCell;
use thiserror::Error;

use home_assistant::{
    BUTTON_EVENT_PRESS, BUTTON_STATE_PRESSED, BUTTON_STATE_RELEASED, DiscoveryConfig, LED_OFF,
    LED_ON, PAYLOAD_AVAILABLE, PAYLOAD_NOT_AVAILABLE,
};

esp_bootloader_esp_idf::esp_app_desc!();

static STACK_RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
static CURRENT_RSSI: AtomicI32 = AtomicI32::new(RSSI_UNKNOWN);

const WIFI_SSID: &str = env!("SSID");
const WIFI_PASSWORD: &str = env!("PASSWORD");

const MQTT_HOST: &str = env!("MQTT_HOST");
const MQTT_PORT: &str = env!("MQTT_PORT");
const MQTT_CLIENT_ID: &str = env!("MQTT_CLIENT_ID");
const MQTT_USERNAME: &str = env!("MQTT_USERNAME");
const MQTT_PASSWORD: &str = env!("MQTT_PASSWORD");
const MQTT_TOPIC_PREFIX: &str = env!("MQTT_TOPIC_PREFIX");

const DEVICE_NAME: &str = match option_env!("DEVICE_NAME") {
    Some(name) => name,
    None => MQTT_CLIENT_ID,
};

const HOME_ASSISTANT_DISCOVERY_PREFIX: &str = match option_env!("HOME_ASSISTANT_DISCOVERY_PREFIX") {
    Some(prefix) => prefix,
    None => "homeassistant",
};

const MQTT_KEEPALIVE_SECS: u16 = 30;
const MQTT_SESSION_EXPIRY_SECS: u32 = 300;
const MQTT_RECONNECT_DELAY_SECS: u64 = 5;
const MQTT_SOCKET_TIMEOUT_SECS: u64 = 45;

const MQTT_RX_CAPACITY: usize = 2_048;
const MQTT_TX_CAPACITY: usize = 4_096;
const TCP_RX_CAPACITY: usize = 1_024;
const TCP_TX_CAPACITY: usize = 1_024;
const MQTT_PACKET_OVERHEAD_ESTIMATE: usize = 128;

const WIFI_RECONNECT_DELAY_SECS: u64 = 5;
const WIFI_RSSI_INTERVAL_SECS: u64 = 5;
const TELEMETRY_INTERVAL_SECS: u64 = 4;
const BUTTON_DEBOUNCE_MS: u64 = 50;
const STARTUP_LED_FLASHES: usize = 10;
const STARTUP_LED_FLASH_INTERVAL_MS: u64 = 100;
const RSSI_UNKNOWN: i32 = i32::MIN;

#[derive(Debug, Error)]
enum StartupError {
    #[error("invalid Wi-Fi configuration value {0}")]
    InvalidWifiConfiguration(&'static str),

    #[error("failed to initialize the temperature sensor")]
    TemperatureSensor,

    #[error("failed to initialize the Wi-Fi controller")]
    WifiController,

    #[error("failed to configure Wi-Fi power saving")]
    WifiPowerSaving,

    #[error("failed to allocate task {0}")]
    Spawn(&'static str),
}

#[derive(Debug, Error)]
enum FatalMqttError {
    #[error("configuration value {0} must not be empty")]
    EmptyConfiguration(&'static str),

    #[error("configuration value {0} contains a forbidden control character")]
    InvalidText(&'static str),

    #[error("configuration value {0} is not a valid MQTT topic prefix")]
    InvalidTopicPrefix(&'static str),

    #[error(
        "MQTT_CLIENT_ID must contain only ASCII letters, digits, underscores, or hyphens because it is used as a Home Assistant discovery object ID"
    )]
    InvalidDiscoveryObjectId,

    #[error("generated topic {0} is not a valid MQTT topic name")]
    InvalidTopic(&'static str),

    #[error("MQTT_PORT must be an integer from 1 through 65535")]
    InvalidPort,

    #[error("failed to serialize Home Assistant discovery configuration")]
    DiscoverySerialization,

    #[error(
        "estimated Home Assistant discovery packet size {required} exceeds MQTT TX capacity {capacity}"
    )]
    DiscoveryTooLarge { required: usize, capacity: usize },

    #[error("invalid MiniMQ configuration for {0}")]
    MiniMqConfiguration(&'static str),
}

#[derive(Debug, Error)]
enum MqttAttemptError {
    #[error("DNS lookup failed")]
    Dns,

    #[error("DNS lookup returned no IPv4 addresses")]
    NoIpv4Address,

    #[error("TCP connection failed")]
    TcpConnect,

    #[error("MQTT handshake failed")]
    Handshake,

    #[error("MQTT subscription failed")]
    Subscribe,

    #[error("failed to publish {0}")]
    Publish(&'static str),

    #[error("failed while receiving MQTT messages")]
    Receive,

    #[error("network configuration was lost")]
    NetworkDown,
}

#[derive(Clone, Copy)]
enum Retain {
    No,
    Yes,
}

#[derive(Debug)]
struct Topics {
    discovery: String,
    availability: String,
    temperature: String,
    rssi: String,
    led_command: String,
    led_state: String,
    button_state: String,
    button_event: String,
    temperature_unique_id: String,
    rssi_unique_id: String,
    led_unique_id: String,
    button_unique_id: String,
}

impl Topics {
    fn new() -> Result<Self, FatalMqttError> {
        validate_required_text("MQTT_HOST", MQTT_HOST)?;
        validate_required_text("MQTT_CLIENT_ID", MQTT_CLIENT_ID)?;
        validate_required_text("MQTT_USERNAME", MQTT_USERNAME)?;
        validate_required_value("MQTT_PASSWORD", MQTT_PASSWORD)?;
        validate_required_text("DEVICE_NAME", DEVICE_NAME)?;
        validate_topic_prefix("MQTT_TOPIC_PREFIX", MQTT_TOPIC_PREFIX)?;
        validate_topic_prefix(
            "HOME_ASSISTANT_DISCOVERY_PREFIX",
            HOME_ASSISTANT_DISCOVERY_PREFIX,
        )?;

        if !MQTT_CLIENT_ID
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(FatalMqttError::InvalidDiscoveryObjectId);
        }

        let topics = Self {
            discovery: format!("{HOME_ASSISTANT_DISCOVERY_PREFIX}/device/{MQTT_CLIENT_ID}/config"),
            availability: format!("{MQTT_TOPIC_PREFIX}/availability"),
            temperature: format!("{MQTT_TOPIC_PREFIX}/temperature"),
            rssi: format!("{MQTT_TOPIC_PREFIX}/rssi"),
            led_command: format!("{MQTT_TOPIC_PREFIX}/led/set"),
            led_state: format!("{MQTT_TOPIC_PREFIX}/led/state"),
            button_state: format!("{MQTT_TOPIC_PREFIX}/button/state"),
            button_event: format!("{MQTT_TOPIC_PREFIX}/button/event"),
            temperature_unique_id: format!("{MQTT_CLIENT_ID}_mcu_temperature"),
            rssi_unique_id: format!("{MQTT_CLIENT_ID}_rssi"),
            led_unique_id: format!("{MQTT_CLIENT_ID}_led"),
            button_unique_id: format!("{MQTT_CLIENT_ID}_onboard_button"),
        };

        for (name, topic) in [
            ("discovery", topics.discovery.as_str()),
            ("availability", topics.availability.as_str()),
            ("temperature", topics.temperature.as_str()),
            ("rssi", topics.rssi.as_str()),
            ("led_command", topics.led_command.as_str()),
            ("led_state", topics.led_state.as_str()),
            ("button_state", topics.button_state.as_str()),
            ("button_event", topics.button_event.as_str()),
        ] {
            validate_topic(name, topic)?;
        }

        Ok(topics)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LedState {
    Off,
    On,
}

impl LedState {
    fn from_payload(payload: &[u8]) -> Option<Self> {
        match payload {
            b"0" => Some(Self::Off),
            b"1" => Some(Self::On),
            _ => None,
        }
    }

    const fn payload(self) -> &'static str {
        match self {
            Self::Off => LED_OFF,
            Self::On => LED_ON,
        }
    }

    const fn output_level(self) -> Level {
        // The onboard LED is active low.
        match self {
            Self::Off => Level::High,
            Self::On => Level::Low,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ButtonState {
    Released,
    Pressed,
}

impl ButtonState {
    fn read(button: &Input<'_>) -> Self {
        if button.is_low() {
            Self::Pressed
        } else {
            Self::Released
        }
    }

    const fn payload(self) -> &'static str {
        match self {
            Self::Released => BUTTON_STATE_RELEASED,
            Self::Pressed => BUTTON_STATE_PRESSED,
        }
    }
}

async fn wait_for_button_transition(button: &mut Input<'_>, current: ButtonState) -> ButtonState {
    match current {
        ButtonState::Released => {
            button.wait_for_low().await;
            ButtonState::Pressed
        }
        ButtonState::Pressed => {
            button.wait_for_high().await;
            ButtonState::Released
        }
    }
}

struct MqttRuntime<'a> {
    topics: &'a Topics,
    discovery_payload: &'a str,
    led: &'a mut Output<'static>,
    temperature_sensor: &'a TemperatureSensor<'static>,
    led_state: &'a mut LedState,
}

impl MqttRuntime<'_> {
    fn set_led(&mut self, state: LedState) {
        *self.led_state = state;
        self.led.set_level(state.output_level());
    }

    fn led_payload(&self) -> &'static str {
        self.led_state.payload()
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    if let Err(error) = run(spawner).await {
        error!("fatal startup error: {}", Display2Format(&error));
        pending::<()>().await;
    }
}

async fn run(spawner: Spawner) -> Result<(), StartupError> {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz);
    let peripherals = esp_hal::init(config);

    // This macro initializes the global allocator and must run before the first allocation.
    esp_alloc::heap_allocator!(size: 72 * 1024);

    validate_wifi_configuration()?;

    let mut led = Output::new(peripherals.GPIO8, Level::Low, OutputConfig::default());
    let button = Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    );

    let temperature_sensor = TemperatureSensor::new(peripherals.TSENS, tsens::Config::default())
        .map_err(|source| {
            warn!(
                "temperature sensor initialization failed: {:?}",
                Debug2Format(&source)
            );
            StartupError::TemperatureSensor
        })?;

    let software_interrupts = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timer_group = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timer_group.timer0, software_interrupts.software_interrupt0);

    let station_config = WifiConfig::Station(
        esp_radio::wifi::sta::StationConfig::default()
            .with_ssid(WIFI_SSID)
            .with_password(WIFI_PASSWORD.into()),
    );

    let mut controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .map_err(|source| {
        warn!(
            "Wi-Fi controller initialization failed: {:?}",
            Debug2Format(&source)
        );
        StartupError::WifiController
    })?;

    controller
        .set_power_saving(PowerSaveMode::Maximum)
        .map_err(|source| {
            warn!(
                "Wi-Fi power-saving configuration failed: {:?}",
                Debug2Format(&source)
            );
            StartupError::WifiPowerSaving
        })?;

    let network_config = Config::dhcpv4(DhcpConfig::default());
    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());

    let (stack, runner) = embassy_net::new(
        Interface::station(),
        network_config,
        STACK_RESOURCES.init(StackResources::new()),
        seed,
    );

    for _ in 0..STARTUP_LED_FLASHES {
        led.toggle();
        Timer::after(Duration::from_millis(STARTUP_LED_FLASH_INTERVAL_MS)).await;
    }

    // Leave the active-low LED off after the startup indication.
    led.set_high();

    // Spawn each token immediately. Embassy spawn tokens must not be dropped, so retaining one
    // while attempting to allocate another would turn a later allocation failure into a panic.
    let wifi =
        wifi_task(controller).map_err(|source| task_allocation_error("wifi_task", &source))?;
    spawner.spawn(wifi);

    let network = net_task(runner).map_err(|source| task_allocation_error("net_task", &source))?;
    spawner.spawn(network);

    let mqtt = mqtt_task(stack, button, led, temperature_sensor)
        .map_err(|source| task_allocation_error("mqtt_task", &source))?;
    spawner.spawn(mqtt);

    Ok(())
}

fn task_allocation_error(name: &'static str, source: &impl Debug) -> StartupError {
    warn!(
        "failed to allocate task {}: {:?}",
        name,
        Debug2Format(source)
    );
    StartupError::Spawn(name)
}

#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    info!("starting Wi-Fi connection task");

    loop {
        match controller.connect_async().await {
            Ok(_) => {
                info!("Wi-Fi connected");

                if let Ok(rssi) = controller.rssi() {
                    CURRENT_RSSI.store(i32::from(rssi), Ordering::Relaxed);
                }

                let mut rssi_ticker = Ticker::every(Duration::from_secs(WIFI_RSSI_INTERVAL_SECS));
                let mut disconnect = core::pin::pin!(controller.wait_for_disconnect_async());

                loop {
                    match select(rssi_ticker.next(), disconnect.as_mut()).await {
                        Either::First(()) => {
                            if let Ok(rssi) = controller.rssi() {
                                CURRENT_RSSI.store(i32::from(rssi), Ordering::Relaxed);
                            }
                        }
                        Either::Second(result) => {
                            match result {
                                Ok(reason) => {
                                    warn!("Wi-Fi disconnected: {:?}", reason);
                                }
                                Err(source) => {
                                    warn!(
                                        "failed while waiting for Wi-Fi disconnect: {:?}",
                                        Debug2Format(&source)
                                    );
                                }
                            }

                            CURRENT_RSSI.store(RSSI_UNKNOWN, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
            Err(source) => {
                warn!("Wi-Fi connection failed: {:?}", Debug2Format(&source));
            }
        }

        Timer::after(Duration::from_secs(WIFI_RECONNECT_DELAY_SECS)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await;
}

#[embassy_executor::task]
async fn mqtt_task(
    stack: Stack<'static>,
    mut button: Input<'static>,
    mut led: Output<'static>,
    temperature_sensor: TemperatureSensor<'static>,
) {
    if let Err(error) = mqtt_task_inner(stack, &mut button, &mut led, &temperature_sensor).await {
        error!("MQTT task stopped: {}", Display2Format(&error));
    }
}

async fn mqtt_task_inner(
    stack: Stack<'static>,
    button: &mut Input<'static>,
    led: &mut Output<'static>,
    temperature_sensor: &TemperatureSensor<'static>,
) -> Result<(), FatalMqttError> {
    let port = MQTT_PORT.parse::<u16>().map_err(|source| {
        warn!("invalid MQTT_PORT: {:?}", Debug2Format(&source));
        FatalMqttError::InvalidPort
    })?;

    if port == 0 {
        return Err(FatalMqttError::InvalidPort);
    }

    let topics = Topics::new()?;
    let discovery_payload = home_assistant::encode(DiscoveryConfig {
        device_id: MQTT_CLIENT_ID,
        device_name: DEVICE_NAME,
        availability_topic: topics.availability.as_str(),
        temperature_topic: topics.temperature.as_str(),
        rssi_topic: topics.rssi.as_str(),
        led_command_topic: topics.led_command.as_str(),
        led_state_topic: topics.led_state.as_str(),
        button_state_topic: topics.button_state.as_str(),
        button_event_topic: topics.button_event.as_str(),
        temperature_unique_id: topics.temperature_unique_id.as_str(),
        rssi_unique_id: topics.rssi_unique_id.as_str(),
        led_unique_id: topics.led_unique_id.as_str(),
        button_unique_id: topics.button_unique_id.as_str(),
    })
    .map_err(|source| {
        warn!(
            "Home Assistant discovery serialization failed: {:?}",
            Debug2Format(&source)
        );
        FatalMqttError::DiscoverySerialization
    })?;

    let estimated_discovery_packet_size = discovery_payload
        .len()
        .saturating_add(topics.discovery.len())
        .saturating_add(MQTT_PACKET_OVERHEAD_ESTIMATE);

    if estimated_discovery_packet_size > MQTT_TX_CAPACITY {
        return Err(FatalMqttError::DiscoveryTooLarge {
            required: estimated_discovery_packet_size,
            capacity: MQTT_TX_CAPACITY,
        });
    }

    info!(
        "Home Assistant discovery: topic={} payload_bytes={}",
        topics.discovery.as_str(),
        discovery_payload.len()
    );

    let mut mqtt_rx = [0_u8; MQTT_RX_CAPACITY];
    let mut mqtt_tx = [0_u8; MQTT_TX_CAPACITY];

    let will = Will::new(
        topics.availability.as_str(),
        PAYLOAD_NOT_AVAILABLE.as_bytes(),
        &[],
    )
    .map_err(|source| {
        warn!("invalid MQTT will: {:?}", Debug2Format(&source));
        FatalMqttError::MiniMqConfiguration("last will")
    })?
    .retained();

    let builder = ConfigBuilder::new(Buffers::new(&mut mqtt_rx, &mut mqtt_tx))
        .client_id(MQTT_CLIENT_ID)
        .map_err(|source| {
            warn!("invalid MQTT client ID: {:?}", Debug2Format(&source));
            FatalMqttError::MiniMqConfiguration("client ID")
        })?
        .keepalive_interval(MQTT_KEEPALIVE_SECS)
        .session_expiry_interval(MQTT_SESSION_EXPIRY_SECS)
        .will(will)
        .map_err(|source| {
            warn!(
                "invalid MQTT will configuration: {:?}",
                Debug2Format(&source)
            );
            FatalMqttError::MiniMqConfiguration("last will")
        })?
        .auth(MQTT_USERNAME, MQTT_PASSWORD.as_bytes())
        .map_err(|source| {
            warn!(
                "invalid MQTT authentication configuration: {:?}",
                Debug2Format(&source)
            );
            FatalMqttError::MiniMqConfiguration("authentication")
        })?;

    let mut session = Session::new(builder);
    let mut led_state = LedState::Off;

    loop {
        stack.wait_config_up().await;

        if let Some(config) = stack.config_v4() {
            info!("network ready: {}", config.address);
        }

        let result = {
            let mut runtime = MqttRuntime {
                topics: &topics,
                discovery_payload: discovery_payload.as_str(),
                led,
                temperature_sensor,
                led_state: &mut led_state,
            };

            run_mqtt_attempt(stack, port, &mut session, button, &mut runtime).await
        };

        if let Err(error) = result {
            warn!("MQTT connection ended: {}", Display2Format(&error));
        }

        Timer::after(Duration::from_secs(MQTT_RECONNECT_DELAY_SECS)).await;
    }
}

async fn run_mqtt_attempt<'buffer>(
    stack: Stack<'static>,
    port: u16,
    session: &mut Session<'buffer>,
    button: &mut Input<'static>,
    runtime: &mut MqttRuntime<'_>,
) -> Result<(), MqttAttemptError> {
    let addresses = stack
        .dns_query(MQTT_HOST, DnsQueryType::A)
        .await
        .map_err(|source| {
            warn!("MQTT DNS lookup failed: {:?}", Debug2Format(&source));
            MqttAttemptError::Dns
        })?;

    let Some(address) = addresses.first().copied() else {
        warn!("MQTT DNS lookup returned no IPv4 addresses");
        return Err(MqttAttemptError::NoIpv4Address);
    };

    let remote_endpoint = (address, port);
    info!("connecting to {}...", Debug2Format(&remote_endpoint));

    let mut tcp_rx = [0_u8; TCP_RX_CAPACITY];
    let mut tcp_tx = [0_u8; TCP_TX_CAPACITY];
    let mut socket = TcpSocket::new(stack, &mut tcp_rx, &mut tcp_tx);
    socket.set_timeout(Some(Duration::from_secs(MQTT_SOCKET_TIMEOUT_SECS)));

    socket.connect(remote_endpoint).await.map_err(|source| {
        warn!("MQTT TCP connection failed: {:?}", Debug2Format(&source));
        MqttAttemptError::TcpConnect
    })?;

    let mut connection = session.connect(socket).await.map_err(|source| {
        warn!("MQTT handshake failed: {:?}", Debug2Format(&source));
        MqttAttemptError::Handshake
    })?;

    match connection.connect_event() {
        ConnectEvent::Connected => {
            info!("MQTT connected with a fresh broker session");
        }
        ConnectEvent::Reconnected => {
            info!("MQTT resumed the existing broker session");
        }
    }

    // Re-subscribing is harmless for a resumed session and guarantees recovery if a previous
    // attempt disconnected after CONNECT but before SUBSCRIBE completed.
    let _ = connection
        .subscribe(
            &[TopicFilter::new(runtime.topics.led_command.as_str())],
            &[],
        )
        .await
        .map_err(|source| {
            warn!("MQTT subscription failed: {:?}", Debug2Format(&source));
            MqttAttemptError::Subscribe
        })?;

    info!(
        "subscription requested for {}",
        runtime.topics.led_command.as_str()
    );

    let mut button_state = ButtonState::read(button);

    // Publish configuration and state before announcing availability.
    publish_text(
        &mut connection,
        runtime.topics.discovery.as_str(),
        runtime.discovery_payload,
        Retain::Yes,
        "Home Assistant discovery",
    )
    .await?;

    publish_text(
        &mut connection,
        runtime.topics.led_state.as_str(),
        runtime.led_payload(),
        Retain::Yes,
        "LED state",
    )
    .await?;

    publish_text(
        &mut connection,
        runtime.topics.button_state.as_str(),
        button_state.payload(),
        Retain::Yes,
        "button state",
    )
    .await?;

    publish_telemetry(&mut connection, runtime.topics, runtime.temperature_sensor).await?;

    publish_text(
        &mut connection,
        runtime.topics.availability.as_str(),
        PAYLOAD_AVAILABLE,
        Retain::Yes,
        "availability",
    )
    .await?;

    info!("MQTT device is online");

    let mut telemetry_ticker = Ticker::every(Duration::from_secs(TELEMETRY_INTERVAL_SECS));

    loop {
        let observed_state = service_until_button_transition(
            &mut connection,
            stack,
            button,
            button_state,
            &mut telemetry_ticker,
            runtime,
        )
        .await?;

        Timer::after(Duration::from_millis(BUTTON_DEBOUNCE_MS)).await;

        let stable_state = ButtonState::read(button);
        if stable_state != observed_state {
            continue;
        }

        button_state = stable_state;

        publish_text(
            &mut connection,
            runtime.topics.button_state.as_str(),
            button_state.payload(),
            Retain::Yes,
            "button state",
        )
        .await?;

        info!("button state changed to {}", button_state.payload());

        if button_state == ButtonState::Pressed {
            publish_text(
                &mut connection,
                runtime.topics.button_event.as_str(),
                BUTTON_EVENT_PRESS,
                Retain::No,
                "button event",
            )
            .await?;

            info!("published button event");
        }
    }
}

async fn service_until_button_transition<IO>(
    connection: &mut Connection<'_, '_, IO>,
    stack: Stack<'static>,
    button: &mut Input<'static>,
    current_button_state: ButtonState,
    telemetry_ticker: &mut Ticker,
    runtime: &mut MqttRuntime<'_>,
) -> Result<ButtonState, MqttAttemptError>
where
    IO: Io,
    IO::Error: Debug,
{
    // Keep the GPIO wait future alive while MQTT and telemetry events are handled. ESP-HAL GPIO
    // waits are not cancellation-safe, so recreating the future after every unrelated event could
    // miss a short button press.
    let mut button_transition =
        core::pin::pin!(wait_for_button_transition(button, current_button_state));

    loop {
        if !stack.is_config_up() {
            return Err(MqttAttemptError::NetworkDown);
        }

        match select3(
            connection.recv(),
            button_transition.as_mut(),
            telemetry_ticker.next(),
        )
        .await
        {
            Either3::First(result) => {
                let requested_state = {
                    let inbound = result.map_err(|source| {
                        warn!("MQTT receive failed: {:?}", Debug2Format(&source));
                        MqttAttemptError::Receive
                    })?;

                    if inbound.topic() != runtime.topics.led_command.as_str() {
                        warn!("ignoring message on unexpected topic {}", inbound.topic());
                        continue;
                    }

                    let Some(state) = LedState::from_payload(inbound.payload()) else {
                        warn!(
                            "invalid LED command payload: {:?}",
                            Debug2Format(&inbound.payload())
                        );
                        continue;
                    };

                    state
                };

                runtime.set_led(requested_state);

                // QoS 0 publishes are not cancellation-safe in MiniMQ, so publish only after the
                // select has completed instead of placing this operation inside a selected future.
                publish_text(
                    connection,
                    runtime.topics.led_state.as_str(),
                    requested_state.payload(),
                    Retain::Yes,
                    "LED state",
                )
                .await?;

                info!("LED state changed to {}", requested_state.payload());
            }
            Either3::Second(state) => return Ok(state),
            Either3::Third(()) => {
                publish_telemetry(connection, runtime.topics, runtime.temperature_sensor).await?;
            }
        }
    }
}

async fn publish_text<IO>(
    connection: &mut Connection<'_, '_, IO>,
    topic: &str,
    payload: &str,
    retain: Retain,
    description: &'static str,
) -> Result<(), MqttAttemptError>
where
    IO: Io,
    IO::Error: Debug,
{
    let publication = Publication::text(topic, payload).qos(QoS::AtMostOnce);
    let publication = match retain {
        Retain::No => publication,
        Retain::Yes => publication.retain(),
    };

    let _ = connection.publish(publication).await.map_err(|source| {
        warn!(
            "failed to publish {}: {:?}",
            description,
            Debug2Format(&source)
        );
        MqttAttemptError::Publish(description)
    })?;

    Ok(())
}

async fn publish_telemetry<IO>(
    connection: &mut Connection<'_, '_, IO>,
    topics: &Topics,
    temperature_sensor: &TemperatureSensor<'static>,
) -> Result<(), MqttAttemptError>
where
    IO: Io,
    IO::Error: Debug,
{
    let rssi = CURRENT_RSSI.load(Ordering::Relaxed);

    if rssi != RSSI_UNKNOWN {
        let payload = format!("{rssi}");
        info!(
            "publishing RSSI {} dBm to {}",
            payload.as_str(),
            topics.rssi.as_str()
        );

        publish_text(
            connection,
            topics.rssi.as_str(),
            payload.as_str(),
            Retain::No,
            "RSSI",
        )
        .await?;
    }

    let fahrenheit = temperature_sensor.get_temperature().to_fahrenheit();
    let payload = format!("{fahrenheit:.2}");

    info!(
        "publishing MCU temperature {}°F to {}",
        payload.as_str(),
        topics.temperature.as_str()
    );

    publish_text(
        connection,
        topics.temperature.as_str(),
        payload.as_str(),
        Retain::No,
        "MCU temperature",
    )
    .await
}

fn validate_wifi_configuration() -> Result<(), StartupError> {
    if WIFI_SSID.is_empty() || WIFI_SSID.chars().any(char::is_control) {
        return Err(StartupError::InvalidWifiConfiguration("SSID"));
    }

    Ok(())
}

fn validate_required_value(name: &'static str, value: &str) -> Result<(), FatalMqttError> {
    if value.is_empty() {
        return Err(FatalMqttError::EmptyConfiguration(name));
    }

    Ok(())
}

fn validate_required_text(name: &'static str, value: &str) -> Result<(), FatalMqttError> {
    validate_required_value(name, value)?;

    if value.chars().any(char::is_control) {
        return Err(FatalMqttError::InvalidText(name));
    }

    Ok(())
}

fn validate_topic_prefix(name: &'static str, value: &str) -> Result<(), FatalMqttError> {
    validate_required_text(name, value)?;

    if value.starts_with('/')
        || value.ends_with('/')
        || value.bytes().any(|byte| matches!(byte, b'+' | b'#'))
    {
        return Err(FatalMqttError::InvalidTopicPrefix(name));
    }

    Ok(())
}

fn validate_topic(name: &'static str, topic: &str) -> Result<(), FatalMqttError> {
    let invalid = topic.is_empty()
        || topic.len() > usize::from(u16::MAX)
        || topic.chars().any(char::is_control)
        || topic.bytes().any(|byte| matches!(byte, b'+' | b'#'));

    if invalid {
        return Err(FatalMqttError::InvalidTopic(name));
    }

    Ok(())
}

mod home_assistant {
    use alloc::string::String;

    use serde::Serialize;

    pub(super) const PAYLOAD_AVAILABLE: &str = "online";
    pub(super) const PAYLOAD_NOT_AVAILABLE: &str = "offline";
    pub(super) const LED_ON: &str = "1";
    pub(super) const LED_OFF: &str = "0";
    pub(super) const BUTTON_STATE_PRESSED: &str = "pressed";
    pub(super) const BUTTON_STATE_RELEASED: &str = "released";
    pub(super) const BUTTON_EVENT_PRESS: &str = "press";

    pub(super) struct DiscoveryConfig<'a> {
        pub(super) device_id: &'a str,
        pub(super) device_name: &'a str,
        pub(super) availability_topic: &'a str,
        pub(super) temperature_topic: &'a str,
        pub(super) rssi_topic: &'a str,
        pub(super) led_command_topic: &'a str,
        pub(super) led_state_topic: &'a str,
        pub(super) button_state_topic: &'a str,
        pub(super) button_event_topic: &'a str,
        pub(super) temperature_unique_id: &'a str,
        pub(super) rssi_unique_id: &'a str,
        pub(super) led_unique_id: &'a str,
        pub(super) button_unique_id: &'a str,
    }

    pub(super) fn encode(config: DiscoveryConfig<'_>) -> Result<String, serde_json::Error> {
        serde_json::to_string(&DeviceDiscovery {
            device: Device {
                identifiers: [config.device_id],
                name: config.device_name,
                manufacturer: "Espressif Systems",
                model: "ESP32-C3",
                serial_number: config.device_id,
                software_version: env!("CARGO_PKG_VERSION"),
            },
            origin: Origin {
                name: env!("CARGO_PKG_NAME"),
                software_version: env!("CARGO_PKG_VERSION"),
            },
            availability_topic: config.availability_topic,
            payload_available: PAYLOAD_AVAILABLE,
            payload_not_available: PAYLOAD_NOT_AVAILABLE,
            components: Components {
                temperature: Sensor {
                    platform: "sensor",
                    name: "MCU Temperature",
                    unique_id: config.temperature_unique_id,
                    state_topic: config.temperature_topic,
                    device_class: "temperature",
                    state_class: "measurement",
                    unit_of_measurement: "°F",
                    suggested_display_precision: Some(2),
                    entity_category: Some("diagnostic"),
                },
                rssi: Sensor {
                    platform: "sensor",
                    name: "Wi-Fi RSSI",
                    unique_id: config.rssi_unique_id,
                    state_topic: config.rssi_topic,
                    device_class: "signal_strength",
                    state_class: "measurement",
                    unit_of_measurement: "dBm",
                    suggested_display_precision: Some(0),
                    entity_category: Some("diagnostic"),
                },
                led: Switch {
                    platform: "switch",
                    name: "LED",
                    unique_id: config.led_unique_id,
                    command_topic: config.led_command_topic,
                    state_topic: config.led_state_topic,
                    payload_on: LED_ON,
                    payload_off: LED_OFF,
                    optimistic: false,
                },
                button_state: BinarySensor {
                    platform: "binary_sensor",
                    name: "Onboard Button",
                    unique_id: config.button_unique_id,
                    state_topic: config.button_state_topic,
                    payload_on: BUTTON_STATE_PRESSED,
                    payload_off: BUTTON_STATE_RELEASED,
                    entity_category: "diagnostic",
                },
                button: DeviceTrigger {
                    platform: "device_automation",
                    automation_type: "trigger",
                    topic: config.button_event_topic,
                    payload: BUTTON_EVENT_PRESS,
                    trigger_type: "button_short_press",
                    subtype: "button_1",
                },
            },
        })
    }

    #[derive(Serialize)]
    struct DeviceDiscovery<'a> {
        device: Device<'a>,
        origin: Origin,
        availability_topic: &'a str,
        payload_available: &'static str,
        payload_not_available: &'static str,
        components: Components<'a>,
    }

    #[derive(Serialize)]
    struct Device<'a> {
        identifiers: [&'a str; 1],
        name: &'a str,
        manufacturer: &'static str,
        model: &'static str,
        serial_number: &'a str,
        #[serde(rename = "sw_version")]
        software_version: &'static str,
    }

    #[derive(Serialize)]
    struct Origin {
        name: &'static str,
        #[serde(rename = "sw_version")]
        software_version: &'static str,
    }

    #[derive(Serialize)]
    struct Components<'a> {
        temperature: Sensor<'a>,
        rssi: Sensor<'a>,
        led: Switch<'a>,
        button_state: BinarySensor<'a>,
        button: DeviceTrigger<'a>,
    }

    #[derive(Serialize)]
    struct Sensor<'a> {
        platform: &'static str,
        name: &'static str,
        unique_id: &'a str,
        state_topic: &'a str,
        device_class: &'static str,
        state_class: &'static str,
        unit_of_measurement: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        suggested_display_precision: Option<u8>,
        #[serde(skip_serializing_if = "Option::is_none")]
        entity_category: Option<&'static str>,
    }

    #[derive(Serialize)]
    struct BinarySensor<'a> {
        platform: &'static str,
        name: &'static str,
        unique_id: &'a str,
        state_topic: &'a str,
        payload_on: &'static str,
        payload_off: &'static str,
        entity_category: &'static str,
    }

    #[derive(Serialize)]
    struct Switch<'a> {
        platform: &'static str,
        name: &'static str,
        unique_id: &'a str,
        command_topic: &'a str,
        state_topic: &'a str,
        payload_on: &'static str,
        payload_off: &'static str,
        optimistic: bool,
    }

    #[derive(Serialize)]
    struct DeviceTrigger<'a> {
        platform: &'static str,
        automation_type: &'static str,
        topic: &'a str,
        payload: &'static str,
        #[serde(rename = "type")]
        trigger_type: &'static str,
        subtype: &'static str,
    }
}
