/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2026 Shomy
*/

//! Multi-device mode.
//!
//! Activated by typing `multi` before a device command (e.g. `multi wf boot
//! boot.img`). Instead of connecting to a single device and exiting, antumbra
//! keeps watching for devices: every time one is plugged in it is assigned a
//! unique id, gets its own session on a dedicated worker thread, and the same
//! command runs against it with its own progress bar. Several devices are
//! flashed concurrently and independently.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use colored::Colorize;
use penumbra::port::{PortBackend, PortType};
use penumbra::{DeviceBuilder, MtkPort};

use crate::cli::helpers::{enable_multi_progress, set_device_label};
use crate::cli::state::PersistedDeviceState;
use crate::cli::{CliArgs, Commands};

/// How often to scan for newly connected devices.
const SCAN_INTERVAL: Duration = Duration::from_millis(500);

/// Immutable, shareable inputs handed to every worker thread.
struct SharedParams {
    command: Commands,
    da_data: Option<Arc<Vec<u8>>>,
    preloader_data: Option<Arc<Vec<u8>>>,
    auth_data: Option<Arc<Vec<u8>>>,
    da_log_level: penumbra::da::DaLogLevel,
}

/// Runs the given command in multi-device mode until the user stops watching.
pub fn run_multi(args: &CliArgs, command: Commands, state: &PersistedDeviceState) -> Result<()> {
    if !command.is_device_command() {
        bail!("multi-device mode only supports device commands");
    }

    // Load the DA / preloader / auth payloads once and share them (read-only)
    // with every worker thread instead of re-reading the files per device.
    let da_data = load_da(args, state)?.map(Arc::new);
    let preloader_data = match &args.preloader_file {
        Some(path) => Some(Arc::new(std::fs::read(path)?)),
        None => None,
    };
    let auth_data = match &args.auth_file {
        Some(path) => Some(Arc::new(std::fs::read(path)?)),
        None => None,
    };

    let shared = Arc::new(SharedParams {
        command,
        da_data,
        preloader_data,
        auth_data,
        da_log_level: args.da_log_level,
    });

    // One shared MultiProgress so every device's bar gets its own line.
    enable_multi_progress();

    let stop = Arc::new(AtomicBool::new(false));
    spawn_stop_watcher(Arc::clone(&stop));

    println!(
        "{}",
        "Multi-device mode: connect devices to start flashing. Press Enter (or Ctrl-C) to stop."
            .bold()
            .magenta()
    );

    let vid = args.vid;
    let pid = args.pid;
    let configured_backend = args.backend;
    // With `Auto`, the first backend that actually *opens* a device wins and is
    // reused for the rest of the session (MediaTek devices on one host share a
    // driver). This mirrors `find_and_open`'s per-backend open fallback, which
    // the single-device path relies on to reach VCOM serial ports.
    let mut locked_backend: Option<PortBackend> = None;

    let mut seen: HashSet<String> = HashSet::new();
    let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        for (port, uid) in
            discover_new_devices(vid, pid, configured_backend, &mut locked_backend, &seen)
        {
            seen.insert(uid.clone());

            let shared = Arc::clone(&shared);
            let handle = thread::Builder::new()
                .name(format!("flash-{uid}"))
                .spawn(move || flash_device(port, uid, &shared))
                // A spawn failure shouldn't abort the whole watcher.
                .expect("failed to spawn flashing worker");

            workers.push(handle);
        }

        // Reap finished workers so the list doesn't grow unbounded.
        workers.retain(|h| !h.is_finished());

        thread::sleep(SCAN_INTERVAL);
    }

    println!("{}", "Stopping: waiting for in-progress flashes to finish...".bold().yellow());
    for handle in workers {
        let _ = handle.join();
    }

    Ok(())
}

