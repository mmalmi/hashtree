use anyhow::{anyhow, Context, Result};
use reqwest::{Client, Url};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::storage::HashtreeStore;
use hashtree_core::{nhash_encode, Cid, DirEntry, HashTree, HashTreeConfig, LinkType};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstalledSitePwa {
    pub name: String,
    pub launch_url: String,
    pub icon_url: Option<String>,
    pub source_app_id: Option<String>,
    pub source_url: String,
    pub source_manifest_url: String,
    pub description: Option<String>,
    pub display_mode: Option<String>,
}

#[derive(Debug, Clone)]
struct PwaAsset {
    path: String,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct AssetReference {
    raw_value: String,
    resolved_url: Url,
}

#[derive(Debug, Clone)]
struct FetchedPwa {
    name: String,
    source_app_id: Option<String>,
    source_url: String,
    source_manifest_url: String,
    description: Option<String>,
    display_mode: Option<String>,
    launch_reference: String,
    icon_path: Option<String>,
    assets: Vec<PwaAsset>,
}

pub async fn install_site_pwa_to_store(
    store: &HashtreeStore,
    url: &str,
) -> Result<InstalledSitePwa> {
    let fetched = fetch_pwa(url).await.context("fetch installable PWA")?;
    let root_cid = store_pwa_assets(store, &fetched.assets)
        .await
        .context("store PWA in hashtree")?;

    store.pin(&root_cid.hash).context("pin stored PWA")?;

    let nhash = nhash_encode(&root_cid.hash).context("encode stored PWA root")?;

    Ok(InstalledSitePwa {
        name: fetched.name,
        launch_url: format!("htree://{nhash}{}", fetched.launch_reference),
        icon_url: fetched
            .icon_path
            .as_ref()
            .map(|path| format!("htree://{nhash}{}", absolute_tree_path(path))),
        source_app_id: fetched.source_app_id,
        source_url: fetched.source_url,
        source_manifest_url: fetched.source_manifest_url,
        description: fetched.description,
        display_mode: fetched.display_mode,
    })
}

pub async fn cache_bookmark_icon_to_store(
    store: &HashtreeStore,
    source_url: Option<&str>,
    source_manifest_url: Option<&str>,
    icon_url: Option<&str>,
) -> Result<Option<String>> {
    let client = build_reqwest_client()?;

    match cache_manifest_icon_to_store(store, &client, source_url, source_manifest_url).await {
        Ok(Some(cached_icon)) => return Ok(Some(cached_icon)),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!("Failed to cache manifest-derived bookmark icon: {}", error);
        }
    }

    let Some(icon_url) = icon_url.filter(|value| is_http_url(value)) else {
        return Ok(None);
    };
    match cache_direct_icon_to_store(store, &client, icon_url).await {
        Ok(cached_icon) => Ok(Some(cached_icon)),
        Err(error) => {
            tracing::warn!(
                "Failed to cache direct bookmark icon {}: {}",
                icon_url,
                error
            );
            Ok(None)
        }
    }
}

fn build_reqwest_client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .context("build reqwest client")
}

async fn fetch_pwa(url: &str) -> Result<FetchedPwa> {
    let client = build_reqwest_client()?;

    let html_response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetch page {url}"))?;
    let html_response = html_response
        .error_for_status()
        .with_context(|| format!("fetch page {url}"))?;
    let source_url = html_response.url().to_string();
    let base_url = html_response.url().clone();
    let original_html = html_response
        .text()
        .await
        .with_context(|| format!("read page body {source_url}"))?;

    let html_path = url_to_path(&base_url, &base_url);
    let manifest_reference = extract_manifest_reference(&original_html, &base_url)
        .ok_or_else(|| anyhow!("page does not expose a web manifest"))?;
    let manifest_url = manifest_reference.resolved_url.clone();
    let manifest_response = client
        .get(manifest_url.clone())
        .send()
        .await
        .with_context(|| format!("fetch manifest {manifest_url}"))?;
    let manifest_response = manifest_response
        .error_for_status()
        .with_context(|| format!("fetch manifest {manifest_url}"))?;
    let source_manifest_url = manifest_response.url().to_string();
    let manifest_url = manifest_response.url().clone();
    let mut manifest: Value = manifest_response
        .json()
        .await
        .with_context(|| format!("parse manifest JSON {source_manifest_url}"))?;

    let mut fetched_urls = HashSet::new();
    let mut assets = Vec::new();
    let mut html_rewrites = Vec::new();
    let mut queued_assets = BTreeSet::new();

    fetched_urls.insert(source_url.clone());
    fetched_urls.insert(source_manifest_url.clone());

    let manifest_path = url_to_path(&manifest_url, &base_url);
    html_rewrites.push((
        manifest_reference.raw_value,
        relative_tree_reference(&html_path, &manifest_path),
    ));

    for stylesheet in extract_link_references(&original_html, "stylesheet", &base_url) {
        let asset_path = url_to_path(&stylesheet.resolved_url, &base_url);
        html_rewrites.push((
            stylesheet.raw_value,
            relative_tree_reference(&html_path, &asset_path),
        ));
        queued_assets.insert(stylesheet.resolved_url.to_string());
    }
    for script in extract_script_references(&original_html, &base_url) {
        let asset_path = url_to_path(&script.resolved_url, &base_url);
        html_rewrites.push((
            script.raw_value,
            relative_tree_reference(&html_path, &asset_path),
        ));
        queued_assets.insert(script.resolved_url.to_string());
    }
    for image in extract_image_references(&original_html, &base_url) {
        let asset_path = url_to_path(&image.resolved_url, &base_url);
        html_rewrites.push((
            image.raw_value,
            relative_tree_reference(&html_path, &asset_path),
        ));
        queued_assets.insert(image.resolved_url.to_string());
    }
    for asset_url in extract_manifest_resource_urls(&manifest, &manifest_url) {
        queued_assets.insert(asset_url.to_string());
    }

    let mut queued_asset_urls: Vec<Url> = queued_assets
        .into_iter()
        .filter_map(|value| Url::parse(&value).ok())
        .collect();
    let mut queued_asset_set: HashSet<String> = queued_asset_urls
        .iter()
        .map(|value| value.to_string())
        .collect();

    let mut queue_index = 0usize;
    while queue_index < queued_asset_urls.len() {
        let asset_url = queued_asset_urls[queue_index].clone();
        queue_index += 1;

        let discovered = fetch_asset(
            &client,
            &base_url,
            &asset_url,
            &mut fetched_urls,
            &mut assets,
        )
        .await;

        for nested_url in discovered {
            if queued_asset_set.insert(nested_url.to_string()) {
                queued_asset_urls.push(nested_url);
            }
        }
    }

    rewrite_manifest_urls(&mut manifest, &manifest_url, &manifest_path);

    let rewritten_html = rewrite_html_urls(&original_html, &html_rewrites);
    assets.push(PwaAsset {
        path: html_path.clone(),
        data: rewritten_html.into_bytes(),
    });
    assets.push(PwaAsset {
        path: manifest_path,
        data: serde_json::to_vec_pretty(&manifest).context("serialize rewritten manifest")?,
    });

    let launch_reference = manifest_start_reference(&manifest, &manifest_url)
        .unwrap_or_else(|| absolute_tree_path(&html_path));
    let icon_path = pick_manifest_icon_path(&manifest, &manifest_url);
    let source_app_id = manifest_app_id(&manifest, &manifest_url);
    let description = manifest_description(&manifest);
    let display_mode = manifest_display_mode(&manifest);
    let name = manifest_name(&manifest)
        .or_else(|| extract_title(&original_html))
        .unwrap_or_else(|| {
            Url::parse(&source_url)
                .ok()
                .and_then(|value| value.host_str().map(str::to_owned))
                .unwrap_or_else(|| "Installed Site".to_string())
        });

    Ok(FetchedPwa {
        name,
        source_app_id,
        source_url,
        source_manifest_url,
        description,
        display_mode,
        launch_reference,
        icon_path,
        assets,
    })
}

