# NixOS module for buzzd, the remote agent host.
#
#   services.buzzd = {
#     enable = true;
#     user = "ben";
#     listen = [ "tcp://127.0.0.1:7420" ];
#   };
#
# buzzd listens on localhost; expose it with a Tailscale service (or any
# tunnel) and give the GUI the token from `buzzd token`, run as that user.
self:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.buzzd;
  settings = {
    listen = cfg.listen;
    credentials = cfg.credentials;
  }
  // lib.optionalAttrs (cfg.name != null) { name = cfg.name; }
  // lib.optionalAttrs (cfg.workspace != null) { workspace = cfg.workspace; };
  configFile = (pkgs.formats.toml { }).generate "buzzd.toml" settings;
in
{
  options.services.buzzd = {
    enable = lib.mkEnableOption "the buzzd agent host";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.buzzd;
      description = "The buzzd package.";
    };
    user = lib.mkOption {
      type = lib.types.str;
      description = ''
        User the host and its agents run as. Agents work in this user's
        files and use the harnesses installed for them.
      '';
    };
    name = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Name shown in the GUI. Defaults to the hostname.";
    };
    listen = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "tcp://127.0.0.1:7420" ];
      description = "Addresses to listen on: tcp://host:port or unix:///path.";
    };
    workspace = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Default working directory for new agents. Defaults to ~/buzz-agents.";
    };
    credentials = lib.mkOption {
      type = lib.types.enum [ "files" "keyring" ];
      default = "files";
      description = "Where agent keys live: owner-only files, or the user's keyring.";
    };
    path = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = ''
        Extra packages on the host's PATH, such as buzz-acp and agent
        harnesses. Tools on the user's login-shell PATH are found too.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # For `buzzd token`, which reads the same state as the service.
    environment.systemPackages = [ cfg.package ];
    systemd.services.buzzd = {
      description = "Buzz agent host";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = cfg.path ++ [ pkgs.bash pkgs.git ];
      serviceConfig = {
        User = cfg.user;
        ExecStart = "${cfg.package}/bin/buzzd --config ${configFile} run";
        Restart = "on-failure";
        RestartSec = 5;
        # Agents are children of the host; stopping it stops them cleanly.
        KillMode = "mixed";
        TimeoutStopSec = 30;
      };
    };
  };
}
