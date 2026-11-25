//! The home page as stacks: datasets on shelves, grouped into lanes by
//! where they come from, with an inspector beside them.
//!
//! Each dataset is a spine on a shelf. A spine shows at a glance what a
//! person needs before opening it: how many columns, whether its watches
//! have found anything, and how many of its answers are verified. Choosing
//! a spine opens the inspector (`/?pick=name`): what the data means, its
//! verified questions and its watches, with the ways in.
//!
//! Everything is drawn on the server, like every other page. The shelves
//! scroll sideways by themselves, so no script is needed.

use crate::app::AppState;
use crate::catalog::Entry;
use beam_accounts::{Watch, WatchStatus};
use maud::{html, Markup};
use std::collections::HashMap;

/// Most column meanings shown in the inspector; the About view has them all.
const INSPECTOR_COLUMNS: usize = 8;

/// One lane of shelves: a numbered label, a title and the spines on it.
pub struct Lane<'a> {
    pub number: &'static str,
    pub title: &'a str,
    /// Accent for the spines, one of the `lane-*` classes in the stylesheet.
    pub tone: &'static str,
    pub entries: Vec<(&'a Entry, String)>,
}

/// What the shelves need to know about every dataset, read once per page.
pub struct Facts {
    watches: HashMap<String, Vec<Watch>>,
    questions: HashMap<String, usize>,
    /// Datasets with a written description, lower case.
    described: std::collections::HashSet<String>,
}

/// The steps a dataset climbs, like levels in a management game: each one
/// is something a person does that makes its answers more dependable.
const LEVELS: [&str; 4] = ["Added", "Described", "Watched", "Verified"];

impl Facts {
    pub fn read(state: &AppState, entries: &[Entry]) -> Self {
        let mut watches: HashMap<String, Vec<Watch>> = HashMap::new();
        for watch in state.accounts.watches(None).unwrap_or_default() {
            watches
                .entry(watch.dataset.to_ascii_lowercase())
                .or_default()
                .push(watch);
        }
        let questions = entries
            .iter()
            .map(|e| {
                let count = state.accounts.questions(&e.set.name).map_or(0, |q| q.len());
                (e.set.name.to_ascii_lowercase(), count)
            })
            .collect();
        let described = entries
            .iter()
            .filter(|e| {
                state
                    .accounts
                    .notes(&e.set.name)
                    .is_ok_and(|notes| notes.iter().any(|n| n.column.is_empty()))
            })
            .map(|e| e.set.name.to_ascii_lowercase())
            .collect();
        Self {
            watches,
            questions,
            described,
        }
    }

    /// How far a dataset has climbed (1 to 4), and the next step if any.
    /// Steps are counted in order, so the level always names the first
    /// thing still missing.
    fn level(&self, name: &str) -> (usize, Option<&'static str>) {
        let done = [
            self.described.contains(&name.to_ascii_lowercase()),
            !self.watches(name).is_empty(),
            self.questions(name) > 0,
        ];
        let reached = 1 + done.iter().take_while(|d| **d).count();
        let next = match reached {
            1 => Some("add a description"),
            2 => Some("add a watch"),
            3 => Some("verify a question"),
            _ => None,
        };
        (reached, next)
    }

    fn watches(&self, name: &str) -> &[Watch] {
        self.watches.get(&name.to_ascii_lowercase()).map_or(&[], Vec::as_slice)
    }

    fn questions(&self, name: &str) -> usize {
        self.questions.get(&name.to_ascii_lowercase()).copied().unwrap_or(0)
    }

    /// The worst latest result among a dataset's watches.
    fn status(&self, name: &str) -> Option<WatchStatus> {
        let watches = self.watches(name);
        if watches.iter().any(|w| w.status == Some(WatchStatus::Alert)) {
            Some(WatchStatus::Alert)
        } else if watches.iter().any(|w| w.status == Some(WatchStatus::Error)) {
            Some(WatchStatus::Error)
        } else if watches.iter().any(|w| w.status == Some(WatchStatus::Ok)) {
            Some(WatchStatus::Ok)
        } else {
            None
        }
    }
}

/// A light with words beside it, so the state never depends on colour alone.
fn light(status: Option<WatchStatus>) -> Markup {
    let (class, words) = match status {
        Some(WatchStatus::Alert) => ("light alert", "Watch alert"),
        Some(WatchStatus::Error) => ("light error", "Cannot check"),
        Some(WatchStatus::Ok) => ("light ok", "Checks OK"),
        None => ("light", "No watches"),
    };
    html! { span class=(class) { (words) } }
}

