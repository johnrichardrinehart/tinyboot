{ config, pkgs, lib, kconfig, ... }: {
  config = lib.mkIf (config.board == "kukui-jacuzzi-fennel") {
    platforms = [ "aarch64-linux" ];
    linux = {
      configFile = with pkgs.tinybootKernelConfigs; lib.mkDefault (pkgs.concatText "kukui-jacuzzi-fennel-kernel.config" [ generic aarch64 chromebook mediatek ]);
      commandLine = [ "console=ttyS0,115200" "console=tty1" ];
      dtbPattern = "mt8183-kukui-jacuzzi-fennel*";
    };
    coreboot.kconfig = with kconfig; {
      VENDOR_GOOGLE = yes;
      BOARD_GOOGLE_FENNEL = yes;
      FMDFILE = freeform ./layout.fmd;
    };
  };
}
