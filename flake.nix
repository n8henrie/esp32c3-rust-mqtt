{
  inputs = {
    # https://github.com/NixOS/nixpkgs/issues/180771#issuecomment-2124815168
    nixpkgs.url = "github:nixos/nixpkgs/release-23.05";
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

    (eachSystem (
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
          ${name} = rustPlatform.buildRustPackage {
            inherit name;
            inherit ((builtins.fromTOML (builtins.readFile ./Cargo.toml)).package) version;
            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            RUSTFLAGS = [
              "-Clink-arg=-Tlinkall.x"
              "-Clink-arg=-Trom_functions.x"
            ];
            buildInputs = with pkgs; [ libiconv ];
            doCheck = false;
            cargoBuildFeatures = [ "nix" ];
            SSID = builtins.getEnv "SSID";
            PASSWORD = builtins.getEnv "PASSWORD";
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
      }
    ));
}
