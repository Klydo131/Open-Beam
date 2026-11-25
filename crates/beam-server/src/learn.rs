//! The Learn page: short lessons a person opens when they want them, never
//! forced on them, each ending in "Try it" on their own data.
//!
//! Built on what the evidence says works:
//! - help people pull when they need it, and can come back to, rather than
//!   a tour pushed at sign-in, which users "frequently skip" (Nielsen
//!   Norman Group, "Onboarding Tutorials vs. Contextual Help", 2023);
//! - one hint per task, kept short and set apart from the real controls
//!   (NN/g, "Instructional Overlays and Coach Marks", 2014); a few steps
//!   at a time;
//! - doing the task on real data rather than only being shown it: "Try it"
//!   opens the real page at `#learn-<lesson>`, where the browser scrolls to
//!   the control and the stylesheet points at it with an arrow and a tip.
//!
//! Motion is CSS only and stops within five seconds, and there is none when
//! the device asks for reduced motion (WCAG 2.2, 2.2.2 and 2.3.3). No
//! script runs: the highlight is the browser's own `:target`.

use crate::app::AppState;
use crate::catalog::{self, Entry};
use crate::web::{layout, page, require_signed_in};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use beam_accounts::Role;
use maud::{html, Markup, PreEscaped};
use std::sync::Arc;

/// Who a lesson is for.
#[derive(Clone, Copy, PartialEq)]
enum Audience {
    Everyone,
    /// People who look after a dataset: its owner, or an administrator.
    Curator,
    Administrator,
}

/// Where "Try it" goes.
#[derive(Clone, Copy)]
enum Place {
    Home,
    Table,
    Chart,
    About,
    AddData,
    Share,
    Accounts,
    Model,
}

struct Lesson {
    id: &'static str,
    title: &'static str,
    /// At most three short steps.
    steps: &'static [&'static str],
    /// The controls drawn in the small picture, and which one the arrow
    /// points at.
    picture: &'static [&'static str],
    point: usize,
    place: Place,
    audience: Audience,
}

