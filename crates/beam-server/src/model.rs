//! The model (ontology): the business objects the datasets describe, how
//! they link, and the other words people use for them, so that people and
//! AI assistants mean the same thing by "customer" or "order".
//!
//! Written by an administrator in a TOML file named by `ontology` in
//! `beam.toml`, and trusted like it. Checked strictly when the server starts:
//! every name it uses must be declared (nothing is guessed), and it may only
//! refer to datasets from the configuration, which every account can see,
//! so it can never reveal anything about a private upload.

use crate::app::AppState;
use crate::chart::format_number;
use crate::feed;
use crate::web::{layout, page, require_signed_in};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use beam_odata::{EdmType, EntitySet};
use maud::{html, Markup};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_OBJECTS: usize = 200;
const MAX_LINKS: usize = 100;
/// A check that could not run (busy, or past the time limit) is tried again
/// after this long.
const RETRY_FAILED: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const MAX_NAME_CHARS: usize = 64;
const MAX_LABEL_CHARS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 1000;
const MAX_SYNONYMS: usize = 20;
const MAX_ONE_OF: usize = 100;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, rename = "object")]
    objects: Vec<Object>,
    #[serde(default, rename = "link")]
    links: Vec<Link>,
    #[serde(default, rename = "metric")]
    metrics: Vec<Metric>,
    #[serde(default, rename = "rule")]
    rules: Vec<Rule>,
}

/// What correct data looks like for one column of an object, in a closed
/// set modelled on W3C SHACL Core: `present` (never empty), `between`
/// (numbers within `min` and `max`), `one_of` (text from `values`) and
/// `unique` (no value twice).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    pub object: String,
    pub column: String,
    pub check: String,
    pub min: Option<Bound>,
    pub max: Option<Bound>,
    #[serde(default)]
    pub values: Vec<String>,
    pub description: Option<String>,
}

/// A limit as written in the file. Whole numbers stay whole, so a bound
/// beyond 2^53 is neither rounded when read nor compared inexactly.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Bound {
    Whole(i64),
    Fraction(f64),
}

impl Bound {
    fn literal(self) -> beam_odata::Literal {
        match self {
            Bound::Whole(v) => beam_odata::Literal::Int(v),
            Bound::Fraction(v) => beam_odata::Literal::Float(v),
        }
    }

    fn as_f64(self) -> f64 {
        match self {
            Bound::Whole(v) => v as f64,
            Bound::Fraction(v) => v,
        }
    }
}

impl std::fmt::Display for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bound::Whole(v) => write!(f, "{v}"),
            Bound::Fraction(v) => write!(f, "{v}"),
        }
    }
}

impl Rule {
    pub fn check(&self) -> beam_odata::RuleCheck {
        use beam_odata::RuleCheck::*;
        match self.check.as_str() {
            "between" => Between {
                min: self.min.map(Bound::literal),
                max: self.max.map(Bound::literal),
            },
            "one_of" => OneOf(self.values.clone()),
            "unique" => Unique,
            _ => Present,
        }
    }

    /// The rule in words.
    pub fn meaning(&self) -> String {
        let (object, column) = (&self.object, &self.column);
        // Bounds as written: rounding 0.001 for display would misstate the rule.
        match self.check.as_str() {
            "between" => match (self.min, self.max) {
                (Some(min), Some(max)) => format!("Every {object}'s {column}, when given, is between {min} and {max}"),
                (Some(min), None) => format!("Every {object}'s {column}, when given, is at least {min}"),
                (_, Some(max)) => format!("Every {object}'s {column}, when given, is at most {max}"),
                _ => String::new(),
            },
            "one_of" => format!(
                "Every {object}'s {column}, when given, is one of {}",
                self.values.join(", ")
            ),
            "unique" => format!("No two {object} rows have the same {column}"),
            _ => format!("Every {object} has a {column}"),
        }
    }
}

/// A business figure defined once, so everyone gets the same number: one
/// aggregate of one column of an object's dataset. Nothing else can be
/// written, so a metric can never become arbitrary SQL.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub name: String,
    pub object: String,
    /// `count`, `sum`, `avg`, `min` or `max`.
    pub aggregate: String,
    /// The column added up; not used by `count`.
    pub column: Option<String>,
    pub label: Option<String>,
    pub description: Option<String>,
}

impl Metric {
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    pub fn aggregate(&self) -> beam_odata::Aggregate {
        use beam_odata::Aggregate::*;
        match self.aggregate.as_str() {
            "sum" => Sum,
            "avg" => Average,
            "min" => Min,
            "max" => Max,
            _ => Count,
        }
    }

    /// How the figure is worked out, in words.
    pub fn definition(&self) -> String {
        let column = self.column.as_deref().unwrap_or("");
        match self.aggregate.as_str() {
            "sum" => format!("Sum of {column} over every {}", self.object),
            "avg" => format!("Average {column} of a {}", self.object),
            "min" => format!("Smallest {column} of any {}", self.object),
            "max" => format!("Largest {column} of any {}", self.object),
            _ => format!("Number of {} rows", self.object),
        }
    }
}

