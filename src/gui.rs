use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use gtk4::glib;
use gtk4::prelude::*;

use crate::download::{assemble, cache_dir, verify_file, Progress, Reporter, TMP_WORKSPACE_MARGIN};
use crate::flash::{flash, list_devices, Device};
use crate::{client, fetch_manifest, Manifest, Source};

/// Follow the OS light/dark preference. GTK 4.8+ exposes the resolved scheme
/// in `gtk-color-scheme` ("prefer-dark" / "prefer-light" / "default"); we
/// mirror the explicit values onto `gtk-application-prefer-dark-theme` so the
/// app adapts on every platform, and let GTK handle the "default" case.
fn sync_system_theme(settings: &gtk4::Settings) {
    let scheme: gtk4::glib::GString = settings.property("gtk-color-scheme");
    match scheme.as_str() {
        "prefer-dark" => settings.set_gtk_application_prefer_dark_theme(true),
        "prefer-light" => settings.set_gtk_application_prefer_dark_theme(false),
        _ => {}
    }
}

enum UiMsg {
    Devices(Vec<Device>),
    Status(String),
    Progress(f64),
    Done(Result<(), String>),
}

struct GuiReporter(mpsc::Sender<UiMsg>);
impl Reporter for GuiReporter {
    fn report(&mut self, p: Progress) {
        let _ = self.0.send(UiMsg::Status(format!(
            "{} ({}/{})",
            p.label, p.done, p.total
        )));
        let frac = if p.total > 0 {
            p.done as f64 / p.total as f64
        } else {
            0.0
        };
        let _ = self.0.send(UiMsg::Progress(frac));
    }
}

pub fn run() -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id("org.arasaka.usb")
        .build();
    app.connect_activate(build_ui);
    app.run();
    Ok(())
}

fn build_ui(app: &gtk4::Application) {
    if let Some(settings) = gtk4::Settings::default() {
        sync_system_theme(&settings);
        settings.connect_notify_local(Some("gtk-color-scheme"), |settings, _| {
            sync_system_theme(settings);
        });
    }
    gtk4::Window::set_default_icon_name("arasaka-usb");
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("Arasaka USB Flasher")
        .default_width(720)
        .default_height(420)
        .build();

    let header = gtk4::HeaderBar::builder()
        .title_widget(&gtk4::Label::builder().label("Arasaka USB Flasher").build())
        .build();
    window.set_titlebar(Some(&header));

    let vbox = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(16)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    let subtitle = gtk4::Label::builder()
        .label("Download, verify, and provision the Arasaka Linux image to a USB drive.")
        .wrap(true)
        .halign(gtk4::Align::Start)
        .build();

    let device_label = gtk4::Label::builder()
        .label("Target drive:")
        .halign(gtk4::Align::Start)
        .build();

    let device_model = gtk4::StringList::new(&[]);
    let device_combo = gtk4::DropDown::builder()
        .model(&device_model)
        .expression(
            gtk4::PropertyExpression::new(
                gtk4::StringObject::static_type(),
                None::<gtk4::ConstantExpression>,
                "string",
            )
            .upcast(),
        )
        .build();

    let refresh_btn = gtk4::Button::builder().label("Refresh").build();

    let progress = gtk4::ProgressBar::builder()
        .fraction(0.0)
        .show_text(true)
        .build();

    let status = gtk4::Label::builder()
        .label("Ready.")
        .wrap(true)
        .halign(gtk4::Align::Start)
        .build();

    let flash_btn = gtk4::Button::builder()
        .label("Flash")
        .css_classes(vec!["suggested-action".to_string()])
        .build();

    let device_row = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(8)
        .build();
    device_row.append(&device_label);
    device_row.append(&device_combo);
    device_row.append(&refresh_btn);

    vbox.append(&subtitle);
    vbox.append(&device_row);
    vbox.append(&progress);
    vbox.append(&status);
    vbox.append(&flash_btn);

    window.set_child(Some(&vbox));
    window.present();

    let ui = Ui {
        device_label: device_label.clone(),
        device_model: device_model.clone(),
        device_combo: device_combo.clone(),
        progress: progress.clone(),
        status: status.clone(),
        flash_btn: flash_btn.clone(),
    };

    let (tx, rx) = mpsc::channel::<UiMsg>();
    let ui_timeout = ui.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        while let Ok(msg) = rx.try_recv() {
            apply_msg(&ui_timeout, msg);
        }
        glib::ControlFlow::Continue
    });

    let devs: Arc<std::sync::Mutex<Vec<Device>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let devs_for_refresh = Arc::clone(&devs);
    let tx_refresh = tx.clone();
    let ui_refresh = ui.clone();
    refresh_btn.connect_clicked(move |_| {
        ui_refresh.status.set_label("Scanning for drives…");
        let devs = Arc::clone(&devs_for_refresh);
        let tx = tx_refresh.clone();
        thread::spawn(move || {
            let list = match list_devices() {
                Ok(l) => l,
                Err(e) => {
                    crate::debug!("device scan failed: {e:?}");
                    let _ = tx.send(UiMsg::Devices(Vec::new()));
                    let _ = tx.send(UiMsg::Status(format!("Device scan failed: {e}")));
                    return;
                }
            };
            *devs.lock().unwrap() = list.clone();
            let _ = tx.send(UiMsg::Devices(list));
        });
    });

    let devs_for_flash = Arc::clone(&devs);
    let tx_flash = tx.clone();
    let ui_flash = ui.clone();
    flash_btn.connect_clicked(move |_| {
        ui_flash.flash_btn.set_sensitive(false);
        ui_flash.status.set_label("Starting…");
        let devs = Arc::clone(&devs_for_flash);
        let tx = tx_flash.clone();
        let sel = ui_flash.device_combo.selected() as usize;
        thread::spawn(move || {
            let dev = {
                let list = devs.lock().unwrap();
                list.get(sel).cloned()
            };
            let Some(dev) = dev else {
                let _ = tx.send(UiMsg::Done(Err(
                    "No drive selected. Refresh the list and pick one.".into(),
                )));
                return;
            };
            let result = run_flash(&tx, &dev);
            let _ = tx.send(UiMsg::Done(result.map_err(|e| e.to_string())));
        });
    });

    let tx_initial = tx.clone();
    thread::spawn(move || {
        let list = match list_devices() {
            Ok(l) => l,
            Err(e) => {
                crate::debug!("initial device scan failed: {e:?}");
                let _ = tx_initial.send(UiMsg::Devices(Vec::new()));
                let _ = tx_initial.send(UiMsg::Status(format!("Device scan failed: {e}")));
                return;
            }
        };
        *devs.lock().unwrap() = list.clone();
        let _ = tx_initial.send(UiMsg::Devices(list));
    });
}