const LESSONS: &[Lesson] = &[
    Lesson {
        id: "datasets",
        title: "Find your data",
        steps: &[
            "Every card on the Datasets page is one dataset.",
            "Choose a card to see what it holds, then open it.",
        ],
        picture: &["sales", "customers", "orders"],
        point: 0,
        place: Place::Home,
        audience: Audience::Everyone,
    },
    Lesson {
        id: "filter",
        title: "Show only the rows you need",
        steps: &[
            "Open a dataset.",
            "Under “Show rows where”, choose a column, a condition and a value.",
            "Select Apply. Select a column heading to sort by it.",
        ],
        picture: &["column", "equals", "value", "Apply"],
        point: 3,
        place: Place::Table,
        audience: Audience::Everyone,
    },
    Lesson {
        id: "chart",
        title: "Turn rows into a chart",
        steps: &[
            "Open a dataset and choose the Chart tab.",
            "Choose what to count or add up, and what to group it by.",
            "Select Draw. The figures are listed under the chart too.",
        ],
        picture: &["Total of", "amount", "by", "region", "Draw"],
        point: 4,
        place: Place::Chart,
        audience: Audience::Everyone,
    },
    Lesson {
        id: "download",
        title: "Take a copy to Excel",
        steps: &[
            "Filter and sort the table the way you want it.",
            "Select Download Excel, or Download CSV for other programs.",
            "The file holds exactly what you see: same rows, same order.",
        ],
        picture: &["Download CSV", "Download Excel"],
        point: 1,
        place: Place::Table,
        audience: Audience::Everyone,
    },
    Lesson {
        id: "excel",
        title: "Keep Excel or Power BI up to date",
        steps: &[
            "In Excel or Power BI, choose Get Data, then OData feed.",
            "Paste the address shown under “Use this data somewhere else”.",
            "Sign in with your Open Beam username and password. Refresh any time.",
        ],
        picture: &["Excel", "Get Data", "OData feed"],
        point: 2,
        place: Place::Table,
        audience: Audience::Everyone,
    },
    Lesson {
        id: "add",
        title: "Add your own file",
        steps: &[
            "Choose Add data and pick a CSV, Excel or Parquet file.",
            "Check the preview: column names and types.",
            "Save it. Only you can see it until you share it.",
        ],
        picture: &["Choose file", "Name", "Add"],
        point: 0,
        place: Place::AddData,
        audience: Audience::Everyone,
    },
    Lesson {
        id: "share",
        title: "Share a dataset with a colleague",
        steps: &[
            "Open a dataset you added and choose the Share tab.",
            "Pick a colleague and select Share.",
            "They see it at once, in the browser and in Excel.",
        ],
        picture: &["Colleague", "Share"],
        point: 1,
        place: Place::Share,
        audience: Audience::Curator,
    },
    Lesson {
        id: "describe",
        title: "Say what the data means",
        steps: &[
            "Open a dataset and choose the About tab.",
            "Write what one row is, and what each column holds.",
            "Select Save descriptions. Everyone who sees the data sees them.",
        ],
        picture: &["About this dataset", "Save descriptions"],
        point: 0,
        place: Place::About,
        audience: Audience::Curator,
    },
    Lesson {
        id: "verify",
        title: "Save an answer people can trust",
        steps: &[
            "Draw the answer on the Chart tab.",
            "Open “Save as a verified question” and name the question.",
            "It is worked out again from current data each time it is read.",
        ],
        picture: &["Chart", "Save as a verified question"],
        point: 1,
        place: Place::Chart,
        audience: Audience::Curator,
    },
    Lesson {
        id: "watch",
        title: "Be told when a number changes",
        steps: &[
            "Open a dataset's About tab and open “Add a watch”.",
            "Choose what to watch: repeats, a total, or an unusual change.",
            "It is checked every hour; the Watches page shows what needs a look.",
        ],
        picture: &["About", "Add a watch"],
        point: 1,
        place: Place::About,
        audience: Audience::Curator,
    },
    Lesson {
        id: "accounts",
        title: "Give someone an account",
        steps: &[
            "Choose Accounts.",
            "Under “Add an account”, enter a username and a first password.",
            "Choose User or Administrator, then Add account.",
        ],
        picture: &["Username", "Password", "Add account"],
        point: 2,
        place: Place::Accounts,
        audience: Audience::Administrator,
    },
    Lesson {
        id: "model",
        title: "Describe how the data fits together",
        steps: &[
            "Choose Model, then Edit the model.",
            "Say what each dataset holds and how they link.",
            "Preview the change, then apply it.",
        ],
        picture: &["Model", "Edit the model"],
        point: 1,
        place: Place::Model,
        audience: Audience::Administrator,
    },
];

/// A pointing arrow, drawn inline (pages load no images from elsewhere).
const ARROW: &str = r#"<svg class="learn-arrow" viewBox="0 0 24 24" width="28" height="28" aria-hidden="true" focusable="false"><path d="M12 3v15m0 0-6-6m6 6 6-6" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

/// The tip shown on a real page when someone arrives from "Try it". Place
/// it directly before the control it points at: it stays hidden unless the
/// address ends in `#learn-<id>`, and then the control after it is marked.
pub fn spot(id: &str, tip: &str) -> Markup {
    // "Got it" goes to a mark just above the tip, which closes it without
    // moving the reader's place on the page.
    html! {
        span id={ "learn-" (id) "-seen" } {}
        div.learn-spot id={ "learn-" (id) } role="note" {
            p.learn-tip {
                (PreEscaped(ARROW))
                span { strong { "Tip: " } (tip) }
                a.learn-done href={ "#learn-" (id) "-seen" } { "Got it" }
            }
        }
    }
}

/// A small picture of the controls a lesson is about, with the arrow over
/// the one to use.
fn picture(lesson: &Lesson) -> Markup {
    html! {
        div.learn-picture aria-hidden="true" {
            @for (index, part) in lesson.picture.iter().enumerate() {
                span class={ "learn-part" @if index == lesson.point { " pointed" } } {
                    @if index == lesson.point { (PreEscaped(ARROW)) }
                    (part)
                }
            }
        }
    }
}