/// A kind of business object, such as a customer: the dataset whose rows
/// are those objects, and the column that tells them apart.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Object {
    pub name: String,
    pub dataset: String,
    pub key: String,
    pub label: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub synonyms: Vec<String>,
}

/// Many objects of one kind point to one of another: each row of `from`
/// names a `to` object in `column`, by that object's key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub name: String,
    pub from: String,
    pub to: String,
    pub column: String,
    pub description: Option<String>,
    /// Name of the dataset that joins the two; `{from dataset}_with_{to
    /// dataset}` when not given.
    pub joined: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Model {
    pub objects: Vec<Object>,
    pub links: Vec<Link>,
    pub metrics: Vec<Metric>,
    pub rules: Vec<Rule>,
}

impl Object {
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

impl Model {
    pub fn object(&self, name: &str) -> Option<&Object> {
        self.objects.iter().find(|o| o.name == name)
    }

    /// The dataset `link` adds: its rows of `from`, with the columns of the
    /// object each one names.
    pub fn joined(&self, link: &Link) -> String {
        link.joined.clone().unwrap_or_else(|| {
            let dataset = |name: &str| self.object(name).map_or("", |o| o.dataset.as_str()).to_string();
            format!("{}_with_{}", dataset(&link.from), dataset(&link.to))
        })
    }

    /// Adds every link's joined dataset to `engine`. Called before uploads
    /// are restored, so these names are never taken by an upload.
    pub fn add_joins(&self, engine: &beam_engine::Engine) -> Result<(), String> {
        for link in &self.links {
            let (Some(from), Some(to)) = (self.object(&link.from), self.object(&link.to)) else {
                continue; // refused by parse
            };
            let name = self.joined(link);
            engine
                .add_lookup(
                    &name,
                    &from.dataset,
                    &link.column,
                    &to.dataset,
                    &to.key,
                    &to.name.to_lowercase(),
                )
                .map_err(|e| format!("link {}: cannot make {name}: {e}", link.name))?;
        }
        Ok(())
    }
}

fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && text.len() <= MAX_NAME_CHARS
}

/// Plain text: no control characters, at most `max` characters.
fn is_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max && !text.chars().any(char::is_control)
}

/// Integers compare with integers, decimals with decimals, and so on.
fn same_kind(a: EdmType, b: EdmType) -> bool {
    use EdmType::*;
    match (a, b) {
        (Int32 | Int64, Int32 | Int64) | (Double | Decimal { .. }, Double | Decimal { .. }) => true,
        _ => std::mem::discriminant(&a) == std::mem::discriminant(&b),
    }
}

/// The datasets a model may describe: those from the configuration, which
/// every account can see. Uploads are private to their owners, so a model
/// naming one would reveal that it exists and what it holds.
pub fn configured_sets(engine: &beam_engine::Engine) -> Vec<EntitySet> {
    engine
        .datasets()
        .iter()
        .filter(|d| !d.uploaded)
        .map(crate::catalog::entity_set)
        .collect()
}

