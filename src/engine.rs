use crate::{
    github::GitHub,
    model::{History, NotificationRecord, Release, Watch, discover, newest},
    storage::Store,
};
use anyhow::Result;
use fancy_regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

pub trait Notifier {
    fn send(
        &self,
        watch: &Watch,
        release: &Release,
        debug: bool,
    ) -> impl std::future::Future<Output = Result<()>>;
}

pub async fn check(
    store: &mut Store,
    github: &mut GitHub,
    notifier: &impl Notifier,
    debug: bool,
    status: impl FnMut(u64, String),
) {
    check_selected(store, github, notifier, debug, None, status).await;
}

pub async fn check_selected(
    store: &mut Store,
    github: &mut GitHub,
    notifier: &impl Notifier,
    debug: bool,
    selected: Option<&[u64]>,
    mut status: impl FnMut(u64, String),
) {
    let watches: Vec<Watch> = store
        .config
        .watches
        .iter()
        .filter(|w| w.enabled && selected.is_none_or(|ids| ids.contains(&w.id)))
        .cloned()
        .collect();
    // Once a watch has recorded the existing releases, new ones appear on the
    // newest page. A first check, a latest request or a debug replay searches
    // every release GitHub lists.
    let all_pages: BTreeSet<&str> = watches
        .iter()
        .filter(|w| {
            debug
                || store
                    .histories
                    .get(&w.id)
                    .is_none_or(|h| !h.initialized || wants_latest(w, h))
        })
        .map(|w| w.repository.as_str())
        .collect();
    let mut fetched: BTreeMap<String, Result<Vec<Release>, String>> = BTreeMap::new();
    for watch in &watches {
        status(watch.id, "Checking…".into());
        if !fetched.contains_key(&watch.repository) {
            fetched.insert(
                watch.repository.clone(),
                github
                    .releases(
                        &watch.repository,
                        all_pages.contains(watch.repository.as_str()),
                    )
                    .await
                    .map_err(|e| format!("{e:#}")),
            );
        }
        let result: Result<String> = async {
            let releases = fetched[&watch.repository]
                .as_ref()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let regex = Regex::new(&watch.pattern)?;
            if debug {
                if let Some(release) = newest(releases, &regex)? {
                    notifier.send(watch, release, true).await?;
                    record_delivery(store, watch, release, true)?;
                    return Ok(format!("Debug replay: {}", release.tag_name));
                }
                return Ok("Debug: no matching release".into());
            }
            let previous = store.histories.get(&watch.id).cloned().unwrap_or_default();
            let history = store.histories.entry(watch.id).or_default();
            history.latest_requested = wants_latest(watch, history);
            discover(history, releases, &regex)?;
            history.latest_request_done = watch.latest_request;
            if let Err(error) = store.save_history() {
                store.histories.insert(watch.id, previous);
                return Err(error);
            }
            let count = deliver(store, watch, notifier).await?;
            Ok(format!("Checked · {count} notification(s) sent"))
        }
        .await;
        status(
            watch.id,
            match result {
                Ok(message) => message,
                Err(error) => format!("Error: {error:#}"),
            },
        );
    }
}

fn wants_latest(watch: &Watch, history: &History) -> bool {
    watch.notify_latest && watch.latest_request > history.latest_request_done
}

pub async fn deliver(store: &mut Store, watch: &Watch, notifier: &impl Notifier) -> Result<usize> {
    let pending = store.histories.entry(watch.id).or_default().pending.clone();
    let mut sent = 0;
    for release in pending.into_values() {
        notifier.send(watch, &release, false).await?;
        record_delivery(store, watch, &release, false)?;
        sent += 1;
    }
    Ok(sent)
}

