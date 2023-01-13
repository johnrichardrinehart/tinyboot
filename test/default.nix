{ nixosConfigurations, lib, pkgsBuildBuild, stdenv, substituteAll, tinyboot-initramfs, tinyboot-kernel, ... }:
let
  config = builtins.getAttr stdenv.hostPlatform.system {
    x86_64-linux = {
      qemuFlags = [ ];
      console = "ttyS0";
      module = modulesPath: "${modulesPath}/installer/sd-card/sd-image-x86_64.nix";
    };
    aarch64-linux = {
      qemuFlags = [ "-M" "virt" "-device" "virtio-gpu-pci" ];
      console = "ttyAMA0";
      module = modulesPath: "${modulesPath}/installer/sd-card/sd-image-aarch64.nix";
    };
  };
in
substituteAll {
  src = ./run.bash;
  isExecutable = true;
  path = with pkgsBuildBuild; [ zstd ];
  qemu = "${pkgsBuildBuild.qemu}/bin/qemu-system-${stdenv.hostPlatform.qemuArch}";
  inherit (stdenv.hostPlatform) system;
  inherit (pkgsBuildBuild) bash;
  inherit (config) console qemuFlags;
  kernel = "${tinyboot-kernel}/${stdenv.hostPlatform.linux-kernel.target}";
  initrd = "${tinyboot-initramfs}/initrd";
  drive = toString (nixosConfigurations.${stdenv.hostPlatform.system}.extendModules {
    modules = [
      ({ modulesPath, ... }: {
        imports = [ (config.module modulesPath) ];
        boot.kernelParams = [ "console=${config.console}" ];
      })
    ];
  }).config.system.build.sdImage;
}
