use crate::{ContentPart, Error, ErrorKind, Item, Result};
use serde::{Deserialize, Serialize};

/// A host-owned input identity. Persist this envelope before handing it to a
/// running agent; the user message's transcript id is its admission receipt.
/// The receipt survives compaction because compaction never rewrites history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserInput {
    pub id: String,
    pub parts: Vec<ContentPart>,
}

impl UserInput {
    pub fn new(parts: Vec<ContentPart>) -> Self {
        Self {
            id: format!("input-{:032x}", rand::random::<u128>()),
            parts,
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::new(vec![ContentPart::InputText { text: text.into() }])
    }

    pub(crate) fn into_item(self) -> Result<Item> {
        if self.id.is_empty() || self.id.len() > 128 {
            return Err(Error::new(
                ErrorKind::Configuration,
                "input id must contain 1–128 bytes",
            ));
        }
        let mut item = Item::user_parts(self.parts);
        item.validate()?;
        let Item::Message { id, .. } = &mut item else {
            unreachable!()
        };
        *id = Some(self.id);
        Ok(item)
    }
}
