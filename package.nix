{
  lib,
  rustc,
  rustPlatform,
  lld,
}:
let
  inherit ((lib.importTOML ./Cargo.toml).package) name version description;
in

rustPlatform.buildRustPackage {
  pname = name;
  inherit version;
  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ lld ];
  env =
    let
      requiredEnvVar =
        var:
        let
          val = builtins.getEnv var;
        in
        if val == "" then throw "required environment variable ${var} not found" else val;
    in
    {
      RUSTC_BOOTSTRAP = "1";
      RUST_SRC_PATH = "${rustc.src}/library";

      SSID = requiredEnvVar "SSID";
      PASSWORD = requiredEnvVar "PASSWORD";

      MQTT_HOST = requiredEnvVar "MQTT_HOST";
      MQTT_PORT = requiredEnvVar "MQTT_PORT";

      MQTT_CLIENT_ID = requiredEnvVar "MQTT_CLIENT_ID";
      MQTT_USERNAME = requiredEnvVar "MQTT_USERNAME";
      MQTT_PASSWORD = requiredEnvVar "MQTT_PASSWORD";

      MQTT_TOPIC_PREFIX = requiredEnvVar "MQTT_TOPIC_PREFIX";
      DEVICE_NAME = requiredEnvVar "DEVICE_NAME";

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
