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
        # Libraries the app needs when it runs, beyond those it links: the
        # audio backends GStreamer and the terminal use, and zstd.
        appRuntimeLibs = runtimeLibs ++ (with pkgs; [ alsa-lib zstd ]);
        craneLib = crane.mkLib pkgs;
        version = "0.2.0";
        sourceFilter = path: type:
          let base = baseNameOf path; in
          !(builtins.elem base [ "target" "target-deb" "dist" ".test-relay" ".claude" "docs" ]);
        # The workspace path-depends on crates outside it, so the source is
        # the repository root with only this workspace and those crates kept;
        # builds run from apps/buzz-gpui.
        src = pkgs.lib.cleanSourceWith {
          src = ../..;
          filter = path: type:
            let
              rel = pkgs.lib.removePrefix (toString ../.. + "/") (toString path);
              under = dir: rel == dir || pkgs.lib.hasPrefix (dir + "/") rel;
              above = dir: pkgs.lib.hasPrefix (rel + "/") dir;
              # buzz-session-control (remote workspace enrollment) and the
              # root-workspace crates it builds on inherit from the root
              # Cargo.toml.
              kept = [
                "apps/buzz-gpui"
                "crates/buzz-remote-sessions"
                "Cargo.toml"
                "crates/buzz-session-control"
                "crates/buzz-core"
                "crates/buzz-sdk"
              ];
            in
            sourceFilter path type && builtins.any (dir: under dir || above dir) kept;
        };
        workspace = {
          cargoToml = ./Cargo.toml;
          cargoLock = ./Cargo.lock;
          postUnpack = ''
            cd "$sourceRoot/apps/buzz-gpui"
            sourceRoot=.
          '';
        };
        # The Sandbox providers that buzz-gpui-voltage's host runs, from the
        # root workspace: buzz-backend-kubernetes (a session on the user's
        # cluster) and buzz-backend-relay (a session through the Sandbox
        # manager). Building one root crate loads every root member.
        providersArgs = {
          pname = "buzz-sandbox-providers";
          inherit version;
          src = pkgs.lib.cleanSourceWith {
            src = ../..;
            filter = path: type:
              let
                rel = pkgs.lib.removePrefix (toString ../.. + "/") (toString path);
                under = dir: rel == dir || pkgs.lib.hasPrefix (dir + "/") rel;
                above = dir: pkgs.lib.hasPrefix (rel + "/") dir;
                kept = [ "Cargo.toml" "Cargo.lock" ".cargo" "crates" "examples/countdown-bot" ];
              in
              sourceFilter path type && builtins.any (dir: under dir || above dir) kept;
          };
          cargoExtraArgs = "--locked -p buzz-backend-kubernetes -p buzz-backend-relay";
          doCheck = false;
          nativeBuildInputs = with pkgs; [ pkg-config cmake perl protobuf ];
          buildInputs = with pkgs; [ openssl ];
        };
        providers = craneLib.buildPackage (providersArgs // {
          cargoArtifacts = craneLib.buildDepsOnly providersArgs;
        });
        buzzdArgs = {
          pname = "buzzd";
          inherit version src;
          inherit (workspace) cargoToml cargoLock postUnpack;
          cargoExtraArgs = "--locked -p buzzd";
          # Tests need a display, a keyring or a relay; they run in CI-like
          # environments, not in the package build.
          doCheck = false;
          nativeBuildInputs = with pkgs; [ pkg-config cmake perl ];
          buildInputs = with pkgs; [ dbus openssl ];
        };
        gpuiArgs = {
          pname = "buzz-gpui";
          inherit version src;
          inherit (workspace) cargoToml cargoLock postUnpack;
          cargoExtraArgs = "--locked -p buzz-gpui";
          # buzz-app's plugin-manager reads files outside its crate: its build
          # script includes ../build_env.rs, and its catalog embeds each
          # bundled plugin's manifest from the app's src/bundled. Crane
          # vendors each crate on its own, so give the crate copies of both.
          cargoVendorDir = craneLib.vendorCargoDeps {
            inherit src;
            inherit (workspace) cargoLock;
            overrideVendorGitCheckout = ps: drv:
              if pkgs.lib.any (p: p.name == "buzzodz-plugins") ps then
                drv.overrideAttrs
                  (old: {
                    postPatch = (old.postPatch or "") + ''
                      cp crates/build_env.rs crates/plugin-manager/build_env.rs
                      substituteInPlace crates/plugin-manager/build.rs \
                        --replace-fail '#[path = "../build_env.rs"]' '#[path = "build_env.rs"]'
                      for manifest in src/bundled/*/manifest.json; do
                        install -Dm644 "$manifest" "crates/plugin-manager/$manifest"
                      done
                      substituteInPlace crates/plugin-manager/src/lib.rs \
                        --replace-fail '"../../../src/bundled/' '"bundled/'
                    '';
                  })
              else drv;
          };
          doCheck = false;
          nativeBuildInputs = with pkgs; [ pkg-config cmake perl makeWrapper ];
          buildInputs = runtimeLibs ++ playback ++ (with pkgs; [ vulkan-headers alsa-lib zstd ]);
        };
        # Buzz's own Node plugins, built as scripts/package-deb.sh builds
        # them: each folder with a manifest ships it and its dist.
        plugins = pkgs.buildNpmPackage {
          pname = "buzz-gpui-plugins";
          inherit version;
          src = pkgs.lib.cleanSourceWith {
            src = ./plugins;
            filter = path: type:
              !(builtins.elem (baseNameOf path) [ "node_modules" "dist" ]);
          };
          nodejs = pkgs.nodejs_22;
          npmDepsHash = "sha256-OMUW+7fKx3/hSjPMdFondCayCFz8YTlEUTX/yaLhRfY=";
          installPhase = ''
            runHook preInstall
            for manifest in */buzz-plugin.json; do
              name=$(dirname "$manifest")
              mkdir -p "$out/$name"
              cp -r "$manifest" "$name/dist" "$out/$name/"
            done
            runHook postInstall
          '';
          # The Node the app runs them with starts each one.
          doInstallCheck = true;
          installCheckPhase = ''
            runHook preInstallCheck
            node ${./scripts/check-plugins.mjs} "$out"
            runHook postInstallCheck
          '';
        };
        # The app in the layout the .deb installs (crates/app/Cargo.toml):
        # the binary finds its plugins in ../share/buzz-gpui/plugins and
        # the Node that runs them in ../lib/buzz-gpui/node.
        gpuiPackage = { pname, features ? [ ], sandboxProviders ? null }:
          let
            cargoFeatures = pkgs.lib.optionalString (features != [ ])
              " --features ${pkgs.lib.concatStringsSep "," features}";
            args = gpuiArgs // {
              inherit pname;
              cargoExtraArgs = gpuiArgs.cargoExtraArgs + cargoFeatures;
            };
          in
          craneLib.buildPackage (args // {
            cargoArtifacts = craneLib.buildDepsOnly (builtins.removeAttrs args [ "src" ] // {
              # The crates patched in from third_party are dependencies, so
              # they build here in full rather than as crane's stubs.
              dummySrc = craneLib.mkDummySrc {
                inherit src;
                extraDummyScript = ''
                  rm -rf $out/apps/buzz-gpui/third_party
                  cp -r ${pkgs.lib.cleanSourceWith { src = ./third_party; filter = sourceFilter; }} $out/apps/buzz-gpui/third_party
                '';
              };
              buildPhaseCargoCommand = "cargoWithProfile build --locked -p buzz-gpui${cargoFeatures}";
            });
            postInstall = ''
              wrapProgram $out/bin/buzz-gpui \
                --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath appRuntimeLibs} \
                --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : ${pkgs.lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" playback} \
                --suffix PATH : ${pkgs.lib.makeBinPath [ pkgs.ffmpeg ]}
              mkdir -p $out/lib/buzz-gpui/node/bin $out/share/buzz-gpui
              ln -s ${pkgs.nodejs_22}/bin/node $out/lib/buzz-gpui/node/bin/node
              cp -r ${plugins} $out/share/buzz-gpui/plugins
              chmod -R u+w $out/share/buzz-gpui/plugins
              install -Dm644 packaging/linux/buzz-gpui.desktop -t $out/share/applications
              for size in 128 256 512; do
                install -Dm644 assets/icons/buzz-$size.png \
                  $out/share/icons/hicolor/''${size}x''${size}/apps/buzz-gpui.png
              done
            '' + pkgs.lib.optionalString (sandboxProviders != null) ''
              # The host finds providers beside its executable first.
              ln -s ${sandboxProviders}/bin/buzz-backend-* $out/bin/
            '';
            passthru = { inherit plugins; };
            meta = {
              mainProgram = "buzz-gpui";
              platforms = pkgs.lib.platforms.linux;
            };
          });
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

        packages.buzz-gpui = gpuiPackage { pname = "buzz-gpui"; };
        # Voltage's build: remote workspaces (Kubernetes Sandboxes), with the
        # buzz-backend-kubernetes and buzz-backend-relay providers beside the
        # app.
        packages.buzz-gpui-voltage = gpuiPackage {
          pname = "buzz-gpui-voltage";
          features = [ "voltage-sandbox" ];
          sandboxProviders = providers;
        };
        packages.buzz-sandbox-providers = providers;

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
