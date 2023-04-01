{ tinybootLog ? "info"
, tinybootTTY ? "tty0" # default to the current foreground virtual terminal
, extraInit ? ""
, extraInittab ? ""
, makeInitrdNG
, busybox
, buildEnv
, tinyboot
, writeScript
, writeText
, ...
}:
let
  initrdEnv = buildEnv {
    name = "initrd-env";
    paths = [
      # starts with a .config crafted from allnoconfig, so we must enable all
      # options we want manually.
      (busybox.override {
        useMusl = true;
        enableStatic = true;
        enableMinimal = true;
        extraConfig = ''
          CONFIG_FEATURE_INIT_MODIFY_CMDLINE y
          CONFIG_FEATURE_INIT_QUIET y
          CONFIG_FEATURE_INIT_SCTTY y
          CONFIG_FEATURE_INIT_SYSLOG y
          CONFIG_FEATURE_MDEV_CONF y
          CONFIG_FEATURE_MDEV_DAEMON y
          CONFIG_FEATURE_MDEV_EXEC y
          CONFIG_FEATURE_MDEV_LOAD_FIRMWARE y
          CONFIG_FEATURE_MDEV_RENAME y
          CONFIG_FEATURE_MDEV_RENAME_REGEXP y
          CONFIG_FEATURE_USE_INITTAB y
          CONFIG_INIT y
          CONFIG_MDEV y
          CONFIG_MKDIR y
          CONFIG_MOUNT y
        '';
      })
      tinyboot
    ];
  };
  rcS = writeScript "rcS" (''
    #!/bin/sh
    mkdir -p /dev/pts /sys /proc /tmp /mnt
    mount -t proc proc /proc
    mount -t sysfs sysfs /sys
    mount -t tmpfs tmpfs /tmp
    mount -t devpts devpts /dev/pts
  '' + extraInit + ''
    mdev -s
  '');
  inittab = writeText "inittab" (''
    ::sysinit:/etc/init.d/rcS
    ::ctrlaltdel:/bin/reboot
    ::shutdown:/bin/umount -ar -t ext4,vfat
    ::restart:/init
    ${tinybootTTY}::once:/bin/tinyboot --log-level=${tinybootLog}
  '' + extraInittab);
in
makeInitrdNG {
  compressor = "xz";
  contents = [
    { object = "${initrdEnv}/bin"; symlink = "/bin"; }
    { object = "${initrdEnv}/bin/init"; symlink = "/init"; }
    { object = "${rcS}"; symlink = "/etc/init.d/rcS"; }
    { object = "${inittab}"; symlink = "/etc/inittab"; }
  ];
}
