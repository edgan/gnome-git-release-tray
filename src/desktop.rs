use anyhow::{Context, Result, bail};
use gnome_git_release_tray::{
    engine::{self, Notifier},
    github::GitHub,
    model::{Config, NotificationRecord, Release, Watch},
    storage::Store,
};
use ksni::TrayMethods;
use std::{sync::mpsc::Sender, time::Duration};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub const APP_ID: &str = "io.github.gnome_git_release_tray";

#[derive(Debug)]
pub enum Command {
    Check(bool),
    CheckSelected(bool, Vec<u64>),
    Save(Option<u64>, String, String, bool, bool),
    Remove(u64),
    Enable(u64, bool),
    Interval(u64),
    ClearHistory(u64),
    Quit,
}

#[derive(Debug)]
pub enum Event {
    Config(Config),
    Histories(std::collections::BTreeMap<u64, Vec<NotificationRecord>>),
    Status(u64, String),
    Message(String),
    Busy(bool),
    Open,
    Check,
    Quit,
    OpenRelease(String),
    Saved,
    SaveFailed(String),
    ClearFailed(String),
    Stopped,
}

struct Tray {
    events: Sender<Event>,
}
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        APP_ID.into()
    }
    fn title(&self) -> String {
        "Git Release Tray".into()
    }
    fn icon_name(&self) -> String {
        APP_ID.into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        crate::logo::tray_pixmap()
    }
    fn icon_theme_path(&self) -> String {
        crate::logo::cached_path()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_string_lossy().into_owned()))
            .unwrap_or_default()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.events.send(Event::Open);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Open".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.events.send(Event::Open);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Check now".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.events.send(Event::Check);
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.events.send(Event::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

struct DesktopNotifier {
    events: Sender<Event>,
}
impl Notifier for DesktopNotifier {
    async fn send(&self, watch: &Watch, release: &Release, debug: bool) -> Result<()> {
        let title = release
            .name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&release.tag_name);
        let escape = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        let icon = crate::logo::cached_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| APP_ID.into());
        let handle = notify_rust::Notification::new()
            .appname("Git Release Tray")
            .summary(&format!(
                "{}{}",
                if debug { "[Debug] " } else { "" },
                watch.repository
            ))
            .body(&escape(title))
            .icon(&icon)
            .hint(notify_rust::Hint::DesktopEntry(APP_ID.into()))
            .action("default", "Open release")
            .action("open", "Open release")
            .timeout(10000)
            .show_async()
            .await
            .context("Cannot send desktop notification; check the session notification service")?;
        let events = self.events.clone();
        let url = release.html_url.clone();
        tokio::spawn(async move {
            handle
                .wait_for_action_async(|response| {
                    if matches!(response, notify_rust::NotificationResponse::Default)
                        || matches!(response, notify_rust::NotificationResponse::Action(action) if action == "open") {
                        let _ = events.send(Event::OpenRelease(url));
                    }
                })
                .await;
        });
        Ok(())
    }
}

pub fn start(commands: UnboundedReceiver<Command>, events: Sender<Event>) {
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(run(commands, events.clone()))
        })();
        if let Err(error) = result {
            let _ = events.send(Event::Message(format!("Startup failed: {error:#}")));
        }
    });
}

