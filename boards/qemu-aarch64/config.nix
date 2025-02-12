# TODO(jared): vboot not tested on this platform
{ config, pkgs, lib, ... }: {
  config = lib.mkIf (config.board == "qemu-aarch64") {
    platforms = [ "aarch64-linux" ];
    qemu.enable = true;
    qemu.flags = [ "-M" "virt,secure=on,virtualization=on" "-cpu" "cortex-a53" "-device" "tpm-tis-device,tpmdev=tpm0" ];
    linux = {
      configFile = with pkgs.tinybootKernelConfigs; lib.mkDefault (pkgs.concatText "qemu-aarch64-kernel.config" [ debug generic aarch64 qemu network ./kernel.config ]);
      dtb = lib.mkDefault (pkgs.buildPackages.runCommand "qemu-aarch64.dtb" { depsBuildBuild = [ pkgs.pkgsBuildBuild.qemu ]; } ''
        qemu-system-aarch64 -M virt,secure=on,virtualization=on,dumpdtb=$out -cpu cortex-a53 -m 2G -smp 2 -nographic
      '');
    };
    tinyboot.tty = lib.mkDefault "ttyAMA0";
    coreboot.kconfig = with lib.kernel; {
      BOARD_EMULATION = yes;
      BOARD_EMULATION_QEMU_AARCH64 = yes;
      FMDFILE = freeform ./layout.fmd;
    };
  };
}
