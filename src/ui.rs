use crate::desktop::{self, Command, Event};
use adw::prelude::*;
use gnome_git_release_tray::model::{Config, NotificationRecord, STABLE_PATTERN, Watch};
use gtk::{gio, glib};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, sync::mpsc, time::Duration};
use tokio::sync::mpsc::UnboundedSender;

struct Editor {
    window: gtk::Window,
    error: gtk::Label,
    form: gtk::Box,
}
type EditorState = Rc<RefCell<Option<Editor>>>;

fn quit(app: &adw::Application, commands: &UnboundedSender<Command>) {
    if commands.send(Command::Quit).is_err() {
        app.quit();
    }
}

struct HistoryView {
    window: gtk::Window,
    id: u64,
    list: gtk::Box,
    clear: gtk::Button,
    error: gtk::Label,
}

#[derive(Default)]
struct HistoryState {
    records: BTreeMap<u64, Vec<NotificationRecord>>,
    view: Option<HistoryView>,
}

impl HistoryState {
    fn refresh(&self) {
        if let Some(view) = self.view.as_ref().filter(|v| v.window.is_visible()) {
            render_history(view, &self.records);
        }
    }
}

type Histories = Rc<RefCell<HistoryState>>;

fn initial_window_height() -> i32 {
    gtk::gdk::Display::default()
        .and_then(|display| display.monitors().item(0))
        .and_then(|monitor| monitor.downcast::<gtk::gdk::Monitor>().ok())
        .map(|monitor| (monitor.geometry().height() / 2).max(1))
        .unwrap_or(400)
}

