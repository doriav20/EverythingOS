use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use evos_gtk::backend::Backend;
use evos_gtk::window::{self, Ui};
use gtk::{gio, glib};

const APP_ID: &str = "org.evos.Search";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.add_main_option(
        "toggle",
        glib::Char::from(b't'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Show the window, or hide it if it is focused (bind this to a hotkey)",
        None,
    );
    let socket: PathBuf =
        std::env::var_os("EVOS_SOCKET").map_or(evos_proto::DEFAULT_SOCKET.into(), PathBuf::from);
    let ui: Rc<RefCell<Option<Rc<Ui>>>> = Rc::default();

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
    });

    app.connect_command_line(move |app, cmd| {
        let toggle = cmd.options_dict().contains("toggle");
        let mut slot = ui.borrow_mut();
        let ui = slot.get_or_insert_with(|| {
            let (backend, notices) = Backend::spawn(socket.clone());
            window::build(app, backend, notices)
        });
        if toggle && ui.window.is_visible() && ui.window.is_active() {
            ui.window.set_visible(false);
        } else {
            ui.window.present();
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}
