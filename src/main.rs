mod desktop;
mod logo;
mod ui;

use adw::prelude::*;
use gtk::{gio, glib};

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(desktop::APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.add_main_option(
        "debug",
        glib::Char::from(b'd'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Enable repeatable manual notification replay",
        None,
    );
    app.connect_activate(ui::activate);
    app.connect_command_line(|app, command| {
        app.activate();
        if command.options_dict().contains("debug") {
            app.activate_action("debug", Some(&true.to_variant()));
        }
        0.into()
    });
    app.run()
}
