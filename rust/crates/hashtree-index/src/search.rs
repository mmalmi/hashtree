use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hashtree_core::Cid;

use crate::{BTree, BTreeError, BTreeOptions};

#[derive(Debug, Clone, Default)]
pub struct SearchIndexOptions {
    pub order: Option<usize>,
    pub stop_words: Option<HashSet<String>>,
    pub min_keyword_length: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: Option<usize>,
    pub full_match: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub id: String,
    pub value: String,
    pub score: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchLinkResult {
    pub id: String,
    pub cid: Cid,
    pub score: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("btree error: {0}")]
    BTree(#[from] BTreeError),
    #[error("{0}")]
    Validation(String),
}

const DEFAULT_MIN_KEYWORD_LENGTH: usize = 2;

pub struct SearchIndex<S: hashtree_core::Store> {
    btree: BTree<S>,
    stop_words: HashSet<String>,
    min_keyword_length: usize,
}

impl<S: hashtree_core::Store> SearchIndex<S> {
    pub fn new(store: Arc<S>, options: SearchIndexOptions) -> Self {
        Self {
            btree: BTree::new(
                store,
                BTreeOptions {
                    order: options.order,
                },
            ),
            stop_words: options.stop_words.unwrap_or_else(default_stop_words),
            min_keyword_length: options
                .min_keyword_length
                .unwrap_or(DEFAULT_MIN_KEYWORD_LENGTH),
        }
    }

    pub fn parse_keywords(&self, text: &str) -> Vec<String> {
        if text.is_empty() {
            return Vec::new();
        }

        let mut keywords = Vec::new();
        let mut seen = HashSet::new();

        for raw_word in text
            .split(|character: char| !character.is_alphanumeric())
            .filter(|token| !token.is_empty())
        {
            for word in expand_keyword_variants(raw_word) {
                if word.chars().count() < self.min_keyword_length
                    || self.stop_words.contains(&word)
                    || is_pure_number(&word)
                    || !seen.insert(word.clone())
                {
                    continue;
                }
                keywords.push(word);
            }
        }

        keywords
    }

    pub async fn index(
        &self,
        root: Option<&Cid>,
        prefix: &str,
        terms: &[String],
        id: &str,
        value: &str,
    ) -> Result<Cid, SearchError> {
        let mut new_root = root.cloned();
        for term in terms {
            new_root = Some(
                self.btree
                    .insert(new_root.as_ref(), &format!("{prefix}{term}:{id}"), value)
                    .await?,
            );
        }

        new_root.ok_or_else(|| {
            SearchError::Validation("search index requires at least one term".to_string())
        })
    }

    pub async fn remove(
        &self,
        root: &Cid,
        prefix: &str,
        terms: &[String],
        id: &str,
    ) -> Result<Option<Cid>, SearchError> {
        let mut new_root = Some(root.clone());
        for term in terms {
            let Some(active_root) = new_root.as_ref() else {
                break;
            };
            new_root = self
                .btree
                .delete(active_root, &format!("{prefix}{term}:{id}"))
                .await?;
        }
        Ok(new_root)
    }

