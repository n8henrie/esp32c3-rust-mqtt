use alloc::vec::Vec;

use serde::Serialize;

pub(crate) const PAYLOAD_AVAILABLE: &str = "online";
pub(crate) const PAYLOAD_NOT_AVAILABLE: &str = "offline";

pub(crate) const LED_ON: &str = "1";
pub(crate) const LED_OFF: &str = "0";

pub(crate) const BUTTON_PRESS: &str = "press";

#[derive(Clone, Copy)]
pub(crate) struct DiscoveryConfig<'a> {
    pub(crate) device_id: &'a str,
    pub(crate) device_name: &'a str,

    pub(crate) availability_topic: &'a str,

    pub(crate) temperature_topic: &'a str,
    pub(crate) temperature_unique_id: &'a str,

    pub(crate) rssi_topic: &'a str,
    pub(crate) rssi_unique_id: &'a str,

    pub(crate) led_command_topic: &'a str,
    pub(crate) led_state_topic: &'a str,
    pub(crate) led_unique_id: &'a str,

    pub(crate) button_event_topic: &'a str,
}

pub(crate) fn encode(config: DiscoveryConfig<'_>) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&DeviceDiscovery {
        device: Device {
            identifiers: [config.device_id],
            name: config.device_name,
            manufacturer: "Espressif Systems",
            model: "ESP32-C3",
            sw_version: env!("CARGO_PKG_VERSION"),
        },
        origin: Origin {
            name: env!("CARGO_PKG_NAME"),
            sw_version: env!("CARGO_PKG_VERSION"),
        },
        availability_topic: config.availability_topic,
        payload_available: PAYLOAD_AVAILABLE,
        payload_not_available: PAYLOAD_NOT_AVAILABLE,
        components: Components {
            temperature: Sensor {
                platform: "sensor",
                name: "Temperature",
                unique_id: config.temperature_unique_id,
                state_topic: config.temperature_topic,
                device_class: "temperature",
                state_class: "measurement",
                unit_of_measurement: "°F",
                suggested_display_precision: Some(2),
                entity_category: None,
            },
            rssi: Sensor {
                platform: "sensor",
                name: "Wi-Fi RSSI",
                unique_id: config.rssi_unique_id,
                state_topic: config.rssi_topic,
                device_class: "signal_strength",
                state_class: "measurement",
                unit_of_measurement: "dBm",
                suggested_display_precision: None,
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
            button: DeviceTrigger {
                platform: "device_automation",
                automation_type: "trigger",
                topic: config.button_event_topic,
                payload: BUTTON_PRESS,
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
    sw_version: &'static str,
}

#[derive(Serialize)]
struct Origin {
    name: &'static str,
    sw_version: &'static str,
}

#[derive(Serialize)]
struct Components<'a> {
    temperature: Sensor<'a>,
    rssi: Sensor<'a>,
    led: Switch<'a>,
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
