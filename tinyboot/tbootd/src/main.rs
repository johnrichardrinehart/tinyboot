pub(crate) mod block_device;
pub(crate) mod boot_loader;
pub(crate) mod message;
const TICK_DURATION: Duration = Duration::from_secs(1);

use crate::{
    block_device::{handle_unmounting, mount_all_devs, MountMsg},
    boot_loader::{kexec_execute, kexec_load},
};
use clap::Parser;
use futures::prelude::*;
use log::{debug, error, info, LevelFilter};
use message::InternalMsg;
use nix::{
    libc,
    unistd::{chown, Gid, Uid},
};
use std::{
    collections::VecDeque,
    io::ErrorKind,
    path::Path,
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tboot::{
    linux::LinuxBootEntry,
    message::{Request, Response, ServerCodec, ServerError},
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::broadcast,
    sync::mpsc,
};
use tokio_serde_cbor::Codec;
use tokio_util::codec::Decoder;

async fn handle_client(
    stream: UnixStream,
    mut client_rx: broadcast::Receiver<Response>,
    client_tx: broadcast::Sender<Request>,
) {
    let codec: ServerCodec = Codec::new();
    let (mut sink, mut stream) = codec.framed(stream).split();

    let mut streaming = false;

    let mut unsent_events: VecDeque<Response> = VecDeque::new();

    loop {
        tokio::select! {
            Some(Ok(req)) = stream.next() => {
                if req == Request::Ping {
                    _ = sink.send(Response::Pong).await;
                } else if req == Request::StartStreaming {
                    streaming = true;
                    while let Some(msg) = unsent_events.pop_front() {
                         _ = sink.send(msg).await;
                    }
                } else if req == Request::StopStreaming {
                    streaming = false;
                } else {
                    _ = client_tx.send(req);
                }
            }
            Ok(msg) = client_rx.recv() => {
                if !streaming && matches!(msg, Response::NewDevice(_) | Response::TimeLeft(_)) {
                    // don't send these responses if the client is not streaming
                    unsent_events.push_back(msg);
                } else {
                    _ = sink.send(msg).await;
                }
            }
            else => break,
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum SelectEntryError {
    #[error("reboot")]
    Reboot,
    #[error("poweroff")]
    Poweroff,
    #[error("no default entry")]
    NoDefaultEntry,
    #[error("io error: {0}")]
    Io(tokio::io::Error),
    #[error("nix error: {0}")]
    Nix(nix::Error),
}

impl From<tokio::io::Error> for SelectEntryError {
    fn from(value: tokio::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<nix::Error> for SelectEntryError {
    fn from(value: nix::Error) -> Self {
        Self::Nix(value)
    }
}

async fn select_entry(
    mut internal_rx: mpsc::Receiver<InternalMsg>,
    response_tx: broadcast::Sender<Response>,
) -> Result<LinuxBootEntry, SelectEntryError> {
    let mut start = Instant::now();

    let mut block_devices = Vec::new();
    let mut found_first_device = false;
    let mut default_entry: Option<LinuxBootEntry> = None;
    let mut has_internal_device = false;
    let mut has_user_interaction = false; // can only be set to true, not back to false
    let mut timeout = Duration::from_secs(10);

    // TODO(jared): do this cleanup elsewhere?
    if Path::new(tboot::TINYBOOT_SOCKET).exists() {
        tokio::fs::remove_file(tboot::TINYBOOT_SOCKET).await?;
    }

    let listener = UnixListener::bind(tboot::TINYBOOT_SOCKET)?;
    chown(
        tboot::TINYBOOT_SOCKET,
        Some(Uid::from_raw(tboot::TINYUSER_UID)),
        Some(Gid::from_raw(tboot::TINYUSER_GID)),
    )?;

    let (request_tx, mut request_rx) = broadcast::channel::<Request>(200);

    loop {
        tokio::select! {
            Ok(msg) = request_rx.recv() => {
                match msg {
                    Request::StartStreaming | Request::StopStreaming | Request::Ping => {/* this is handled by the client task */}
                    Request::ListBlockDevices => {
                        _ = response_tx.send(Response::ListBlockDevices(block_devices.clone()));
                    },
                    Request::Boot(entry) => return Ok(entry),
                    Request::UserIsPresent => {
                        has_user_interaction = true;
                        _ = response_tx.send(Response::TimeLeft(None));
                    }
                    Request::Reboot => {
                        return Err(SelectEntryError::Reboot);
                    },
                    Request::Poweroff => {
                        return Err(SelectEntryError::Poweroff);
                    },
                }
            }
            Ok((stream, _)) = listener.accept() => {
                tokio::spawn(handle_client(stream, response_tx.subscribe(), request_tx.clone()));
            },
            Some(internal_msg) = internal_rx.recv() => {
                match internal_msg {
                    InternalMsg::Device(device) => {
                            // only start timeout when we actually have a device to boot
                            if !found_first_device {
                                found_first_device = true;
                                start = Instant::now();
                            }

                            let new_timeout = device.timeout;
                            if new_timeout > timeout {
                                timeout = new_timeout;
                            }

                            let new_entries = &device.boot_entries;

                            // TODO(jared): improve selection of default device
                            if default_entry.is_none() && !has_internal_device {
                                default_entry = new_entries.iter().find(|&entry| entry.default).cloned();

                                // Ensure that if none of the entries from the bootloader were marked as
                                // default, we still have some default entry to boot into.
                                if default_entry.is_none() {
                                    default_entry = new_entries.first().cloned();
                                }

                                if let Some(entry) = &default_entry {
                                    debug!("assigned default entry: {}", entry.display);
                                }
                            }

                            if !device.removable {
                                has_internal_device = true;
                            }

                            _ = response_tx.send(Response::NewDevice(device.clone()));
                            block_devices.push(device);
                    }
                    InternalMsg::Tick => {
                        let elapsed = start.elapsed();

                        // don't send TimeLeft response if timeout <= elapsed, this will panic
                        if !has_user_interaction && timeout > elapsed {
                            _ = response_tx.send(Response::TimeLeft(Some(timeout - elapsed)));
                        }

                        // Timeout has occurred without any user interaction
                        if !has_user_interaction && elapsed >= timeout {
                            if let Some(default_entry) = default_entry {
                                return Ok(default_entry);
                            } else {
                                return Err(SelectEntryError::NoDefaultEntry);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum PrepareBootError {
    #[error("failed to select entry: {0}")]
    SelectEntry(SelectEntryError),
    #[error("io error: {0}")]
    Io(std::io::Error),
}

impl From<SelectEntryError> for PrepareBootError {
    fn from(value: SelectEntryError) -> Self {
        Self::SelectEntry(value)
    }
}

impl From<std::io::Error> for PrepareBootError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

async fn prepare_boot(
    internal_tx: mpsc::Sender<InternalMsg>,
    internal_rx: mpsc::Receiver<InternalMsg>,
) -> Result<(), PrepareBootError> {
    let (response_tx, _) = broadcast::channel::<Response>(200);

    let tick_tx = internal_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TICK_DURATION).await;
            if tick_tx.send(InternalMsg::Tick).await.is_err() {
                break;
            }
        }
    });

    let res = select_entry(internal_rx, response_tx.clone()).await;
    let entry = match res {
        Err(e) => {
            if matches!(e, SelectEntryError::Reboot | SelectEntryError::Poweroff) {
                _ = response_tx.send(Response::ServerDone);
            }
            return Err(e.into());
        }
        Ok(entry) => entry,
    };

    let linux = entry.linux.as_path();
    let initrd = entry.initrd.as_deref();
    let cmdline = entry.cmdline.unwrap_or_default();
    let cmdline = cmdline.as_str();

    match kexec_load(linux, initrd, cmdline).await {
        Ok(()) => _ = response_tx.send(Response::ServerDone),
        Err(e) => {
            _ = response_tx.send(Response::ServerError(match e.kind() {
                ErrorKind::PermissionDenied => ServerError::ValidationFailed,
                _ => ServerError::Unknown,
            }));
            return Err(PrepareBootError::Io(e));
        }
    };

    Ok(())
}

#[derive(Debug, Parser)]
struct Config {
    #[arg(long, value_parser, default_value_t = LevelFilter::Info)]
    log_level: LevelFilter,
}

const VERSION: Option<&'static str> = option_env!("version");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();

    tboot::log::setup_logging(cfg.log_level, Some(Path::new(tboot::log::TBOOTD_LOG_FILE)))?;

    info!("running version {}", VERSION.unwrap_or("devel"));
    debug!("config: {:?}", cfg);

    if (unsafe { libc::getuid() }) != 0 {
        error!("tinyboot not running as root");
        process::exit(1);
    }

    loop {
        let (internal_tx, internal_rx) = mpsc::channel::<InternalMsg>(100);
        let (mount_tx, mount_rx) = mpsc::channel::<MountMsg>(100);

        let done = Arc::new(AtomicBool::new(false));
        let mount_handle = mount_all_devs(internal_tx.clone(), mount_tx.clone(), done.clone());

        let unmount_handle = handle_unmounting(mount_rx);

        let res = prepare_boot(internal_tx, internal_rx).await;

        done.store(true, Ordering::Relaxed);

        if mount_tx.send(MountMsg::UnmountAll).await.is_ok() {
            // wait for unmounting to finish
            info!("waiting for disks to be unmounted");
            _ = mount_handle.await;
            _ = unmount_handle.await;
        }

        match res {
            Err(PrepareBootError::SelectEntry(SelectEntryError::Reboot)) => {
                info!("rebooting");
                unsafe {
                    libc::reboot(libc::LINUX_REBOOT_CMD_RESTART);
                }
            }
            Err(PrepareBootError::SelectEntry(SelectEntryError::Poweroff)) => {
                info!("powering off");
                unsafe {
                    libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
                }
            }
            Err(e) => error!("failed to prepare boot: {e}"),
            Ok(()) => {
                info!("kexec'ing");
                kexec_execute()?
            }
        }
    }
}
