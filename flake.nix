{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs";
    # https://github.com/NixOS/nixpkgs/issues/180771#issuecomment-2124815168
    nixpkgs-stable.url = "github:nixos/nixpkgs/release-23.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self
    , nixpkgs
    , nixpkgs-stable
    , rust-overlay
    ,
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
            pkgsCross = import nixpkgs-stable {
              inherit system;
              crossSystem = {
                inherit system;
                rustc.config = "riscv32imc-unknown-none-elf";
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
          ${name} =
            let
              nonemptyEnvVar = name:
                let var = builtins.getEnv name;
                in if var == "" then abort "Environment variable ${name} is empty but required (make sure to use `--impure`)"
                else var;
            in
            rustPlatform.buildRustPackage {
              inherit name;
              inherit ((builtins.fromTOML (builtins.readFile ./Cargo.toml)).package) version;
              src = pkgs.lib.cleanSource ./.;
              cargoLock.lockFile = ./Cargo.lock;
              RUSTFLAGS = [
                "-C link-arg=-Tlinkall.x"
                "-C link-arg=-Trom_functions.x"
                "-C force-frame-pointers"
              ];
              doCheck = false;

              SSID = nonemptyEnvVar "SSID";
              PASSWORD = nonemptyEnvVar "PASSWORD";
              PUBLISH_TOPIC = nonemptyEnvVar "PUBLISH_TOPIC";
              RECEIVE_TOPIC = nonemptyEnvVar "RECEIVE_TOPIC";
              ESPFLASH_PORT = nonemptyEnvVar "ESPFLASH_PORT";

              EMBASSY_EXECUTOR_TASK_ARENA_SIZE = "16384";
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
                toolchain
                rustfmt
                rustPackages.clippy
                rust-analyzer
                cargo-espflash
              ];
            };
        };

        apps.default =
          let
            script = pkgs.writeShellScriptBin "run" ''
              ${pkgs.espflash}/bin/espflash flash --monitor ${self.outputs.packages.${system}.${name}}/bin/${name}
            '';
          in
          {
            type = "app";
            program = "${script}/bin/run";
          };
      }
    );
}