pub fn activate(app: &adw::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    if let Some(display) = gtk::gdk::Display::default()
        && let Ok(icon) = crate::logo::cached_path()
        && let Some(directory) = icon.parent()
    {
        gtk::IconTheme::for_display(&display).add_search_path(directory);
    }
    gtk::Window::set_default_icon_name(desktop::APP_ID);
    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
    let (events, updates) = mpsc::channel();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Git Release Tray")
        .default_width(700)
        .default_height(initial_window_height())
        .resizable(true)
        .build();
    window.connect_realize(|window| {
        if let Some(surface) = window.surface()
            && let Some(monitor) =
                gtk::prelude::WidgetExt::display(window).monitor_at_surface(&surface)
        {
            window.set_default_size(700, (monitor.geometry().height() / 2).max(1));
        }
    });
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    let add = gtk::Button::with_label("Add watch");
    add.add_css_class("suggested-action");
    let check = gtk::Button::with_label("Check now");
    let quit_button = gtk::Button::from_icon_name("application-exit-symbolic");
    quit_button.set_tooltip_text(Some("Quit and stop monitoring"));
    content.append(&header);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    toolbar.set_margin_top(12);
    toolbar.set_margin_bottom(12);
    toolbar.set_margin_start(24);
    toolbar.set_margin_end(24);
    toolbar.append(&add);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    toolbar.append(&spacer);
    toolbar.append(&check);
    content.append(&toolbar);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let page = gtk::Box::new(gtk::Orientation::Vertical, 18);
    page.set_margin_top(20);
    page.set_margin_bottom(20);
    page.set_margin_start(24);
    page.set_margin_end(24);
    let title = gtk::Label::new(Some("GitHub release notifications"));
    title.add_css_class("title-1");
    title.set_xalign(0.0);
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let logo = gtk::Image::from_icon_name(desktop::APP_ID);
    logo.set_pixel_size(48);
    heading.append(&logo);
    heading.append(&title);
    page.append(&heading);
    let intro = gtk::Label::new(Some(
        "Watch release titles and tags using regular expressions. Closing this window keeps monitoring active.",
    ));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    intro.add_css_class("dim-label");
    page.append(&intro);
    let options = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    options.append(&gtk::Label::new(Some("Check every")));
    let interval = gtk::SpinButton::with_range(1.0, 24.0, 1.0);
    interval.set_value(5.0);
    options.append(&interval);
    options.append(&gtk::Label::new(Some("hours")));
    let settings = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings")
        .build();
    settings.update_property(&[gtk::accessible::Property::Label("Settings")]);
    let popover = gtk::Popover::new();
    let settings_content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    settings_content.set_margin_top(12);
    settings_content.set_margin_bottom(12);
    settings_content.set_margin_start(12);
    settings_content.set_margin_end(12);
    let settings_title = gtk::Label::new(Some("Settings"));
    settings_title.add_css_class("heading");
    settings_title.set_xalign(0.0);
    settings_content.append(&settings_title);
    settings_content.append(&options);
    let debug = gtk::CheckButton::with_label("Debug mode");
    debug.set_tooltip_text(Some(
        "Manual checks ignore notification history. Automatic checks still notify only once.",
    ));
    settings_content.append(&debug);
    let debug_help = gtk::Label::new(Some(
        "Check now replays the latest matching release, even if already notified. Automatic checks still notify only once.",
    ));
    debug_help.set_wrap(true);
    debug_help.set_max_width_chars(36);
    debug_help.set_xalign(0.0);
    debug_help.add_css_class("dim-label");
    settings_content.append(&debug_help);
    popover.set_child(Some(&settings_content));
    settings.set_popover(Some(&popover));
    toolbar.append(&settings);
    toolbar.append(&quit_button);
    {
        let settings = settings.clone();
        debug.connect_toggled(move |button| {
            if button.is_active() {
                settings.add_css_class("accent");
                settings.set_tooltip_text(Some("Settings — Debug mode on"));
            } else {
                settings.remove_css_class("accent");
                settings.set_tooltip_text(Some("Settings"));
            }
        });
    }
    let message = gtk::Label::new(Some("Starting…"));
    message.set_wrap(true);
    message.set_xalign(0.0);
    message.set_selectable(true);
    page.append(&message);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.append(&list);
    let scroller = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .overlay_scrolling(false)
        .propagate_natural_height(false)
        .vexpand(true)
        .child(&page)
        .build();
    content.append(&scroller);
    window.set_content(Some(&content));
    window.connect_close_request(|window| {
        window.set_visible(false);
        glib::Propagation::Stop
    });
    let hold = app.hold();
    let editor: EditorState = Rc::new(RefCell::new(None));
    {
        let window = window.clone();
        let commands = commands.clone();
        let editor = editor.clone();
        add.connect_clicked(move |_| open_editor(&window, None, &commands, &editor));
    }
    {
        let commands = commands.clone();
        let debug = debug.clone();
        check.connect_clicked(move |_| desktop::send(&commands, Command::Check(debug.is_active())));
    }
    let interval_changed = {
        let commands = commands.clone();
        interval.connect_value_changed(move |interval| {
            desktop::send(&commands, Command::Interval(interval.value_as_int() as u64));
        })
    };
    interval.connect_changed(|interval| {
        if let Ok(hours) = interval.text().parse::<u64>()
            && (1..=24).contains(&hours)
            && interval.value_as_int() as u64 != hours
        {
            interval.update();
        }
    });
    {
        let app = app.clone();
        let commands = commands.clone();
        quit_button.connect_clicked(move |_| {
            quit(&app, &commands);
        });
    }
    let action = gio::SimpleAction::new("debug", Some(glib::VariantTy::BOOLEAN));
    {
        let debug = debug.clone();
        action.connect_activate(move |_, value| {
            debug.set_active(value.and_then(|v| v.get::<bool>()).unwrap_or(false))
        });
    }
    app.add_action(&action);
    let quit_action = gio::SimpleAction::new("quit", None);
    {
        let app = app.clone();
        let commands = commands.clone();
        quit_action.connect_activate(move |_, _| quit(&app, &commands));
    }
    app.add_action(&quit_action);
    app.set_accels_for_action("app.quit", &["<Primary>q"]);
    let statuses: Rc<RefCell<BTreeMap<u64, gtk::Label>>> = Rc::new(RefCell::new(BTreeMap::new()));
    let histories: Histories = Rc::new(RefCell::new(HistoryState::default()));
    let app = app.clone();
    let window_for_events = window.clone();
    let commands_for_events = commands.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let _keep_running = &hold;
        for event in updates.try_iter() {
            match event {
                Event::Config(config) => {
                    // Loading or acknowledging settings must not trigger another save.
                    interval.block_signal(&interval_changed);
                    interval.set_value(config.interval_hours as f64);
                    interval.unblock_signal(&interval_changed);
                    render(
                        &list,
                        &config,
                        &commands_for_events,
                        &window_for_events,
                        &editor,
                        &statuses,
                        &histories,
                    );
                    message.set_text("Monitoring while running. Use Quit to stop.");
                }
                Event::Histories(updated) => {
                    let mut state = histories.borrow_mut();
                    state.records = updated;
                    state.refresh();
                }
                Event::Status(id, text) => {
                    if let Some(label) = statuses.borrow().get(&id) {
                        let now = glib::DateTime::now_local()
                            .ok()
                            .and_then(|d| d.format("%H:%M:%S").ok())
                            .map(|s| s.to_string())
                            .unwrap_or_default();
                        label.set_text(&format!("{now} · {text}"));
                    }
                }
                Event::Message(text) => {
                    message.set_text(&text);
                    check.set_sensitive(true);
                }
                Event::Busy(busy) => {
                    check.set_sensitive(!busy);
                    check.set_label(if busy { "Checking…" } else { "Check now" });
                }
                Event::Open => window_for_events.present(),
                Event::Check => {
                    desktop::send(&commands_for_events, Command::Check(debug.is_active()))
                }
                Event::Quit => {
                    quit(&app, &commands_for_events);
                }
                Event::OpenRelease(url) => {
                    if url::Url::parse(&url)
                        .is_ok_and(|u| u.scheme() == "https" && u.host_str() == Some("github.com"))
                    {
                        let message = message.clone();
                        gio::AppInfo::launch_default_for_uri_async(
                            &url,
                            None::<&gio::AppLaunchContext>,
                            None::<&gio::Cancellable>,
                            move |result| {
                                if let Err(error) = result {
                                    message.set_text(&format!("Cannot open release: {error}"));
                                }
                            },
                        );
                    }
                }
                Event::Stopped => app.quit(),
                Event::SaveFailed(text) => {
                    message.set_text(&text);
                    if let Some(editor) = editor.borrow().as_ref() {
                        editor.error.set_text(&text);
                        editor.form.set_sensitive(true);
                        editor.window.set_deletable(true);
                    }
                }
                Event::ClearFailed(text) => {
                    message.set_text(&text);
                    if let Some(view) = histories.borrow().view.as_ref() {
                        view.error.set_text(&text);
                    }
                }
                Event::Saved => {
                    if let Some(editor) = editor.borrow_mut().take() {
                        editor.window.set_deletable(true);
                        editor.window.close();
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });
    desktop::start(receiver, events);
    window.present();
}

fn render(
    list: &gtk::Box,
    config: &Config,
    commands: &UnboundedSender<Command>,
    window: &adw::ApplicationWindow,
    editor: &EditorState,
    statuses: &Rc<RefCell<BTreeMap<u64, gtk::Label>>>,
    histories: &Histories,
) {
    let previous_statuses: BTreeMap<u64, String> = statuses
        .borrow()
        .iter()
        .map(|(id, label)| (*id, label.text().to_string()))
        .collect();
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    statuses.borrow_mut().clear();
    if config.watches.is_empty() {
        let empty = adw::StatusPage::builder()
            .title("No repositories yet")
            .description("Add a GitHub releases URL to start watching.")
            .icon_name(desktop::APP_ID)
            .build();
        list.append(&empty);
        return;
    }
    for watch in &config.watches {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 8);
        row.add_css_class("card");
        row.set_margin_top(4);
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 8);
        inner.set_margin_top(14);
        inner.set_margin_bottom(14);
        inner.set_margin_start(14);
        inner.set_margin_end(14);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let enabled = gtk::CheckButton::new();
        enabled.set_active(watch.enabled);
        enabled.set_tooltip_text(Some("Enable this watch"));
        top.append(&enabled);
        let name = gtk::Label::new(Some(&watch.repository));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.add_css_class("heading");
        top.append(&name);
        let history_button = gtk::Button::with_label("History");
        top.append(&history_button);
        {
            let parent = window.clone();
            let watch = watch.clone();
            let histories = histories.clone();
            let commands = commands.clone();
            history_button
                .connect_clicked(move |_| open_history(&parent, &watch, &histories, &commands));
        }
        let edit = gtk::Button::with_label("Edit");
        top.append(&edit);
        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.set_tooltip_text(Some("Delete watch"));
        top.append(&remove);
        inner.append(&top);
        let pattern = gtk::Label::new(Some(&format!(
            "Regex: {}",
            if watch.pattern.is_empty() {
                "(all releases)"
            } else {
                &watch.pattern
            }
        )));
        pattern.set_xalign(0.0);
        pattern.set_wrap(true);
        pattern.set_selectable(true);
        inner.append(&pattern);
        let status = gtk::Label::new(Some(if watch.enabled {
            previous_statuses
                .get(&watch.id)
                .map(String::as_str)
                .filter(|text| *text != "Paused")
                .unwrap_or("Waiting for check…")
        } else {
            "Paused"
        }));
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.add_css_class("dim-label");
        inner.append(&status);
        statuses.borrow_mut().insert(watch.id, status);
        row.append(&inner);
        list.append(&row);
        let id = watch.id;
        let tx = commands.clone();
        enabled.connect_toggled(move |button| {
            desktop::send(&tx, Command::Enable(id, button.is_active()))
        });
        let tx = commands.clone();
        remove.connect_clicked(move |_| desktop::send(&tx, Command::Remove(id)));
        let tx = commands.clone();
        let parent = window.clone();
        let watch = watch.clone();
        let editor = editor.clone();
        edit.connect_clicked(move |_| open_editor(&parent, Some(watch.clone()), &tx, &editor));
    }
}