async fn cache_manifest_icon_to_store(
    store: &HashtreeStore,
    client: &Client,
    source_url: Option<&str>,
    source_manifest_url: Option<&str>,
) -> Result<Option<String>> {
    let Some((manifest, manifest_url)) =
        fetch_manifest_for_icon(client, source_url, source_manifest_url).await?
    else {
        return Ok(None);
    };
    let Some(icon_url) = pick_manifest_icon_url(&manifest, &manifest_url) else {
        return Ok(None);
    };
    cache_icon_url_to_store(store, client, &icon_url)
        .await
        .map(Some)
}

async fn fetch_manifest_for_icon(
    client: &Client,
    source_url: Option<&str>,
    source_manifest_url: Option<&str>,
) -> Result<Option<(Value, Url)>> {
    if let Some(manifest_url) = source_manifest_url.filter(|value| is_http_url(value)) {
        return fetch_manifest_json(client, manifest_url).await.map(Some);
    }

    let Some(source_url) = source_url.filter(|value| is_http_url(value)) else {
        return Ok(None);
    };

    let html_response = client
        .get(source_url)
        .send()
        .await
        .with_context(|| format!("fetch page {source_url}"))?;
    let html_response = html_response
        .error_for_status()
        .with_context(|| format!("fetch page {source_url}"))?;
    let base_url = html_response.url().clone();
    let html = html_response
        .text()
        .await
        .with_context(|| format!("read page body {}", base_url))?;
    let Some(manifest_reference) = extract_manifest_reference(&html, &base_url) else {
        return Ok(None);
    };

    fetch_manifest_json(client, manifest_reference.resolved_url.as_str())
        .await
        .map(Some)
}

async fn fetch_manifest_json(client: &Client, manifest_url: &str) -> Result<(Value, Url)> {
    let parsed_manifest_url =
        Url::parse(manifest_url).with_context(|| format!("parse manifest url {manifest_url}"))?;
    let manifest_response = client
        .get(parsed_manifest_url.clone())
        .send()
        .await
        .with_context(|| format!("fetch manifest {parsed_manifest_url}"))?;
    let manifest_response = manifest_response
        .error_for_status()
        .with_context(|| format!("fetch manifest {parsed_manifest_url}"))?;
    let resolved_manifest_url = manifest_response.url().clone();
    let manifest: Value = manifest_response
        .json()
        .await
        .with_context(|| format!("parse manifest JSON {}", resolved_manifest_url))?;
    Ok((manifest, resolved_manifest_url))
}

async fn cache_direct_icon_to_store(
    store: &HashtreeStore,
    client: &Client,
    icon_url: &str,
) -> Result<String> {
    let parsed_icon_url =
        Url::parse(icon_url).with_context(|| format!("parse icon url {icon_url}"))?;
    cache_icon_url_to_store(store, client, &parsed_icon_url).await
}

async fn cache_icon_url_to_store(
    store: &HashtreeStore,
    client: &Client,
    icon_url: &Url,
) -> Result<String> {
    if !matches!(icon_url.scheme(), "http" | "https") {
        return Err(anyhow!("icon URL must use http:// or https://"));
    }

    let response = client
        .get(icon_url.clone())
        .send()
        .await
        .with_context(|| format!("fetch icon {icon_url}"))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("fetch icon {icon_url}"))?;
    let resolved_icon_url = response.url().clone();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("read icon body {resolved_icon_url}"))?;

    if !looks_like_image_payload(&content_type, &bytes) {
        return Err(anyhow!("icon response was not an image"));
    }

    let icon_path = icon_asset_path(&resolved_icon_url, &content_type, &bytes);
    let root_cid = store_pwa_assets(
        store,
        &[PwaAsset {
            path: icon_path.clone(),
            data: bytes.to_vec(),
        }],
    )
    .await
    .context("store bookmark icon in hashtree")?;
    store
        .pin(&root_cid.hash)
        .context("pin cached bookmark icon")?;

    let nhash = nhash_encode(&root_cid.hash).context("encode cached bookmark icon root")?;
    Ok(format!("htree://{nhash}{}", absolute_tree_path(&icon_path)))
}

