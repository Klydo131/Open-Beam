//! OData v4 support for Open Beam.
//!
//! This crate is pure logic with no I/O: it parses OData query options,
//! plans them into parameterised SQL (DuckDB dialect), and renders the service
//! document and CSDL metadata. Client-supplied values never reach the SQL
//! text; they are returned as bind parameters, and every identifier is checked
//! against the entity set's schema before it is quoted.

mod filter;
mod metadata;
mod query;
mod summary;

pub use filter::{Expr, FilterError};
pub use metadata::{metadata_xml, service_document};
pub use query::{plan, Plan, QueryError};
pub use summary::{plan_summary, Aggregate, Bucket, Summary};

/// The subset of EDM primitive types Open Beam exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdmType {
    Boolean,
    Int32,
    Int64,
    Double,
    Decimal { precision: u8, scale: u8 },
    String,
    Date,
    DateTimeOffset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    pub name: String,
    pub edm: EdmType,
}

/// One queryable collection. `name` and every property name must be simple
/// identifiers (`[A-Za-z_][A-Za-z0-9_]*`); the engine guarantees this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitySet {
    pub name: String,
    pub key: String,
    pub properties: Vec<Property>,
}

impl EntitySet {
    pub fn property(&self, name: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.name == name)
    }
}

/// A value bound to a `?` placeholder in planned SQL.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