fn hint(text: &str) -> gtk::Label {
    let hint = gtk::Label::new(Some(text));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    hint.add_css_class("dim-label");
    hint
}

fn open_editor(
    parent: &adw::ApplicationWindow,
    watch: Option<Watch>,
    commands: &UnboundedSender<Command>,
    editor: &EditorState,
) {
    if let Some(existing) = editor.borrow().as_ref().filter(|e| e.window.is_visible()) {
        existing.window.present();
        return;
    }
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(if watch.is_some() {
            "Edit watch"
        } else {
            "Add watch"
        })
        .default_width(520)
        .default_height(initial_window_height())
        .resizable(true)
        .build();
    let form = gtk::Box::new(gtk::Orientation::Vertical, 12);
    form.set_margin_top(24);
    form.set_margin_bottom(24);
    form.set_margin_start(24);
    form.set_margin_end(24);
    let url = gtk::Entry::builder()
        .placeholder_text("https://github.com/owner/repository/releases")
        .build();
    let regex = gtk::Entry::builder()
        .placeholder_text("e.g. ^v[0-9]+\\.[0-9]+\\.[0-9]+$")
        .build();
    let stable = gtk::CheckButton::with_label("Stable releases only");
    let enabled = gtk::CheckButton::with_label("Enabled");
    enabled.set_active(true);
    let latest = gtk::CheckButton::with_label("Notify latest not future");
    if let Some(w) = &watch {
        url.set_text(&format!("https://github.com/{}/releases", w.repository));
        regex.set_text(&w.pattern);
        stable.set_active(w.pattern == STABLE_PATTERN);
        enabled.set_active(w.enabled);
        latest.set_active(w.notify_latest);
    }
    regex.set_sensitive(!stable.is_active());
    {
        // Unchecking restores the custom regex, or empty (all releases).
        let custom = RefCell::new(String::new());
        let regex = regex.clone();
        stable.connect_toggled(move |stable| {
            if stable.is_active() {
                *custom.borrow_mut() = regex.text().to_string();
                regex.set_text(STABLE_PATTERN);
            } else {
                regex.set_text(&custom.borrow());
            }
            regex.set_sensitive(!stable.is_active());
        });
    }
    for (label, entry) in [
        ("GitHub repository or releases URL", &url),
        ("Regex matching release title or tag", &regex),
    ] {
        let label = gtk::Label::new(Some(label));
        label.set_xalign(0.0);
        form.append(&label);
        form.append(entry);
    }
    form.append(&hint(
        "Empty matches all releases, including prereleases. Lookaround is supported. Matching is case sensitive; use (?i) to ignore case.",
    ));
    form.append(&stable);
    form.append(&hint(
        "Skips releases GitHub marks as prereleases, and releases whose tag or title mentions beta, nightly, preview or RC. Fills in and locks the regex above.",
    ));
    form.append(&enabled);
    form.append(&latest);
    form.append(&hint(
        "When enabled, notify once for the newest matching existing release, then continue watching future releases. Otherwise, the first check silently records existing releases.",
    ));
    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    form.append(&error);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&save);
    form.append(&buttons);
    let close = dialog.clone();
    cancel.connect_clicked(move |_| close.close());
    *editor.borrow_mut() = Some(Editor {
        window: dialog.clone(),
        error: error.clone(),
        form: form.clone(),
    });
    let tx = commands.clone();
    let saving_window = dialog.clone();
    let saving_form = form.clone();
    save.connect_clicked(move |_| {
        let result = gnome_git_release_tray::model::repository(url.text().as_str())
            .and_then(|_| fancy_regex::Regex::new(regex.text().as_str()).map_err(Into::into));
        if let Err(e) = result {
            error.set_text(&e.to_string());
            return;
        }
        error.set_text("Saving…");
        let result = tx.send(Command::Save(
                watch.as_ref().map(|w| w.id),
                url.text().to_string(),
                regex.text().to_string(),
                enabled.is_active(),
                latest.is_active(),
            ));
        if result.is_err() {
            error.set_text("Monitoring could not start. Close this editor and check the startup error in the main window.");
        } else {
            saving_form.set_sensitive(false);
            saving_window.set_deletable(false);
        }
    });
    let scroll = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .overlay_scrolling(false)
        .propagate_natural_height(false)
        .child(&form)
        .build();
    dialog.set_child(Some(&scroll));
    dialog.present();
}

