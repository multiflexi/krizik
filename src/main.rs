//! Křižík — monitor USB testers and multimeters over Bluetooth LE and USB.

mod atkc;
mod ble;
mod config;
mod export;
mod firmware;
mod fnirsi;
mod owon;
mod protocol;
mod session;
mod ui;
mod usb;
mod ut70;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

pub const APP_ID: &str = "io.github.multiflexi.Krizik";

fn main() -> glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--probe") {
        let rest: Vec<&str> = args[pos + 1..].iter().map(String::as_str).collect();
        let (addr, secs) = match rest.as_slice() {
            [] => (None, 10),
            [one] if one.parse::<u64>().is_ok() => (None, one.parse().unwrap()),
            [a] => (Some(*a), 10),
            [a, s, ..] => (Some(*a), s.parse().unwrap_or(10)),
        };
        std::process::exit(ble::probe_cli(addr, secs));
    }
    if let Some(code) = firmware_cli(&args) {
        std::process::exit(code);
    }
    let demo = args.iter().any(|a| a == "--demo");
    args.retain(|a| a != "--demo");

    gio::resources_register_include!("krizik.gresource").expect("failed to register resources");

    let app = adw::Application::builder().application_id(APP_ID).build();
    let window: Rc<RefCell<Option<Rc<ui::window::Window>>>> = Rc::default();
    app.connect_activate(glib::clone!(
        #[strong]
        window,
        move |app| {
            let w = window.borrow_mut().get_or_insert_with(|| ui::window::Window::new(app, demo)).clone();
            w.present();
        }
    ));
    app.connect_shutdown(glib::clone!(
        #[strong]
        window,
        move |_| {
            if let Some(w) = window.borrow_mut().take() {
                w.shutdown();
                w.ble.shutdown(Duration::from_secs(3));
            }
        }
    ));
    app.run_with_args(&args)
}

/// `--usb-info`, `--firmware-check FILE`, `--firmware-install FILE`.
fn firmware_cli(args: &[String]) -> Option<i32> {
    let pos = args.iter().position(|a| a == "--usb-info" || a.starts_with("--firmware-"))?;
    let cmd = args[pos].as_str();
    if cmd == "--usb-info" {
        return Some(match (firmware::detect(), firmware::read_app_info()) {
            (None, _) => {
                eprintln!("No tester is connected over USB");
                1
            }
            (Some(firmware::Mode::Bootloader), _) => {
                println!("Tester is in update mode (bootloader)");
                0
            }
            (Some(_), Ok(i)) => {
                println!(
                    "{} · serial {} · firmware {} · hardware {}.{} · bootloader {}.{}",
                    i.model_name,
                    i.serial,
                    i.firmware,
                    i.hardware / 10,
                    i.hardware % 10,
                    i.bootloader / 10,
                    i.bootloader % 10
                );
                0
            }
            (Some(_), Err(e)) => {
                eprintln!("{e}");
                1
            }
        });
    }
    let action = match cmd {
        "--firmware-check" => firmware::Action::Check,
        "--firmware-install" => firmware::Action::Install,
        _ => {
            eprintln!("Unknown option {cmd}");
            return Some(2);
        }
    };
    let Some(path) = args.get(pos + 1) else {
        eprintln!("Usage: krizik {cmd} FILE.atk");
        return Some(2);
    };
    let fw = match firmware::FirmwareFile::load(std::path::Path::new(path)) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return Some(1);
        }
    };
    println!("{}", fw.describe());
    let mut last = -1i64;
    let result = firmware::update(&fw, action, |p| match p {
        firmware::Progress::Stage(s) => println!("{s}"),
        firmware::Progress::Written(f) => {
            let pct = (f * 100.0) as i64;
            if pct / 10 != last / 10 {
                last = pct;
                println!("  {pct} %");
            }
        }
    });
    Some(match result {
        Ok(Some(i)) => {
            println!("Done. The tester reports firmware {}.", i.firmware);
            0
        }
        Ok(None) => {
            println!("Done, but the tester's version could not be read back.");
            0
        }
        Err(e) => {
            eprintln!("Failed: {e}");
            1
        }
    })
}