/// Where "Try it" opens for this person, or why it cannot.
fn try_it(lesson: &Lesson, visible: &[Entry], account: &beam_accounts::Account) -> Result<String, &'static str> {
    let anchor = format!("#learn-{}", lesson.id);
    let any = visible.first().map(|e| e.set.name.clone());
    let curated = visible
        .iter()
        .find(|e| crate::curate::can_curate(account, e))
        .map(|e| e.set.name.clone());
    let owned = visible
        .iter()
        .find(|e| {
            e.upload.as_ref().is_some_and(|u| u.owner_id == account.id)
                || (account.role == Role::Admin && e.upload.is_some())
        })
        .map(|e| e.set.name.clone());
    Ok(match lesson.place {
        Place::Home => format!("/{anchor}"),
        Place::AddData => format!("/data/new{anchor}"),
        Place::Accounts => format!("/admin/accounts{anchor}"),
        Place::Model => format!("/model{anchor}"),
        Place::Table => format!(
            "/datasets/{}{anchor}",
            any.ok_or("There is no dataset to try it on yet.")?
        ),
        Place::Chart => {
            let name = if lesson.audience == Audience::Curator {
                curated
            } else {
                any
            };
            format!(
                "/datasets/{}/chart{anchor}",
                name.ok_or("Add a dataset first, then try it.")?
            )
        }
        Place::About => format!(
            "/datasets/{}/about{anchor}",
            curated.ok_or("Add a dataset of your own first, then try it there.")?
        ),
        Place::Share => format!(
            "/datasets/{}/share{anchor}",
            owned.ok_or("Add a file of your own first: datasets you add are the ones you can share.")?
        ),
    })
}

pub async fn page_route(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let account = &signed.session.account;
    let visible = catalog::visible(&state, account);
    let admin = account.role == Role::Admin;
    let shown = |audience: Audience| match audience {
        Audience::Everyone | Audience::Curator => true,
        Audience::Administrator => admin,
    };
    let section = |heading: &str, audience: Audience| {
        let lessons: Vec<&Lesson> = LESSONS.iter().filter(|l| l.audience == audience).collect();
        html! {
            @if shown(audience) {
                section.learn-section {
                    h2 { (heading) }
                    ol.learn-lessons {
                        @for lesson in &lessons {
                            li.learn-lesson id=(lesson.id) {
                                h3 { (lesson.title) }
                                (picture(lesson))
                                ol.learn-steps { @for step in lesson.steps { li { (step) } } }
                                @match try_it(lesson, &visible, account) {
                                    Ok(href) => a.button href=(href) { "Try it" },
                                    Err(why) => p.hint { (why) },
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    let body = html! {
        div.intro { div {
            h1 { "Learn" }
            p.lede { "Short lessons, one task each. Read the steps, then select Try it: the real page opens and an arrow shows where to start." }
        } }
        (section("The basics", Audience::Everyone))
        (section("Look after your data", Audience::Curator))
        (section("For administrators", Audience::Administrator))
        p.hint { "Longer guides, for installing and running Open Beam, are in the project's docs/guides folder." }
    };
    page(StatusCode::OK, layout("Learn", Some(&signed), body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_lesson_is_short_and_points_at_a_part_of_its_picture() {
        let mut ids = std::collections::HashSet::new();
        for lesson in LESSONS {
            assert!(ids.insert(lesson.id), "{} twice", lesson.id);
            assert!(
                (1..=3).contains(&lesson.steps.len()),
                "{}: one to three steps",
                lesson.id
            );
            assert!(lesson.point < lesson.picture.len(), "{}", lesson.id);
            for step in lesson.steps {
                assert!(
                    step.split_whitespace().count() <= 16,
                    "{}: keep each step short: {step}",
                    lesson.id
                );
            }
        }
    }
}