/// Reads and checks a model against the configured datasets `sets`. Every
/// problem is reported, not only the first.
pub fn parse(text: &str, sets: &[EntitySet]) -> Result<Model, Vec<String>> {
    if text.len() > MAX_FILE_BYTES {
        return Err(vec![format!("the file is larger than {} KB", MAX_FILE_BYTES / 1024)]);
    }
    let file: File = toml::from_str(text).map_err(|e| vec![e.to_string()])?;
    let mut problems = Vec::new();
    if file.objects.len() > MAX_OBJECTS || file.links.len() > MAX_LINKS {
        problems.push(format!("at most {MAX_OBJECTS} objects and {MAX_LINKS} links"));
    }
    let set = |name: &str| sets.iter().find(|s| s.name == name);

    let mut names = HashSet::new();
    let mut datasets = HashSet::new();
    let mut words: Vec<(String, &str)> = Vec::new();
    for object in &file.objects {
        let at = format!("object {}", object.name);
        if !is_name(&object.name) {
            problems.push(format!("{at}: a name is letters, digits and _, starting with a letter"));
        }
        if !names.insert(object.name.to_ascii_lowercase()) {
            problems.push(format!("{at}: declared twice"));
        }
        match set(&object.dataset) {
            None => problems.push(format!(
                "{at}: no configured dataset named {} (uploads cannot be used)",
                object.dataset
            )),
            Some(s) if s.property(&object.key).is_none() => {
                problems.push(format!("{at}: dataset {} has no column {}", object.dataset, object.key))
            }
            Some(_) => {}
        }
        if !datasets.insert(object.dataset.clone()) {
            problems.push(format!(
                "{at}: dataset {} already describes another object",
                object.dataset
            ));
        }
        if object.label.as_deref().is_some_and(|l| !is_text(l, MAX_LABEL_CHARS)) {
            problems.push(format!(
                "{at}: label must be plain text of at most {MAX_LABEL_CHARS} characters"
            ));
        }
        if object
            .description
            .as_deref()
            .is_some_and(|d| !is_text(d, MAX_DESCRIPTION_CHARS))
        {
            problems.push(format!(
                "{at}: description must be plain text of at most {MAX_DESCRIPTION_CHARS} characters"
            ));
        }
        if object.synonyms.len() > MAX_SYNONYMS || object.synonyms.iter().any(|s| !is_text(s, MAX_LABEL_CHARS)) {
            problems.push(format!(
                "{at}: at most {MAX_SYNONYMS} synonyms, each plain text of at most {MAX_LABEL_CHARS} characters"
            ));
        }
        for word in std::iter::once(&object.name)
            .chain(object.label.as_ref())
            .chain(&object.synonyms)
        {
            words.push((word.trim().to_lowercase(), &object.name));
        }
    }
    // A word may mean only one object, or an assistant cannot tell which.
    for (index, (word, owner)) in words.iter().enumerate() {
        if let Some((_, other)) = words[..index].iter().find(|(w, o)| w == word && o != owner) {
            problems.push(format!("\"{word}\" names both {other} and {owner}"));
        }
    }

    let object = |name: &str| file.objects.iter().find(|o| o.name == name);
    let mut link_names = HashSet::new();
    let mut joined_names: HashSet<String> = sets.iter().map(|s| s.name.to_ascii_lowercase()).collect();
    for link in &file.links {
        let at = format!("link {}", link.name);
        if !is_name(&link.name) {
            problems.push(format!("{at}: a name is letters, digits and _, starting with a letter"));
        }
        if !link_names.insert(link.name.to_ascii_lowercase()) {
            problems.push(format!("{at}: declared twice"));
        }
        if file.links.iter().take_while(|l| !std::ptr::eq(*l, link)).any(|l| {
            (l.from.as_str(), l.to.as_str(), l.column.as_str())
                == (link.from.as_str(), link.to.as_str(), link.column.as_str())
        }) {
            problems.push(format!("{at}: repeats another link"));
        }
        if link
            .description
            .as_deref()
            .is_some_and(|d| !is_text(d, MAX_DESCRIPTION_CHARS))
        {
            problems.push(format!(
                "{at}: description must be plain text of at most {MAX_DESCRIPTION_CHARS} characters"
            ));
        }
        let (Some(from), Some(to)) = (object(&link.from), object(&link.to)) else {
            problems.push(format!("{at}: from and to must be declared objects"));
            continue;
        };
        if from.name == to.name {
            problems.push(format!("{at}: an object cannot link to itself"));
        }
        let joined = link
            .joined
            .clone()
            .unwrap_or_else(|| format!("{}_with_{}", from.dataset, to.dataset));
        if !is_name(&joined) {
            problems.push(format!(
                "{at}: joined must be letters, digits and _, starting with a letter"
            ));
        } else if !joined_names.insert(joined.to_ascii_lowercase()) {
            problems.push(format!(
                "{at}: the dataset {joined} already exists; give this link its own joined name"
            ));
        }
        let (Some(from_set), Some(to_set)) = (set(&from.dataset), set(&to.dataset)) else {
            continue; // reported with the object
        };
        match (from_set.property(&link.column), to_set.property(&to.key)) {
            (None, _) => problems.push(format!("{at}: dataset {} has no column {}", from.dataset, link.column)),
            (Some(column), Some(key)) if !same_kind(column.edm, key.edm) => problems.push(format!(
                "{at}: {} holds different values from {}'s key {}",
                link.column, to.name, to.key
            )),
            _ => {}
        }
    }
    let mut metric_names = HashSet::new();
    if file.metrics.len() > MAX_LINKS {
        problems.push(format!("at most {MAX_LINKS} metrics"));
    }
    for metric in &file.metrics {
        let at = format!("metric {}", metric.name);
        if !is_name(&metric.name) {
            problems.push(format!("{at}: a name is letters, digits and _, starting with a letter"));
        }
        if !metric_names.insert(metric.name.to_ascii_lowercase()) {
            problems.push(format!("{at}: declared twice"));
        }
        if metric.label.as_deref().is_some_and(|l| !is_text(l, MAX_LABEL_CHARS))
            || metric
                .description
                .as_deref()
                .is_some_and(|d| !is_text(d, MAX_DESCRIPTION_CHARS))
        {
            problems.push(format!("{at}: label and description must be plain text"));
        }
        if !["count", "sum", "avg", "min", "max"].contains(&metric.aggregate.as_str()) {
            problems.push(format!("{at}: aggregate is one of count, sum, avg, min, max"));
            continue;
        }
        let Some(dataset) = object(&metric.object).and_then(|o| set(&o.dataset)) else {
            problems.push(format!("{at}: object must be a declared object"));
            continue;
        };
        match (metric.aggregate.as_str(), metric.column.as_deref()) {
            ("count", Some(_)) => problems.push(format!("{at}: count takes no column")),
            ("count", None) => {}
            (_, None) => problems.push(format!("{at}: {} needs a column", metric.aggregate)),
            (_, Some(column)) => match dataset.property(column) {
                None => problems.push(format!("{at}: dataset {} has no column {column}", dataset.name)),
                Some(p)
                    if !matches!(
                        p.edm,
                        EdmType::Int32 | EdmType::Int64 | EdmType::Double | EdmType::Decimal { .. }
                    ) =>
                {
                    problems.push(format!("{at}: {column} does not hold numbers"))
                }
                Some(_) => {}
            },
        }
    }
    let mut rule_names = HashSet::new();
    if file.rules.len() > MAX_LINKS {
        problems.push(format!("at most {MAX_LINKS} rules"));
    }
    for rule in &file.rules {
        let at = format!("rule {}", rule.name);
        if !is_name(&rule.name) {
            problems.push(format!("{at}: a name is letters, digits and _, starting with a letter"));
        }
        if !rule_names.insert(rule.name.to_ascii_lowercase()) {
            problems.push(format!("{at}: declared twice"));
        }
        if rule
            .description
            .as_deref()
            .is_some_and(|d| !is_text(d, MAX_DESCRIPTION_CHARS))
        {
            problems.push(format!("{at}: description must be plain text"));
        }
        let Some(dataset) = object(&rule.object).and_then(|o| set(&o.dataset)) else {
            problems.push(format!("{at}: object must be a declared object"));
            continue;
        };
        let Some(property) = dataset.property(&rule.column) else {
            problems.push(format!("{at}: dataset {} has no column {}", dataset.name, rule.column));
            continue;
        };
        let numeric = matches!(
            property.edm,
            EdmType::Int32 | EdmType::Int64 | EdmType::Double | EdmType::Decimal { .. }
        );
        let extra_bounds = rule.min.is_some() || rule.max.is_some();
        let extra_values = !rule.values.is_empty();
        let fault = match rule.check.as_str() {
            "present" | "unique" if extra_bounds || extra_values => Some("takes no min, max or values"),
            "present" | "unique" => None,
            "between" if !numeric => Some("between needs a number column"),
            "between" if extra_values => Some("between takes no values"),
            "between" if !extra_bounds => Some("between needs min, max or both"),
            "between" if rule.min.into_iter().chain(rule.max).any(|v| !v.as_f64().is_finite()) => {
                Some("min and max must be ordinary numbers")
            }
            "between"
                if match (rule.min, rule.max) {
                    (Some(Bound::Whole(a)), Some(Bound::Whole(b))) => a > b,
                    (Some(a), Some(b)) => a.as_f64() > b.as_f64(),
                    _ => false,
                } =>
            {
                Some("min must not be above max")
            }
            "between" => None,
            "one_of" if property.edm != EdmType::String => Some("one_of needs a text column"),
            "one_of" if extra_bounds => Some("one_of takes no min or max"),
            "one_of" if rule.values.is_empty() || rule.values.len() > MAX_ONE_OF => {
                Some("one_of needs between 1 and 100 values")
            }
            "one_of" if rule.values.iter().any(|v| !is_text(v, MAX_LABEL_CHARS)) => {
                Some("one_of values must be plain text")
            }
            "one_of" if rule.values.iter().collect::<HashSet<_>>().len() != rule.values.len() => {
                Some("one_of lists a value twice")
            }
            "one_of" => None,
            _ => Some("check is one of present, between, one_of, unique"),
        };
        if let Some(fault) = fault {
            problems.push(format!("{at}: {fault}"));
        }
    }
    if problems.is_empty() {
        Ok(Model {
            objects: file.objects,
            links: file.links,
            metrics: file.metrics,
            rules: file.rules,
        })
    } else {
        Err(problems)
    }
}