async fn fetch_asset(
    client: &Client,
    base_url: &Url,
    asset_url: &Url,
    fetched_urls: &mut HashSet<String>,
    assets: &mut Vec<PwaAsset>,
) -> Vec<Url> {
    if !matches!(asset_url.scheme(), "http" | "https") {
        return Vec::new();
    }
    if !fetched_urls.insert(asset_url.to_string()) {
        return Vec::new();
    }

    let response = match client.get(asset_url.clone()).send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!("Failed to fetch PWA asset {}: {}", asset_url, error);
            return Vec::new();
        }
    };
    let response = match response.error_for_status() {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!("Failed to fetch PWA asset {}: {}", asset_url, error);
            return Vec::new();
        }
    };

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let path = url_to_path(asset_url, base_url);

    if content_type.starts_with("text/css") || path.ends_with(".css") {
        let css = match response.text().await {
            Ok(css) => css,
            Err(error) => {
                tracing::warn!("Failed to read CSS asset {}: {}", asset_url, error);
                return Vec::new();
            }
        };
        let nested_urls = extract_css_urls(&css, asset_url);
        let rewritten_css = rewrite_css_urls(&css, &path, asset_url, base_url);
        assets.push(PwaAsset {
            path,
            data: rewritten_css.into_bytes(),
        });
        return nested_urls;
    }

    if content_type.starts_with("text/html") || path.ends_with(".html") || path.ends_with(".htm") {
        let html = match response.text().await {
            Ok(html) => html,
            Err(error) => {
                tracing::warn!("Failed to read HTML asset {}: {}", asset_url, error);
                return Vec::new();
            }
        };
        let (rewritten_html, nested_urls) =
            rewrite_html_asset_urls(&html, &path, asset_url, base_url);
        assets.push(PwaAsset {
            path,
            data: rewritten_html.into_bytes(),
        });
        return nested_urls;
    }

    match response.bytes().await {
        Ok(bytes) => {
            assets.push(PwaAsset {
                path,
                data: bytes.to_vec(),
            });
        }
        Err(error) => {
            tracing::warn!("Failed to read PWA asset {}: {}", asset_url, error);
        }
    }

    Vec::new()
}

async fn store_pwa_assets(store: &HashtreeStore, assets: &[PwaAsset]) -> Result<Cid> {
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    let mut file_entries = HashMap::new();
    let mut dir_paths = HashSet::from([String::new()]);

    for asset in assets {
        let clean_path = normalize_asset_path(&asset.path);
        if clean_path.is_empty() {
            continue;
        }
        let (cid, size) = tree
            .put(&asset.data)
            .await
            .with_context(|| format!("store asset {}", clean_path))?;

        let (parent, name) = split_parent_and_name(&clean_path);
        dir_paths.extend(parent_chain(&clean_path));
        file_entries.insert(clean_path, (parent, name, cid, size));
    }

    let mut sorted_dirs: Vec<String> = dir_paths.into_iter().collect();
    sorted_dirs.sort_by(|a, b| dir_depth(b).cmp(&dir_depth(a)).then_with(|| a.cmp(b)));

    let mut dir_cids: HashMap<String, Cid> = HashMap::new();
    for dir_path in sorted_dirs {
        let mut entries = Vec::new();

        for (parent, name, cid, size) in file_entries.values() {
            if *parent == dir_path {
                entries.push(DirEntry::from_cid(name.clone(), cid).with_size(*size));
            }
        }

        for (subdir_path, cid) in &dir_cids {
            if parent_path(subdir_path) == dir_path {
                let name = file_name(subdir_path).unwrap_or_else(|| subdir_path.clone());
                entries.push(DirEntry::from_cid(name, cid).with_link_type(LinkType::Dir));
            }
        }

        let cid = tree
            .put_directory(entries)
            .await
            .with_context(|| format!("create directory {}", display_dir(&dir_path)))?;
        dir_cids.insert(dir_path, cid);
    }

    dir_cids
        .remove("")
        .ok_or_else(|| anyhow!("failed to build PWA root directory"))
}