    pub async fn search(
        &self,
        root: Option<&Cid>,
        prefix: &str,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let Some(root) = root else {
            return Ok(Vec::new());
        };

        let limit = options.limit.unwrap_or(20);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let keywords = self.parse_keywords(query);
        if keywords.is_empty() {
            return Ok(Vec::new());
        }

        #[derive(Debug)]
        struct Aggregate {
            value: String,
            score: usize,
            exact_matches: usize,
            prefix_distance: usize,
        }

        let mut results = HashMap::<String, Aggregate>::new();
        for keyword in keywords {
            let search_prefix = if options.full_match {
                format!("{prefix}{keyword}:")
            } else {
                format!("{prefix}{keyword}")
            };
            for (count, (key, value)) in self
                .btree
                .prefix(root, &search_prefix)
                .await?
                .into_iter()
                .enumerate()
            {
                if count >= limit.saturating_mul(2) {
                    break;
                }

                let Some((term, id)) = decode_search_key(prefix, &key) else {
                    continue;
                };
                let aggregate = results.entry(id).or_insert_with(|| Aggregate {
                    value,
                    score: 0,
                    exact_matches: 0,
                    prefix_distance: 0,
                });
                aggregate.score += 1;
                if term == keyword {
                    aggregate.exact_matches += 1;
                }
                aggregate.prefix_distance += term.len().saturating_sub(keyword.len());
            }
        }

        let mut sorted = results.into_iter().collect::<Vec<_>>();
        sorted.sort_by(|left, right| {
            let left_data = &left.1;
            let right_data = &right.1;
            right_data
                .score
                .cmp(&left_data.score)
                .then(right_data.exact_matches.cmp(&left_data.exact_matches))
                .then(left_data.prefix_distance.cmp(&right_data.prefix_distance))
                .then(left.0.cmp(&right.0))
        });
        sorted.truncate(limit);
        Ok(sorted
            .into_iter()
            .map(|(id, aggregate)| SearchResult {
                id,
                value: aggregate.value,
                score: aggregate.score,
            })
            .collect())
    }

    pub async fn merge(
        &self,
        base: Option<&Cid>,
        other: Option<&Cid>,
        prefer_other: bool,
    ) -> Result<Option<Cid>, SearchError> {
        Ok(self.btree.merge(base, other, prefer_other).await?)
    }

    pub async fn build_links<I>(&self, items: I) -> Result<Option<Cid>, SearchError>
    where
        I: IntoIterator<Item = (String, Cid)>,
    {
        Ok(self.btree.build_links(items).await?)
    }

    pub async fn index_link(
        &self,
        root: Option<&Cid>,
        prefix: &str,
        terms: &[String],
        id: &str,
        target_cid: &Cid,
    ) -> Result<Cid, SearchError> {
        let mut new_root = root.cloned();
        for term in terms {
            new_root = Some(
                self.btree
                    .insert_link(
                        new_root.as_ref(),
                        &format!("{prefix}{term}:{id}"),
                        target_cid,
                    )
                    .await?,
            );
        }

        new_root.ok_or_else(|| {
            SearchError::Validation("search index requires at least one term".to_string())
        })
    }

    pub async fn remove_link(
        &self,
        root: &Cid,
        prefix: &str,
        terms: &[String],
        id: &str,
    ) -> Result<Option<Cid>, SearchError> {
        let mut new_root = Some(root.clone());
        for term in terms {
            let Some(active_root) = new_root.as_ref() else {
                break;
            };
            new_root = self
                .btree
                .delete(active_root, &format!("{prefix}{term}:{id}"))
                .await?;
        }
        Ok(new_root)
    }

