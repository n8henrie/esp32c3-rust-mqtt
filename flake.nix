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

      system = "aarch64-darwin";
      overlays = [ (import rust-overlay) ];
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
      packages.${system} = {
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
      devShells.${system}.default =
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
