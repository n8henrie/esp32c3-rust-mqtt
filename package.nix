{
  lib,
  rustc,
  rustPlatform,
  lld,
}:
let
  inherit ((fromTOML (builtins.readFile ./Cargo.toml)).package) name version description;
in

rustPlatform.buildRustPackage {
  pname = name;
  inherit version;
  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ lld ];
  env = {
    RUSTC_BOOTSTRAP = "1";
    RUST_SRC_PATH = "${rustc.src}/library";

    RUSTFLAGS = lib.concatStringsSep " " [
      "-C link-arg=-Tlinkall.x"
      "-C force-frame-pointers"
      "-C link-arg=-Tdefmt.x"

      "-C debuginfo=2"
      "-C strip=none"
    ];
    SSID = builtins.getEnv "SSID";
    PASSWORD = builtins.getEnv "PASSWORD";

    MQTT_HOST = builtins.getEnv "MQTT_HOST";
    MQTT_PORT = builtins.getEnv "MQTT_PORT";

    PUBLISH_TOPIC = builtins.getEnv "PUBLISH_TOPIC";
    RECEIVE_TOPIC = builtins.getEnv "RECEIVE_TOPIC";
    WILL_TOPIC = builtins.getEnv "WILL_TOPIC";
    TEMP_TOPIC = builtins.getEnv "TEMP_TOPIC";
    RSSI_TOPIC = builtins.getEnv "RSSI_TOPIC";

    MQTT_CLIENT_ID = builtins.getEnv "MQTT_CLIENT_ID";
    MQTT_USERNAME = builtins.getEnv "MQTT_USERNAME";
    MQTT_PASSWORD = builtins.getEnv "MQTT_PASSWORD";

    DEFMT_LOG = "debug";
  };

  auditable = false;
  doCheck = false;
  dontFixup = true;

  buildPhase = "cargo build --frozen --release --target riscv32imc-unknown-none-elf -Zbuild-std=core,alloc";
  installPhase = ''
    runHook preInstall
    install -Dm755  "target/riscv32imc-unknown-none-elf/release/${name}" "$out/bin/${name}"
    runHook postInstall
  '';

  meta = {
    inherit description;
    license = lib.licenses.mit;
  };
}
