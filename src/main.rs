#![windows_subsystem = "windows"]

mod audio;
mod capture;
mod clitest;
mod config;
mod convert;
mod d3d;
mod dupl;
mod encoder;
mod frames;
mod logging;
mod mux;
mod ring;
mod supervisor;
mod tray;

use std::sync::mpsc;
use std::sync::Arc;

use windows::core::w;
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MF_VERSION, MFSTARTUP_FULL};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::CreateMutexW;

use config::{Config, RuntimeConfig};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let console_mode = args.len() > 1;
    if console_mode {
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }

    let cfg_dir = config::config_dir();
    let _ = std::fs::create_dir_all(&cfg_dir);
    logging::init(cfg_dir.join("clips.log"), console_mode);

    // Single instance guard.
    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\ClipsInstantReplay"));
        if windows::Win32::Foundation::GetLastError() == windows::Win32::Foundation::ERROR_ALREADY_EXISTS {
            log!("main: another instance is running, exiting");
            return;
        }
        std::mem::forget(mutex); // held for process lifetime
    }

    let flag_secs = |flag: &str| -> Option<u64> {
        args.iter()
            .position(|a| a == flag)
            .map(|i| args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(5))
    };
    if let Some(secs) = flag_secs("--capture-test") {
        std::process::exit(clitest::capture_test(secs));
    }
    if let Some(secs) = flag_secs("--encode-test") {
        std::process::exit(clitest::encode_test(secs));
    }
    if let Some(secs) = flag_secs("--record-test") {
        // --backend wgc|dxgi overrides auto-detection for the test run.
        let backend = args
            .iter()
            .position(|a| a == "--backend")
            .and_then(|i| args.get(i + 1).cloned());
        std::process::exit(clitest::record_test(secs, backend));
    }

    let persisted = Config::load(&Config::path());
    let _ = persisted.save(&Config::path()); // materialize defaults on first run
    let runtime = Arc::new(RuntimeConfig::from(&persisted));
    log!("main: starting, config: {:?}", persisted);

    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() {
            log!("main: CoInitializeEx failed: {hr:?}");
            return;
        }
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            log!("main: MFStartup failed: {e}");
            return;
        }
    }

    let (tx, rx) = mpsc::channel::<supervisor::Ctl>();

    let tray = match tray::Tray::create(tx.clone(), runtime.clone()) {
        Ok(t) => t,
        Err(e) => {
            log!("main: tray creation failed: {e}");
            return;
        }
    };
    let tray_handle = tray.handle();

    let sup_runtime = runtime.clone();
    let sup_tx = tx.clone();
    let sup = std::thread::Builder::new()
        .name("supervisor".into())
        .spawn(move || supervisor::run(rx, sup_runtime, tray_handle, sup_tx))
        .expect("spawn supervisor");

    tray.run_message_loop();

    let _ = tx.send(supervisor::Ctl::Quit);
    let _ = sup.join();
    drop(tray);
    unsafe {
        let _ = MFShutdown();
    }
    log!("main: clean exit");
}