fn manifest_name(manifest: &Value) -> Option<String> {
    manifest
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| manifest.get("short_name").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn manifest_description(manifest: &Value) -> Option<String> {
    manifest
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn manifest_start_reference(manifest: &Value, manifest_url: &Url) -> Option<String> {
    let start_url = manifest.get("start_url")?.as_str()?;
    let resolved = manifest_url.join(start_url).ok()?;
    Some(absolute_tree_reference_for_url(&resolved, manifest_url))
}

fn manifest_app_id(manifest: &Value, manifest_url: &Url) -> Option<String> {
    let raw_id = manifest.get("id")?.as_str()?.trim();
    if raw_id.is_empty() {
        return None;
    }

    Some(
        manifest_url
            .join(raw_id)
            .map(|resolved| resolved.to_string())
            .unwrap_or_else(|_| raw_id.to_string()),
    )
}

fn manifest_display_mode(manifest: &Value) -> Option<String> {
    let display_mode = manifest
        .get("display")?
        .as_str()?
        .trim()
        .to_ascii_lowercase();
    match display_mode.as_str() {
        "browser" | "minimal-ui" | "standalone" | "fullscreen" => Some(display_mode),
        _ => None,
    }
}

fn extract_manifest_resource_urls(manifest: &Value, manifest_url: &Url) -> Vec<Url> {
    let mut urls = Vec::new();

    collect_manifest_url_field(manifest.get("start_url"), manifest_url, &mut urls);
    collect_manifest_icon_like_urls(manifest.get("icons"), manifest_url, &mut urls);
    collect_manifest_icon_like_urls(manifest.get("screenshots"), manifest_url, &mut urls);

    if let Some(shortcuts) = manifest.get("shortcuts").and_then(Value::as_array) {
        for shortcut in shortcuts {
            collect_manifest_url_field(shortcut.get("url"), manifest_url, &mut urls);
            collect_manifest_icon_like_urls(shortcut.get("icons"), manifest_url, &mut urls);
        }
    }

    if let Some(protocol_handlers) = manifest.get("protocol_handlers").and_then(Value::as_array) {
        for handler in protocol_handlers {
            collect_manifest_url_field(handler.get("url"), manifest_url, &mut urls);
        }
    }

    if let Some(file_handlers) = manifest.get("file_handlers").and_then(Value::as_array) {
        for handler in file_handlers {
            collect_manifest_url_field(handler.get("action"), manifest_url, &mut urls);
            collect_manifest_icon_like_urls(handler.get("icons"), manifest_url, &mut urls);
        }
    }

    if let Some(share_target) = manifest.get("share_target") {
        collect_manifest_url_field(share_target.get("action"), manifest_url, &mut urls);
        collect_manifest_url_field(share_target.get("url_template"), manifest_url, &mut urls);
    }

    if let Some(note_taking) = manifest.get("note_taking") {
        collect_manifest_url_field(note_taking.get("new_note_url"), manifest_url, &mut urls);
    }

    if let Some(tab_strip) = manifest.get("tab_strip") {
        if let Some(home_tab) = tab_strip.get("home_tab") {
            collect_manifest_url_field(home_tab.get("url"), manifest_url, &mut urls);
            collect_manifest_icon_like_urls(home_tab.get("icons"), manifest_url, &mut urls);
        }
        if let Some(new_tab_button) = tab_strip.get("new_tab_button") {
            collect_manifest_url_field(new_tab_button.get("url"), manifest_url, &mut urls);
            collect_manifest_icon_like_urls(new_tab_button.get("icons"), manifest_url, &mut urls);
        }
    }

    urls
}

fn pick_manifest_icon_url(manifest: &Value, manifest_url: &Url) -> Option<Url> {
    let icons = manifest.get("icons")?.as_array()?;
    let icon = icons
        .iter()
        .filter_map(|value| {
            let src = value.get("src")?.as_str()?;
            let size = value
                .get("sizes")
                .and_then(Value::as_str)
                .and_then(parse_largest_icon_size)
                .unwrap_or(0);
            Some((src, size))
        })
        .max_by(|(_, left), (_, right)| left.cmp(right))?;
    manifest_url.join(icon.0).ok()
}

fn pick_manifest_icon_path(manifest: &Value, manifest_url: &Url) -> Option<String> {
    let resolved = pick_manifest_icon_url(manifest, manifest_url)?;
    Some(url_to_path(&resolved, manifest_url))
}

fn parse_largest_icon_size(sizes: &str) -> Option<u32> {
    sizes
        .split_whitespace()
        .filter_map(|value| value.split_once('x'))
        .filter_map(|(width, height)| {
            let width = width.parse::<u32>().ok()?;
            let height = height.parse::<u32>().ok()?;
            Some(width.max(height))
        })
        .max()
}

fn rewrite_manifest_urls(manifest: &mut Value, manifest_url: &Url, manifest_path: &str) {
    rewrite_manifest_url_field(manifest, "start_url", manifest_url, manifest_path);
    rewrite_manifest_icon_like_array(manifest, "icons", manifest_url, manifest_path);
    rewrite_manifest_icon_like_array(manifest, "screenshots", manifest_url, manifest_path);

    if let Some(shortcuts) = manifest.get_mut("shortcuts").and_then(Value::as_array_mut) {
        for shortcut in shortcuts {
            rewrite_manifest_url_field(shortcut, "url", manifest_url, manifest_path);
            rewrite_manifest_icon_like_array(shortcut, "icons", manifest_url, manifest_path);
        }
    }

    if let Some(protocol_handlers) = manifest
        .get_mut("protocol_handlers")
        .and_then(Value::as_array_mut)
    {
        for handler in protocol_handlers {
            rewrite_manifest_url_field(handler, "url", manifest_url, manifest_path);
        }
    }

    if let Some(file_handlers) = manifest
        .get_mut("file_handlers")
        .and_then(Value::as_array_mut)
    {
        for handler in file_handlers {
            rewrite_manifest_url_field(handler, "action", manifest_url, manifest_path);
            rewrite_manifest_icon_like_array(handler, "icons", manifest_url, manifest_path);
        }
    }

    if let Some(share_target) = manifest.get_mut("share_target") {
        rewrite_manifest_url_field(share_target, "action", manifest_url, manifest_path);
        rewrite_manifest_url_field(share_target, "url_template", manifest_url, manifest_path);
    }

    if let Some(note_taking) = manifest.get_mut("note_taking") {
        rewrite_manifest_url_field(note_taking, "new_note_url", manifest_url, manifest_path);
    }

    if let Some(tab_strip) = manifest.get_mut("tab_strip") {
        if let Some(home_tab) = tab_strip.get_mut("home_tab") {
            rewrite_manifest_url_field(home_tab, "url", manifest_url, manifest_path);
            rewrite_manifest_icon_like_array(home_tab, "icons", manifest_url, manifest_path);
        }
        if let Some(new_tab_button) = tab_strip.get_mut("new_tab_button") {
            rewrite_manifest_url_field(new_tab_button, "url", manifest_url, manifest_path);
            rewrite_manifest_icon_like_array(new_tab_button, "icons", manifest_url, manifest_path);
        }
    }
}

fn collect_manifest_url_field(value: Option<&Value>, manifest_url: &Url, urls: &mut Vec<Url>) {
    let Some(raw_value) = value.and_then(Value::as_str) else {
        return;
    };
    let Some(resolved) = resolve_resource_url(raw_value, manifest_url) else {
        return;
    };
    urls.push(resolved);
}

fn collect_manifest_icon_like_urls(value: Option<&Value>, manifest_url: &Url, urls: &mut Vec<Url>) {
    let Some(items) = value.and_then(Value::as_array) else {
        return;
    };
    for item in items {
        collect_manifest_url_field(item.get("src"), manifest_url, urls);
    }
}

fn rewrite_manifest_url_field(
    object: &mut Value,
    key: &str,
    manifest_url: &Url,
    manifest_path: &str,
) {
    let Some(value) = object.get_mut(key) else {
        return;
    };
    let Some(raw_value) = value.as_str() else {
        return;
    };
    let Some(resolved) = resolve_resource_url(raw_value, manifest_url) else {
        return;
    };

    *value = Value::String(relative_tree_reference_for_url(
        manifest_path,
        &resolved,
        manifest_url,
    ));
}

fn rewrite_manifest_icon_like_array(
    object: &mut Value,
    key: &str,
    manifest_url: &Url,
    manifest_path: &str,
) {
    let Some(items) = object.get_mut(key).and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        rewrite_manifest_url_field(item, "src", manifest_url, manifest_path);
    }
}

fn relative_tree_reference_for_url(from_path: &str, target_url: &Url, base_url: &Url) -> String {
    let target_path = url_to_path(target_url, base_url);
    let mut relative = relative_tree_reference(from_path, &target_path);
    if let Some(query) = target_url.query() {
        relative.push('?');
        relative.push_str(query);
    }
    if let Some(fragment) = target_url.fragment() {
        relative.push('#');
        relative.push_str(fragment);
    }
    relative
}

fn absolute_tree_reference_for_url(target_url: &Url, base_url: &Url) -> String {
    let target_path = url_to_path(target_url, base_url);
    let mut absolute = absolute_tree_path(&target_path);
    if let Some(query) = target_url.query() {
        absolute.push('?');
        absolute.push_str(query);
    }
    if let Some(fragment) = target_url.fragment() {
        absolute.push('#');
        absolute.push_str(fragment);
    }
    absolute
}

fn rewrite_html_asset_urls(
    html: &str,
    html_path: &str,
    html_url: &Url,
    base_url: &Url,
) -> (String, Vec<Url>) {
    let mut html_rewrites = Vec::new();
    let mut queued_assets = BTreeSet::new();

    for stylesheet in extract_link_references(html, "stylesheet", html_url) {
        let asset_path = url_to_path(&stylesheet.resolved_url, base_url);
        html_rewrites.push((
            stylesheet.raw_value,
            relative_tree_reference(html_path, &asset_path),
        ));
        queued_assets.insert(stylesheet.resolved_url);
    }
    for script in extract_script_references(html, html_url) {
        let asset_path = url_to_path(&script.resolved_url, base_url);
        html_rewrites.push((
            script.raw_value,
            relative_tree_reference(html_path, &asset_path),
        ));
        queued_assets.insert(script.resolved_url);
    }
    for image in extract_image_references(html, html_url) {
        let asset_path = url_to_path(&image.resolved_url, base_url);
        html_rewrites.push((
            image.raw_value,
            relative_tree_reference(html_path, &asset_path),
        ));
        queued_assets.insert(image.resolved_url);
    }

    (
        rewrite_html_urls(html, &html_rewrites),
        queued_assets.into_iter().collect(),
    )
}

fn rewrite_html_urls(html: &str, rewrites: &[(String, String)]) -> String {
    let mut output = html.to_string();
    let mut sorted_rewrites = rewrites.to_vec();
    sorted_rewrites.sort_by(|(left, _), (right, _)| right.len().cmp(&left.len()));
    for (from, to) in sorted_rewrites {
        output = output.replace(&from, &to);
    }
    output
}

fn rewrite_css_urls(css: &str, css_path: &str, css_url: &Url, base_url: &Url) -> String {
    let mut output = String::with_capacity(css.len());
    let mut cursor = 0usize;

    while let Some(found) = css[cursor..].find("url(") {
        let start = cursor + found;
        output.push_str(&css[cursor..start]);
        let mut value_start = start + 4;
        while let Some(ch) = css[value_start..].chars().next() {
            if ch.is_whitespace() {
                value_start += ch.len_utf8();
                continue;
            }
            break;
        }

        let mut quoted = None;
        if let Some(ch) = css[value_start..].chars().next() {
            if ch == '"' || ch == '\'' {
                quoted = Some(ch);
                value_start += ch.len_utf8();
            }
        }

        let mut value_end = value_start;
        while value_end < css.len() {
            let ch = css[value_end..].chars().next().unwrap_or(')');
            if let Some(quote) = quoted {
                if ch == quote {
                    break;
                }
            } else if ch == ')' {
                break;
            }
            value_end += ch.len_utf8();
        }

        let raw_value = css[value_start..value_end].trim();
        let mut after_value = value_end;
        if quoted.is_some() && after_value < css.len() {
            after_value += css[after_value..]
                .chars()
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
        }
        while after_value < css.len() {
            let ch = css[after_value..].chars().next().unwrap_or(')');
            after_value += ch.len_utf8();
            if ch == ')' {
                break;
            }
        }

        if let Some(resolved) = resolve_resource_url(raw_value, css_url) {
            let target_path = url_to_path(&resolved, base_url);
            output.push_str(&format!(
                "url(\"{}\")",
                relative_tree_reference(css_path, &target_path)
            ));
        } else {
            output.push_str(&css[start..after_value]);
        }

        cursor = after_value;
    }

    output.push_str(&css[cursor..]);
    output
}

fn is_http_url(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false)
}

