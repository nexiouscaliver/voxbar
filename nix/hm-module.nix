# Home-manager module for VoxBar speech-to-text
#
# Provides a systemd user service for autostart.
# Usage: imports = [ handy.homeManagerModules.default ];
#        services.voxbar.enable = true;
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.voxbar;
in
{
  options.services.voxbar = {
    enable = lib.mkEnableOption "VoxBar speech-to-text user service";

    package = lib.mkOption {
      type = lib.types.package;
      defaultText = lib.literalExpression "voxbar.packages.\${system}.voxbar";
      description = "The VoxBar package to use.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.voxbar = {
      Unit = {
        Description = "VoxBar speech-to-text";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        ExecStart = "${cfg.package}/bin/voxbar";
        Restart = "on-failure";
        RestartSec = 5;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
