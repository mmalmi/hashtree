use std::sync::Arc;

use hashtree_collection::{
    federated_search, load_collection_manifest_metadata, normalize_collection_item,
    CollectionDefinition, CollectionPublishedSchema, CollectionSchema, CollectionSearchEntry,
    CollectionSearchIndexDefinition, CollectionSource, CollectionWriteContext, CollectionWriter,
    FederatedCollectionSource, FederatedSearchOptions, NormalizeCollectionItemOptions,
    COLLECTION_MANIFEST_METADATA_FILE, MANIFEST_BY_ID,
};
use hashtree_core::{Cid, HashTree, HashTreeConfig, MemoryStore};
use hashtree_index::{SearchIndexOptions, SearchOptions};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Song {
    id: String,
    title: String,
    artist: String,
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogSong {
    id: String,
    title: String,
    artist: String,
    artist_id: String,
    album: String,
    album_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MigratingSong {
    id: String,
    title: String,
    artist: String,
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacySong {
    id: String,
    title: String,
    creator: String,
    tags: Vec<String>,
}

fn song_definition() -> CollectionDefinition<Song> {
    CollectionDefinition::new(|song: &Song| song.id.clone())
        .with_key_index("artist", |song| {
            vec![format!("artist:{}", song.artist.to_lowercase())]
        })
        .with_key_index("tag", |song| {
            song.tags
                .iter()
                .map(|tag| format!("tag:{}", tag.to_lowercase()))
                .collect()
        })
        .with_search_index(
            CollectionSearchIndexDefinition::new("songs")
                .with_prefix("s:")
                .with_options(SearchIndexOptions {
                    order: Some(4),
                    ..Default::default()
                })
                .with_text(|song: &Song| {
                    let mut text = vec![song.title.clone(), song.artist.clone()];
                    text.extend(song.tags.iter().cloned());
                    text
                }),
        )
}

fn by_id_only_song_definition() -> CollectionDefinition<Song> {
    CollectionDefinition::new(|song: &Song| song.id.clone())
}

fn catalog_definition() -> CollectionDefinition<CatalogSong> {
    CollectionDefinition::new(|song: &CatalogSong| song.id.clone())
        .with_search_index(
            CollectionSearchIndexDefinition::new("songs")
                .with_root_name("catalog-search")
                .with_prefix("s:")
                .with_text(|song: &CatalogSong| {
                    vec![song.title.clone(), song.artist.clone(), song.album.clone()]
                }),
        )
        .with_search_index(
            CollectionSearchIndexDefinition::new("artists")
                .with_root_name("catalog-search")
                .with_prefix("a:")
                .with_entries(|song: &CatalogSong, context| {
                    let Some(artist_cid) = context
                        .write_context
                        .and_then(|context| context.get("artistCid"))
                        .cloned()
                    else {
                        return Vec::new();
                    };
                    vec![CollectionSearchEntry::new(vec![song.artist.clone()])
                        .with_id(song.artist_id.clone())
                        .with_cid(artist_cid)]
                }),
        )
        .with_search_index(
            CollectionSearchIndexDefinition::new("albums")
                .with_root_name("catalog-search")
                .with_prefix("l:")
                .with_entries(|song: &CatalogSong, context| {
                    let Some(album_cid) = context
                        .write_context
                        .and_then(|context| context.get("albumCid"))
                        .cloned()
                    else {
                        return Vec::new();
                    };
                    vec![
                        CollectionSearchEntry::new(vec![song.album.clone(), song.artist.clone()])
                            .with_id(song.album_id.clone())
                            .with_cid(album_cid),
                    ]
                }),
        )
}

fn migrating_song_definition() -> CollectionDefinition<MigratingSong> {
    CollectionDefinition::new(|song: &MigratingSong| song.id.clone())
        .with_schema(
            CollectionSchema::new(2)
                .with_defaults(|song: &MigratingSong| {
                    let mut next = song.clone();
                    if next.artist.trim().is_empty() {
                        next.artist = "Unknown".to_string();
                    }
                    next
                })
                .with_migrate_from(|legacy: LegacySong, from_version| {
                    if from_version != 1 {
                        return Err(hashtree_collection::CollectionError::Validation(
                            "unsupported migration".to_string(),
                        ));
                    }

                    Ok(MigratingSong {
                        id: legacy.id,
                        title: legacy.title,
                        artist: legacy.creator,
                        tags: legacy.tags,
                    })
                })
                .with_normalize(|song: &MigratingSong| {
                    let mut next = song.clone();
                    next.title = next.title.trim().to_string();
                    next.artist = {
                        let artist = next.artist.trim();
                        if artist.is_empty() {
                            "Unknown".to_string()
                        } else {
                            artist.to_string()
                        }
                    };
                    next.tags = next
                        .tags
                        .iter()
                        .map(|tag| tag.trim().to_string())
                        .filter(|tag| !tag.is_empty())
                        .fold(Vec::<String>::new(), |mut unique, tag| {
                            if !unique.iter().any(|existing| existing == &tag) {
                                unique.push(tag);
                            }
                            unique
                        });
                    next
                })
                .with_validate(|song: &MigratingSong| {
                    if song.id.trim().is_empty() {
                        return Err(hashtree_collection::CollectionError::Validation(
                            "id required".to_string(),
                        ));
                    }
                    Ok(())
                }),
        )
        .with_published_schema(
            CollectionPublishedSchema::new()
                .with_item_format("example/song@1")
                .with_projection_format("example/song-index@1"),
        )
        .with_key_index("artist", |song| {
            vec![format!("artist:{}", song.artist.to_lowercase())]
        })
        .with_search_index(
            CollectionSearchIndexDefinition::new("songs")
                .with_prefix("s:")
                .with_text(|song: &MigratingSong| {
                    let mut text = vec![song.title.clone(), song.artist.clone()];
                    text.extend(song.tags.iter().cloned());
                    text
                }),
        )
}

fn cid_from_seed(seed: u8) -> Cid {
    let mut hash = [0u8; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = seed.wrapping_add(index as u8);
    }
    Cid::public(hash)
}

#[tokio::test]
async fn put_and_delete_update_by_id_key_and_search_indexes() {
    let store = Arc::new(MemoryStore::new());
    let definition = song_definition();
    let mut writer = CollectionWriter::new(Arc::clone(&store), definition.clone());
    let song_a = Song {
        id: "song-a".to_string(),
        title: "Midnight Orchard".to_string(),
        artist: "Ada".to_string(),
        tags: vec!["dream-pop".to_string()],
    };
    let song_b = Song {
        id: "song-b".to_string(),
        title: "Sun Clock".to_string(),
        artist: "Bea".to_string(),
        tags: vec!["ambient".to_string()],
    };

    writer.put(&song_a, &cid_from_seed(1), None).await.unwrap();
    writer.put(&song_b, &cid_from_seed(2), None).await.unwrap();

    let source =
        CollectionSource::with_definition(Arc::clone(&store), writer.snapshot(), &definition);
    assert_eq!(source.get("song-a").await.unwrap(), Some(cid_from_seed(1)));
    assert_eq!(
        source
            .search(
                "songs",
                "midnight",
                SearchOptions {
                    limit: Some(10),
                    full_match: false,
                },
            )
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["song-a".to_string()]
    );
    assert_eq!(
        source
            .query_index("artist", Some("artist:ada"), None)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.key)
            .collect::<Vec<_>>(),
        vec!["artist:ada".to_string()]
    );

    writer.delete(&song_a).await.unwrap();

    let source =
        CollectionSource::with_definition(Arc::clone(&store), writer.snapshot(), &definition);
    assert_eq!(source.get("song-a").await.unwrap(), None);
    assert!(source
        .search("songs", "midnight", SearchOptions::default())
        .await
        .unwrap()
        .is_empty());
    assert!(source
        .query_index("artist", Some("artist:ada"), None)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn previous_item_cleanup_and_root_reload_remove_stale_search_terms() {
    let store = Arc::new(MemoryStore::new());
    let definition = song_definition();
    let mut writer = CollectionWriter::new(Arc::clone(&store), definition.clone());
    let original = Song {
        id: "song-a".to_string(),
        title: "Old Horizon".to_string(),
        artist: "Ada".to_string(),
        tags: vec!["night".to_string()],
    };
    let replacement = Song {
        id: "song-a".to_string(),
        title: "New Horizon".to_string(),
        artist: "Bea".to_string(),
        tags: vec!["day".to_string()],
    };

    writer
        .put(&original, &cid_from_seed(10), None)
        .await
        .unwrap();
    writer
        .replace(&replacement, &cid_from_seed(11), &original)
        .await
        .unwrap();

    let root = writer.write_root().await.unwrap().expect("collection root");
    let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
    let entries = tree.list_directory(&root).await.unwrap();
    let names = entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&MANIFEST_BY_ID));
    assert!(names.contains(&"artist"));
    assert!(names.contains(&"tag"));
    assert!(names.contains(&"songs"));

    let source = CollectionSource::from_root(Arc::clone(&store), &definition, Some(&root))
        .await
        .unwrap();
    assert_eq!(source.get("song-a").await.unwrap(), Some(cid_from_seed(11)));
    assert!(source
        .search("songs", "old", SearchOptions::default())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        source
            .search("songs", "new", SearchOptions::default())
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["song-a".to_string()]
    );
}

#[tokio::test]
async fn indexed_overwrite_requires_previous_item() {
    let store = Arc::new(MemoryStore::new());
    let definition = song_definition();
    let mut writer = CollectionWriter::new(Arc::clone(&store), definition.clone());
    let original = Song {
        id: "song-a".to_string(),
        title: "Old Horizon".to_string(),
        artist: "Ada".to_string(),
        tags: vec!["night".to_string()],
    };
    let replacement = Song {
        id: "song-a".to_string(),
        title: "New Horizon".to_string(),
        artist: "Bea".to_string(),
        tags: vec!["day".to_string()],
    };

    writer
        .put(&original, &cid_from_seed(12), None)
        .await
        .unwrap();

    let error = writer
        .put(&replacement, &cid_from_seed(13), None)
        .await
        .expect_err("indexed overwrite should require previous item");
    assert!(matches!(
        error,
        hashtree_collection::CollectionError::MissingPreviousForOverwrite { ref id }
            if id == "song-a"
    ));

    let source =
        CollectionSource::with_definition(Arc::clone(&store), writer.snapshot(), &definition);
    assert_eq!(source.get("song-a").await.unwrap(), Some(cid_from_seed(12)));
    assert_eq!(
        source
            .search("songs", "old", SearchOptions::default())
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["song-a".to_string()]
    );
    assert!(source
        .search("songs", "new", SearchOptions::default())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn by_id_only_overwrite_does_not_require_previous_item() {
    let store = Arc::new(MemoryStore::new());
    let definition = by_id_only_song_definition();
    let mut writer = CollectionWriter::new(Arc::clone(&store), definition.clone());
    let original = Song {
        id: "song-a".to_string(),
        title: "Old Horizon".to_string(),
        artist: "Ada".to_string(),
        tags: vec![],
    };
    let replacement = Song {
        id: "song-a".to_string(),
        title: "New Horizon".to_string(),
        artist: "Bea".to_string(),
        tags: vec![],
    };

    writer
        .put(&original, &cid_from_seed(14), None)
        .await
        .unwrap();
    writer
        .put(&replacement, &cid_from_seed(15), None)
        .await
        .unwrap();

    let source =
        CollectionSource::with_definition(Arc::clone(&store), writer.snapshot(), &definition);
    assert_eq!(source.get("song-a").await.unwrap(), Some(cid_from_seed(15)));
}

#[tokio::test]
async fn write_root_publishes_schema_metadata_when_declared() {
    let store = Arc::new(MemoryStore::new());
    let definition = migrating_song_definition();
    let mut writer = CollectionWriter::new(Arc::clone(&store), definition);
    let song = MigratingSong {
        id: "song-c".to_string(),
        title: "Lantern Bloom".to_string(),
        artist: "Ada".to_string(),
        tags: vec!["night".to_string()],
    };

    writer.put(&song, &cid_from_seed(42), None).await.unwrap();

    let root = writer.write_root().await.unwrap().expect("collection root");
    let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
    let names = tree
        .list_directory(&root)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    assert!(names
        .iter()
        .any(|name| name == COLLECTION_MANIFEST_METADATA_FILE));
    let metadata = load_collection_manifest_metadata(Arc::clone(&store), Some(&root))
        .await
        .unwrap()
        .expect("published metadata");

    assert_eq!(metadata.version(), 1);
    assert_eq!(metadata.schema_version(), 2);
    assert_eq!(
        metadata.published_schema().cloned(),
        Some(
            CollectionPublishedSchema::new()
                .with_item_format("example/song@1")
                .with_projection_format("example/song-index@1")
        )
    );
}

#[tokio::test]
async fn shared_search_roots_support_derived_entity_targets() {
    let store = Arc::new(MemoryStore::new());
    let definition = catalog_definition();
    let mut writer = CollectionWriter::new(Arc::clone(&store), definition.clone());
    let song = CatalogSong {
        id: "song-1".to_string(),
        title: "Quiet Bloom".to_string(),
        artist: "Open Meridian".to_string(),
        artist_id: "artist-1".to_string(),
        album: "Harbor Echo".to_string(),
        album_id: "album-1".to_string(),
    };
    let mut context = CollectionWriteContext::new();
    context.insert("artistCid".to_string(), cid_from_seed(51));
    context.insert("albumCid".to_string(), cid_from_seed(52));

    writer
        .put_with_context(&song, &cid_from_seed(50), None, Some(&context), None)
        .await
        .unwrap();

    let snapshot = writer.snapshot();
    assert_eq!(
        snapshot.search_root("songs"),
        snapshot.search_root("artists")
    );
    assert_eq!(
        snapshot.search_root("songs"),
        snapshot.search_root("albums")
    );

    let source = CollectionSource::with_definition(Arc::clone(&store), snapshot, &definition);
    assert_eq!(
        source
            .search("songs", "quiet", SearchOptions::default())
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["song-1".to_string()]
    );
    assert_eq!(
        source
            .search("artists", "open", SearchOptions::default())
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["artist-1".to_string()]
    );
    assert_eq!(
        source
            .search("albums", "harbor", SearchOptions::default())
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["album-1".to_string()]
    );
}

#[tokio::test]
async fn rebuild_keeps_only_the_last_item_for_each_id() {
    let store = Arc::new(MemoryStore::new());
    let definition = song_definition();
    let mut writer = CollectionWriter::new(Arc::clone(&store), definition.clone());
    let original = Song {
        id: "song-a".to_string(),
        title: "Old Horizon".to_string(),
        artist: "Ada".to_string(),
        tags: vec!["night".to_string()],
    };
    let replacement = Song {
        id: "song-a".to_string(),
        title: "New Horizon".to_string(),
        artist: "Bea".to_string(),
        tags: vec!["day".to_string()],
    };
    let other = Song {
        id: "song-b".to_string(),
        title: "Sun Clock".to_string(),
        artist: "Bea".to_string(),
        tags: vec!["ambient".to_string()],
    };

    writer
        .rebuild(vec![
            (original, cid_from_seed(20)),
            (replacement, cid_from_seed(21)),
            (other, cid_from_seed(22)),
        ])
        .await
        .unwrap();

    let source =
        CollectionSource::with_definition(Arc::clone(&store), writer.snapshot(), &definition);
    assert_eq!(source.get("song-a").await.unwrap(), Some(cid_from_seed(21)));
    assert!(source
        .query_index("artist", Some("artist:ada"), None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        source
            .search("songs", "new", SearchOptions::default())
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["song-a".to_string()]
    );
    assert!(source
        .search("songs", "old", SearchOptions::default())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn reindex_rebuilds_from_canonical_entries_and_clears_stale_state() {
    let store = Arc::new(MemoryStore::new());
    let definition = song_definition();
    let mut writer = CollectionWriter::new(Arc::clone(&store), definition.clone());
    let original = Song {
        id: "song-a".to_string(),
        title: "Old Horizon".to_string(),
        artist: "Ada".to_string(),
        tags: vec!["night".to_string()],
    };
    let replacement = Song {
        id: "song-a".to_string(),
        title: "New Horizon".to_string(),
        artist: "Bea".to_string(),
        tags: vec!["day".to_string()],
    };
    let other = Song {
        id: "song-b".to_string(),
        title: "Sun Clock".to_string(),
        artist: "Bea".to_string(),
        tags: vec!["ambient".to_string()],
    };

    writer
        .put(&original, &cid_from_seed(30), None)
        .await
        .unwrap();
    writer
        .reindex(vec![
            (replacement, cid_from_seed(31)),
            (other, cid_from_seed(32)),
        ])
        .await
        .unwrap();

    let source =
        CollectionSource::with_definition(Arc::clone(&store), writer.snapshot(), &definition);
    assert_eq!(source.get("song-a").await.unwrap(), Some(cid_from_seed(31)));
    assert!(source
        .query_index("artist", Some("artist:ada"), None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        source
            .query_index("artist", Some("artist:bea"), None)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.key)
            .collect::<Vec<_>>(),
        vec!["artist:bea".to_string()]
    );
    assert_eq!(
        source
            .search("songs", "new", SearchOptions::default())
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["song-a".to_string()]
    );
    assert!(source
        .search("songs", "old", SearchOptions::default())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn schema_hooks_normalize_migrate_and_validate_items() {
    let definition = migrating_song_definition();
    let migrated = normalize_collection_item(
        &definition,
        LegacySong {
            id: "song-c".to_string(),
            title: "  Lantern Bloom  ".to_string(),
            creator: " Ada ".to_string(),
            tags: vec![
                "ambient".to_string(),
                "ambient".to_string(),
                "  night ".to_string(),
            ],
        },
        NormalizeCollectionItemOptions {
            from_version: Some(1),
        },
    )
    .unwrap();

    assert_eq!(
        migrated,
        MigratingSong {
            id: "song-c".to_string(),
            title: "Lantern Bloom".to_string(),
            artist: "Ada".to_string(),
            tags: vec!["ambient".to_string(), "night".to_string()],
        }
    );

    let invalid = normalize_collection_item(
        &definition,
        MigratingSong {
            id: "   ".to_string(),
            title: "Invalid".to_string(),
            artist: "".to_string(),
            tags: Vec::new(),
        },
        NormalizeCollectionItemOptions::default(),
    );
    assert!(matches!(
        invalid,
        Err(hashtree_collection::CollectionError::Validation(message)) if message == "id required"
    ));

    let store = Arc::new(MemoryStore::new());
    let mut writer = CollectionWriter::new(Arc::clone(&store), definition.clone());
    writer
        .put(
            &MigratingSong {
                id: "song-d".to_string(),
                title: "  Quiet Harbor  ".to_string(),
                artist: "   ".to_string(),
                tags: vec!["ambient".to_string(), "ambient".to_string()],
            },
            &cid_from_seed(60),
            None,
        )
        .await
        .unwrap();

    let source =
        CollectionSource::with_definition(Arc::clone(&store), writer.snapshot(), &definition);
    assert_eq!(
        source
            .query_index("artist", Some("artist:unknown"), None)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.key)
            .collect::<Vec<_>>(),
        vec!["artist:unknown".to_string()]
    );
    assert_eq!(
        source
            .search("songs", "quiet", SearchOptions::default())
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["song-d".to_string()]
    );
}

#[tokio::test]
async fn federated_search_dedupes_hits_and_applies_boosts() {
    let store = Arc::new(MemoryStore::new());
    let definition = song_definition();
    let mut global_writer = CollectionWriter::new(Arc::clone(&store), definition.clone());
    let mut self_writer = CollectionWriter::new(Arc::clone(&store), definition.clone());

    global_writer
        .put(
            &Song {
                id: "shared-song".to_string(),
                title: "Starlight Echo".to_string(),
                artist: "Ada".to_string(),
                tags: Vec::new(),
            },
            &cid_from_seed(70),
            None,
        )
        .await
        .unwrap();
    global_writer
        .put(
            &Song {
                id: "global-only".to_string(),
                title: "Garden Static".to_string(),
                artist: "Bea".to_string(),
                tags: Vec::new(),
            },
            &cid_from_seed(71),
            None,
        )
        .await
        .unwrap();
    self_writer
        .put(
            &Song {
                id: "shared-song".to_string(),
                title: "Starlight Echo".to_string(),
                artist: "Ada".to_string(),
                tags: Vec::new(),
            },
            &cid_from_seed(80),
            None,
        )
        .await
        .unwrap();
    self_writer
        .put(
            &Song {
                id: "self-only".to_string(),
                title: "Starlight Ritual".to_string(),
                artist: "Ada".to_string(),
                tags: Vec::new(),
            },
            &cid_from_seed(81),
            None,
        )
        .await
        .unwrap();

    let global_source = CollectionSource::with_definition(
        Arc::clone(&store),
        global_writer.snapshot(),
        &definition,
    );
    let self_source =
        CollectionSource::with_definition(Arc::clone(&store), self_writer.snapshot(), &definition);

    let results = federated_search(
        vec![
            FederatedCollectionSource::new("global-catalog", &global_source),
            FederatedCollectionSource::new("self-catalog", &self_source).with_boost(2),
        ],
        "songs",
        "starlight",
        FederatedSearchOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(
        results
            .iter()
            .map(|result| result.id.as_str())
            .collect::<Vec<_>>(),
        vec!["shared-song", "self-only"]
    );
    assert_eq!(
        results[0]
            .source_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["global-catalog", "self-catalog"]
    );
    assert_eq!(results[0].best_source_id, "self-catalog".to_string());
    assert!(results[0].score > results[1].score);
}
