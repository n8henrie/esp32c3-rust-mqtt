# https://github.com/NixOS/nixpkgs/issues/393680
{
  cargo,
  makeRustPlatform,
  wrapRustcWith,
  rustc-unwrapped,
  rustPlatform,
  symlinkJoin,
}:
makeRustPlatform {
  inherit cargo;
  rustc = wrapRustcWith {
    inherit rustc-unwrapped;
    sysroot = symlinkJoin {
      name = "rust-${rustc-unwrapped.version}-sysroot-with-src";
      paths = [ rustc-unwrapped ];
      postBuild = ''
        mkdir -p $out/lib/rustlib/src
        ln -s ${rustPlatform.rustcSrc} $out/lib/rustlib/src/rust
      '';
    };
  };
}