fn looks_like_image_payload(content_type: &str, bytes: &[u8]) -> bool {
    let normalized_content_type = content_type
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if normalized_content_type.starts_with("image/") {
        return true;
    }

    bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
        || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(&[0x00, 0x00, 0x01, 0x00])
        || bytes.starts_with(&[0x00, 0x00, 0x02, 0x00])
        || (bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
        || bytes_trimmed_starts_with_svg(bytes)
}

fn bytes_trimmed_starts_with_svg(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let trimmed = text.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '\u{FEFF}');
    trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && trimmed.contains("<svg"))
}

fn icon_asset_path(icon_url: &Url, content_type: &str, bytes: &[u8]) -> String {
    let mut path = url_to_path(icon_url, icon_url);
    let has_extension = Path::new(&path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| !value.is_empty())
        .unwrap_or(false);

    if !has_extension {
        let extension = infer_icon_extension(content_type, bytes).unwrap_or("bin");
        if path == "index.html" {
            path = format!("icon.{extension}");
        } else {
            path = format!("{path}.{extension}");
        }
    }

    path
}

fn infer_icon_extension(content_type: &str, bytes: &[u8]) -> Option<&'static str> {
    let normalized_content_type = content_type
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if normalized_content_type == "image/png" || bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some("png");
    }
    if normalized_content_type == "image/jpeg" || bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if normalized_content_type == "image/gif"
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
    {
        return Some("gif");
    }
    if normalized_content_type == "image/webp"
        || (bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
    {
        return Some("webp");
    }
    if normalized_content_type == "image/svg+xml" || bytes_trimmed_starts_with_svg(bytes) {
        return Some("svg");
    }
    if normalized_content_type == "image/x-icon"
        || normalized_content_type == "image/vnd.microsoft.icon"
        || bytes.starts_with(&[0x00, 0x00, 0x01, 0x00])
        || bytes.starts_with(&[0x00, 0x00, 0x02, 0x00])
    {
        return Some("ico");
    }
    None
}

fn extract_manifest_reference(html: &str, base_url: &Url) -> Option<AssetReference> {
    extract_tag_attributes(html, "link")
        .into_iter()
        .find(|attrs| rel_contains(attrs, "manifest") && attrs.contains_key("href"))
        .and_then(|attrs| attrs.get("href").cloned())
        .and_then(|href| asset_reference(href, base_url))
}

fn extract_link_references(html: &str, rel: &str, base_url: &Url) -> Vec<AssetReference> {
    extract_tag_attributes(html, "link")
        .into_iter()
        .filter(|attrs| rel_contains(attrs, rel))
        .filter_map(|attrs| attrs.get("href").cloned())
        .filter_map(|href| asset_reference(href, base_url))
        .collect()
}

fn extract_script_references(html: &str, base_url: &Url) -> Vec<AssetReference> {
    extract_tag_attributes(html, "script")
        .into_iter()
        .filter_map(|attrs| attrs.get("src").cloned())
        .filter_map(|src| asset_reference(src, base_url))
        .collect()
}

fn extract_image_references(html: &str, base_url: &Url) -> Vec<AssetReference> {
    extract_tag_attributes(html, "img")
        .into_iter()
        .filter_map(|attrs| attrs.get("src").cloned())
        .filter_map(|src| asset_reference(src, base_url))
        .collect()
}