/// What the data says about the model: for each object whether its key
/// tells the rows apart, and for each link how many rows point nowhere.
#[derive(Debug, Clone, Default)]
pub struct Checks {
    /// `(rows, distinct keys, empty keys)` per object, in order.
    pub keys: Vec<Option<[f64; 3]>>,
    /// `(linked, pointing nowhere)` per link, in order.
    pub links: Vec<Option<[f64; 2]>>,
    /// The current value of each metric, in order.
    pub metrics: Vec<Option<[f64; 1]>>,
    /// `(rows, breaking)` per rule, in order.
    pub rules: Vec<Option<[f64; 2]>>,
}

async fn figures<const N: usize>(state: &Arc<AppState>, planned: Option<beam_odata::Summary>) -> Option<[f64; N]> {
    let planned = planned?;
    let rows = feed::run(state, planned.sql, &planned.params, N).await.ok()?;
    let row = rows.first()?;
    let mut out = [0.0; N];
    for (slot, value) in out.iter_mut().zip(row) {
        *slot = value.as_f64()?;
    }
    Some(out)
}

impl Checks {
    fn complete(&self) -> bool {
        self.keys.iter().all(Option::is_some)
            && self.links.iter().all(Option::is_some)
            && self.metrics.iter().all(Option::is_some)
            && self.rules.iter().all(Option::is_some)
    }
}

