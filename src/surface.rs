// The lock surface. Prints "locked" once the compositor confirms the lock.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use gtk4_session_lock::Instance;

use crate::{Options, pam, ppm, snapshot_dir};

const DEFAULT_COLORS: &str = "
@define-color bg #0f1825;
@define-color bg_alt #253447;
@define-color accent #418aa2;
@define-color fg #e8f4f8;
@define-color fg_dim #6b8494;
@define-color fg_muted #96a3ae;
@define-color urgent #f4d06f;
@define-color shadow #05090f;
";

struct Screen {
    clock: gtk::Label,
    date: gtk::Label,
    field: gtk::Box,
    input: gtk::Text,
    message: gtk::Label,
}

struct Lock {
    options: Options,
    user: String,
    screens: RefCell<Vec<Screen>>,
    checking: Cell<bool>,
    failed: Cell<bool>,
    message: RefCell<String>,
    /// Set while copying text between screens, so the copies do not echo
    syncing: Cell<bool>,
    instance: Instance,
}

fn config_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("wakelock")
}

fn load_style() {
    // Automatic font rendering clips the top row of some glyphs with the GPU renderers
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_font_rendering(gtk::FontRendering::Manual);
    }
    let colors = std::fs::read_to_string(config_dir().join("colors.css")).unwrap_or_default();
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&format!("{DEFAULT_COLORS}\n{colors}\n{}", include_str!("style.css")));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 10,
    );
}

fn monitors() -> Vec<gdk::Monitor> {
    let Some(display) = gdk::Display::default() else { return Vec::new() };
    let list = display.monitors();
    (0..list.n_items()).filter_map(|i| list.item(i).and_downcast::<gdk::Monitor>()).collect()
}

// grim -s takes ~250ms at 2560x1440, a full-size PPM ~25ms, so shrink and blur here. GTK's CSS
// blur also left the screen edges sharp.
const SHRINK: usize = 4;
const BLUR_RADIUS: usize = 3;

/// Saved to disk so a restarted surface can reuse them
fn take_snapshots() {
    let dir = snapshot_dir();
    let threads: Vec<_> = monitors()
        .iter()
        .filter_map(|m| m.connector())
        .map(|connector| {
            let path = dir.join(format!("{connector}.ppm"));
            std::thread::spawn(move || {
                let output = std::process::Command::new("grim")
                    .args(["-t", "ppm", "-o", connector.as_str(), "-"])
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()
                    .filter(|o| o.status.success())?;
                let (width, height, mut rgb) = ppm::shrink(&output.stdout, SHRINK)?;
                ppm::blur(width, height, &mut rgb, BLUR_RADIUS);
                let mut file = Vec::with_capacity(rgb.len() + 20);
                file.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
                file.extend_from_slice(&rgb);
                std::fs::write(path, file).ok()
            })
        })
        .collect();
    for thread in threads {
        let _ = thread.join();
    }
}

fn load_snapshot(path: &std::path::Path) -> Option<gdk::Texture> {
    let data = std::fs::read(path).ok()?;
    let (width, height, rgb) = ppm::shrink(&data, 1)?;
    let bytes = glib::Bytes::from_owned(rgb);
    let texture = gdk::MemoryTexture::new(width as i32, height as i32, gdk::MemoryFormat::R8g8b8, &bytes, width * 3);
    Some(texture.upcast())
}

fn username() -> String {
    unsafe {
        let passwd = libc::getpwuid(libc::getuid());
        if passwd.is_null() {
            return std::env::var("USER").unwrap_or_default();
        }
        std::ffi::CStr::from_ptr((*passwd).pw_name).to_string_lossy().into_owned()
    }
}