fn extract_css_urls(css: &str, css_url: &Url) -> Vec<Url> {
    let mut urls = Vec::new();
    let mut cursor = 0usize;

    while let Some(found) = css[cursor..].find("url(") {
        let start = cursor + found + 4;
        let mut value_start = start;
        while let Some(ch) = css[value_start..].chars().next() {
            if ch.is_whitespace() {
                value_start += ch.len_utf8();
                continue;
            }
            break;
        }

        let mut quoted = None;
        if let Some(ch) = css[value_start..].chars().next() {
            if ch == '"' || ch == '\'' {
                quoted = Some(ch);
                value_start += ch.len_utf8();
            }
        }

        let mut value_end = value_start;
        while value_end < css.len() {
            let ch = css[value_end..].chars().next().unwrap_or(')');
            if let Some(quote) = quoted {
                if ch == quote {
                    break;
                }
            } else if ch == ')' {
                break;
            }
            value_end += ch.len_utf8();
        }

        let raw_value = css[value_start..value_end].trim();
        if let Some(resolved) = resolve_resource_url(raw_value, css_url) {
            urls.push(resolved);
        }

        let mut after_value = value_end;
        if quoted.is_some() && after_value < css.len() {
            after_value += css[after_value..]
                .chars()
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
        }
        while after_value < css.len() {
            let ch = css[after_value..].chars().next().unwrap_or(')');
            after_value += ch.len_utf8();
            if ch == ')' {
                break;
            }
        }

        cursor = after_value;
    }

    urls
}

fn extract_tag_attributes(html: &str, tag_name: &str) -> Vec<HashMap<String, String>> {
    let needle = format!("<{}", tag_name.to_ascii_lowercase());
    let lowercase_html = html.to_ascii_lowercase();
    let mut results = Vec::new();
    let mut cursor = 0usize;

    while let Some(found) = lowercase_html[cursor..].find(&needle) {
        let start = cursor + found;
        let end = match find_tag_end(html, start + 1) {
            Some(end) => end,
            None => break,
        };
        let tag_body = &html[start + 1..end];
        if tag_body
            .split_whitespace()
            .next()
            .map(|name| name.eq_ignore_ascii_case(tag_name))
            .unwrap_or(false)
        {
            results.push(parse_attributes(tag_body));
        }
        cursor = end + 1;
    }

    results
}

fn find_tag_end(html: &str, mut cursor: usize) -> Option<usize> {
    let mut quote = None;
    while cursor < html.len() {
        let ch = html[cursor..].chars().next()?;
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == '>' {
            return Some(cursor);
        }
        cursor += ch.len_utf8();
    }
    None
}

fn parse_attributes(tag_body: &str) -> HashMap<String, String> {
    let mut attributes = HashMap::new();
    let mut cursor = tag_body
        .chars()
        .position(char::is_whitespace)
        .unwrap_or(tag_body.len());

    while cursor < tag_body.len() {
        while cursor < tag_body.len() {
            let ch = tag_body[cursor..].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() && ch != '/' {
                break;
            }
            cursor += ch.len_utf8();
        }
        if cursor >= tag_body.len() {
            break;
        }

        let name_start = cursor;
        while cursor < tag_body.len() {
            let ch = tag_body[cursor..].chars().next().unwrap_or(' ');
            if ch.is_whitespace() || ch == '=' || ch == '/' {
                break;
            }
            cursor += ch.len_utf8();
        }
        if name_start == cursor {
            break;
        }

        let name = tag_body[name_start..cursor].to_ascii_lowercase();
        while cursor < tag_body.len()
            && tag_body[cursor..]
                .chars()
                .next()
                .unwrap_or(' ')
                .is_whitespace()
        {
            cursor += tag_body[cursor..]
                .chars()
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(1);
        }

        let mut value = String::new();
        if cursor < tag_body.len() && tag_body[cursor..].starts_with('=') {
            cursor += 1;
            while cursor < tag_body.len()
                && tag_body[cursor..]
                    .chars()
                    .next()
                    .unwrap_or(' ')
                    .is_whitespace()
            {
                cursor += tag_body[cursor..]
                    .chars()
                    .next()
                    .map(|ch| ch.len_utf8())
                    .unwrap_or(1);
            }
            if cursor < tag_body.len() {
                let next = tag_body[cursor..].chars().next().unwrap_or('"');
                if next == '"' || next == '\'' {
                    let quote = next;
                    cursor += quote.len_utf8();
                    let value_start = cursor;
                    while cursor < tag_body.len() {
                        let ch = tag_body[cursor..].chars().next().unwrap_or(quote);
                        if ch == quote {
                            break;
                        }
                        cursor += ch.len_utf8();
                    }
                    value = tag_body[value_start..cursor].to_string();
                    if cursor < tag_body.len() {
                        cursor += quote.len_utf8();
                    }
                } else {
                    let value_start = cursor;
                    while cursor < tag_body.len() {
                        let ch = tag_body[cursor..].chars().next().unwrap_or(' ');
                        if ch.is_whitespace() || ch == '/' {
                            break;
                        }
                        cursor += ch.len_utf8();
                    }
                    value = tag_body[value_start..cursor].to_string();
                }
            }
        }

        attributes.insert(name, value);
    }

    attributes
}

fn rel_contains(attrs: &HashMap<String, String>, token: &str) -> bool {
    attrs
        .get("rel")
        .map(|value| {
            value
                .split_whitespace()
                .any(|part| part.eq_ignore_ascii_case(token))
        })
        .unwrap_or(false)
}

fn asset_reference(raw_value: String, base_url: &Url) -> Option<AssetReference> {
    let resolved_url = resolve_resource_url(&raw_value, base_url)?;
    Some(AssetReference {
        raw_value,
        resolved_url,
    })
}

fn resolve_resource_url(value: &str, base_url: &Url) -> Option<Url> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("data:")
        || trimmed.starts_with("javascript:")
        || trimmed.starts_with("mailto:")
        || trimmed.starts_with("tel:")
    {
        return None;
    }
    let resolved = base_url.join(trimmed).ok()?;
    if matches!(resolved.scheme(), "http" | "https") {
        Some(resolved)
    } else {
        None
    }
}

