use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use evos_gtk::backend::Backend;
use evos_gtk::cmdline;
use evos_gtk::settings::Settings;
use evos_gtk::window::{self, Ui};
use gtk::{gio, glib};

const APP_ID: &str = "org.evos.Search";

type Windows = Rc<RefCell<Vec<Rc<Ui>>>>;
type OpenWindow = Rc<dyn Fn(&adw::Application) -> Rc<Ui>>;

/// The focused search window, or the most recent one.
fn current(app: &adw::Application, windows: &Windows) -> Option<Rc<Ui>> {
    let windows = windows.borrow();
    let active = app.active_window();
    windows
        .iter()
        .find(|ui| active.as_ref() == Some(ui.window.upcast_ref()))
        .or(windows.last())
        .cloned()
}

fn main() -> glib::ExitCode {
    // Check the arguments here first, so -help and mistakes reach the
    // terminal that ran us even when a running instance handles them.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cmdline::parse(&args) {
        Ok(l) if l.help => {
            print!("{}", cmdline::USAGE);
            return glib::ExitCode::SUCCESS;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("evos-gtk: {e}");
            return glib::ExitCode::from(2);
        }
    }

    // No GOption entries: the command line goes to the primary instance
    // as it is and is parsed there.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let socket: PathBuf =
        std::env::var_os("EVOS_SOCKET").map_or(evos_proto::DEFAULT_SOCKET.into(), PathBuf::from);
    let windows: Windows = Rc::default();
    let settings = Rc::new(RefCell::new(Settings::load(&Settings::default_file())));

    let open_window: OpenWindow = {
        let windows = windows.clone();
        Rc::new(move |app| {
            let (backend, notices) = Backend::spawn(socket.clone());
            let ui = window::build(app, backend, notices, settings.clone());
            windows.borrow_mut().push(ui.clone());
            let list = Rc::downgrade(&windows);
            ui.window.connect_destroy(move |w| {
                if let Some(list) = list.upgrade() {
                    list.borrow_mut().retain(|ui| &ui.window != w);
                }
            });
            ui
        })
    };

    let (open, list) = (open_window.clone(), windows.clone());
    app.connect_startup(move |app| {
        let about = gio::SimpleAction::new("about", None);
        let a = app.clone();
        about.connect_activate(move |_, _| {
            let dialog = adw::AboutDialog::builder()
                .application_name("EverythingOS")
                .application_icon(APP_ID)
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Instant file search for Linux, including network shares")
                .license_type(gtk::License::MitX11)
                .build();
            dialog.present(a.active_window().as_ref());
        });
        app.add_action(&about);

        let new_window = gio::SimpleAction::new("new-window", None);
        let (a, open) = (app.clone(), open.clone());
        new_window.connect_activate(move |_, _| open(&a).window.present());
        app.add_action(&new_window);

        let preferences = gio::SimpleAction::new("preferences", None);
        let (a, windows) = (app.clone(), list.clone());
        preferences.connect_activate(move |_, _| {
            if let Some(ui) = current(&a, &windows) {
                evos_gtk::prefs::show(&ui.window, ui.backend());
            }
        });
        app.add_action(&preferences);

        let quit = gio::SimpleAction::new("quit", None);
        let a = app.clone();
        quit.connect_activate(move |_, _| a.quit());
        app.add_action(&quit);
    });

    app.connect_command_line(move |app, cmd| {
        let args: Vec<String> = cmd
            .arguments()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let Ok(launch) = cmdline::parse(&args) else {
            return glib::ExitCode::from(2);
        };
        if launch.exit {
            app.quit();
            return glib::ExitCode::SUCCESS;
        }
        let ui = match current(app, &windows) {
            Some(ui) if !launch.new_window => ui,
            _ => open_window(app),
        };
        if launch.toggle && ui.window.is_visible() && ui.window.is_active() {
            ui.window.set_visible(false);
            return glib::ExitCode::SUCCESS;
        }
        ui.window.present();
        ui.apply(&launch);
        glib::ExitCode::SUCCESS
    });
    app.run()
}
