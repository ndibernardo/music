{
  description = "Rust development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      flake-parts,
      rust-overlay,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem =
        { system, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          };
          nightlyRustfmt = pkgs.rust-bin.nightly.latest.rustfmt;
        in
        {
          devShells.default = pkgs.mkShell {
            packages = [
              rustToolchain
              nightlyRustfmt
              pkgs.nixd
              pkgs.nixfmt

              # GTK4 and rendering stack
              pkgs.gtk4
              pkgs.glib
              pkgs.pango
              pkgs.cairo
              pkgs.gdk-pixbuf
              pkgs.graphene
              pkgs.libadwaita

              # Build tooling
              pkgs.pkg-config
              pkgs.wrapGAppsHook4

              # Audio (rodio ALSA backend)
              pkgs.alsa-lib
            ];

            shellHook = ''
              # Use nightly rustfmt so unstable options (imports_granularity, group_imports) apply.
              export RUSTFMT="${nightlyRustfmt}/bin/rustfmt"
              export XDG_DATA_DIRS="${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:$XDG_DATA_DIRS"

              echo "Rust development environment — musicplayer-rs"
              echo "Rust version: $(rustc --version)"
              echo "Cargo version: $(cargo --version)"
            '';
          };
        };
    };
}