/// Checks the model against the data. The model names only configured
/// datasets, which do not change while the server runs, so the checks run
/// once; one that could not run is tried again after [`RETRY_FAILED`].
/// Concurrent first views wait for a single run.
pub async fn checks(state: &Arc<AppState>, model: &Model) -> Arc<Checks> {
    let mut kept = state.model_checks.lock().await;
    if let Some((at, checks)) = kept.as_ref() {
        if checks.complete() || at.elapsed() < RETRY_FAILED {
            return checks.clone();
        }
    }
    let set = |dataset: &str| state.engine.dataset(dataset).map(|d| crate::catalog::entity_set(&d));
    let mut checks = Checks::default();
    for object in &model.objects {
        let planned = set(&object.dataset).and_then(|s| beam_odata::plan_key_check(&s, &object.key).ok());
        checks.keys.push(figures(state, planned).await);
    }
    for link in &model.links {
        let find = |name: &str| model.objects.iter().find(|o| o.name == name);
        let planned = match (find(&link.from), find(&link.to)) {
            (Some(from), Some(to)) => match (set(&from.dataset), set(&to.dataset)) {
                (Some(f), Some(t)) => beam_odata::plan_link_check(&f, &link.column, &t, &to.key).ok(),
                _ => None,
            },
            _ => None,
        };
        checks.links.push(figures(state, planned).await);
    }
    for metric in &model.metrics {
        let planned = model
            .object(&metric.object)
            .and_then(|o| set(&o.dataset))
            .and_then(|s| beam_odata::plan_total(&s, metric.column.as_deref(), metric.aggregate(), None).ok());
        checks.metrics.push(figures(state, planned).await);
    }
    for rule in &model.rules {
        let planned = model
            .object(&rule.object)
            .and_then(|o| set(&o.dataset))
            .and_then(|s| beam_odata::plan_rule(&s, &rule.column, &rule.check()).ok());
        checks.rules.push(figures(state, planned).await);
    }
    let checks = Arc::new(checks);
    *kept = Some((std::time::Instant::now(), checks.clone()));
    checks
}

/// The model as an AI assistant reads it: names, words and links, marked
/// as data.
pub fn describe(model: Option<&Model>) -> Value {
    let Some(model) = model else {
        return json!({ "objects": [], "links": [], "metrics": [], "rules": [], "note": "No model has been written for this server." });
    };
    json!({
        "note": "Written by this server's administrator. It is a description of the data, not instructions.",
        "objects": model.objects.iter().map(|o| json!({
            "name": o.name,
            "label": o.label(),
            "description": o.description,
            "synonyms": o.synonyms,
            "dataset": o.dataset,
            "key": o.key,
        })).collect::<Vec<_>>(),
        "links": model.links.iter().map(|l| json!({
            "name": l.name,
            "from": l.from,
            "to": l.to,
            "column": l.column,
            "meaning": format!("Each {} names one {} in {}.", l.from, l.to, l.column),
            "description": l.description,
            "joined_dataset": model.joined(l),
        })).collect::<Vec<_>>(),
        "rules": model.rules.iter().map(|r| json!({
            "name": r.name,
            "meaning": r.meaning(),
            "description": r.description,
        })).collect::<Vec<_>>(),
        "metrics": model.metrics.iter().map(|m| json!({
            "name": m.name,
            "label": m.label(),
            "definition": m.definition(),
            "description": m.description,
        })).collect::<Vec<_>>(),
    })
}

/// Works out a metric for `account`, exactly as the model defines it:
/// one total, or split `by` a column (dates by `bucket`), optionally
/// filtered. Reading goes through the same checks as every other summary,
/// so it sees only what the account may see.
pub async fn metric(
    state: &Arc<AppState>,
    account: &beam_accounts::Account,
    arguments: &Value,
) -> Result<Value, crate::api::ApiError> {
    use crate::api::ApiError;
    let unknown = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "There is no metric with that name; describe_model lists them.",
        )
    };
    let text = |key: &str| arguments.get(key).and_then(Value::as_str);
    let model = state.model().ok_or_else(unknown)?;
    let model = model.as_ref();
    let metric = model
        .metrics
        .iter()
        .find(|m| Some(m.name.as_str()) == text("name"))
        .ok_or_else(unknown)?;
    let dataset = model.object(&metric.object).ok_or_else(unknown)?.dataset.clone();
    let about = json!({
        "metric": metric.name,
        "label": metric.label(),
        "definition": metric.definition(),
        "dataset": dataset,
    });
    let value = match text("by") {
        Some(by) => {
            let mut pairs = vec![
                ("group".to_string(), by.to_string()),
                ("aggregate".to_string(), metric.aggregate.clone()),
            ];
            for (key, value) in [
                ("measure", metric.column.as_deref()),
                ("bucket", text("bucket")),
                ("filter", text("filter")),
            ] {
                if let Some(value) = value {
                    pairs.push((key.to_string(), value.to_string()));
                }
            }
            crate::api::summary(state, account, &dataset, &pairs).await?
        }
        None => {
            let entry = crate::catalog::find(state, account, &dataset).ok_or_else(unknown)?;
            let planned =
                beam_odata::plan_total(&entry.set, metric.column.as_deref(), metric.aggregate(), text("filter"))
                    .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
            let rows = feed::run(state, planned.sql, &planned.params, 1).await?;
            json!({ "value": rows.first().map_or(Value::Null, |r| r[0].clone()) })
        }
    };
    let mut answer = about;
    if let (Some(answer), Some(value)) = (answer.as_object_mut(), value.as_object()) {
        answer.extend(value.clone());
    }
    Ok(answer)
}