fn open_history(
    parent: &adw::ApplicationWindow,
    watch: &Watch,
    histories: &Histories,
    commands: &UnboundedSender<Command>,
) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(format!("Notification history — {}", watch.repository))
        .default_width(600)
        .default_height(initial_window_height())
        .resizable(true)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    let description = gtk::Label::new(Some(
        "Delivered notifications, newest first. Debug replays are marked. History starts with notifications sent after this feature was added.",
    ));
    description.set_wrap(true);
    description.set_xalign(0.0);
    content.append(&description);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let scroll = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .overlay_scrolling(false)
        .propagate_natural_height(false)
        .vexpand(true)
        .child(&list)
        .build();
    content.append(&scroll);
    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    content.append(&error);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let clear = gtk::Button::with_label("Clear history");
    clear.add_css_class("destructive-action");
    clear.set_tooltip_text(Some(
        "Delete this list. Releases already notified are still not notified again.",
    ));
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let refresh = gtk::Button::with_label("Refresh");
    let close = gtk::Button::with_label("Close");
    buttons.append(&clear);
    buttons.append(&spacer);
    buttons.append(&refresh);
    buttons.append(&close);
    content.append(&buttons);
    let view = HistoryView {
        window: window.clone(),
        id: watch.id,
        list,
        clear: clear.clone(),
        error: error.clone(),
    };
    render_history(&view, &histories.borrow().records);
    histories.borrow_mut().view = Some(view);
    {
        let histories = histories.clone();
        refresh.connect_clicked(move |_| histories.borrow().refresh());
    }
    let id = watch.id;
    let tx = commands.clone();
    clear.connect_clicked(move |button| {
        if tx.send(Command::ClearHistory(id)).is_err() {
            error.set_text("Monitoring could not start. Close this window and check the startup error in the main window.");
        } else {
            // The button is re-enabled when refreshed history still has records.
            error.set_text("");
            button.set_sensitive(false);
        }
    });
    let dialog = window.clone();
    close.connect_clicked(move |_| dialog.close());
    window.set_child(Some(&content));
    window.present();
}