fn record_delivery(store: &mut Store, watch: &Watch, release: &Release, debug: bool) -> Result<()> {
    let history = store.histories.entry(watch.id).or_default();
    let pending = if debug {
        None
    } else {
        history.pending.remove(&release.id)
    };
    history.notifications.push(NotificationRecord {
        sent_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        release: release.clone(),
        debug,
    });
    if let Err(error) = store.save_history() {
        let history = store.histories.get_mut(&watch.id).unwrap();
        history.notifications.pop();
        if let Some(pending) = pending {
            history.pending.insert(pending.id, pending);
        }
        return Err(error.context("Notification sent, but its history could not be saved"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::History;
    use std::cell::Cell;
    struct Sink {
        fail: Cell<bool>,
        count: Cell<usize>,
    }
    impl Notifier for Sink {
        async fn send(&self, _: &Watch, _: &Release, _: bool) -> Result<()> {
            if self.fail.get() {
                anyhow::bail!("Notification service unavailable");
            }
            self.count.set(self.count.get() + 1);
            Ok(())
        }
    }
    #[test]
    fn failed_delivery_retries_after_restart() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        let state = dir.path().join("history.json");
        let mut store = Store::at(config.clone(), state.clone()).unwrap();
        let watch = Watch {
            id: 1,
            repository: "a/b".into(),
            pattern: "".into(),
            enabled: true,
            notify_latest: false,
            latest_request: 0,
        };
        let release = Release {
            id: 2,
            name: None,
            tag_name: "v2".into(),
            html_url: "https://github.com/a/b/releases/tag/v2".into(),
            published_at: Some("2026-01-01T00:00:00Z".into()),
            draft: false,
            prerelease: false,
        };
        store.histories.insert(
            1,
            History {
                initialized: true,
                seen: [2].into(),
                pending: [(2, release)].into(),
                latest_requested: false,
                latest_request_done: 0,
                notifications: vec![],
            },
        );
        store.save_history().unwrap();
        let sink = Sink {
            fail: Cell::new(true),
            count: Cell::new(0),
        };
        assert!(
            runtime
                .block_on(deliver(&mut store, &watch, &sink))
                .is_err()
        );
        let mut store = Store::at(config.clone(), state.clone()).unwrap();
        assert!(store.histories[&1].notifications.is_empty());
        sink.fail.set(false);
        assert_eq!(
            runtime
                .block_on(deliver(&mut store, &watch, &sink))
                .unwrap(),
            1
        );
        let mut store = Store::at(config, state).unwrap();
        assert_eq!(
            runtime
                .block_on(deliver(&mut store, &watch, &sink))
                .unwrap(),
            0
        );
        assert_eq!(sink.count.get(), 1);
        let records = &store.histories[&1].notifications;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].release.id, 2);
        assert!(!records[0].debug);
        assert!(records[0].sent_at > 0);
    }
}

