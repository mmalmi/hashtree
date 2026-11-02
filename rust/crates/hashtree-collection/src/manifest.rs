use std::sync::Arc;

use hashtree_core::{Cid, HashTree, HashTreeConfig, Store};
use serde::{Deserialize, Serialize};

use crate::helpers::find_manifest_cid;
use crate::{get_schema_version, CollectionDefinition, CollectionError, CollectionPublishedSchema};

pub const COLLECTION_MANIFEST_METADATA_FILE: &str = ".collection-manifest.json";

#[derive(Debug, Clone, PartialEq)]
pub struct CollectionManifestMetadata {
    version: u32,
    schema_version: u32,
    published_schema: Option<CollectionPublishedSchema>,
}

impl CollectionManifestMetadata {
    pub fn new(schema_version: u32) -> Self {
        Self {
            version: 1,
            schema_version,
            published_schema: None,
        }
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn with_published_schema(mut self, published_schema: CollectionPublishedSchema) -> Self {
        self.published_schema = Some(published_schema);
        self
    }

    pub fn published_schema(&self) -> Option<&CollectionPublishedSchema> {
        self.published_schema.as_ref()
    }

    pub(crate) fn from_definition<T>(definition: &CollectionDefinition<T>) -> Option<Self> {
        let schema_version = get_schema_version(definition);
        let published_schema = definition.published_schema().cloned();
        if schema_version == 1 && published_schema.is_none() {
            return None;
        }

        Some(Self {
            version: 1,
            schema_version,
            published_schema,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SerializedCollectionManifestMetadata {
    version: u32,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_schema: Option<SerializedCollectionPublishedSchema>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SerializedCollectionPublishedSchema {
    #[serde(skip_serializing_if = "Option::is_none")]
    item_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    projection_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_ref: Option<SerializedCid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SerializedCid {
    hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

impl From<&CollectionManifestMetadata> for SerializedCollectionManifestMetadata {
    fn from(value: &CollectionManifestMetadata) -> Self {
        Self {
            version: value.version,
            schema_version: value.schema_version,
            published_schema: value.published_schema.as_ref().map(Into::into),
        }
    }
}

impl From<&CollectionPublishedSchema> for SerializedCollectionPublishedSchema {
    fn from(value: &CollectionPublishedSchema) -> Self {
        Self {
            item_format: value.item_format().map(ToOwned::to_owned),
            projection_format: value.projection_format().map(ToOwned::to_owned),
            schema_ref: value.schema_ref().map(Into::into),
        }
    }
}

impl From<&Cid> for SerializedCid {
    fn from(value: &Cid) -> Self {
        Self {
            hash: hex::encode(value.hash),
            key: value.key.map(hex::encode),
        }
    }
}

impl TryFrom<SerializedCollectionManifestMetadata> for CollectionManifestMetadata {
    type Error = CollectionError;

    fn try_from(value: SerializedCollectionManifestMetadata) -> Result<Self, Self::Error> {
        Ok(Self {
            version: value.version,
            schema_version: value.schema_version,
            published_schema: value.published_schema.map(TryInto::try_into).transpose()?,
        })
    }
}

impl TryFrom<SerializedCollectionPublishedSchema> for CollectionPublishedSchema {
    type Error = CollectionError;

    fn try_from(value: SerializedCollectionPublishedSchema) -> Result<Self, Self::Error> {
        let mut schema = CollectionPublishedSchema::new();
        if let Some(item_format) = value.item_format {
            schema = schema.with_item_format(item_format);
        }
        if let Some(projection_format) = value.projection_format {
            schema = schema.with_projection_format(projection_format);
        }
        if let Some(schema_ref) = value.schema_ref {
            schema = schema.with_schema_ref(schema_ref.try_into()?);
        }
        Ok(schema)
    }
}

impl TryFrom<SerializedCid> for Cid {
    type Error = CollectionError;

    fn try_from(value: SerializedCid) -> Result<Self, Self::Error> {
        let hash = hex::decode(&value.hash).map_err(|err| {
            CollectionError::Validation(format!("invalid collection schema_ref hash hex: {err}"))
        })?;
        let hash: [u8; 32] = hash.try_into().map_err(|_| {
            CollectionError::Validation("invalid collection schema_ref hash length".to_string())
        })?;
        let key = value
            .key
            .map(|key| {
                hashtree_core::ObjectKey::from_hex(&key).map_err(|err| {
                    CollectionError::Validation(format!("invalid collection schema_ref key: {err}"))
                })
            })
            .transpose()?;

        Ok(Cid { hash, key })
    }
}

pub async fn load_collection_manifest_metadata<S: Store>(
    store: Arc<S>,
    root: Option<&Cid>,
) -> Result<Option<CollectionManifestMetadata>, CollectionError> {
    let Some(root) = root else {
        return Ok(None);
    };

    let tree = HashTree::new(HashTreeConfig::new(store));
    let entries = tree.list_directory(root).await?;
    let Some(metadata_cid) = find_manifest_cid(&entries, COLLECTION_MANIFEST_METADATA_FILE) else {
        return Ok(None);
    };
    let Some(bytes) = tree.get(&metadata_cid, None).await? else {
        return Ok(None);
    };
    let serialized = serde_json::from_slice::<SerializedCollectionManifestMetadata>(&bytes)
        .map_err(|err| {
            CollectionError::Validation(format!("invalid collection manifest metadata: {err}"))
        })?;
    Ok(Some(serialized.try_into()?))
}

pub(crate) async fn write_collection_manifest_metadata<S: Store, T>(
    tree: &HashTree<S>,
    definition: &CollectionDefinition<T>,
) -> Result<Option<(Cid, u64)>, CollectionError> {
    let Some(metadata) = CollectionManifestMetadata::from_definition(definition) else {
        return Ok(None);
    };
    let bytes = serde_json::to_vec(&SerializedCollectionManifestMetadata::from(&metadata))
        .map_err(|err| {
            CollectionError::Validation(format!("serialize collection manifest metadata: {err}"))
        })?;
    Ok(Some(tree.put_file(&bytes).await?))
}
