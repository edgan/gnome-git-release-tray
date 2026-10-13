use anyhow::{Context, Result, bail};
use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Watch {
    pub id: u64,
    pub repository: String,
    pub pattern: String,
    pub enabled: bool,
    pub notify_latest: bool,
    #[serde(default)]
    pub latest_request: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub interval_hours: u64,
    pub next_id: u64,
    pub watches: Vec<Watch>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_hours: 5,
            next_id: 1,
            watches: vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    pub id: u64,
    pub name: Option<String>,
    pub tag_name: String,
    pub html_url: String,
    pub published_at: Option<String>,
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationRecord {
    pub sent_at: u64,
    pub release: Release,
    pub debug: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct History {
    pub initialized: bool,
    pub seen: BTreeSet<u64>,
    pub pending: BTreeMap<u64, Release>,
    pub latest_requested: bool,
    #[serde(default)]
    pub latest_request_done: u64,
    #[serde(default)]
    pub notifications: Vec<NotificationRecord>,
}

/// Pattern behind the editor's "Stable releases only" toggle. Watches using it
/// also skip GitHub prereleases and need both the tag and the title to match.
pub const STABLE_PATTERN: &str = r"^(?!.*([Bb]eta|[Nn]ightly|[Pp]review|[Rr][Cc]))";

pub fn repository(input: &str) -> Result<String> {
    let url = Url::parse(input.trim())?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        bail!("Use an https://github.com/owner/repository release URL");
    }
    let path: Vec<_> = url
        .path()
        .trim_end_matches('/')
        .split('/')
        .skip(1)
        .collect();
    let valid_suffix = path.len() == 2
        || (path.get(2) == Some(&"releases")
            && (path.len() == 3
                || (path.len() == 4 && path[3] == "latest")
                || (path.len() >= 5 && path[3] == "tag")));
    if !valid_suffix
        || path.iter().take(2).any(|p| {
            p.is_empty()
                || !p
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    {
        bail!("Expected a GitHub repository or releases URL");
    }
    Ok(format!("{}/{}", path[0], path[1]).to_lowercase())
}

pub fn matches(release: &Release, regex: &Regex) -> Result<bool> {
    if release.draft || release.published_at.is_none() {
        return Ok(false);
    }
    let stable = regex.as_str() == STABLE_PATTERN;
    if stable && release.prerelease {
        return Ok(false);
    }
    let tag_matches = regex
        .is_match(&release.tag_name)
        .context("Regex evaluation failed for release tag")?;
    // Other watches need the tag or the title to match; stable-only watches need
    // both, so a prerelease marker in either one excludes the release.
    if tag_matches != stable {
        return Ok(tag_matches);
    }
    match release.name.as_deref().filter(|name| !name.is_empty()) {
        Some(name) => regex
            .is_match(name)
            .context("Regex evaluation failed for release title"),
        None => Ok(tag_matches),
    }
}

pub fn newest<'a>(releases: &'a [Release], regex: &Regex) -> Result<Option<&'a Release>> {
    let mut newest: Option<&Release> = None;
    for release in releases {
        if matches(release, regex)?
            && newest.is_none_or(|previous| {
                (&release.published_at, release.id) > (&previous.published_at, previous.id)
            })
        {
            newest = Some(release);
        }
    }
    Ok(newest)
}

/// Commit discovery only after every regex evaluation succeeds. Debug replay
/// bypasses discovery and never changes deduplication state.
pub fn discover(history: &mut History, releases: &[Release], regex: &Regex) -> Result<()> {
    let mut next = history.clone();
    for (id, release) in &history.pending {
        if !matches(release, regex)? {
            next.pending.remove(id);
        }
    }
    for release in releases
        .iter()
        .filter(|r| !r.draft && r.published_at.is_some())
    {
        if next.seen.insert(release.id) && next.initialized && matches(release, regex)? {
            next.pending.insert(release.id, release.clone());
        }
    }
    if next.latest_requested {
        if let Some(release) = newest(releases, regex)? {
            next.pending.insert(release.id, release.clone());
        }
        next.latest_requested = false;
    }
    next.initialized = true;
    *history = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn release(id: u64, tag: &str) -> Release {
        Release {
            id,
            name: Some(format!("Release {tag}")),
            tag_name: tag.into(),
            html_url: format!("https://github.com/a/b/releases/tag/{tag}"),
            published_at: Some(format!("2026-01-{id:02}T00:00:00Z")),
            draft: false,
            prerelease: false,
        }
    }
    #[test]
    fn lookahead_excludes_release_candidates() {
        let regex = Regex::new(r"^(?!.*RC).*$").unwrap();
        assert!(matches(&release(1, "v10.11.0"), &regex).unwrap());
        assert!(!matches(&release(2, "v10.11.0-RC1"), &regex).unwrap());
        let regex = Regex::new(r"(?i)^(?!.*rc).*$").unwrap();
        let mut candidate = release(2, "v10.11.0-rc1");
        candidate.name = None;
        assert!(!matches(&candidate, &regex).unwrap());
        candidate.name = Some(String::new());
        assert!(!matches(&candidate, &regex).unwrap());
        assert!(Regex::new(r"(?<=v)\d+").unwrap().is_match("v12").unwrap());
        assert!(Regex::new(r"^(v)\1$").unwrap().is_match("vv").unwrap());
    }

    #[test]
    fn stable_pattern_excludes_prereleases() {
        let regex = Regex::new(STABLE_PATTERN).unwrap();
        for (tag, title) in [
            ("5.1.3", "SABnzbd 5.1.3"),
            ("v10.11.0", "10.11.0"),
            ("v4.0.0", ""),
        ] {
            let mut stable = release(1, tag);
            stable.name = Some(title.into());
            assert!(matches(&stable, &regex).unwrap(), "{tag}");
        }
        for (tag, title) in [
            ("5.2.0Beta2", "SABnzbd 5.2.0Beta2"),
            ("v2.0.0-beta.1", "Version 2.0.0"),
            ("v2.0.0", "Version 2.0.0 Beta"),
            ("v2.0.0-beta.1", "Beta 1"),
            ("nightly-20261011", "Nightly build"),
            ("v3.0.0-preview.2", "Preview 2"),
            ("5.1.0RC2", "SABnzbd 5.1.0RC2"),
            ("v10.11.0-rc1", "10.11.0-rc1"),
        ] {
            let mut prerelease = release(1, tag);
            prerelease.name = Some(title.into());
            assert!(!matches(&prerelease, &regex).unwrap(), "{tag} {title}");
        }
        let mut flagged = release(1, "v5.0.0");
        flagged.prerelease = true;
        assert!(!matches(&flagged, &regex).unwrap());
        // Only the stable pattern changes how tag, title and the flag combine.
        let any = Regex::new("").unwrap();
        assert!(matches(&flagged, &any).unwrap());
        let mut title_only = release(1, "v0.0.5");
        title_only.name = Some("T3 Code v0.0.5".into());
        assert!(matches(&title_only, &Regex::new("T3 Code v[0-9]").unwrap()).unwrap());
        let beta_tag = Regex::new(r"^(?!.*beta)").unwrap();
        let mut plain_title = release(1, "v2.0.0-beta.1");
        plain_title.name = Some("Version 2.0.0".into());
        assert!(matches(&plain_title, &beta_tag).unwrap());
    }

    #[test]
    fn regex_runtime_errors_do_not_change_discovery() {
        let regex = fancy_regex::RegexBuilder::new(r"^(a+)+(?=b)$")
            .backtrack_limit(1)
            .build()
            .unwrap();
        let mut history = History {
            initialized: true,
            ..Default::default()
        };
        let before = serde_json::to_string(&history).unwrap();
        let result = discover(
            &mut history,
            &[release(1, "aaaaaaaaaaaaaaaaaaaaaaaac")],
            &regex,
        );
        assert!(result.is_err());
        assert_eq!(serde_json::to_string(&history).unwrap(), before);
    }

    #[test]
    fn old_history_loads_without_delivery_records() {
        let history: History = serde_json::from_str(r#"{"initialized":true,"seen":[1,2],"pending":{},"latest_requested":false,"latest_request_done":1}"#).unwrap();
        assert!(history.notifications.is_empty());
        assert_eq!(history.seen.len(), 2);
        assert!(history.initialized);
    }

    #[test]
    fn urls() {
        for suffix in [
            "",
            "/",
            "/releases",
            "/releases/latest",
            "/releases/tag/v1",
            "/releases/tag/release/v1",
        ] {
            assert_eq!(
                repository(&format!("https://github.com/A/B{suffix}")).unwrap(),
                "a/b"
            );
        }
        for url in [
            "https://evil.com/a/b",
            "http://github.com/a/b",
            "https://github.com/a",
            "https://github.com/a/b/issues",
            "https://github.com/a/b/releases/download/v1/file",
            "https://user@github.com/a/b",
        ] {
            assert!(repository(url).is_err(), "{url}");
        }
    }
    #[test]
    fn baseline_restart_and_filter_edits() {
        let mut h = History::default();
        discover(&mut h, &[release(1, "v1")], &Regex::new("v").unwrap()).unwrap();
        assert!(h.pending.is_empty());
        let mut h: History = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        discover(
            &mut h,
            &[release(1, "v1"), release(2, "v2")],
            &Regex::new("v2").unwrap(),
        )
        .unwrap();
        assert_eq!(h.pending.keys().copied().collect::<Vec<_>>(), vec![2]);
        h.pending.clear();
        discover(
            &mut h,
            &[release(1, "v1"), release(2, "v2")],
            &Regex::new("").unwrap(),
        )
        .unwrap();
        assert!(h.pending.is_empty());
    }
    #[test]
    fn latest_and_replay() {
        let releases = vec![release(2, "v2-beta"), release(1, "v1")];
        let re = Regex::new("v").unwrap();
        let mut h = History {
            latest_requested: true,
            ..Default::default()
        };
        discover(&mut h, &releases, &re).unwrap();
        assert_eq!(h.pending.len(), 1);
        assert!(h.pending.contains_key(&2));
        // Failed submission remains pending on the next discovery.
        discover(&mut h, &releases, &re).unwrap();
        assert_eq!(h.pending.len(), 1);
        h.pending.clear();
        let before = serde_json::to_string(&h).unwrap();
        for _ in 0..2 {
            assert_eq!(newest(&releases, &re).unwrap().unwrap().id, 2);
        }
        assert_eq!(before, serde_json::to_string(&h).unwrap());
        discover(&mut h, &[release(3, "v3")], &re).unwrap();
        assert!(h.pending.contains_key(&3));
    }
    #[test]
    fn title_tag_and_drafts() {
        let mut r = release(1, "v1");
        assert!(matches(&r, &Regex::new("Release").unwrap()).unwrap());
        assert!(!matches(&r, &Regex::new("release").unwrap()).unwrap());
        r.draft = true;
        assert!(!matches(&r, &Regex::new("").unwrap()).unwrap());
        let invalid = String::from("[");
        assert!(Regex::new(&invalid).is_err());
    }
}
