# The Voltage build from a release: the AppImage that the release workflow
# publishes, run in the FHS environment of nixpkgs' appimageTools, so Nix
# users install it without compiling it. The AppImage carries the app, its
# provider, Node, Buzz's plugins and GStreamer. The environment supplies
# the desktop libraries the AppImage leaves to the system.
# `src` is not an argument: callPackage would fill it from nixpkgs' `src`
# alias, which throws.
{ lib, appimageTools, fetchurl, version, url, hash }:
let
  pname = "buzz-gpui";
  src = fetchurl { inherit url hash; };
  contents = appimageTools.extractType2 { inherit pname version src; };
in
appimageTools.wrapType2 {
  inherit pname version src;
  extraPkgs = pkgs: with pkgs; [
    alsa-lib
    dbus
    fontconfig
    freetype
    fribidi
    harfbuzz
    libdrm
    libgpg-error
    libGL
    libkrb5
    libx11
    libxcb
    libxkbcommon
    vulkan-loader
    wayland
  ];
  extraInstallCommands = ''
    install -Dm644 ${contents}/usr/share/applications/buzz-gpui.desktop -t $out/share/applications
    for size in 128 256 512; do
      install -Dm644 ${contents}/usr/share/icons/hicolor/''${size}x''${size}/apps/buzz-gpui.png \
        -t $out/share/icons/hicolor/''${size}x''${size}/apps
    done
  '';
  meta = {
    description = "Buzz desktop app, Voltage build, from its release AppImage";
    homepage = "https://github.com/voltagecloud/buzz/releases";
    mainProgram = "buzz-gpui";
    platforms = [ "x86_64-linux" "aarch64-linux" ];
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
