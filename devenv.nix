{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  packages = [
    pkgs.git
    pkgs.bacon
    pkgs.cargo-nextest
  ];

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
    lld.enable = true;
  };

  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy.enable = true;
    nixfmt.enable = true;
  };

  claude.code.enable = true;
}