fn extract_title(html: &str) -> Option<String> {
    let lowercase = html.to_ascii_lowercase();
    let start = lowercase.find("<title>")?;
    let end = lowercase[start + 7..].find("</title>")?;
    let value = html[start + 7..start + 7 + end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn url_to_path(url: &Url, base_url: &Url) -> String {
    let mut path = url.path().trim_start_matches('/').to_string();
    if path.is_empty() || path.ends_with('/') {
        path.push_str("index.html");
    }

    if url.origin() == base_url.origin() {
        return path;
    }

    let host = url.host_str().unwrap_or("external");
    format!("_external/{host}/{path}")
}

fn relative_tree_reference(from_path: &str, target_path: &str) -> String {
    let from_clean = normalize_asset_path(from_path);
    let target_clean = normalize_asset_path(target_path);
    if target_clean.is_empty() {
        return "index.html".to_string();
    }

    let from_parent = parent_path(&from_clean);
    let from_segments: Vec<&str> = from_parent
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let target_segments: Vec<&str> = target_clean
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    let mut common = 0usize;
    while common < from_segments.len()
        && common < target_segments.len()
        && from_segments[common] == target_segments[common]
    {
        common += 1;
    }

    let mut relative_segments: Vec<String> =
        vec!["..".to_string(); from_segments.len().saturating_sub(common)];
    relative_segments.extend(
        target_segments[common..]
            .iter()
            .map(|segment| (*segment).to_string()),
    );

    if relative_segments.is_empty() {
        file_name(&target_clean).unwrap_or_else(|| "index.html".to_string())
    } else {
        relative_segments.join("/")
    }
}

fn root_relative_path(path: &str) -> String {
    let clean = normalize_asset_path(path);
    if clean.is_empty() {
        "/index.html".to_string()
    } else {
        format!("/{}", clean)
    }
}

fn absolute_tree_path(path: &str) -> String {
    root_relative_path(path)
}

fn normalize_asset_path(path: &str) -> String {
    path.trim_matches('/').to_string()
}

fn split_parent_and_name(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

fn parent_chain(path: &str) -> Vec<String> {
    let mut parents = Vec::new();
    let mut current = parent_path(path);
    parents.push(String::new());
    while !current.is_empty() {
        parents.push(current.clone());
        current = parent_path(&current);
    }
    parents
}

fn parent_path(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

fn file_name(path: &str) -> Option<String> {
    path.rsplit('/').next().map(str::to_owned)
}

fn dir_depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.split('/').count()
    }
}

fn display_dir(path: &str) -> &str {
    if path.is_empty() {
        "/"
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_manifest_url_finds_manifest_link() {
        let html = r#"
          <html>
            <head>
              <link rel="manifest" href="/manifest.webmanifest">
            </head>
          </html>
        "#;
        let base_url = Url::parse("https://jumble.social/").unwrap();

        assert_eq!(
            extract_manifest_reference(html, &base_url)
                .unwrap()
                .resolved_url
                .as_str(),
            "https://jumble.social/manifest.webmanifest"
        );
    }

    #[test]
    fn parse_attributes_supports_quoted_values() {
        let attrs = parse_attributes(
            r#"link rel="manifest preload" href='/manifest.webmanifest' crossorigin"#,
        );

        assert_eq!(
            attrs.get("rel").map(String::as_str),
            Some("manifest preload")
        );
        assert_eq!(
            attrs.get("href").map(String::as_str),
            Some("/manifest.webmanifest")
        );
        assert_eq!(attrs.get("crossorigin").map(String::as_str), Some(""));
    }

    #[test]
    fn url_to_path_maps_root_and_trailing_slash_to_index() {
        let base_url = Url::parse("https://jumble.social/").unwrap();
        assert_eq!(
            url_to_path(&Url::parse("https://jumble.social/").unwrap(), &base_url),
            "index.html"
        );
        assert_eq!(
            url_to_path(
                &Url::parse("https://jumble.social/app/").unwrap(),
                &base_url
            ),
            "app/index.html"
        );
        assert_eq!(
            url_to_path(&Url::parse("https://jumble.social/app").unwrap(), &base_url),
            "app"
        );
        assert_eq!(
            url_to_path(
                &Url::parse("https://cdn.example.com/fonts/app.woff2").unwrap(),
                &base_url
            ),
            "_external/cdn.example.com/fonts/app.woff2"
        );
    }

    #[test]
    fn rewrite_css_urls_rewrites_relative_and_absolute_urls_to_root_paths() {
        let css = r#"
          body { background-image: url("../img/bg.png"); }
          @font-face { src: url("https://cdn.example.com/fonts/app.woff2"); }
        "#;
        let css_url = Url::parse("https://jumble.social/assets/main.css").unwrap();

        let rewritten = rewrite_css_urls(
            css,
            "assets/main.css",
            &css_url,
            &Url::parse("https://jumble.social/").unwrap(),
        );

        assert!(rewritten.contains("url(\"../img/bg.png\")"));
        assert!(rewritten.contains("url(\"../_external/cdn.example.com/fonts/app.woff2\")"));
    }

    #[test]
    fn rewrite_html_asset_urls_rewrites_nested_page_dependencies() {
        let html = r#"
          <link rel="stylesheet" href="/assets/main.css">
          <script type="module" src="bundle.js"></script>
          <img src="https://cdn.example.com/logo.png">
        "#;
        let html_url = Url::parse("https://jumble.social/app/index.html").unwrap();
        let base_url = Url::parse("https://jumble.social/").unwrap();

        let (rewritten, mut nested_urls) =
            rewrite_html_asset_urls(html, "app/index.html", &html_url, &base_url);
        nested_urls.sort_by(|left, right| left.as_str().cmp(right.as_str()));

        assert!(rewritten.contains(r#"href="../assets/main.css""#));
        assert!(rewritten.contains(r#"src="bundle.js""#));
        assert!(rewritten.contains(r#"src="../_external/cdn.example.com/logo.png""#));
        assert_eq!(
            nested_urls
                .into_iter()
                .map(|url| url.to_string())
                .collect::<Vec<_>>(),
            vec![
                "https://cdn.example.com/logo.png",
                "https://jumble.social/app/bundle.js",
                "https://jumble.social/assets/main.css",
            ]
        );
    }

    #[test]
    fn manifest_start_reference_preserves_query_and_fragment() {
        let manifest = json!({
            "start_url": "../index.html?source=pwa#home"
        });
        let manifest_url = Url::parse("https://jumble.social/app/manifest.webmanifest").unwrap();

        assert_eq!(
            manifest_start_reference(&manifest, &manifest_url),
            Some("/index.html?source=pwa#home".to_string())
        );
    }

    #[test]
    fn installed_site_pwa_serialization_includes_manifest_metadata() {
        let installed = InstalledSitePwa {
            name: "Example App".to_string(),
            launch_url: "htree://nhash-example/app/index.html".to_string(),
            icon_url: Some("htree://nhash-example/icons/pwa-192.png".to_string()),
            source_app_id: Some("https://example.com/app".to_string()),
            source_url: "https://example.com/app".to_string(),
            source_manifest_url: "https://example.com/manifest.webmanifest".to_string(),
            description: Some("Portable notes".to_string()),
            display_mode: Some("minimal-ui".to_string()),
        };

        let value = serde_json::to_value(installed).unwrap();

        assert_eq!(
            value.get("description").and_then(Value::as_str),
            Some("Portable notes")
        );
        assert_eq!(
            value.get("displayMode").and_then(Value::as_str),
            Some("minimal-ui")
        );
    }

    #[test]
    fn manifest_metadata_extractors_read_description_and_supported_display() {
        let manifest = json!({
            "description": " Portable notes ",
            "display": "minimal-ui"
        });

        assert_eq!(
            manifest_description(&manifest),
            Some("Portable notes".to_string())
        );
        assert_eq!(
            manifest_display_mode(&manifest),
            Some("minimal-ui".to_string())
        );
    }

    #[test]
    fn manifest_display_mode_ignores_unsupported_values() {
        let manifest = json!({
            "display": "window-controls-overlay"
        });

        assert_eq!(manifest_display_mode(&manifest), None);
    }

    #[test]
    fn extract_manifest_resource_urls_collects_nested_manifest_fields() {
        let manifest = json!({
            "start_url": "../index.html?source=pwa#home",
            "icons": [
                {"src": "icons/app.png"},
                {"src": "https://cdn.example.com/icons/maskable.png"}
            ],
            "screenshots": [
                {"src": "shots/hero.png"}
            ],
            "shortcuts": [
                {
                    "url": "../launch/compose.html?mode=quick#composer",
                    "icons": [
                        {"src": "icons/shortcut.png"}
                    ]
                }
            ],
            "protocol_handlers": [
                {"url": "open?uri=placeholder"}
            ],
            "file_handlers": [
                {
                    "action": "/open-file",
                    "icons": [
                        {"src": "icons/file.png"}
                    ]
                }
            ],
            "share_target": {
                "action": "/share/submit?from=manifest"
            },
            "note_taking": {
                "new_note_url": "/notes/new.html"
            },
            "tab_strip": {
                "home_tab": {
                    "url": "/home.html",
                    "icons": [
                        {"src": "icons/home.png"}
                    ]
                },
                "new_tab_button": {
                    "url": "tabs/new.html"
                }
            }
        });
        let manifest_url = Url::parse("https://jumble.social/app/manifest.webmanifest").unwrap();

        let mut urls: Vec<String> = extract_manifest_resource_urls(&manifest, &manifest_url)
            .into_iter()
            .map(|url| url.to_string())
            .collect();
        urls.sort();

        assert_eq!(
            urls,
            vec![
                "https://cdn.example.com/icons/maskable.png",
                "https://jumble.social/app/icons/app.png",
                "https://jumble.social/app/icons/file.png",
                "https://jumble.social/app/icons/home.png",
                "https://jumble.social/app/icons/shortcut.png",
                "https://jumble.social/app/open?uri=placeholder",
                "https://jumble.social/app/shots/hero.png",
                "https://jumble.social/app/tabs/new.html",
                "https://jumble.social/home.html",
                "https://jumble.social/index.html?source=pwa#home",
                "https://jumble.social/launch/compose.html?mode=quick#composer",
                "https://jumble.social/notes/new.html",
                "https://jumble.social/open-file",
                "https://jumble.social/share/submit?from=manifest",
            ]
        );
    }

    #[test]
    fn rewrite_manifest_urls_rewrites_nested_manifest_fields() {
        let mut manifest = json!({
            "start_url": "../index.html?source=pwa#home",
            "icons": [
                {"src": "icons/app.png"},
                {"src": "https://cdn.example.com/icons/maskable.png"}
            ],
            "screenshots": [
                {"src": "shots/hero.png"}
            ],
            "shortcuts": [
                {
                    "url": "../launch/compose.html?mode=quick#composer",
                    "icons": [
                        {"src": "icons/shortcut.png"}
                    ]
                }
            ],
            "protocol_handlers": [
                {"url": "open?uri=placeholder"}
            ],
            "file_handlers": [
                {
                    "action": "/open-file",
                    "icons": [
                        {"src": "icons/file.png"}
                    ]
                }
            ],
            "share_target": {
                "action": "/share/submit?from=manifest"
            },
            "note_taking": {
                "new_note_url": "/notes/new.html"
            },
            "tab_strip": {
                "home_tab": {
                    "url": "/home.html",
                    "icons": [
                        {"src": "icons/home.png"}
                    ]
                },
                "new_tab_button": {
                    "url": "tabs/new.html"
                }
            }
        });
        let manifest_url = Url::parse("https://jumble.social/app/manifest.webmanifest").unwrap();

        rewrite_manifest_urls(&mut manifest, &manifest_url, "app/manifest.webmanifest");

        assert_eq!(
            manifest.get("start_url").and_then(Value::as_str),
            Some("../index.html?source=pwa#home")
        );
        assert_eq!(
            manifest["icons"][0].get("src").and_then(Value::as_str),
            Some("icons/app.png")
        );
        assert_eq!(
            manifest["icons"][1].get("src").and_then(Value::as_str),
            Some("../_external/cdn.example.com/icons/maskable.png")
        );
        assert_eq!(
            manifest["screenshots"][0]
                .get("src")
                .and_then(Value::as_str),
            Some("shots/hero.png")
        );
        assert_eq!(
            manifest["shortcuts"][0].get("url").and_then(Value::as_str),
            Some("../launch/compose.html?mode=quick#composer")
        );
        assert_eq!(
            manifest["shortcuts"][0]["icons"][0]
                .get("src")
                .and_then(Value::as_str),
            Some("icons/shortcut.png")
        );
        assert_eq!(
            manifest["protocol_handlers"][0]
                .get("url")
                .and_then(Value::as_str),
            Some("open?uri=placeholder")
        );
        assert_eq!(
            manifest["file_handlers"][0]
                .get("action")
                .and_then(Value::as_str),
            Some("../open-file")
        );
        assert_eq!(
            manifest["file_handlers"][0]["icons"][0]
                .get("src")
                .and_then(Value::as_str),
            Some("icons/file.png")
        );
        assert_eq!(
            manifest["share_target"]
                .get("action")
                .and_then(Value::as_str),
            Some("../share/submit?from=manifest")
        );
        assert_eq!(
            manifest["note_taking"]
                .get("new_note_url")
                .and_then(Value::as_str),
            Some("../notes/new.html")
        );
        assert_eq!(
            manifest["tab_strip"]["home_tab"]
                .get("url")
                .and_then(Value::as_str),
            Some("../home.html")
        );
        assert_eq!(
            manifest["tab_strip"]["home_tab"]["icons"][0]
                .get("src")
                .and_then(Value::as_str),
            Some("icons/home.png")
        );
        assert_eq!(
            manifest["tab_strip"]["new_tab_button"]
                .get("url")
                .and_then(Value::as_str),
            Some("tabs/new.html")
        );
    }
}