/// "1 row", "2 rows", in the page's number format.
fn counted(n: f64, one: &str, many: &str) -> String {
    format!("{} {}", format_number(n), if n == 1.0 { one } else { many })
}

fn key_status(figures: Option<[f64; 3]>) -> Markup {
    match figures {
        None => html! { span.status { "Could not be checked" } },
        Some([1.0, 1.0, 0.0]) => html! { span.status.ok { "Its 1 row has a key" } },
        Some([rows, distinct, empty]) if empty == 0.0 && distinct == rows => {
            html! { span.status.ok { "Each of " (format_number(rows)) " rows has its own key" } }
        }
        Some([rows, distinct, empty]) => html! {
            span.status.alert { (format_number(rows - distinct)) " repeated, " (counted(empty, "empty key", "empty keys")) }
        },
    }
}

fn rule_status(figures: Option<[f64; 2]>) -> Markup {
    match figures {
        None => html! { span.status { "Could not be checked" } },
        Some([1.0, 0.0]) => html! { span.status.ok { "Its 1 row keeps it" } },
        Some([rows, 0.0]) => html! { span.status.ok { "All " (format_number(rows)) " rows keep it" } },
        Some([_, 1.0]) => html! { span.status.alert { "1 row breaks it" } },
        Some([_, breaking]) => html! { span.status.alert { (format_number(breaking)) " rows break it" } },
    }
}

fn link_status(figures: Option<[f64; 2]>) -> Markup {
    match figures {
        None => html! { span.status { "Could not be checked" } },
        Some([1.0, 0.0]) => html! { span.status.ok { "It points to one" } },
        Some([linked, 0.0]) => html! { span.status.ok { "All " (format_number(linked)) " point to one" } },
        Some([_, orphans]) => html! { span.status.alert { (counted(orphans, "points", "point")) " to nothing" } },
    }
}