#[cfg(test)]
mod polling_tests {
    use super::*;
    use crate::github::tests::{response, server};
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingSink(RefCell<Vec<(u64, u64, bool)>>);
    impl Notifier for RecordingSink {
        async fn send(&self, watch: &Watch, release: &Release, debug: bool) -> Result<()> {
            self.0.borrow_mut().push((watch.id, release.id, debug));
            Ok(())
        }
    }
    fn release(id: u64) -> Release {
        Release {
            id,
            name: None,
            tag_name: format!("v{id}"),
            html_url: format!("https://github.com/a/b/releases/tag/v{id}"),
            published_at: Some(format!("2026-01-{id:02}T00:00:00Z")),
            draft: false,
            prerelease: false,
        }
    }
    fn watches() -> Vec<Watch> {
        vec![
            Watch {
                id: 1,
                repository: "a/b".into(),
                pattern: "^v".into(),
                enabled: true,
                notify_latest: false,
                latest_request: 0,
            },
            Watch {
                id: 2,
                repository: "a/b".into(),
                pattern: "".into(),
                enabled: true,
                notify_latest: true,
                latest_request: 1,
            },
            Watch {
                id: 3,
                repository: "disabled/repo".into(),
                pattern: "".into(),
                enabled: false,
                notify_latest: false,
                latest_request: 0,
            },
        ]
    }
    #[test]
    fn shared_requests_latest_debug_restart_and_future() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        let state = dir.path().join("history.json");
        let mut store = Store::at(config.clone(), state.clone()).unwrap();
        store.config.watches = watches();
        store.config.next_id = 4;
        store.save_config().unwrap();
        let old = serde_json::to_string(&vec![release(1)]).unwrap();
        let new = serde_json::to_string(&vec![release(2), release(1)]).unwrap();
        let (base, join) = server(vec![
            response("200 OK", "", &old),
            response("200 OK", "", &old),
            response("200 OK", "", &old),
            response("200 OK", "", &old),
            response("200 OK", "", &new),
        ]);
        let mut github = GitHub::with_base(base, None).unwrap();
        let sink = RecordingSink::default();
        rt.block_on(check(&mut store, &mut github, &sink, false, |_, _| {}));
        assert_eq!(*sink.0.borrow(), vec![(2, 1, false)]);
        let before = store.histories.clone();
        for _ in 0..2 {
            rt.block_on(check(&mut store, &mut github, &sink, true, |_, _| {}));
        }
        assert_eq!(sink.0.borrow().len(), 5);
        for (id, history) in &store.histories {
            assert_eq!(history.seen, before[id].seen);
            assert_eq!(history.pending.len(), before[id].pending.len());
            assert_eq!(history.initialized, before[id].initialized);
            assert_eq!(history.latest_request_done, before[id].latest_request_done);
            assert_eq!(history.notifications.iter().filter(|n| n.debug).count(), 2);
        }
        assert!(sink.0.borrow()[1..].iter().all(|event| event.2));
        let mut store = Store::at(config, state).unwrap();
        rt.block_on(check(&mut store, &mut github, &sink, false, |_, _| {}));
        assert_eq!(sink.0.borrow().len(), 5);
        rt.block_on(check(&mut store, &mut github, &sink, false, |_, _| {}));
        assert_eq!(&sink.0.borrow()[5..], &[(1, 2, false), (2, 2, false)]);
        assert_eq!(store.histories[&1].notifications.len(), 3);
        assert_eq!(store.histories[&2].notifications.len(), 4);
        assert_eq!(join.join().unwrap().len(), 5);
    }
    #[test]
    fn targeted_check_leaves_existing_monitors_untouched() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::at(
            dir.path().join("config.json"),
            dir.path().join("history.json"),
        )
        .unwrap();
        store.config.watches = watches();
        store.config.watches[1].repository = "new/repo".into();
        let old = crate::model::History {
            initialized: true,
            seen: [99].into(),
            ..Default::default()
        };
        let before = serde_json::to_string(&old).unwrap();
        store.histories.insert(1, old);
        let (base, join) = server(vec![response("200 OK", "", "[]")]);
        let mut github = GitHub::with_base(base, None).unwrap();
        let sink = RecordingSink::default();
        let mut statuses = BTreeMap::new();
        rt.block_on(check_selected(
            &mut store,
            &mut github,
            &sink,
            false,
            Some(&[2]),
            |id, text| {
                statuses.insert(id, text);
            },
        ));
        assert_eq!(statuses.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert_eq!(serde_json::to_string(&store.histories[&1]).unwrap(), before);
        assert!(store.histories[&2].initialized);
        assert!(!store.histories.contains_key(&3));
        assert!(sink.0.borrow().is_empty());
        let requests = join.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("/repos/new/repo/releases?"));
    }

    #[test]
    fn initialized_watches_fetch_only_the_newest_page() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::at(
            dir.path().join("config.json"),
            dir.path().join("history.json"),
        )
        .unwrap();
        store.config.watches = watches()[..1].to_vec();
        let next = "Link: <http://unused>; rel=\"next\"\r\n";
        let newest = serde_json::to_string(&vec![release(3), release(2)]).unwrap();
        let (base, join) = server(vec![
            response(
                "200 OK",
                next,
                &serde_json::to_string(&vec![release(2)]).unwrap(),
            ),
            response(
                "200 OK",
                "",
                &serde_json::to_string(&vec![release(1)]).unwrap(),
            ),
            response("200 OK", next, &newest),
            response("200 OK", next, &newest),
            response(
                "200 OK",
                "",
                &serde_json::to_string(&vec![release(1)]).unwrap(),
            ),
        ]);
        let mut github = GitHub::with_base(base, None).unwrap();
        let sink = RecordingSink::default();
        rt.block_on(check(&mut store, &mut github, &sink, false, |_, _| {}));
        assert!(sink.0.borrow().is_empty());
        assert_eq!(store.histories[&1].seen, [1, 2].into());
        rt.block_on(check(&mut store, &mut github, &sink, false, |_, _| {}));
        assert_eq!(*sink.0.borrow(), vec![(1, 3, false)]);
        // A latest request needs every page to find an older match.
        let watch = &mut store.config.watches[0];
        watch.pattern = "^v1$".into();
        watch.notify_latest = true;
        watch.latest_request = 1;
        rt.block_on(check(&mut store, &mut github, &sink, false, |_, _| {}));
        assert_eq!(*sink.0.borrow(), vec![(1, 3, false), (1, 1, false)]);
        let requests = join.join().unwrap();
        assert_eq!(requests.len(), 5);
        for (request, page) in requests.iter().zip([1, 2, 1, 1, 2]) {
            assert!(request.contains(&format!("&page={page} ")), "{request}");
        }
    }

    #[test]
    fn failed_fetch_does_not_initialize_or_notify() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::at(
            dir.path().join("config.json"),
            dir.path().join("history.json"),
        )
        .unwrap();
        store.config.watches = watches();
        let (base, join) = server(vec![response("500 Internal Server Error", "", "{}")]);
        let mut github = GitHub::with_base(base, None).unwrap();
        let sink = RecordingSink::default();
        let mut statuses = BTreeMap::new();
        rt.block_on(check(&mut store, &mut github, &sink, false, |id, text| {
            statuses.insert(id, text);
        }));
        assert!(store.histories.is_empty());
        assert!(sink.0.borrow().is_empty());
        assert_eq!(statuses.len(), 2);
        assert!(statuses.values().all(|s| s.starts_with("Error:")));
        join.join().unwrap();
    }
}
