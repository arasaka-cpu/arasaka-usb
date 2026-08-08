#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::Result;

fn main() -> Result<()> {
    let prog = std::env::args().next().unwrap_or_default();
    let rest: Vec<String> = std::env::args().skip(1).collect();

    if rest.iter().any(|a| a == "--list-devices" || a == "-l") {
        return list_devices_cli();
    }
    if rest.iter().any(|a| a == "--version" || a == "-V") {
        println!("arasaka-usb {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "usage: {prog} [--list-devices] [--version]\n\
             --list-devices  print detected removable drives and exit"
        );
        return Ok(());
    }
    arasaka_usb::gui::run()
}

fn list_devices_cli() -> Result<()> {
    let devs = arasaka_usb::flash::list_devices()?;
    if devs.is_empty() {
        eprintln!("no removable devices detected");
        std::process::exit(1);
    }
    for d in &devs {
        let gb = d.size as f64 / (1024.0 * 1024.0 * 1024.0);
        println!("{:<12} {:>8.2} GiB  model: {}", d.path, gb, d.model);
    }
    Ok(())
}
