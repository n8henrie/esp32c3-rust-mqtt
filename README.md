# esp32c3-rust-mqtt [![Build project with nix](https://github.com/n8henrie/esp32c3-rust-mqtt/actions/workflows/build.yml/badge.svg)](https://github.com/n8henrie/esp32c3-rust-mqtt/actions/workflows/build.yml)

This will eventually be a little rust-based esp32 project for connecting up esp32 to mqtt, using nix as a build system.
I'm hoping to have it build from aarch64-darwin as well as x86_64-linux.
It will eventually be hooked into home-assistant.

Using nix adds some complexity but should help ensure reproducibility; my blog post detailing my process for getting a prior iteration working (prior to [the release of `esp-wifi` on crates.io](https://crates.io/crates/esp-wifi)) may be useful to novices and hobbyists (such as myself): https://n8henrie.com/2023/09/compiling-rust-for-the-esp32-with-nix/

## Security

This project takes the straightforward route of pulling sensitive values from environment variables and burning them into the microcontroller.
This means that an attacker with physical access could almost certainly pull that information from the microcontroller or the build artifacts from compiling this project.

These values include but are not limited to:
- network SSID
- network password
- MQTT username
- MQTT password

Further, I have taken the lazy route of inheriting these environment values into the nix environment, and as such the nix store will have a *world-readable* copy of all these values.
This could be mitigated to some degree by something like sops or agenix, but the final binary (also in the nix store) would still contain the values, so I don't think this effort is worthwhile at this time.

## Quickstart

```
$ git clone https://github.com/n8henrie/esp32c3-rust-mqtt
$ cd esp32c3-rust-mqtt
$ cp .env{.sample,}
$ $EDITOR .env
$ . .env
$ cargo run --release
```

Alternatively, `nix run --impure` instead of `cargo run --release`.

## Power Usage

Just out of interest, my ESP32C3 running this project uses about 0.075A @ 5.24V (single point in time), and its usage over time seems a little lower at 0.10Wh over 18 mins.

## TODO

- [ ] `defmt` doesn't like the variables in the `println` statements. Is it worth converting them?
- [ ] would a brief sleep in the hot loop meaningfully decrease the power usage?
- [ ] is there a way to have each receive and publish task by their own `embassy_executor::task`?
  - [ ] if so, is this desireable?

## Related projects to keep an eye on

- https://github.com/bjoernQ/esp32-rust-nostd-temperature-logger
- https://github.com/mountainlizard/mountain-mqtt/
- https://github.com/siemtim-dev/embedded-mqttc
- https://github.com/11mad11/esp32
- https://github.com/bjoernQ/esp32-rust-nostd-temperature-logger