async fn run(mut commands: UnboundedReceiver<Command>, events: Sender<Event>) -> Result<()> {
    let mut store = Store::open()?;
    let mut github = GitHub::new()?;
    let notifier = DesktopNotifier {
        events: events.clone(),
    };
    let tray_events = events.clone();
    let tray_task = tokio::spawn(async move {
        match (Tray {
            events: tray_events.clone(),
        })
        .spawn()
        .await
        {
            Ok(handle) => {
                let _handle = handle;
                std::future::pending::<()>().await;
            }
            Err(error) => {
                let _=tray_events.send(Event::Message(format!("Tray unavailable: {error}. Enable the GNOME AppIndicator extension; the window still works.")));
            }
        }
    });
    let _ = events.send(Event::Config(store.config.clone()));
    publish_histories(&store, &events);
    let mut next = tokio::time::Instant::now();
    let mut queued = std::collections::VecDeque::new();
    loop {
        let command = if let Some(command) = queued.pop_front() {
            command
        } else {
            tokio::select! {
                command=commands.recv()=>match command {Some(c)=>c,None=>break},
                _=tokio::time::sleep_until(next)=>Command::Check(false),
            }
        };
        if let Command::Quit = command {
            break;
        }
        let check = match &command {
            Command::Check(debug) => Some((*debug, None)),
            Command::CheckSelected(debug, ids) => Some((*debug, Some(ids.clone()))),
            _ => None,
        };
        if let Some((debug, selected)) = check {
            if selected.is_none() {
                next = tokio::time::Instant::now()
                    + Duration::from_secs(store.config.interval_hours * 3600);
            }
            let mut remaining: Vec<u64> = store
                .config
                .watches
                .iter()
                .filter(|w| w.enabled && selected.as_ref().is_none_or(|ids| ids.contains(&w.id)))
                .map(|w| w.id)
                .collect();
            let _ = events.send(Event::Busy(true));
            // Resume unfinished monitors after a settings edit without repeating
            // monitors that already completed this batch.
            let incoming = tokio::select! {
                _=engine::check_selected(&mut store,&mut github,&notifier,debug,selected.as_deref(),|id,message| {
                    if message != "Checking…" { remaining.retain(|pending| *pending != id); }
                    let _=events.send(Event::Status(id,message));
                })=>None,
                command=commands.recv()=>command,
            };
            if let Some(command) = incoming {
                if !remaining.is_empty() {
                    queued.push_front(Command::CheckSelected(debug, remaining));
                }
                queued.push_front(command);
            }
            publish_histories(&store, &events);
            let _ = events.send(Event::Busy(false));
            continue;
        }
        if let Command::ClearHistory(id) = command {
            if let Err(error) = clear_history(&mut store, id) {
                let _ = events.send(Event::ClearFailed(format!(
                    "Could not clear history: {error:#}"
                )));
            }
            publish_histories(&store, &events);
            continue;
        }
        let old_config = store.config.clone();
        let is_save = matches!(command, Command::Save(..));
        let interval_changed = matches!(command, Command::Interval(_));
        let result = change(&mut store, command).and_then(|target| {
            store.save_config()?;
            Ok(target)
        });
        match result {
            Ok(target) => {
                let _ = events.send(Event::Config(store.config.clone()));
                if is_save {
                    let _ = events.send(Event::Saved);
                }
                if interval_changed {
                    next = tokio::time::Instant::now()
                        + Duration::from_secs(store.config.interval_hours * 3600);
                }
                if let Some(id) = target {
                    for command in &mut queued {
                        if let Command::CheckSelected(_, ids) = command {
                            ids.retain(|pending| *pending != id);
                        }
                    }
                    queued.push_front(Command::CheckSelected(false, vec![id]));
                }
            }
            Err(error) => {
                store.config = old_config;
                let _ = events.send(Event::Config(store.config.clone()));
                let _ = events.send(Event::SaveFailed(format!(
                    "Could not save settings: {error:#}"
                )));
            }
        }
    }
    tray_task.abort();
    let _ = events.send(Event::Stopped);
    Ok(())
}

fn publish_histories(store: &Store, events: &Sender<Event>) {
    let histories = store
        .histories
        .iter()
        .map(|(id, history)| (*id, history.notifications.clone()))
        .collect();
    let _ = events.send(Event::Histories(histories));
}

/// Clears only the delivered-notification log; deduplication state is kept so
/// already-notified releases are not delivered again.
fn clear_history(store: &mut Store, id: u64) -> Result<()> {
    let Some(history) = store.histories.get_mut(&id) else {
        return Ok(());
    };
    let cleared = std::mem::take(&mut history.notifications);
    if cleared.is_empty() {
        return Ok(());
    }
    if let Err(error) = store.save_history() {
        if let Some(history) = store.histories.get_mut(&id) {
            history.notifications = cleared;
        }
        return Err(error);
    }
    Ok(())
}

fn change(store: &mut Store, command: Command) -> Result<Option<u64>> {
    let mut target = None;
    match command {
        Command::Save(id, url, pattern, enabled, notify_latest) => {
            let repository = gnome_git_release_tray::model::repository(&url)?;
            fancy_regex::Regex::new(&pattern)?;
            let old = id
                .and_then(|id| store.config.watches.iter().find(|w| w.id == id))
                .cloned();
            if id.is_some() && old.is_none() {
                bail!("This watch was removed; close the editor and add it again");
            }
            let reset = old.as_ref().is_none_or(|w| w.repository != repository);
            let new_id = if reset {
                let id = store.config.next_id;
                store.config.next_id += 1;
                id
            } else {
                id.unwrap()
            };
            if let Some(old) = &old {
                store.config.watches.retain(|w| w.id != old.id);
            }
            // Persist the request with the settings, then acknowledge it in history
            // only after discovery. No cross-file settings transaction is needed.
            let previous_request = old
                .as_ref()
                .filter(|_| !reset)
                .map_or(0, |w| w.latest_request);
            let request_latest =
                notify_latest && (reset || old.as_ref().is_some_and(|w| !w.notify_latest));
            let latest_request = previous_request + u64::from(request_latest);
            store.config.watches.push(Watch {
                id: new_id,
                repository,
                pattern,
                enabled,
                notify_latest,
                latest_request,
            });
            store.config.watches.sort_by_key(|w| w.id);
            target = enabled.then_some(new_id);
        }
        Command::Remove(id) => store.config.watches.retain(|w| w.id != id),
        Command::Enable(id, enabled) => {
            store
                .config
                .watches
                .iter_mut()
                .find(|w| w.id == id)
                .context("Watch no longer exists")?
                .enabled = enabled;
            target = enabled.then_some(id);
        }
        Command::Interval(hours) => {
            if !(1..=24).contains(&hours) {
                bail!("Choose an interval from 1 to 24 hours");
            }
            store.config.interval_hours = hours;
        }
        _ => {}
    }
    Ok(target)
}