pub async fn page_route(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let body = match state.model() {
        None => html! {
            h1 { "Model" }
            p.empty { "No model has been written yet. An administrator can describe the business objects the datasets hold, and how they link, in a file named by ontology in beam.toml; see the guide." }
        },
        Some(model) => {
            let checks = checks(&state, &model).await;
            html! {
                div.intro { div {
                    h1 { "Model" }
                    p.lede { "The business objects the datasets hold, the other names people use for them, and how they link. Each is checked against the data." }
                } }
                section.panel {
                    h2 { "Objects" }
                    div.tablewrap { table {
                        thead { tr { th { "Object" } th { "Also called" } th { "Dataset and key" } th { "Check" } } }
                        tbody {
                            @for (index, object) in model.objects.iter().enumerate() {
                                tr {
                                    th scope="row" { (object.label())
                                        @if let Some(text) = &object.description { p.hint { (text) } } }
                                    td { (object.synonyms.join(", ")) }
                                    td { a href={ "/datasets/" (object.dataset) } { (object.dataset) } " by " code { (object.key) } }
                                    td { (key_status(checks.keys.get(index).copied().flatten())) }
                                }
                            }
                        }
                    } }
                }
                @if !model.metrics.is_empty() {
                    section.panel {
                        h2 { "Metrics" }
                        div.tablewrap { table {
                            thead { tr { th { "Metric" } th { "Definition" } th.num { "Current value" } } }
                            tbody {
                                @for (index, metric) in model.metrics.iter().enumerate() {
                                    tr {
                                        th scope="row" { (metric.label())
                                            @if let Some(text) = &metric.description { p.hint { (text) } } }
                                        td { (metric.definition()) }
                                        td.num { (checks.metrics.get(index).copied().flatten().map_or("Could not be worked out".to_string(), |[v]| format_number(v))) }
                                    }
                                }
                            }
                        } }
                    }
                }
                @if !model.rules.is_empty() {
                    section.panel {
                        h2 { "Rules" }
                        div.tablewrap { table {
                            thead { tr { th { "Rule" } th { "Meaning" } th { "Check" } } }
                            tbody {
                                @for (index, rule) in model.rules.iter().enumerate() {
                                    tr {
                                        th scope="row" { (rule.name)
                                            @if let Some(text) = &rule.description { p.hint { (text) } } }
                                        td { (rule.meaning()) }
                                        td { (rule_status(checks.rules.get(index).copied().flatten())) }
                                    }
                                }
                            }
                        } }
                    }
                }
                section.panel {
                    h2 { "Links" }
                    @if model.links.is_empty() { p.empty { "No links declared." } } @else {
                        div.tablewrap { table {
                            thead { tr { th { "Link" } th { "Meaning" } th { "Joined dataset" } th { "Check" } } }
                            tbody {
                                @for (index, link) in model.links.iter().enumerate() {
                                    tr {
                                        th scope="row" { (link.name)
                                            @if let Some(text) = &link.description { p.hint { (text) } } }
                                        td { "Each " (link.from) " names one " (link.to) " in " code { (link.column) } }
                                        td { @let joined = model.joined(link); a href={ "/datasets/" (joined) } { (joined) } }
                                        td { (link_status(checks.links.get(index).copied().flatten())) }
                                    }
                                }
                            }
                        } }
                    }
                }
            }
        }
    };
    let can_edit = state.model_path.is_some() && signed.session.account.role == beam_accounts::Role::Admin;
    let body = html! {
        (body)
        @if can_edit { p { a.button.quiet href="/model/edit" { "Edit the model" } } }
    };
    page(StatusCode::OK, layout("Model", Some(&signed), body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_odata::Property;

    fn set(name: &str, columns: &[(&str, EdmType)]) -> EntitySet {
        EntitySet {
            name: name.into(),
            key: columns[0].0.into(),
            properties: columns
                .iter()
                .map(|(n, edm)| Property {
                    name: (*n).into(),
                    edm: *edm,
                })
                .collect(),
        }
    }

    fn sets() -> Vec<EntitySet> {
        vec![
            set(
                "customers",
                &[("customer_id", EdmType::Int64), ("name", EdmType::String)],
            ),
            set(
                "orders",
                &[
                    ("order_id", EdmType::Int64),
                    ("customer_id", EdmType::Int32),
                    ("note", EdmType::String),
                ],
            ),
        ]
    }

    const GOOD: &str = r#"
        [[object]]
        name = "Customer"
        dataset = "customers"
        key = "customer_id"
        synonyms = ["client", "account"]
        description = "A person or company that buys from us."

        [[object]]
        name = "Order"
        dataset = "orders"
        key = "order_id"

        [[link]]
        name = "placed_by"
        from = "Order"
        to = "Customer"
        column = "customer_id"
        description = "The customer who placed the order."
    "#;

    #[test]
    fn a_sound_model_is_accepted() {
        let model = parse(GOOD, &sets()).unwrap();
        assert_eq!((model.objects.len(), model.links.len()), (2, 1));
        assert_eq!(model.objects[1].label(), "Order");
        let described = describe(Some(&model));
        assert_eq!(
            described["links"][0]["meaning"],
            "Each Order names one Customer in customer_id."
        );
    }

    #[test]
    fn every_undeclared_or_ambiguous_name_is_reported() {
        let bad = r#"
            [[object]]
            name = "Customer"
            dataset = "customers"
            key = "nope"
            synonyms = ["order"]

            [[object]]
            name = "Order"
            dataset = "uploads_are_not_allowed"
            key = "order_id"

            [[link]]
            name = "placed_by"
            from = "Order"
            to = "Ghost"
            column = "customer_id"
        "#;
        let problems = parse(bad, &sets()).unwrap_err();
        let all = problems.join("\n");
        for expected in [
            "dataset customers has no column nope",
            "no configured dataset named uploads_are_not_allowed",
            "\"order\" names both Customer and Order",
            "from and to must be declared objects",
        ] {
            assert!(all.contains(expected), "missing {expected:?} in\n{all}");
        }
        // Unknown settings are refused rather than ignored.
        assert!(parse(
            "[[object]]\nname = \"A\"\ndataset = \"customers\"\nkey = \"customer_id\"\ncolour = \"red\"\n",
            &sets()
        )
        .is_err());
    }

    #[test]
    fn a_link_must_join_like_values_and_text_must_be_plain() {
        let mismatched = GOOD.replace("column = \"customer_id\"", "column = \"note\"");
        let problems = parse(&mismatched, &sets()).unwrap_err().join("\n");
        assert!(
            problems.contains("note holds different values from Customer's key"),
            "{problems}"
        );
        let control = GOOD.replace("A person or company", "A person\u{0007}");
        assert!(parse(&control, &sets()).is_err());
        let clash = GOOD.replace(
            "description = \"The customer who placed the order.\"",
            "joined = \"customers\"",
        );
        assert!(parse(&clash, &sets())
            .unwrap_err()
            .join("\n")
            .contains("the dataset customers already exists"));
        let to_itself = GOOD
            .replace("to = \"Customer\"", "to = \"Order\"")
            .replace("column = \"customer_id\"", "column = \"order_id\"");
        assert!(parse(&to_itself, &sets())
            .unwrap_err()
            .join("\n")
            .contains("cannot link to itself"));
        for (metric, expected) in [
            ("aggregate = \"sum\"\ncolumn = \"name\"", "name does not hold numbers"),
            (
                "aggregate = \"count\"\ncolumn = \"customer_id\"",
                "count takes no column",
            ),
            ("aggregate = \"sum\"", "sum needs a column"),
            (
                "aggregate = \"median\"\ncolumn = \"customer_id\"",
                "aggregate is one of",
            ),
            ("aggregate = \"sum\"\ncolumn = \"missing\"", "has no column missing"),
        ] {
            let text = format!("{GOOD}\n[[metric]]\nname = \"m\"\nobject = \"Customer\"\n{metric}\n");
            let problems = parse(&text, &sets()).unwrap_err().join("\n");
            assert!(problems.contains(expected), "{metric}: {problems}");
        }
        // Bounds read and shown exactly as written; whole numbers stay whole.
        let tiny = format!("{GOOD}\n[[rule]]\nname = \"r\"\nobject = \"Customer\"\ncolumn = \"customer_id\"\ncheck = \"between\"\nmin = 0.001\nmax = 9007199254740993\n");
        let model = parse(&tiny, &sets()).unwrap();
        assert_eq!(model.rules[0].max, Some(Bound::Whole(9007199254740993)));
        assert_eq!(
            model.rules[0].meaning(),
            "Every Customer's customer_id, when given, is between 0.001 and 9007199254740993"
        );
        let unique = GOOD.to_string()
            + "\n[[rule]]\nname = \"u\"\nobject = \"Order\"\ncolumn = \"order_id\"\ncheck = \"unique\"\n";
        assert_eq!(
            parse(&unique, &sets()).unwrap().rules[0].meaning(),
            "No two Order rows have the same order_id"
        );
        // Anything beyond the closed grammar, such as an expression, is refused.
        let expression =
            format!("{GOOD}\n[[metric]]\nname = \"m\"\nobject = \"Customer\"\naggregate = \"count\"\nsql = \"1/0\"\n");
        assert!(parse(&expression, &sets()).is_err());
        for (rule, expected) in [
            (
                "check = \"between\"\ncolumn = \"name\"\nmin = 1.0",
                "between needs a number column",
            ),
            (
                "check = \"between\"\ncolumn = \"customer_id\"",
                "between needs min, max or both",
            ),
            (
                "check = \"between\"\ncolumn = \"customer_id\"\nmin = 5.0\nmax = 1.0",
                "min must not be above max",
            ),
            (
                "check = \"between\"\ncolumn = \"customer_id\"\nmin = nan",
                "ordinary numbers",
            ),
            (
                "check = \"one_of\"\ncolumn = \"customer_id\"\nvalues = [\"a\"]",
                "one_of needs a text column",
            ),
            (
                "check = \"one_of\"\ncolumn = \"name\"",
                "one_of needs between 1 and 100 values",
            ),
            (
                "check = \"present\"\ncolumn = \"name\"\nmin = 1.0",
                "takes no min, max or values",
            ),
            ("check = \"pattern\"\ncolumn = \"name\"", "check is one of"),
            (
                "check = \"one_of\"\ncolumn = \"name\"\nvalues = [\"a\", \"a\"]",
                "one_of lists a value twice",
            ),
            ("check = \"present\"\ncolumn = \"nope\"", "has no column nope"),
        ] {
            let text = format!("{GOOD}\n[[rule]]\nname = \"r\"\nobject = \"Customer\"\n{rule}\n");
            let problems = parse(&text, &sets()).unwrap_err().join("\n");
            assert!(problems.contains(expected), "{rule}: {problems}");
        }
        let repeated = format!(
            "{GOOD}\n[[link]]\nname = \"again\"\nfrom = \"Order\"\nto = \"Customer\"\ncolumn = \"customer_id\"\n"
        );
        assert!(parse(&repeated, &sets())
            .unwrap_err()
            .join("\n")
            .contains("link again: repeats another link"));
        let twice = format!("{GOOD}\n[[object]]\nname = \"customer\"\ndataset = \"orders\"\nkey = \"order_id\"\n");
        let problems = parse(&twice, &sets()).unwrap_err().join("\n");
        assert!(
            problems.contains("declared twice") && problems.contains("already describes another object"),
            "{problems}"
        );
    }

    #[test]
    fn statuses_count_one_in_the_singular() {
        let text = |m: Markup| m.into_string();
        assert!(text(key_status(Some([1.0, 1.0, 0.0]))).contains("Its 1 row has a key"));
        assert!(text(key_status(Some([3.0, 2.0, 1.0]))).contains("1 repeated, 1 empty key<"));
        assert!(text(key_status(Some([3.0, 1.0, 2.0]))).contains("2 empty keys"));
        assert!(text(rule_status(Some([1.0, 0.0]))).contains("Its 1 row keeps it"));
        assert!(text(link_status(Some([1.0, 0.0]))).contains("It points to one"));
        assert!(text(link_status(Some([5.0, 1.0]))).contains("1 points to nothing"));
        assert!(text(link_status(Some([5.0, 2.0]))).contains("2 point to nothing"));
    }
}
