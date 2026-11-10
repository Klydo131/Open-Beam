//! Which datasets a person may see.
//!
//! Every page and every feed request finds datasets through this module, so
//! there is one place where visibility is decided. Configured datasets are
//! visible to every signed-in account. An uploaded dataset is visible to its
//! owner, to the accounts it is shared with, and to administrators.

use crate::app::AppState;
use beam_accounts::{Account, Upload};
use beam_engine::{ColumnType, Dataset};
use beam_odata::{EdmType, EntitySet, Property};

pub struct Entry {
    pub set: EntitySet,
    /// Present for datasets someone uploaded; `None` for configured ones.
    pub upload: Option<Upload>,
}

pub fn entity_set(dataset: &Dataset) -> EntitySet {
    EntitySet {
        name: dataset.name.clone(),
        key: dataset.key.clone(),
        properties: dataset
            .columns
            .iter()
            .map(|c| Property {
                name: c.name.clone(),
                edm: match c.ty {
                    ColumnType::Boolean => EdmType::Boolean,
                    ColumnType::Int32 => EdmType::Int32,
                    ColumnType::Int64 => EdmType::Int64,
                    ColumnType::Float64 => EdmType::Double,
                    ColumnType::Decimal { precision, scale } => EdmType::Decimal { precision, scale },
                    ColumnType::Text => EdmType::String,
                    ColumnType::Date => EdmType::Date,
                    ColumnType::Timestamp => EdmType::DateTimeOffset,
                },
            })
            .collect(),
    }
}

/// The datasets `account` may read, in the order they were added.
///
/// Fails closed: if the ownership records cannot be read, uploaded datasets
/// are left out rather than shown to everyone.
pub fn visible(state: &AppState, account: &Account) -> Vec<Entry> {
    let uploads = state.accounts.uploads().unwrap_or_default();
    state
        .engine
        .datasets()
        .iter()
        .filter_map(|dataset| {
            if !dataset.uploaded {
                return Some(Entry {
                    set: entity_set(dataset),
                    upload: None,
                });
            }
            let upload = uploads.iter().find(|u| u.name == dataset.name)?;
            state
                .accounts
                .can_read(account, upload)
                .unwrap_or(false)
                .then(|| Entry {
                    set: entity_set(dataset),
                    upload: Some(upload.clone()),
                })
        })
        .collect()
}

/// One dataset by name, if `account` may read it. Unknown and forbidden
/// datasets look the same, so names of others' private datasets do not leak.
pub fn find(state: &AppState, account: &Account, name: &str) -> Option<Entry> {
    visible(state, account).into_iter().find(|entry| entry.set.name == name)
}
