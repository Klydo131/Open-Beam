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
}

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
        Self { watches, questions }
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
        @for lane in lanes.iter().filter(|l| !l.entries.is_empty()) {
            section.lane aria-labelledby={ "lane-" (lane.number) } {
                div.section-label {
                    span #{ "lane-" (lane.number) } { (lane.number) " / " (lane.title) }
                    span.tag { (lane.entries.len()) @if lane.entries.len() == 1 { " dataset" } @else { " datasets" } }
                }
                ul.shelf {
                    @for (entry, note) in &lane.entries {
                        @let name = &entry.set.name;
                        @let chosen = picked.is_some_and(|p| p.eq_ignore_ascii_case(name));
                        @let questions = facts.questions(name);
                        li {
                            a class={ "spine " (lane.tone) @if chosen { " chosen" } }
                                href={ "/?pick=" (name) "#inspector" }
                                aria-current=[chosen.then_some("true")] {
                                strong.spine-title { (name) }
                                span.spine-meta { (entry.set.properties.len()) " columns" }
                                span.spine-meta { (note) }
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
            div.section-label { span { "INSPECTOR" } span.tag { (set.properties.len()) " columns" } }
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