pub fn send(sender: &UnboundedSender<Command>, command: Command) {
    let _ = sender.send(command);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_select_only_the_affected_monitor() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::at(
            dir.path().join("config.json"),
            dir.path().join("history.json"),
        )
        .unwrap();
        for id in 1..=2 {
            let target = change(
                &mut store,
                Command::Save(
                    None,
                    format!("https://github.com/a/repo{id}"),
                    String::new(),
                    true,
                    false,
                ),
            )
            .unwrap();
            assert_eq!(target, Some(id));
        }
        assert_eq!(change(&mut store, Command::Interval(3)).unwrap(), None);
        assert_eq!(change(&mut store, Command::Enable(1, false)).unwrap(), None);
        assert_eq!(
            change(&mut store, Command::Enable(1, true)).unwrap(),
            Some(1)
        );
        assert_eq!(change(&mut store, Command::Remove(2)).unwrap(), None);
    }

    #[test]
    fn latest_requests_and_repository_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::at(
            dir.path().join("config.json"),
            dir.path().join("history.json"),
        )
        .unwrap();
        let url = "https://github.com/a/b/releases";
        change(
            &mut store,
            Command::Save(None, url.into(), "".into(), true, false),
        )
        .unwrap();
        assert_eq!(store.config.watches[0].latest_request, 0);
        change(
            &mut store,
            Command::Save(Some(1), url.into(), "".into(), true, true),
        )
        .unwrap();
        assert_eq!(store.config.watches[0].latest_request, 1);
        change(
            &mut store,
            Command::Save(Some(1), url.into(), "v2".into(), true, true),
        )
        .unwrap();
        assert_eq!(store.config.watches[0].latest_request, 1);
        change(
            &mut store,
            Command::Save(Some(1), url.into(), "v2".into(), true, false),
        )
        .unwrap();
        change(
            &mut store,
            Command::Save(Some(1), url.into(), "v2".into(), true, true),
        )
        .unwrap();
        assert_eq!(store.config.watches[0].latest_request, 2);
        change(&mut store, Command::Enable(1, false)).unwrap();
        assert!(!store.config.watches[0].enabled);
        change(
            &mut store,
            Command::Save(
                Some(1),
                "https://github.com/a/c".into(),
                "".into(),
                true,
                false,
            ),
        )
        .unwrap();
        assert_eq!(store.config.watches[0].id, 2);
        assert_eq!(store.config.watches[0].latest_request, 0);
        assert!(store.histories.is_empty());
    }

    #[test]
    fn clearing_history_keeps_deduplication_state() {
        use gnome_git_release_tray::model::History;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        let state = dir.path().join("state").join("history.json");
        let mut store = Store::at(config.clone(), state.clone()).unwrap();
        let release = Release {
            id: 7,
            name: None,
            tag_name: "v1".into(),
            html_url: String::new(),
            published_at: Some("2026-01-01T00:00:00Z".into()),
            draft: false,
            prerelease: false,
        };
        store.histories.insert(
            1,
            History {
                initialized: true,
                seen: [7].into(),
                notifications: vec![NotificationRecord {
                    sent_at: 1,
                    release,
                    debug: false,
                }],
                ..Default::default()
            },
        );
        // A file where the state directory belongs makes the save fail.
        std::fs::write(dir.path().join("state"), "").unwrap();
        assert!(clear_history(&mut store, 1).is_err());
        assert_eq!(store.histories[&1].notifications.len(), 1);
        std::fs::remove_file(dir.path().join("state")).unwrap();
        clear_history(&mut store, 1).unwrap();
        clear_history(&mut store, 2).unwrap();
        let restored = Store::at(config, state).unwrap();
        assert!(restored.histories[&1].notifications.is_empty());
        assert!(restored.histories[&1].initialized);
        assert!(restored.histories[&1].seen.contains(&7));
    }
}
