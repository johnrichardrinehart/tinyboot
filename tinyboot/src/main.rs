pub(crate) mod boot_loader;
pub(crate) mod fs;
pub(crate) mod grub;
pub(crate) mod list;
pub(crate) mod syslinux;
pub(crate) mod util;

use crate::boot_loader::{kexec_execute, kexec_load, BootLoader, MenuEntry};
use crate::fs::{detect_fs_type, find_block_device, unmount, FsType};
use crate::grub::GrubBootLoader;
use crate::list::MenuList;
use crate::syslinux::SyslinuxBootLoader;
use clap::Parser;
use log::LevelFilter;
use log::{debug, error, info};
use nix::mount;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use termion::event::Key;
use termion::input::TermRead;
use termion::raw::IntoRawMode;
use tui::backend::{Backend, TermionBackend};
use tui::layout::{Alignment, Constraint, Direction, Layout};
use tui::style::{Color, Style};
use tui::text::Spans;
use tui::widgets::{Block, List, Paragraph};
use tui::{Frame, Terminal};

fn ui<B: Backend>(
    f: &mut Frame<B>,
    boot_entries: &mut MenuList,
    (has_user_interaction, elapsed, timeout): (bool, Duration, Duration),
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if has_user_interaction {
            vec![Constraint::Percentage(100)]
        } else {
            vec![Constraint::Percentage(90), Constraint::Percentage(10)]
        })
        .split(f.size());

    let items = List::new(boot_entries.display.2.to_vec())
        .block(
            Block::default()
                .title(boot_entries.display.1.clone())
                .title_alignment(Alignment::Left),
        )
        .highlight_style(Style::default().bg(Color::White).fg(Color::Black))
        .highlight_symbol(">>");

    f.render_stateful_widget(items, chunks[0], &mut boot_entries.display.0);

    if !has_user_interaction {
        let time_left = (timeout - elapsed).as_secs();
        let text = vec![Spans::from(format!("Boot in {time_left:?} s."))];
        let paragraph = Paragraph::new(text).alignment(Alignment::Center);
        f.render_widget(paragraph, chunks[1]);
    }
}

fn logic<B: Backend>(terminal: &mut Terminal<B>) -> anyhow::Result<()> {
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
            // TODO(jared): provide menu for picking device configuration
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

    terminal.clear()?;

    let start_instant = Instant::now();

    let mut has_user_interaction = false;

    let timeout = boot_loader.timeout();
    let menu_entries = boot_loader.menu_entries()?;
    let selected_entry_id: Option<String> = 'selection: {
        if let (1, Some(MenuEntry::BootEntry((id, ..)))) = (menu_entries.len(), menu_entries.get(0))
        {
            break 'selection Some(id.to_string());
        }

        let mut boot_entries =
            MenuList::new("tinyboot", menu_entries).expect("menu_entries non-empty");
        // let mut boot_entries = StatefulList::with_items(menu_entries);
        loop {
            terminal.draw(|f| {
                ui(
                    f,
                    &mut boot_entries,
                    (has_user_interaction, start_instant.elapsed(), timeout),
                )
            })?;
            match rx.recv()? {
                Msg::Key(key) => {
                    has_user_interaction = true;
                    match key {
                        Key::Char('l') | Key::Char('\n') => {
                            if let Some(chosen) = boot_entries.select() {
                                break 'selection Some(chosen.to_string());
                            }
                        }
                        Key::Left | Key::Char('h') => boot_entries.exit(),
                        Key::Down | Key::Char('j') => boot_entries.next(),
                        Key::Up | Key::Char('k') => boot_entries.previous(),
                        Key::Char('r') => terminal.clear()?,
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

    let (kernel, initrd, cmdline) = boot_loader.boot_info(selected_entry_id)?;
    kexec_load(kernel, initrd, cmdline)?;

    let mountpoint = boot_loader.mountpoint();
    unmount(mountpoint);

    Ok(kexec_execute()?)
}

#[derive(Debug, Parser)]
struct Config {
    #[arg(long, value_parser, default_value_t = LevelFilter::Info)]
    log_level: LevelFilter,

    #[arg(long, default_value = "/tmp/tinyboot.log")]
    log_file: PathBuf,
}

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
        .chain(fern::log_file(&cfg.log_file)?)
        .apply()?;

    info!("started");
    debug!("config: {:?}", cfg);

    let stdout = io::stdout().lock().into_raw_mode()?;
    let backend = TermionBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    if let Err(e) = logic(&mut terminal) {
        error!("{e}");
    }

    terminal.show_cursor()?;

    Ok(())
}