pub fn run(options: &Options, snapshot: bool) -> std::process::ExitCode {
    if let Err(e) = gtk::init() {
        eprintln!("wakelock: {e}");
        return std::process::ExitCode::from(2);
    }
    if !gtk4_session_lock::is_supported() {
        eprintln!("wakelock: the compositor does not support ext-session-lock-v1");
        return std::process::ExitCode::from(2);
    }
    load_style();
    if snapshot {
        take_snapshots();
    }

    let main_loop = glib::MainLoop::new(None, false);
    let lock = Rc::new(Lock {
        options: Options { pam_service: options.pam_service.clone(), pam_dir: options.pam_dir.clone() },
        user: username(),
        screens: RefCell::new(Vec::new()),
        checking: Cell::new(false),
        failed: Cell::new(false),
        message: RefCell::new(String::new()),
        syncing: Cell::new(false),
        instance: Instance::new(),
    });

    let weak = Rc::downgrade(&lock);
    lock.instance.connect_monitor(move |instance, monitor| {
        if let Some(lock) = weak.upgrade() {
            let window = lock.build_window(monitor);
            instance.assign_window_to_monitor(&window, monitor);
            window.present();
        }
    });
    lock.instance.connect_locked(|_| {
        let mut out = std::io::stdout();
        let _ = std::io::Write::write_all(&mut out, b"locked\n");
        let _ = std::io::Write::flush(&mut out);
    });
    lock.instance.connect_failed(|_| {
        eprintln!("wakelock: the compositor refused the lock");
        std::process::exit(2);
    });
    let quit = main_loop.clone();
    lock.instance.connect_unlocked(move |_| quit.quit());

    if !lock.instance.lock() {
        eprintln!("wakelock: cannot lock the session");
        return std::process::ExitCode::from(2);
    }

    let weak = Rc::downgrade(&lock);
    glib::timeout_add_local(Duration::from_secs(1), move || match weak.upgrade() {
        Some(lock) => {
            lock.update_clock();
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });

    main_loop.run();
    if let Some(display) = gdk::Display::default() {
        display.sync();
    }
    std::process::ExitCode::SUCCESS
}

impl Lock {
    fn build_window(self: &Rc<Self>, monitor: &gdk::Monitor) -> gtk::Window {
        let window = gtk::Window::new();
        window.add_css_class("wakelock");
        window.set_decorated(false);

        let overlay = gtk::Overlay::new();
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_can_shrink(true);
        picture.add_css_class("backdrop-image");
        if let Some(connector) = monitor.connector() {
            let path = snapshot_dir().join(format!("{connector}.ppm"));
            if let Some(texture) = load_snapshot(&path) {
                picture.set_paintable(Some(&texture));
            }
        }
        overlay.set_child(Some(&picture));
        let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
        dim.add_css_class("backdrop-dim");
        overlay.add_overlay(&dim);

        let column = gtk::Box::new(gtk::Orientation::Vertical, 12);
        column.set_halign(gtk::Align::Center);
        column.set_valign(gtk::Align::Center);
        let clock = gtk::Label::new(None);
        clock.add_css_class("clock");
        column.append(&clock);
        let date = gtk::Label::new(None);
        date.add_css_class("date");
        column.append(&date);
        let gap = gtk::Box::new(gtk::Orientation::Vertical, 0);
        gap.set_size_request(1, 36);
        column.append(&gap);

        let field = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        field.add_css_class("field");
        field.set_halign(gtk::Align::Center);
        let input = gtk::Text::new();
        input.set_hexpand(true);
        input.set_valign(gtk::Align::Center);
        input.set_visibility(false);
        input.set_invisible_char('•');
        input.set_placeholder_text(Some("Password"));
        field.append(&input);
        column.append(&field);

        let message = gtk::Label::new(None);
        message.add_css_class("message");
        message.set_wrap(true);
        message.set_justify(gtk::Justification::Center);
        message.set_max_width_chars(1);
        message.set_size_request(320, -1);
        message.set_halign(gtk::Align::Center);
        column.append(&message);
        overlay.add_overlay(&column);
        window.set_child(Some(&overlay));

        let weak = Rc::downgrade(self);
        input.connect_changed(move |input| {
            let Some(lock) = weak.upgrade() else { return };
            if lock.syncing.get() {
                return;
            }
            lock.syncing.set(true);
            let text = input.text();
            for screen in lock.screens.borrow().iter() {
                if &screen.input != input {
                    screen.input.set_text(&text);
                }
            }
            lock.syncing.set(false);
            if !text.is_empty() {
                lock.failed.set(false);
            }
            lock.update();
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(lock) = weak.upgrade() else { return glib::Propagation::Proceed };
            match key {
                gdk::Key::Return | gdk::Key::KP_Enter => lock.submit(),
                gdk::Key::Escape => lock.set_password(""),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        window.add_controller(keys);

        self.screens.borrow_mut().push(Screen { clock, date, field, input: input.clone(), message });
        let text = self.screens.borrow().first().map(|s| s.input.text()).unwrap_or_default();
        self.syncing.set(true);
        input.set_text(&text);
        self.syncing.set(false);
        self.update_clock();
        self.update();
        input.grab_focus();
        window
    }

    fn password(&self) -> String {
        self.screens.borrow().first().map(|s| s.input.text().to_string()).unwrap_or_default()
    }

    fn set_password(&self, text: &str) {
        self.syncing.set(true);
        for screen in self.screens.borrow().iter() {
            screen.input.set_text(text);
        }
        self.syncing.set(false);
        self.update();
    }

    fn update_clock(&self) {
        let Ok(now) = glib::DateTime::now_local() else { return };
        let time = now.format("%H:%M").map(|s| s.to_string()).unwrap_or_default();
        let date = now.format("%A, %B %-e").map(|s| s.to_string()).unwrap_or_default();
        for screen in self.screens.borrow().iter() {
            if screen.clock.text() != time {
                screen.clock.set_text(&time);
            }
            if screen.date.text() != date {
                screen.date.set_text(&date);
            }
        }
    }

    fn update(&self) {
        let filled = !self.password().is_empty();
        let failed = self.failed.get();
        let checking = self.checking.get();
        let message = if checking { "Checking...".to_owned() } else { self.message.borrow().clone() };
        for screen in self.screens.borrow().iter() {
            for (class, on) in [("filled", filled && !failed), ("failed", failed)] {
                if on {
                    screen.field.add_css_class(class);
                } else {
                    screen.field.remove_css_class(class);
                }
            }
            if failed {
                screen.message.add_css_class("failed");
            } else {
                screen.message.remove_css_class("failed");
            }
            screen.message.set_text(&message);
            screen.message.set_visible(!message.is_empty());
            screen.input.set_editable(!checking);
        }
    }

    fn submit(self: &Rc<Self>) {
        let password = self.password();
        if self.checking.get() || password.is_empty() {
            return;
        }
        self.checking.set(true);
        self.failed.set(false);
        self.message.replace(String::new());
        self.update();

        let (sender, receiver) = async_channel::bounded(1);
        let service = self.options.pam_service.clone();
        let dir = self.options.pam_dir.clone();
        let user = self.user.clone();
        std::thread::spawn(move || {
            let outcome = pam::authenticate(&service, dir.as_deref(), &user, password);
            let _ = sender.send_blocking(outcome);
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(outcome) = receiver.recv().await else { return };
            let Some(lock) = weak.upgrade() else { return };
            lock.checking.set(false);
            if outcome.success {
                lock.instance.unlock();
                return;
            }
            lock.failed.set(true);
            let message = match outcome.error {
                Some(error) => format!("Could not check the password ({error})"),
                None if !outcome.notice.is_empty() => outcome.notice,
                None => "Wrong password".to_owned(),
            };
            lock.message.replace(message);
            lock.set_password("");
        });
    }
}