    pub async fn search_links(
        &self,
        root: Option<&Cid>,
        prefix: &str,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchLinkResult>, SearchError> {
        let Some(root) = root else {
            return Ok(Vec::new());
        };

        let limit = options.limit.unwrap_or(20);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let keywords = self.parse_keywords(query);
        if keywords.is_empty() {
            return Ok(Vec::new());
        }

        #[derive(Debug)]
        struct Aggregate {
            cid: Cid,
            score: usize,
            exact_matches: usize,
            prefix_distance: usize,
        }

        let mut results = HashMap::<String, Aggregate>::new();
        for keyword in keywords {
            let search_prefix = if options.full_match {
                format!("{prefix}{keyword}:")
            } else {
                format!("{prefix}{keyword}")
            };
            for (count, (key, cid)) in self
                .btree
                .prefix_links(root, &search_prefix)
                .await?
                .into_iter()
                .enumerate()
            {
                if count >= limit.saturating_mul(2) {
                    break;
                }

                let Some((term, id)) = decode_search_key(prefix, &key) else {
                    continue;
                };
                let aggregate = results.entry(id).or_insert_with(|| Aggregate {
                    cid,
                    score: 0,
                    exact_matches: 0,
                    prefix_distance: 0,
                });
                aggregate.score += 1;
                if term == keyword {
                    aggregate.exact_matches += 1;
                }
                aggregate.prefix_distance += term.len().saturating_sub(keyword.len());
            }
        }

        let mut sorted = results.into_iter().collect::<Vec<_>>();
        sorted.sort_by(|left, right| {
            let left_data = &left.1;
            let right_data = &right.1;
            right_data
                .score
                .cmp(&left_data.score)
                .then(right_data.exact_matches.cmp(&left_data.exact_matches))
                .then(left_data.prefix_distance.cmp(&right_data.prefix_distance))
                .then(left.0.cmp(&right.0))
        });
        sorted.truncate(limit);
        Ok(sorted
            .into_iter()
            .map(|(id, aggregate)| SearchLinkResult {
                id,
                cid: aggregate.cid,
                score: aggregate.score,
            })
            .collect())
    }

    pub async fn merge_links(
        &self,
        base: Option<&Cid>,
        other: Option<&Cid>,
        prefer_other: bool,
    ) -> Result<Option<Cid>, SearchError> {
        Ok(self.btree.merge_links(base, other, prefer_other).await?)
    }
}

fn decode_search_key(prefix: &str, key: &str) -> Option<(String, String)> {
    if !key.starts_with(prefix) {
        return None;
    }
    let after_prefix = &key[prefix.len()..];
    let colon_index = after_prefix.find(':')?;
    Some((
        after_prefix[..colon_index].to_string(),
        after_prefix[colon_index + 1..].to_string(),
    ))
}

fn expand_keyword_variants(raw_word: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let normalized = raw_word.to_lowercase();
    if !normalized.is_empty() {
        variants.push(normalized);
    }

    for segment in split_keyword_segments(raw_word) {
        let normalized_segment = segment.to_lowercase();
        if normalized_segment.is_empty()
            || variants
                .iter()
                .any(|existing| existing == &normalized_segment)
        {
            continue;
        }
        variants.push(normalized_segment);
    }

    variants
}

fn split_keyword_segments(raw_word: &str) -> Vec<String> {
    let chars = raw_word.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return Vec::new();
    }

    let mut parts = Vec::new();
    let mut start = 0usize;
    for index in 1..chars.len() {
        let previous = chars[index - 1];
        let current = chars[index];
        let next = chars.get(index + 1).copied();
        if is_keyword_boundary(previous, current, next) {
            parts.push(chars[start..index].iter().collect::<String>());
            start = index;
        }
    }
    parts.push(chars[start..].iter().collect::<String>());
    parts
}

fn is_keyword_boundary(previous: char, current: char, next: Option<char>) -> bool {
    (previous.is_lowercase() && current.is_uppercase())
        || (previous.is_alphabetic() && current.is_numeric())
        || (previous.is_numeric() && current.is_alphabetic())
        || (previous.is_uppercase()
            && current.is_uppercase()
            && next.is_some_and(|next| next.is_lowercase()))
}

fn is_pure_number(word: &str) -> bool {
    if !word.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    !(word.len() == 4 && (word.starts_with("19") || word.starts_with("20")))
}

fn default_stop_words() -> HashSet<String> {
    [
        "a", "an", "the", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with", "by",
        "from", "is", "it", "as", "be", "was", "are", "this", "that", "these", "those", "i", "you",
        "he", "she", "we", "they", "my", "your", "his", "her", "its", "our", "their", "what",
        "which", "who", "whom", "how", "when", "where", "why", "will", "would", "could", "should",
        "can", "may", "might", "must", "have", "has", "had", "do", "does", "did", "been", "being",
        "get", "got", "just", "now", "then", "so", "if", "not", "no", "yes", "all", "any", "some",
        "more", "most", "other", "into", "over", "after", "before", "about", "up", "down", "out",
        "off", "through", "during", "under", "again", "further", "once",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}