#[derive(Clone)]
struct Ui {
    device_label: gtk4::Label,
    device_model: gtk4::StringList,
    device_combo: gtk4::DropDown,
    progress: gtk4::ProgressBar,
    status: gtk4::Label,
    flash_btn: gtk4::Button,
}

fn apply_msg(ui: &Ui, msg: UiMsg) {
    match msg {
        UiMsg::Devices(devices) => {
            if devices.is_empty() {
                ui.status.set_label(
                    "No removable drives detected. Connect a USB drive and select Refresh.",
                );
                ui.device_label.set_label("Target drive: (none)");
            } else {
                ui.device_label
                    .set_label(&format!("Target drive: ({})", devices.len()));
                let items: Vec<String> = devices
                    .iter()
                    .map(|d| {
                        let gb = d.size as f64 / (1024.0 * 1024.0 * 1024.0);
                        format!("{} — {} ({:.1} GiB)", d.path, d.model, gb)
                    })
                    .collect();
                let refs: Vec<&str> = items.iter().map(|s| s.as_str()).collect();
                ui.device_model.splice(0, ui.device_model.n_items(), &refs);
                ui.device_combo.set_selected(0);
            }
        }
        UiMsg::Status(s) => ui.status.set_label(&s),
        UiMsg::Progress(f) => ui.progress.set_fraction(f),
        UiMsg::Done(Ok(())) => {
            ui.status
                .set_label("Provisioning complete. The drive is safe to remove.");
            ui.flash_btn.set_sensitive(true);
        }
        UiMsg::Done(Err(e)) => {
            ui.status.set_label(&e);
            ui.flash_btn.set_sensitive(true);
        }
    }
}

fn run_flash(tx: &mpsc::Sender<UiMsg>, dev: &Device) -> Result<()> {
    let src = Source::default();
    let c = client()?;
    let _ = tx.send(UiMsg::Status(
        "Locating current Arasaka Linux image…".to_string(),
    ));
    let m: Manifest = fetch_manifest(&c, &src)?;
    let _ = tx.send(UiMsg::Status(format!(
        "{} — {} bytes, {} parts",
        m.file,
        m.total,
        m.parts.len()
    )));

    // Keep the verified image on disk so a later launch reuses it instead of
    // re-downloading. The manifest is always fetched first; if the cached
    // file no longer matches (newer build, new sha/size), it is replaced.
    let cache_dir = cache_dir(m.total + TMP_WORKSPACE_MARGIN);
    let image = cache_dir.join(&m.file);
    let digest = match verify_file(&image, &m) {
        Ok(d) => {
            let _ = tx.send(UiMsg::Status(format!(
                "Using cached image {} (already verified)",
                m.file
            )));
            d
        }
        Err(_) => {
            let partial = cache_dir.join(format!("{}.partial", m.file));
            let reporter = GuiReporter(tx.clone());
            let _ = tx.send(UiMsg::Progress(0.0));
            let d = assemble(&src, &m, &partial, Box::new(reporter))?;
            std::fs::rename(&partial, &image).context("move verified image into cache")?;
            d
        }
    };
    let _ = tx.send(UiMsg::Status(format!("sha256 verified: {}", digest)));
    let _ = tx.send(UiMsg::Progress(0.0));

    let dev = dev.clone();
    flash(&dev, &image, |written| {
        let frac = written as f64 / m.total as f64;
        let _ = tx.send(UiMsg::Progress(frac));
        let _ = tx.send(UiMsg::Status(format!("Flashing… {:.1}%", frac * 100.0)));
    })?;
    Ok(())
}