pub fn lanes(lanes: &[Lane<'_>], facts: &Facts, picked: Option<&str>) -> Markup {
    html! {
        @for (index, lane) in lanes.iter().filter(|l| !l.entries.is_empty()).enumerate() {
            section.lane aria-labelledby={ "lane-" (lane.number) } {
                div.section-label {
                    h2 #{ "lane-" (lane.number) } { (lane.title) }
                    span.count { (lane.entries.len()) }
                }
                @if index == 0 { (crate::learn::spot("datasets", "each card is a dataset. Choose one to see what it holds.")) }
                ul.shelf {
                    @for (entry, note) in &lane.entries {
                        @let name = &entry.set.name;
                        @let chosen = picked.is_some_and(|p| p.eq_ignore_ascii_case(name));
                        @let questions = facts.questions(name);
                        li {
                            a class={ "spine " (lane.tone) @if chosen { " chosen" } }
                                href={ "/?pick=" (name) "#inspector" }
                                aria-current=[chosen.then_some("true")] {
                                strong.spine-title { (breakable(name)) }
                                span.spine-meta { (entry.set.properties.len()) " columns" }
                                span.spine-meta { (note) }
                                @let (reached, next) = facts.level(name);
                                span.level title={ "Level " (reached) " of 4: " (LEVELS[reached - 1]) } {
                                    @for step in 1..=4 { span class={ "pip" @if step <= reached { " on" } } {} }
                                    " " (LEVELS[reached - 1])
                                }
                                @if let Some(next) = next { span.spine-meta.next { "Next: " (next) } }
                                span.spine-foot {
                                    (light(facts.status(name)))
                                    @if questions > 0 {
                                        span.verified { "✓ " (questions) " verified" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The counter bar along the top of the stacks: totals at a glance.
pub fn counters(entries: &[Entry], facts: &Facts) -> Markup {
    let names: Vec<&str> = entries.iter().map(|e| e.set.name.as_str()).collect();
    let described = names
        .iter()
        .filter(|n| facts.described.contains(&n.to_ascii_lowercase()))
        .count();
    let watched = names.iter().filter(|n| !facts.watches(n).is_empty()).count();
    let trusted = names.iter().filter(|n| facts.level(n).0 == 4).count();
    let verified: usize = names.iter().map(|n| facts.questions(n)).sum();
    let alerts = names
        .iter()
        .flat_map(|n| facts.watches(n))
        .filter(|w| w.status == Some(WatchStatus::Alert))
        .count();
    html! {
        dl.counters aria-label="Summary" {
            div { dt { "Datasets" } dd { (names.len()) } }
            div { dt { "Described" } dd { (described) } }
            div { dt { "Watched" } dd { (watched) } }
            div { dt { "Verified" } dd { (trusted) } }
            div { dt { "Verified questions" } dd { (verified) } }
            div class={ @if alerts > 0 { "alerting" } } { dt { "Alerts" } dd { (alerts) } }
        }
    }
}

/// The inspector for the chosen dataset.
pub fn inspector(state: &AppState, entry: &Entry, facts: &Facts) -> Markup {
    let set = &entry.set;
    let notes: HashMap<String, String> = state
        .accounts
        .notes(&set.name)
        .unwrap_or_default()
        .into_iter()
        .map(|n| (n.column, n.text))
        .collect();
    let questions = state.accounts.questions(&set.name).unwrap_or_default();
    let watches = facts.watches(&set.name);
    let base = format!("/datasets/{}", set.name);
    let explained: Vec<(&str, &str)> = set
        .properties
        .iter()
        .filter_map(|p| notes.get(&p.name).map(|t| (p.name.as_str(), t.as_str())))
        .collect();
    html! {
        aside.inspector #inspector aria-labelledby="inspector-title" {
            div.section-label { h2 { "Details" } span.count { (set.properties.len()) " columns" } }
            h2 #inspector-title { (set.name) }
            @match notes.get("") {
                Some(text) => p.prose { (text) },
                None => p.muted { "No description yet. Its owner or an administrator can write one on the About view." },
            }
            div.inspector-actions {
                a.button href=(base) { "Open table" }
                a.button.quiet href={ (base) "/chart" } { "Chart" }
                a.button.quiet href={ (base) "/about" } { "About" }
            }
            @if !explained.is_empty() {
                h3 { "What the columns mean" }
                dl {
                    @for (column, text) in explained.iter().take(INSPECTOR_COLUMNS) { dt { (column) } dd { (text) } }
                }
                @if explained.len() > INSPECTOR_COLUMNS {
                    p.hint { a href={ (base) "/about" } { "All " (explained.len()) " meanings" } }
                }
            }
            h3 { "Verified questions" }
            @if questions.is_empty() {
                p.muted { "None yet." }
            } @else {
                ul.plain {
                    @for q in &questions {
                        li { a href={ (base) "/chart?" (q.settings) } { (q.question) } }
                    }
                }
            }
            h3 { "Watches" }
            @if watches.is_empty() {
                p.muted { "None yet." }
            } @else {
                ul.plain {
                    @for w in watches {
                        li {
                            strong { (w.name) } br;
                            (light(w.status))
                            @if let Some(message) = &w.message { br; span.hint { (message) } }
                        }
                    }
                }
            }
        }
    }
}

/// A dataset name that may wrap after each `_`, so `orders_with_customers`
/// breaks between words on a narrow card instead of inside one.
fn breakable(name: &str) -> Markup {
    let mut parts = name.split_inclusive('_').peekable();
    html! {
        @while let Some(part) = parts.next() {
            (part)
            @if parts.peek().is_some() { wbr; }
        }
    }
}