/// Discovers newly connected devices and returns them **already opened**,
/// paired with their uid.
///
/// For an explicit `--backend`, only that backend is used. For `Auto`, the
/// backend that first manages to open a device is locked in via `locked` and
/// reused on later scans; until then every candidate backend is tried in
/// priority order, so a device whose driver one backend can't claim (e.g. a
/// MediaTek VCOM serial port rejected by nusb/libusb) still gets opened by the
/// next one. Devices already in `seen` are skipped, and a uid is only ever
/// considered handled once it has been opened successfully — so a failed open
/// on the wrong backend doesn't block the right one.
fn discover_new_devices(
    vid: Option<u16>,
    pid: Option<u16>,
    configured_backend: PortBackend,
    locked: &mut Option<PortBackend>,
    seen: &HashSet<String>,
) -> Vec<(PortType, String)> {
    let candidates = match configured_backend {
        PortBackend::Auto => (*locked).map_or_else(PortBackend::auto_candidates, |b| vec![b]),
        explicit => vec![explicit],
    };

    for backend in candidates {
        let ports = match PortType::find_all(vid, pid, backend) {
            Ok(ports) => ports,
            Err(e) => {
                log::warn!("Device scan failed on {backend:?}: {e}");
                continue;
            }
        };

        let mut opened = Vec::new();
        for mut port in ports {
            let uid = port.uid();
            if seen.contains(&uid) {
                continue;
            }

            match port.open() {
                Ok(()) => opened.push((port, uid)),
                Err(e) => log::debug!("[{uid}] can't open on {backend:?}: {e}"),
            }
        }

        if !opened.is_empty() {
            // This backend works here; stick with it (only meaningful for Auto).
            if configured_backend == PortBackend::Auto {
                *locked = Some(backend);
            }
            return opened;
        }
    }

    Vec::new()
}

/// Brings an already-opened device up and runs the command against it.
///
/// Each device gets its own [`PersistedDeviceState`] (defaulted, never written
/// back to disk) so concurrent sessions never share cached device info.
fn flash_device(mut port: PortType, uid: String, shared: &SharedParams) {
    set_device_label(uid.clone());

    // The port was already opened during discovery; this is a cheap no-op that
    // also guards against a future refactor handing over an unopened port.
    if let Err(e) = port.open() {
        log::error!("[{uid}] failed to open port: {e}");
        return;
    }

    // Own the payloads for the lifetime of this device so the borrowed slices
    // handed to the builder stay valid.
    let da = shared.da_data.as_deref();
    let pl = shared.preloader_data.as_deref();
    let auth = shared.auth_data.as_deref();

    let mut builder = DeviceBuilder::new(port).with_log_level(shared.da_log_level);
    if let Some(da) = da {
        builder = builder.with_da_data(da);
    }
    if let Some(pl) = pl {
        builder = builder.with_preloader(pl);
    }
    if let Some(auth) = auth {
        builder = builder.with_auth(auth);
    }

    let mut dev = match builder.build() {
        Ok(dev) => dev,
        Err(e) => {
            log::error!("[{uid}] failed to build device: {e}");
            return;
        }
    };

    if let Err(e) = dev.init() {
        log::error!("[{uid}] init failed: {e}");
        return;
    }

    let mut state = PersistedDeviceState::default();
    match shared.command.run_on_device(&mut dev, &mut state) {
        Ok(()) => log::info!("[{uid}] done"),
        Err(e) => log::error!("[{uid}] failed: {e}"),
    }
}

/// Loads the DA payload from the `--da` flag or, failing that, the persisted
/// path, mirroring the single-device path.
fn load_da(args: &CliArgs, state: &PersistedDeviceState) -> Result<Option<Vec<u8>>> {
    if let Some(path) = &args.da_file {
        return Ok(Some(std::fs::read(path)?));
    }
    if let Some(path) = &state.da_file_path {
        return Ok(Some(std::fs::read(Path::new(path))?));
    }
    Ok(None)
}

/// Watches stdin on a background thread; the first line (or EOF) requests a
/// graceful stop. Ctrl-C still hard-stops the process as usual.
fn spawn_stop_watcher(stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        // A single read is enough: any input (even an empty Enter) or EOF stops.
        let _ = stdin.lock().read_line(&mut line);
        stop.store(true, Ordering::Relaxed);
        let _ = std::io::stdout().flush();
    });
}
