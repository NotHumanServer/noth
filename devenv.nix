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
    pkgs.gum
    pkgs.jq
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

  # cargo は feature や profile ごとに成果物を作り分けて古いものを消さないので、ビルドのたびに掃除する
  claude.code.hooks.prune-target = {
    name = "target/ の古い成果物を掃除";
    hookType = "PostToolUse";
    matcher = "^Bash$";
    command = "\"$DEVENV_ROOT\"/scripts/prune-target.sh --hook";
  };

  enterShell = ''
    "$DEVENV_ROOT"/scripts/prune-target.sh || true
  '';
}