fn render_history(view: &HistoryView, records: &BTreeMap<u64, Vec<NotificationRecord>>) {
    let list = &view.list;
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let records = records.get(&view.id).map(Vec::as_slice).unwrap_or_default();
    view.clear.set_sensitive(!records.is_empty());
    if records.is_empty() {
        list.append(&adw::StatusPage::builder().title("No notifications yet")
            .description("Successful deliveries will appear here. Failed deliveries and the initial release baseline are not notifications.")
            .icon_name("document-open-recent-symbolic").build());
        return;
    }
    for record in records.iter().rev() {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let title = record
            .release
            .name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&record.release.tag_name);
        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_selectable(true);
        label.add_css_class("heading");
        row.append(&label);
        let timestamp = i64::try_from(record.sent_at)
            .ok()
            .and_then(|seconds| glib::DateTime::from_unix_local(seconds).ok())
            .and_then(|time| time.format("%Y-%m-%d %H:%M:%S %Z").ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Unknown time".into());
        let details = gtk::Label::new(Some(&format!(
            "{} · {}{}",
            timestamp,
            record.release.tag_name,
            if record.debug { " · Debug replay" } else { "" }
        )));
        details.set_xalign(0.0);
        details.set_wrap(true);
        details.set_selectable(true);
        row.append(&details);
        if url::Url::parse(&record.release.html_url)
            .is_ok_and(|url| url.scheme() == "https" && url.host_str() == Some("github.com"))
        {
            let link = gtk::LinkButton::with_label(&record.release.html_url, "Open release");
            link.set_halign(gtk::Align::Start);
            row.append(&link);
        }
        list.append(&row);
        list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    }
}
