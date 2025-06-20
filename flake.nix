{
  description = "flake to build a no_std rust project for the esp32c3";
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit ((builtins.fromTOML (builtins.readFile ./Cargo.toml)).package) name;

      systems = [
        "x86_64-darwin"
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem =
        with nixpkgs.lib;
        f: foldAttrs mergeAttrs { } (map (s: mapAttrs (_: v: { ${s} = v; }) (f s)) systems);
      overlays = [ (import rust-overlay) ];
    in
    eachSystem (
      system:
      let
        pkgs = import nixpkgs { inherit overlays system; };
        toolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml);
        rustPlatform =
          let
            pkgsCross = import nixpkgs {
              inherit system;
              crossSystem = {
                inherit system;
                rust.rustcTarget = "riscv32imc-unknown-none-elf";
              };
            };
          in
          pkgsCross.makeRustPlatform {
            rustc = toolchain;
            cargo = toolchain;
          };
      in
      {
        packages = {
          default = self.outputs.packages.${system}.${name};
          ${name} = rustPlatform.buildRustPackage {
            inherit name;
            inherit ((builtins.fromTOML (builtins.readFile ./Cargo.toml)).package) version;
            src = pkgs.lib.cleanSource ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes."rust-mqtt-0.3.0" = "sha256-jFXwQ0yG+t9mFam1XqH22kTNUVpsofpic0Ph6zzW8tg=";
            };
            RUSTFLAGS = [
              "-C link-arg=-Tlinkall.x"
              "-C force-frame-pointers"
              "-C linker=rust-lld"
            ];
            doCheck = false;

            SSID = builtins.getEnv "SSID";
            PASSWORD = builtins.getEnv "PASSWORD";
            MQTT_HOST = builtins.getEnv "MQTT_HOST";
            PUBLISH_TOPIC = builtins.getEnv "PUBLISH_TOPIC";
            RECEIVE_TOPIC = builtins.getEnv "RECEIVE_TOPIC";
          };
        };

        # nix develop -i -k SSID -k PASSWORD -c \
        #   cargo build --target=riscv32imc-unknown-none-elf --release
        devShells = {
          default = self.outputs.devShells.${system}.${name};
          ${name} =
            with pkgs;
            mkShell {
              buildInputs = [
                cargo-espflash
                mosquitto
                rust-analyzer
                rustfmt
                toolchain
              ];
            };
        };

        apps.default =
          let
            ESPFLASH_PORT =
              let
                port = builtins.getEnv "ESPFLASH_PORT";
              in
              if port == "" then abort ''port is unset -- source .env and run with "--impure"'' else port;
            script = (
              pkgs.writeShellScriptBin "run" ''
                ${pkgs.espflash}/bin/espflash \
                  flash \
                  --monitor \
                  --port "${ESPFLASH_PORT}" \
                  ${self.outputs.packages.${system}.${name}}/bin/${name}
              ''
            );
          in
          {
            type = "app";
            program = "${script}/bin/run";
          };
      }
    );
}
