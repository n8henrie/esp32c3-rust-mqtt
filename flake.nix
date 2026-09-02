{
  description = "flake to build a no_std rust project for the esp32c3";
  inputs.nixpkgs.url = "github:nixos/nixpkgs";
  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      inherit ((fromTOML (builtins.readFile ./Cargo.toml)).package) name;

      systems = [
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem =
        with nixpkgs.lib;
        f: foldAttrs mergeAttrs { } (map (s: mapAttrs (_: v: { ${s} = v; }) (f s)) systems);
    in
    eachSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages = {
          default = self.outputs.packages.${system}.${name};
          ${name} = pkgs.callPackage ./package.nix {
            rustPlatform = pkgs.callPackage ./rustplatform-with-src.nix { };
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

        apps =
          let
            ESPFLASH_PORT =
              let
                port = builtins.getEnv "ESPFLASH_PORT";
              in
              if port == "" then
                abort ''espflash port is unset -- source .env and run with "--impure"''
              else
                port;
            makeApp = text: {
              type = "app";
              program = pkgs.lib.getExe (pkgs.writeShellScriptBin "run" text);
            };
          in
          {
            default = makeApp ''
              ${pkgs.espflash}/bin/espflash \
                flash \
                --monitor \
                --log-format defmt \
                --port "${ESPFLASH_PORT}" \
                ${self.outputs.packages.${system}.${name}}/bin/${name}
            '';
            monitor = makeApp ''
              ${pkgs.espflash}/bin/espflash \
                monitor \
                --log-format defmt \
                --port "${ESPFLASH_PORT}"
            '';
          };
      }
    );
}
