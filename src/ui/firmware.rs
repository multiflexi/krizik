//! Firmware update dialog (USB only).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use super::window::Window;
use crate::firmware::{self, Action, AppInfo, FirmwareFile, Mode, Progress};

#[derive(Clone, Debug, PartialEq)]
enum Tester {
    Absent,
    App(Result<AppInfo, String>),
    Bootloader,
}

struct Dialog {
    win: Rc<Window>,
    dialog: adw::Dialog,
    tester_row: adw::ActionRow,
    tester_icon: gtk::Image,
    file_row: adw::ActionRow,
    install: gtk::Button,
    progress: gtk::ProgressBar,
    stage: gtk::Label,
    tester: RefCell<Option<Tester>>,
    file: RefCell<Option<(PathBuf, FirmwareFile)>>,
    busy: Cell<bool>,
}

pub fn show(win: &Rc<Window>) {
    let page = adw::PreferencesPage::new();

    let intro = adw::PreferencesGroup::builder()
        .description(
            "Connect the tester's PC port — the USB-C port opposite the buttons — to this device with a data cable. \
             Firmware cannot be updated over Bluetooth.",
        )
        .build();
    page.add(&intro);

    let group = adw::PreferencesGroup::new();
    let tester_icon = gtk::Image::from_icon_name("krizik-tester-symbolic");
    let tester_row = adw::ActionRow::builder().title("Tester").subtitle("Looking for a tester on USB…").build();
    tester_row.add_prefix(&tester_icon);
    group.add(&tester_row);

    let choose = gtk::Button::builder().label("Choose…").valign(gtk::Align::Center).build();
    let file_row = adw::ActionRow::builder()
        .title("Firmware File")
        .subtitle("An .atk file from ALIENTEK's firmware package")
        .activatable_widget(&choose)
        .build();
    file_row.add_prefix(&gtk::Image::from_icon_name("document-open-symbolic"));
    file_row.add_suffix(&choose);
    group.add(&file_row);
    page.add(&group);

    let install = gtk::Button::builder()
        .label("Install")
        .halign(gtk::Align::Center)
        .sensitive(false)
        .css_classes(["pill", "suggested-action"])
        .build();
    let progress = gtk::ProgressBar::builder().visible(false).show_text(true).build();
    let stage = gtk::Label::builder().wrap(true).visible(false).css_classes(["dim-label"]).build();
    let bottom = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    bottom.append(&install);
    bottom.append(&progress);
    bottom.append(&stage);
    let g = adw::PreferencesGroup::new();
    g.add(&bottom);
    page.add(&g);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    let dialog =
        adw::Dialog::builder().title("Update Firmware").content_width(480).content_height(560).child(&toolbar).build();

    let d = Rc::new(Dialog {
        win: win.clone(),
        dialog,
        tester_row,
        tester_icon,
        file_row,
        install,
        progress,
        stage,
        tester: RefCell::new(None),
        file: RefCell::new(None),
        busy: Cell::new(false),
    });

    choose.connect_clicked(glib::clone!(
        #[weak]
        d,
        move |_| d.choose_file()
    ));
    d.install.connect_clicked(glib::clone!(
        #[weak]
        d,
        move |_| d.confirm()
    ));

    // Watch for the tester while the dialog is open.
    d.poll();
    let weak = Rc::downgrade(&d);
    let source = glib::timeout_add_local(Duration::from_millis(1000), move || match weak.upgrade() {
        Some(d) => {
            d.poll();
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    let source = RefCell::new(Some(source));
    // Keep the dialog state alive while it is open; release it on close.
    let keep = RefCell::new(Some(d.clone()));
    d.dialog.connect_closed(move |_| {
        if let Some(s) = source.borrow_mut().take() {
            s.remove();
        }
        keep.borrow_mut().take();
    });
    d.dialog.present(Some(&win.window));
}

impl Dialog {
    fn poll(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let now = match firmware::detect() {
            None => Tester::Absent,
            Some(Mode::Bootloader) => Tester::Bootloader,
            Some(Mode::App) => match self.tester.borrow().clone() {
                // Query the version only once per connection.
                Some(t @ Tester::App(_)) => t,
                _ => {
                    self.set_tester(Tester::App(Err("Reading the version…".into())));
                    let d = self.clone();
                    glib::spawn_future_local(async move {
                        let r = gio::spawn_blocking(firmware::read_app_info).await;
                        let r = match r {
                            Ok(Ok(i)) => Ok(i),
                            Ok(Err(e)) => Err(e.to_string()),
                            Err(_) => Err("Reading the version failed".into()),
                        };
                        if !d.busy.get() && firmware::detect() == Some(Mode::App) {
                            d.set_tester(Tester::App(r));
                        }
                    });
                    return;
                }
            },
        };
        if self.tester.borrow().as_ref() != Some(&now) {
            self.set_tester(now);
        }
    }

    fn set_tester(&self, t: Tester) {
        let (subtitle, icon) = match &t {
            Tester::Absent => ("Not connected over USB".to_string(), "krizik-tester-symbolic"),
            Tester::Bootloader => ("In update mode".to_string(), "emblem-synchronizing-symbolic"),
            Tester::App(Ok(i)) => (
                format!(
                    "{} · firmware {} · hardware {}.{}",
                    i.model_name,
                    i.firmware,
                    i.hardware / 10,
                    i.hardware % 10
                ),
                "object-select-symbolic",
            ),
            Tester::App(Err(e)) => (e.clone(), "dialog-warning-symbolic"),
        };
        self.tester_row.set_subtitle(&glib::markup_escape_text(&subtitle));
        self.tester_icon.set_icon_name(Some(icon));
        *self.tester.borrow_mut() = Some(t);
        self.update_install();
    }

    fn update_install(&self) {
        let usable = matches!(*self.tester.borrow(), Some(Tester::App(Ok(_))) | Some(Tester::Bootloader));
        self.install.set_sensitive(!self.busy.get() && usable && self.file.borrow().is_some());
    }

    fn choose_file(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("C2 firmware (*.atk)"));
        filter.add_pattern("*.atk");
        filter.add_pattern("*.ATK");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let chooser = gtk::FileDialog::builder().title("Choose Firmware").filters(&filters).modal(true).build();
        let d = self.clone();
        glib::spawn_future_local(async move {
            let Ok(file) = chooser.open_future(Some(&d.win.window)).await else { return };
            let Some(path) = file.path() else { return };
            match FirmwareFile::load(&path) {
                Ok(fw) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    d.file_row.set_title(&glib::markup_escape_text(&name));
                    d.file_row.set_subtitle(&fw.describe());
                    *d.file.borrow_mut() = Some((path, fw));
                }
                Err(e) => {
                    d.file_row.set_title("Firmware File");
                    d.file_row.set_subtitle(&e);
                    *d.file.borrow_mut() = None;
                }
            }
            d.update_install();
        });
    }

    fn confirm(self: &Rc<Self>) {
        let Some((_, fw)) = self.file.borrow().clone() else { return };
        let current = match &*self.tester.borrow() {
            Some(Tester::App(Ok(i))) => format!(" (currently {})", i.firmware),
            _ => String::new(),
        };
        let s = fw.first().series();
        let alert = adw::AlertDialog::new(
            Some("Install Firmware?"),
            Some(&format!(
                "The tester{current} restarts into update mode and firmware {}.{}.x is written. \
                 Do not disconnect it until it restarts.\n\n\
                 If an update is interrupted, hold the tester's return button while connecting it to start the update mode, then try again.",
                s / 10,
                s % 10
            )),
        );
        alert.add_responses(&[("cancel", "_Cancel"), ("install", "_Install")]);
        alert.set_response_appearance("install", adw::ResponseAppearance::Suggested);
        alert.set_close_response("cancel");
        let d = self.clone();
        glib::spawn_future_local(async move {
            if alert.choose_future(Some(&d.dialog)).await == "install" {
                d.run(fw).await;
            }
        });
    }

    async fn run(self: &Rc<Self>, fw: FirmwareFile) {
        self.busy.set(true);
        self.update_install();
        self.dialog.set_can_close(false);
        self.progress.set_visible(true);
        self.progress.set_fraction(0.0);
        self.progress.set_text(Some(""));
        self.stage.set_visible(true);
        let cookie = self.win.app.inhibit(
            Some(&self.win.window),
            gtk::ApplicationInhibitFlags::SUSPEND | gtk::ApplicationInhibitFlags::LOGOUT,
            Some("Updating the tester's firmware"),
        );

        let (tx, rx) = async_channel::unbounded();
        let handle = std::thread::spawn(move || {
            firmware::update(&fw, Action::Install, |p| {
                let _ = tx.send_blocking(p);
            })
        });
        while let Ok(p) = rx.recv().await {
            match p {
                Progress::Stage(s) => self.stage.set_label(&s),
                Progress::Written(f) => {
                    self.progress.set_fraction(f);
                    self.progress.set_text(Some(&format!("{:.0} %", f * 100.0)));
                }
            }
        }
        let result = gio::spawn_blocking(move || handle.join()).await;

        if cookie != 0 {
            self.win.app.uninhibit(cookie);
        }
        self.busy.set(false);
        self.dialog.set_can_close(true);
        match result {
            Ok(Ok(Ok(info))) => {
                self.progress.set_fraction(1.0);
                self.progress.set_text(Some("100 %"));
                let msg = match &info {
                    Some(i) => format!("Firmware {} installed", i.firmware),
                    None => "Firmware installed".to_string(),
                };
                self.stage.set_label(&msg);
                self.win.toast(&msg);
                if let Some(i) = info {
                    self.set_tester(Tester::App(Ok(i)));
                }
            }
            Ok(Ok(Err(e))) => {
                self.stage.set_label(&format!("Update failed: {e}"));
                self.win.toast("Firmware update failed");
                *self.tester.borrow_mut() = None;
            }
            _ => {
                self.stage.set_label("Update failed unexpectedly");
                *self.tester.borrow_mut() = None;
            }
        }
        self.update_install();
    }
}
