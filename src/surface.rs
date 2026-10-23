// The lock surface. Prints "locked" once the compositor confirms the lock.
use std::time::Duration;

use gtk::{gdk, glib, prelude::*};
use gtk4_session_lock::Instance;
use relm4::{RelmWidgetExt, prelude::*};

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
    let instance = Instance::new();
    instance.connect_locked(|_| {
        let mut out = std::io::stdout();
        let _ = std::io::Write::write_all(&mut out, b"locked\n");
        let _ = std::io::Write::flush(&mut out);
    });
    instance.connect_failed(|_| {
        eprintln!("wakelock: the compositor refused the lock");
        std::process::exit(2);
    });
    let quit = main_loop.clone();
    instance.connect_unlocked(move |_| quit.quit());
    let options = Options { pam_service: options.pam_service.clone(), pam_dir: options.pam_dir.clone() };
    let _lock = Lock::builder().launch((options, instance.clone())).detach();

    if !instance.lock() {
        eprintln!("wakelock: cannot lock the session");
        return std::process::ExitCode::from(2);
    }
    main_loop.run();
    if let Some(display) = gdk::Display::default() {
        display.sync();
    }
    std::process::ExitCode::SUCCESS
}

#[derive(Debug, Clone, Default, PartialEq)]
struct View {
    time: String,
    date: String,
    filled: bool,
    failed: bool,
    checking: bool,
    message: String,
}

struct Lock {
    options: Options,
    user: String,
    instance: Instance,
    /// One buffer for every screen's field, so they stay in sync
    password: gtk::EntryBuffer,
    screens: Vec<Controller<Screen>>,
    failed: bool,
    checking: bool,
    message: String,
    view: View,
}

#[derive(Debug)]
enum LockMsg {
    Monitor(gdk::Monitor),
    Tick,
    Changed,
    Submit,
    Clear,
    Checked(pam::Outcome),
}

impl Component for Lock {
    type Init = (Options, Instance);
    type Input = LockMsg;
    type Output = ();
    type CommandOutput = ();
    type Root = ();
    type Widgets = ();

    fn init_root() {}

    fn init((options, instance): Self::Init, _: (), sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let password = gtk::EntryBuffer::default();
        let changed = sender.clone();
        password.connect_text_notify(move |_| changed.input(LockMsg::Changed));
        let monitors = sender.clone();
        instance.connect_monitor(move |_, monitor| monitors.input(LockMsg::Monitor(monitor.clone())));
        glib::timeout_add_local(Duration::from_secs(1), move || {
            sender.input(LockMsg::Tick);
            glib::ControlFlow::Continue
        });
        let mut lock = Lock {
            options,
            user: username(),
            instance,
            password,
            screens: Vec::new(),
            failed: false,
            checking: false,
            message: String::new(),
            view: View::default(),
        };
        lock.refresh();
        ComponentParts { model: lock, widgets: () }
    }

    fn update(&mut self, msg: LockMsg, sender: ComponentSender<Self>, _: &()) {
        match msg {
            LockMsg::Monitor(monitor) => {
                let screen = Screen::builder()
                    .launch((monitor.clone(), self.password.clone(), self.view.clone()))
                    .forward(sender.input_sender(), |msg| msg);
                self.instance.assign_window_to_monitor(screen.widget(), &monitor);
                screen.widget().present();
                self.screens.push(screen);
            }
            LockMsg::Tick => {}
            LockMsg::Changed => {
                if !self.password.text().is_empty() {
                    self.failed = false;
                }
            }
            LockMsg::Submit => self.submit(&sender),
            LockMsg::Clear => self.password.set_text(""),
            LockMsg::Checked(outcome) => {
                self.checking = false;
                if outcome.success {
                    self.instance.unlock();
                    return;
                }
                self.failed = true;
                self.message = match outcome.error {
                    Some(error) => format!("Could not check the password ({error})"),
                    None if !outcome.notice.is_empty() => outcome.notice,
                    None => "Wrong password".to_owned(),
                };
                self.password.set_text("");
            }
        }
        self.refresh();
    }
}

