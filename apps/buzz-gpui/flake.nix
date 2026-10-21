{
  description = "Buzz GPUI client and agent host daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, crane, ... }:
    {
      nixosModules.default = import ./nix/module.nix self;
    }
    // flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        runtimeLibs = with pkgs; [
          wayland
          vulkan-loader
          libxcb
          libxkbcommon
          libx11
          libxcursor
          libxi
          fontconfig
          freetype
          openssl
          dbus
          sqlite
        ];
        # Audio and video playback. GStreamer's setup hook puts each plugin
        # set on GST_PLUGIN_SYSTEM_PATH_1_0 inside the shell.
        playback = with pkgs.gst_all_1; [
          gstreamer
          gst-plugins-base
          gst-plugins-good
          gst-libav
        ];
        craneLib = crane.mkLib pkgs;
        buzzdArgs = {
          pname = "buzzd";
          version = "0.2.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let base = baseNameOf path; in
              !(builtins.elem base [ "target" "target-deb" "dist" ".test-relay" ".claude" "docs" ]);
          };
          cargoExtraArgs = "--locked -p buzzd";
          # Tests need a display, a keyring or a relay; they run in CI-like
          # environments, not in the package build.
          doCheck = false;
          nativeBuildInputs = with pkgs; [ pkg-config cmake perl ];
          buildInputs = with pkgs; [ dbus openssl ];
        };
      in
      {
        # The dependencies build in their own derivation, so CI can keep
        # them and rebuild only this workspace's crates.
        packages.buzzd = craneLib.buildPackage (buzzdArgs // {
          cargoArtifacts = craneLib.buildDepsOnly (buzzdArgs // {
            # Only the release build is reused; skip crane's extra check.
            buildPhaseCargoCommand = "cargoWithProfile build --locked -p buzzd";
          });
          meta.mainProgram = "buzzd";
        });
        packages.default = self.packages.${system}.buzzd;

        devShells.default = pkgs.mkShell {
          # The Rust toolchain buzzd is built with, so local and CI checks match.
          nativeBuildInputs = with pkgs; [ pkg-config cmake perl rustc cargo clippy rustfmt nodejs_22 ];
          buildInputs = runtimeLibs ++ playback ++ (with pkgs; [ vulkan-headers alsa-lib zstd ]);
          env = {
            RUST_BACKTRACE = "1";
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
          };
        };
      });
}
