let
  pkgs = import <nixpkgs> { };
in
pkgs.callPackage ./package.nix {
  rustPlatform = pkgs.callPackage ./rustplatform-with-src.nix { };
}