impl Lock {
    fn submit(&mut self, sender: &ComponentSender<Self>) {
        let password = self.password.text().to_string();
        if self.checking || password.is_empty() {
            return;
        }
        self.checking = true;
        self.failed = false;
        self.message.clear();
        let (service, dir, user) = (self.options.pam_service.clone(), self.options.pam_dir.clone(), self.user.clone());
        let (outcome, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = outcome.send_blocking(pam::authenticate(&service, dir.as_deref(), &user, password));
        });
        let sender = sender.clone();
        glib::spawn_future_local(async move {
            if let Ok(outcome) = receiver.recv().await {
                sender.input(LockMsg::Checked(outcome));
            }
        });
    }

    fn refresh(&mut self) {
        let now = glib::DateTime::now_local().ok();
        let format = |f: &str| now.as_ref().and_then(|n| n.format(f).ok()).map(|s| s.to_string()).unwrap_or_default();
        let view = View {
            time: format("%H:%M"),
            date: format("%A, %B %-e"),
            filled: !self.password.text().is_empty(),
            failed: self.failed,
            checking: self.checking,
            message: if self.checking { "Checking...".to_owned() } else { self.message.clone() },
        };
        if view != self.view {
            self.view = view;
            for screen in &self.screens {
                screen.emit(self.view.clone());
            }
        }
    }
}

struct Screen {
    password: gtk::EntryBuffer,
    view: View,
}

#[relm4::component]
impl SimpleComponent for Screen {
    type Init = (gdk::Monitor, gtk::EntryBuffer, View);
    type Input = View;
    type Output = LockMsg;

    view! {
        gtk::Window {
            add_css_class: "wakelock",
            set_decorated: false,
            add_controller = gtk::EventControllerKey {
                set_propagation_phase: gtk::PropagationPhase::Capture,
                connect_key_pressed[sender] => move |_, key, _, _| {
                    let msg = match key {
                        gdk::Key::Return | gdk::Key::KP_Enter => LockMsg::Submit,
                        gdk::Key::Escape => LockMsg::Clear,
                        _ => return glib::Propagation::Proceed,
                    };
                    let _ = sender.output(msg);
                    glib::Propagation::Stop
                },
            },

            gtk::Overlay {
                gtk::Picture {
                    set_content_fit: gtk::ContentFit::Cover,
                    set_can_shrink: true,
                    add_css_class: "backdrop-image",
                    set_paintable: snapshot.as_ref(),
                },
                add_overlay = &gtk::Box {
                    add_css_class: "backdrop-dim",
                },
                add_overlay = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 12,
                    set_halign: gtk::Align::Center,
                    set_valign: gtk::Align::Center,

                    gtk::Label {
                        add_css_class: "clock",
                        #[watch]
                        set_label: &model.view.time,
                    },
                    gtk::Label {
                        add_css_class: "date",
                        #[watch]
                        set_label: &model.view.date,
                    },
                    gtk::Box {
                        set_size_request: (1, 36),
                    },
                    gtk::Box {
                        add_css_class: "field",
                        set_halign: gtk::Align::Center,
                        #[watch]
                        set_class_active: ("filled", model.view.filled && !model.view.failed),
                        #[watch]
                        set_class_active: ("failed", model.view.failed),

                        #[name = "input"]
                        gtk::Text {
                            set_buffer: &model.password,
                            set_hexpand: true,
                            set_valign: gtk::Align::Center,
                            set_visibility: false,
                            set_invisible_char: '•',
                            set_placeholder_text: Some("Password"),
                            #[watch]
                            set_editable: !model.view.checking,
                        },
                    },
                    gtk::Label {
                        add_css_class: "message",
                        set_wrap: true,
                        set_justify: gtk::Justification::Center,
                        set_max_width_chars: 1,
                        set_size_request: (320, -1),
                        set_halign: gtk::Align::Center,
                        #[watch]
                        set_class_active: ("failed", model.view.failed),
                        #[watch]
                        set_label: &model.view.message,
                        #[watch]
                        set_visible: !model.view.message.is_empty(),
                    },
                },
            },
        }
    }

    fn init(
        (monitor, password, view): Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let snapshot =
            monitor.connector().and_then(|c| load_snapshot(&snapshot_dir().join(format!("{c}.ppm"))));
        let model = Screen { password, view };
        let widgets = view_output!();
        widgets.input.grab_focus();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, view: View, _sender: ComponentSender<Self>) {
        self.view = view;
    }
}

