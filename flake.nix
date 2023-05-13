{
  description = "A small initramfs for linuxboot";
  inputs = {
    crane.inputs.nixpkgs.follows = "nixpkgs";
    crane.url = "github:ipetkov/crane";
    nixpkgs.url = "nixpkgs/nixos-unstable";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";

    # TODO(jared): delete if/when merged
    nixpkgs-extlinux-specialisation.url = "github:jmbaur/nixpkgs/extlinux-specialisation";
  };
  outputs = inputs: with inputs;
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f {
        inherit system;
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ self.overlays.default ];
        };
      });
    in
    {
      nixosConfigurations =
        let
          base = forAllSystems ({ system, ... }: nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              ({ modulesPath, ... }: {
                disabledModules = [ "${modulesPath}/system/boot/loader/generic-extlinux-compatible" ];
                imports = [ "${nixpkgs-extlinux-specialisation}/nixos/modules/system/boot/loader/generic-extlinux-compatible" ];
              })
              ./test/module.nix
            ];
          });
          extend = extension: nixpkgs.lib.mapAttrs'
            (system: config: nixpkgs.lib.nameValuePair "${extension}-${system}" (config.extendModules {
              modules = [ ./test/${extension}.nix ];
            }));
        in
        nixpkgs.lib.foldAttrs (curr: acc: acc // curr) { } (map (b: extend b base) [ "bls" "grub" "extlinux" ]);
      overlays.default = nixpkgs.lib.composeManyExtensions [
        rust-overlay.overlays.default
        (final: prev: {
          flashrom = prev.callPackage ./flashrom.nix { };
          wolftpm = prev.callPackage ./wolftpm.nix { };
          tinyboot = prev.callPackage ./tinyboot { inherit crane; };
          tinyboot-kernel = prev.callPackage ./kernel.nix { };
          tinyboot-initramfs = prev.callPackage ./initramfs.nix { };
          buildFitImage = prev.callPackage ./fitimage { };
          buildCoreboot = prev.callPackage ./coreboot.nix { };
          coreboot = prev.callPackage ./boards { };
        })
      ];
      devShells = forAllSystems ({ pkgs, ... }: {
        default = with pkgs; mkShellNoCC ({
          inputsFrom = [ tinyboot ];
          nativeBuildInputs = [ bashInteractive grub2 cargo-insta ];
        } // lib.optionalAttrs (tinyboot?env) { inherit (tinyboot) env; });
      });
      legacyPackages = forAllSystems ({ pkgs, ... }: pkgs);
      apps = forAllSystems ({ pkgs, system, ... }: (pkgs.lib.mapAttrs'
        (testName: nixosSystem:
          pkgs.lib.nameValuePair testName {
            type = "app";
            program =
              if nixosSystem.config.nixpkgs.system == system then
                toString (pkgs.callPackage ./test { inherit testName nixosSystem; })
              else
                let
                  pkgsCross = {
                    x86_64-linux = pkgs.pkgsCross.gnu64;
                    aarch64-linux = pkgs.pkgsCross.aarch64-multiplatform;
                  }.${nixosSystem.config.nixpkgs.system};
                in
                toString (pkgsCross.callPackage ./test { inherit testName nixosSystem; });
          })
        self.nixosConfigurations) // { default = self.apps.${system}."bls-${system}"; });
    };
}
