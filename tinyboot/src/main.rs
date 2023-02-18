pub(crate) mod boot_loader;
pub(crate) mod fs;
pub(crate) mod grub;
pub(crate) mod syslinux;
pub(crate) mod util;

use crate::boot_loader::{kexec_execute, kexec_load, BootLoader, MenuEntry};
use crate::fs::{detect_fs_type, find_block_device, unmount, FsType};
use crate::grub::GrubBootLoader;
use crate::syslinux::SyslinuxBootLoader;
use clap::Parser;
use log::LevelFilter;
use log::{debug, error, info};
use nix::{libc, mount};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use termion::event::Key;
use termion::input::TermRead;
use termion::raw::IntoRawMode;
use termion::{clear, cursor};

fn flatten_entries(entries: Vec<MenuEntry>) -> Vec<(&str, &str)> {
    let mut flattened = Vec::new();
    for entry in entries {
        match entry {
            MenuEntry::BootEntry(boot_entry) => flattened.push(boot_entry),
            MenuEntry::SubMenu((_, _, sub_entries)) => {
                flattened.extend(flatten_entries(sub_entries));
            }
        }
    }
    flattened
}

fn logic() -> anyhow::Result<()> {
    let mut boot_loaders = find_block_device(|_| true)?
        .iter()
        .filter_map(|dev| {
            let mountpoint = PathBuf::from("/mnt").join(
                dev.to_str()
                    .expect("invalid unicode")
                    .trim_start_matches('/')
                    .replace('/', "-"),
            );

            let Ok(fstype) = detect_fs_type(dev) else {
                debug!("failed to detect fstype on {:?}", dev);
                return None;
            };
            debug!("detected {:?} fstype on {:?}", fstype, dev);

            if let Err(e) = std::fs::create_dir_all(&mountpoint) {
                error!("failed to create mountpoint: {e}");
                return None;
            }

            if let Err(e) = mount::mount(
                Some(dev.as_path()),
                &mountpoint,
                Some(match fstype {
                    FsType::Ext4(..) => "ext4",
                    FsType::Fat32(..) | FsType::Fat16(..) => "vfat",
                }),
                mount::MsFlags::MS_RDONLY,
                None::<&[u8]>,
            ) {
                error!("mount({}): {e}", dev.display());
                return None;
            }

            debug!("mounted {} at {}", dev.display(), mountpoint.display());

            let boot_loader: Box<dyn BootLoader> = 'loader: {
                match GrubBootLoader::new(&mountpoint) {
                    Ok(grub) => {
                        debug!("found grub bootloader");
                        break 'loader Box::new(grub);
                    }
                    Err(e) => error!(
                        "error loading grub configuration from {}: {e}",
                        mountpoint.display()
                    ),
                }
                match SyslinuxBootLoader::new(&mountpoint) {
                    Ok(syslinux) => {
                        debug!("found syslinux bootloader");
                        break 'loader Box::new(syslinux);
                    }
                    Err(e) => error!(
                        "error loading syslinux configuration from {}: {e}",
                        mountpoint.display()
                    ),
                }
                unmount(&mountpoint);
                return None;
            };

            Some(boot_loader)
        })
        .collect::<Vec<Box<dyn BootLoader>>>();

    let mut boot_loader = {
        if boot_loaders.is_empty() {
            anyhow::bail!("no boot configurations");
        } else {
            // TODO(jared): provide a way for the user to pick the device to boot from
            let chosen_loader = boot_loaders.swap_remove(0);

            // unmount non-chosen devices
            for loader in boot_loaders {
                unmount(loader.mountpoint());
            }

            chosen_loader
        }
    };

    info!(
        "using boot loader from device mounted at {}",
        boot_loader.mountpoint().display()
    );

    enum Msg {
        Key(Key),
        Tick,
    }

    let (tx, rx) = mpsc::channel::<Msg>();

    let tick_tx = tx.clone();
    thread::spawn(move || {
        let tick_duration = Duration::from_secs(1);
        loop {
            thread::sleep(tick_duration);
            if tick_tx.send(Msg::Tick).is_err() {
                break;
            }
        }
    });

    thread::spawn(move || {
        let mut keys = io::stdin().lock().keys();
        while let Some(Ok(key)) = keys.next() {
            if tx.send(Msg::Key(key)).is_err() {
                break;
            }
        }
    });

    let start_instant = Instant::now();

    let timeout = boot_loader.timeout();
    let mut menu_entries = flatten_entries(boot_loader.menu_entries()?);

    menu_entries.push(("reboot", "Reboot"));
    menu_entries.push(("poweroff", "Poweroff"));
    menu_entries.push(("shell", "Exit to shell"));

    let selected_entry_id: Option<&str> = 'selection: {
        let mut stdout = io::stdout().into_raw_mode()?;

        write!(
            stdout,
            "--------------------------------------------------------------------------------\r\n"
        )?;
        for (i, entry) in menu_entries.iter().enumerate() {
            write!(stdout, "{}:      {}\r\n", i + 1, entry.1)?;
        }
        write!(stdout, r#"Enter choice: "#)?;

        let mut has_user_interaction = false;
        let mut user_input = String::new();

        loop {
            stdout.flush()?;
            match rx.recv()? {
                Msg::Key(key) => {
                    has_user_interaction = true;
                    match key {
                        Key::Backspace => {
                            _ = user_input.pop();
                            write!(stdout, "{}{}", cursor::Left(1), clear::AfterCursor)?;
                        }
                        Key::Ctrl('u') => {
                            write!(
                                stdout,
                                "{}{}",
                                cursor::Left(user_input.len() as u16),
                                clear::AfterCursor
                            )?;
                            user_input.clear();
                        }
                        Key::Char('\n') => {
                            if user_input.is_empty() {
                                anyhow::bail!("exit")
                            }

                            let Ok(num) = str::parse::<usize>(&user_input) else {
                                anyhow::bail!("did not input a number");
                            };

                            let Some(entry) = menu_entries.get(num - 1) else {
                                anyhow::bail!("boot entry does not exist");
                            };

                            break 'selection Some(entry.0);
                        }
                        Key::Char(c) => {
                            if c.is_ascii_digit() {
                                user_input.push(c);
                                write!(stdout, "{c}")?;
                            } else {
                                anyhow::bail!("not a numeric input");
                            }
                        }
                        Key::Ctrl('c') | Key::Ctrl('g') => {
                            anyhow::bail!("exit")
                        }
                        _ => {}
                    };
                }
                Msg::Tick => {
                    // Timeout has occurred without any user interaction
                    if !has_user_interaction && start_instant.elapsed() >= timeout {
                        break 'selection None;
                    }
                }
            }
        }
    };

    let mountpoint = boot_loader.mountpoint();

    match selected_entry_id {
        Some("shell") => {
            unmount(mountpoint);
            anyhow::bail!("exit")
        }
        Some("poweroff") => {
            unmount(mountpoint);
            unsafe { libc::sync() };
            let ret = unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF) };
            if ret < 0 {
                anyhow::bail!(io::Error::last_os_error());
            }
        }
        Some("reboot") => {
            unmount(mountpoint);
            unsafe { libc::sync() };
            let ret = unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_RESTART) };
            if ret < 0 {
                anyhow::bail!(io::Error::last_os_error());
            }
        }
        _ => {
            let (kernel, initrd, cmdline) =
                boot_loader.boot_info(selected_entry_id.map(|s| s.to_string()))?;
            kexec_load(kernel, initrd, cmdline)?;

            let mountpoint = boot_loader.mountpoint();
            unmount(mountpoint);

            kexec_execute()?;
        }
    }

    Ok(())
}

#[derive(Debug, Parser)]
struct Config {
    #[arg(long, value_parser, default_value_t = LevelFilter::Info)]
    log_level: LevelFilter,
}

const VERSION: Option<&'static str> = option_env!("version");

fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}] {}",
                record.target(),
                record.level(),
                message
            ))
        })
        .level(cfg.log_level)
        .chain(io::stderr())
        .apply()?;

    info!("running version {}", VERSION.unwrap_or("devel"));
    debug!("config: {:?}", cfg);

    if let Err(e) = logic() {
        error!("{e}");
    }

    loop {
        Command::new("/bin/sh").spawn()?.wait()?;
    }
}
