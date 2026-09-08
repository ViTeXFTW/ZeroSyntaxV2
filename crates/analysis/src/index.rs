//! Cross-file symbol index: which named definitions (objects, weapons, ...)
//! exist across the workspace, and where. Powers reference diagnostics,
//! reference completions, and go-to-definition.
//!
//! The server owns one [`WorkspaceIndex`], updating it per file as documents
//! change ([`WorkspaceIndex::set_file`]).

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zerosyntax_schema::{RefKind, ValueType};
use zerosyntax_syntax::ast::{Block, Field, Module};
use zerosyntax_syntax::{Parse, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::model::{scope_schema, ScopeSchema};
use crate::{Analyzer, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssetKind {
    Audio,
    Texture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAsset {
    pub kind: AssetKind,
    pub name: String,
    pub uri: String,
}

/// Model data discovered from a W3D asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAsset {
    pub name: String,
    pub members: Vec<String>,
}

/// A definition's location within a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    pub span: Span,
}

/// A named definition discovered in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub kind: RefKind,
    pub span: Span,
}

/// A place where a definition is *referenced* (a Reference-typed field value).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceSite {
    pub name: String,
    pub kind: RefKind,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleTagDefinition {
    pub object: String,
    pub name: String,
    pub span: Span,
    #[serde(default)]
    pub is_reference: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelMemberStrictness {
    Off,
    #[default]
    Compatible,
    Strict,
}

/// A definition name's entry: display casing plus all its locations.
struct NameEntry {
    /// The name as first written (for completion display).
    display: String,
    locations: Vec<Location>,
}

struct ModuleTagEntry {
    name: String,
    location: Location,
}

/// Workspace-wide symbol table, grouped by reference kind then name.
///
/// Name lookup is **case-insensitive**, mirroring the engine: shipped game
/// data references `MappedImage SAPathFinder1` as `SAPathfinder1` and the
/// game resolves it.
#[derive(Default)]
pub struct WorkspaceIndex {
    by_kind: HashMap<RefKind, HashMap<String, NameEntry>>,
    /// Reverse map (lowercased names) so a file's entries can be removed.
    files: HashMap<String, Vec<(RefKind, String)>>,
    /// Reference *sites*, keyed (kind, lowercased name) — powers
    /// find-references, rename, and the unused-definition hint. Maintained by
    /// [`set_file_refs`](Self::set_file_refs), independently of definitions
    /// and of [`generation`](Self::generation) (site churn must not
    /// invalidate diagnostics caches on every keystroke).
    sites: HashMap<(RefKind, String), Vec<Location>>,
    /// Reverse map for site removal.
    file_sites: HashMap<String, Vec<(RefKind, String)>>,
    /// Bumped whenever the *name set* changes (not mere span shifts), so
    /// consumers (the per-block diagnostics cache) can invalidate cheaply.
    generation: u64,
    /// Module tags per object (case-insensitive object name key).
    /// Populated from all indexed files. Powers RemoveModule completions.
    object_tags: HashMap<String, Vec<ModuleTagEntry>>,
    /// RemoveModule value sites, keyed by (object_lower, tag_lower).
    object_tag_sites: HashMap<(String, String), Vec<Location>>,
    /// Reverse map: file → (object_lower, tag, is_reference) for re-indexing.
    file_tags: HashMap<String, Vec<(String, String, bool)>>,
    /// String table keys from companion `.str` files, keyed by the INI file URI.
    /// Powers DisplayName completions when a map.str is present.
    ini_str_keys: HashMap<String, Vec<String>>,
    /// W3D model assets, keyed case-insensitively by model name. Each entry
    /// keeps the per-file contributions, so re-indexing or removing one asset
    /// file (e.g. a patch archive overriding a base-game model) never drops
    /// another file's model of the same name.
    model_assets: HashMap<String, Vec<(Arc<str>, ModelAsset)>>,
    /// Reverse map for removing/replacing models contributed by one asset file.
    file_models: HashMap<String, Vec<String>>,
    asset_names: HashMap<AssetKind, HashMap<String, Vec<(String, FileAsset)>>>,
    texture_assets: HashMap<String, Vec<(String, FileAsset)>>,
    file_assets: HashMap<String, Vec<FileAsset>>,
    object_models: HashMap<String, Vec<(String, Vec<String>)>>,
    file_object_models: HashMap<String, Vec<(String, Vec<String>)>>,
    object_parents: HashMap<String, Vec<(String, String)>>,
    file_object_parents: HashMap<String, Vec<(String, String)>>,
    model_member_strictness: ModelMemberStrictness,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// A counter that changes whenever reference resolution could change.
    /// Re-indexing a file with the same definition names does **not** bump it,
    /// so a keystroke that doesn't touch a block header keeps caches warm.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn model_member_strictness(&self) -> ModelMemberStrictness {
        self.model_member_strictness
    }

    pub fn set_model_member_strictness(&mut self, value: ModelMemberStrictness) {
        if self.model_member_strictness != value {
            self.model_member_strictness = value;
            self.generation += 1;
        }
    }

    /// Replace all definitions contributed by `file` with `defs`.
    pub fn set_file(&mut self, file: &str, defs: Vec<Definition>) {
        let names: Vec<(RefKind, String)> = defs
            .iter()
            .map(|d| (d.kind, d.name.to_ascii_lowercase()))
            .collect();
        let changed = match self.files.get(file) {
            Some(old) => *old != names,
            None => !names.is_empty(),
        };
        if changed {
            self.generation += 1;
        }
        self.remove_entries(file);
        for def in defs {
            let entry = self
                .by_kind
                .entry(def.kind)
                .or_default()
                .entry(def.name.to_ascii_lowercase())
                .or_insert_with(|| NameEntry {
                    display: def.name.clone(),
                    locations: Vec::new(),
                });
            entry.locations.push(Location {
                file: file.to_string(),
                span: def.span,
            });
        }
        self.files.insert(file.to_string(), names);
    }

    /// Replace all reference sites contributed by `file`. Does **not** bump
    /// the generation: cross-file consumers of sites (the unused-definition
    /// hint) tolerate staleness until the affected file is next analyzed.
    pub fn set_file_refs(&mut self, file: &str, refs: Vec<ReferenceSite>) {
        self.remove_site_entries(file);
        let mut keys = Vec::with_capacity(refs.len());
        for r in refs {
            let key = (r.kind, r.name.to_ascii_lowercase());
            self.sites.entry(key.clone()).or_default().push(Location {
                file: file.to_string(),
                span: r.span,
            });
            keys.push(key);
        }
        if !keys.is_empty() {
            self.file_sites.insert(file.to_string(), keys);
        }
    }

    /// Drop all definitions contributed by `file`.
    pub fn remove_file(&mut self, file: &str) {
        if self.files.get(file).is_some_and(|v| !v.is_empty())
            || self.file_models.get(file).is_some_and(|v| !v.is_empty())
            || self
                .file_object_models
                .get(file)
                .is_some_and(|v| !v.is_empty())
            || self
                .file_object_parents
                .get(file)
                .is_some_and(|v| !v.is_empty())
            || self.file_tags.get(file).is_some_and(|v| !v.is_empty())
        {
            self.generation += 1;
        }
        self.remove_entries(file);
        self.remove_site_entries(file);
        self.remove_model_entries(file);
        self.set_file_assets(file, Vec::new());
        self.remove_object_model_entries(file);
        self.remove_object_parent_entries(file);
        self.remove_tag_entries(file);
    }

    fn remove_site_entries(&mut self, file: &str) {
        if let Some(keys) = self.file_sites.remove(file) {
            for key in keys {
                if let Some(locs) = self.sites.get_mut(&key) {
                    locs.retain(|l| l.file != file);
                    if locs.is_empty() {
                        self.sites.remove(&key);
                    }
                }
            }
        }
    }

    fn remove_entries(&mut self, file: &str) {
        if let Some(entries) = self.files.remove(file) {
            for (kind, lower) in entries {
                if let Some(names) = self.by_kind.get_mut(&kind) {
                    if let Some(entry) = names.get_mut(&lower) {
                        entry.locations.retain(|l| l.file != file);
                        if entry.locations.is_empty() {
                            names.remove(&lower);
                        }
                    }
                }
            }
        }
    }

    /// Replace W3D model assets contributed by `file`.
    pub fn set_file_models(&mut self, file: &str, models: Vec<ModelAsset>) {
        let changed = match self.file_models.get(file) {
            Some(old) => {
                let old = old
                    .iter()
                    .filter_map(|name| self.model_assets.get(name))
                    .flatten()
                    .filter(|(source, _)| source.as_ref() == file)
                    .map(|(_, model)| model)
                    .collect::<Vec<_>>();
                normalized_model_asset_refs(&old) != normalized_model_assets(&models)
            }
            None => !models.is_empty(),
        };
        if changed {
            self.generation += 1;
        }
        self.remove_model_entries(file);
        let source: Arc<str> = Arc::from(file);
        let mut stored = Vec::with_capacity(models.len());
        for mut model in models {
            dedup_case_insensitive(&mut model.members);
            let lower = model.name.to_ascii_lowercase();
            self.model_assets
                .entry(lower.clone())
                .or_default()
                .push((source.clone(), model));
            stored.push(lower);
        }
        if !stored.is_empty() {
            self.file_models.insert(file.to_string(), stored);
        }
    }

    /// Insert pre-normalized W3D models while constructing a fresh index.
    ///
    /// Unlike [`set_file_models`](Self::set_file_models), this avoids change
    /// detection and member normalization. Callers must supply a file that is
    /// not already present and model members already deduplicated
    /// case-insensitively (the W3D catalog guarantees this).
    pub fn insert_file_models_prepared(&mut self, file: &str, models: Vec<ModelAsset>) {
        debug_assert!(!self.file_models.contains_key(file));
        if models.is_empty() {
            return;
        }
        self.generation += 1;
        let source: Arc<str> = Arc::from(file);
        let mut stored = Vec::with_capacity(models.len());
        for model in models {
            let lower = model.name.to_ascii_lowercase();
            self.model_assets
                .entry(lower.clone())
                .or_default()
                .push((source.clone(), model));
            stored.push(lower);
        }
        self.file_models.insert(file.to_string(), stored);
    }

    fn remove_model_entries(&mut self, file: &str) {
        if let Some(models) = self.file_models.remove(file) {
            for lower in models {
                if let Some(contribs) = self.model_assets.get_mut(&lower) {
                    contribs.retain(|(source, _)| source.as_ref() != file);
                    if contribs.is_empty() {
                        self.model_assets.remove(&lower);
                    }
                }
            }
        }
    }

    /// Replace raw audio/texture assets contributed by a file, directory entry,
    /// or synthetic archive contribution.
    pub fn set_file_assets(&mut self, file: &str, assets: Vec<FileAsset>) {
        let affected = self
            .file_assets
            .get(file)
            .into_iter()
            .flatten()
            .chain(&assets)
            .map(|asset| (asset.kind, asset.name.to_ascii_lowercase()))
            .collect::<std::collections::HashSet<_>>();
        let before = affected
            .iter()
            .map(|(kind, name)| ((*kind, name.clone()), self.is_asset(*kind, name)))
            .collect::<Vec<_>>();
        self.remove_asset_entries(file);
        for asset in &assets {
            self.asset_names
                .entry(asset.kind)
                .or_default()
                .entry(asset.name.to_ascii_lowercase())
                .or_default()
                .push((file.to_string(), asset.clone()));
            if asset.kind == AssetKind::Texture {
                self.texture_assets
                    .entry(asset_stem(&asset.name))
                    .or_default()
                    .push((file.to_string(), asset.clone()));
            }
        }
        if assets.is_empty() {
            self.file_assets.remove(file);
        } else {
            self.file_assets.insert(file.to_string(), assets);
        }
        if before
            .into_iter()
            .any(|((kind, name), existed)| existed != self.is_asset(kind, &name))
        {
            self.generation += 1;
        }
    }

    fn remove_asset_entries(&mut self, file: &str) {
        let Some(assets) = self.file_assets.remove(file) else {
            return;
        };
        for asset in assets {
            let lower = asset.name.to_ascii_lowercase();
            if let Some(names) = self.asset_names.get_mut(&asset.kind) {
                if let Some(contribs) = names.get_mut(&lower) {
                    contribs.retain(|(source, _)| source != file);
                    if contribs.is_empty() {
                        names.remove(&lower);
                    }
                }
                if names.is_empty() {
                    self.asset_names.remove(&asset.kind);
                }
            }
            if asset.kind == AssetKind::Texture {
                let stem = asset_stem(&asset.name);
                if let Some(contribs) = self.texture_assets.get_mut(&stem) {
                    contribs.retain(|(source, _)| source != file);
                    if contribs.is_empty() {
                        self.texture_assets.remove(&stem);
                    }
                }
            }
        }
    }

    pub fn has_assets(&self, kind: AssetKind) -> bool {
        self.asset_names
            .get(&kind)
            .is_some_and(|names| !names.is_empty())
    }

    pub fn is_asset(&self, kind: AssetKind, name: &str) -> bool {
        self.asset_names
            .get(&kind)
            .is_some_and(|names| names.contains_key(&name.to_ascii_lowercase()))
    }

    pub fn asset_names(&self, kind: AssetKind) -> impl Iterator<Item = &str> {
        self.asset_names
            .get(&kind)
            .into_iter()
            .flat_map(|names| names.values().filter_map(|sources| sources.first()))
            .map(|(_, asset)| asset.name.as_str())
    }

    pub fn set_file_object_models(&mut self, file: &str, objects: Vec<(String, Vec<String>)>) {
        let normalized = normalize_object_models(&objects);
        let changed = self.file_object_models.get(file) != Some(&normalized);
        if changed {
            self.generation += 1;
        }
        self.remove_object_model_entries(file);
        for (name, models) in &objects {
            self.object_models
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push((file.to_string(), models.clone()));
        }
        if !normalized.is_empty() {
            self.file_object_models.insert(file.to_string(), normalized);
        }
    }

    fn remove_object_model_entries(&mut self, file: &str) {
        if let Some(objects) = self.file_object_models.remove(file) {
            for (name, _) in objects {
                if let Some(entries) = self.object_models.get_mut(&name) {
                    entries.retain(|(source, _)| source != file);
                    if entries.is_empty() {
                        self.object_models.remove(&name);
                    }
                }
            }
        }
    }

    pub fn set_file_object_parents(&mut self, file: &str, parents: Vec<(String, String)>) {
        let normalized = parents
            .iter()
            .map(|(child, parent)| (child.to_ascii_lowercase(), parent.to_ascii_lowercase()))
            .collect::<Vec<_>>();
        if self.file_object_parents.get(file) != Some(&normalized) {
            self.generation += 1;
        }
        self.remove_object_parent_entries(file);
        for (child, parent) in &parents {
            self.object_parents
                .entry(child.to_ascii_lowercase())
                .or_default()
                .push((file.to_string(), parent.clone()));
        }
        if !normalized.is_empty() {
            self.file_object_parents
                .insert(file.to_string(), normalized);
        }
    }

    fn remove_object_parent_entries(&mut self, file: &str) {
        if let Some(parents) = self.file_object_parents.remove(file) {
            for (child, _) in parents {
                if let Some(entries) = self.object_parents.get_mut(&child) {
                    entries.retain(|(source, _)| source != file);
                    if entries.is_empty() {
                        self.object_parents.remove(&child);
                    }
                }
            }
        }
    }

    pub fn models_for_object<'a>(&'a self, name: &str) -> Vec<&'a str> {
        let mut out = Vec::new();
        let mut pending = vec![name.to_ascii_lowercase()];
        let mut seen = std::collections::HashSet::new();
        while let Some(object) = pending.pop() {
            if !seen.insert(object.clone()) {
                continue;
            }
            if let Some(entries) = self.object_models.get(&object) {
                out.extend(
                    entries
                        .iter()
                        .flat_map(|(_, models)| models.iter().map(String::as_str)),
                );
            }
            if let Some(parents) = self.object_parents.get(&object) {
                pending.extend(
                    parents
                        .iter()
                        .map(|(_, parent)| parent.to_ascii_lowercase()),
                );
            }
        }
        out
    }

    /// Replace module-tag entries contributed by `file`.
    /// Called alongside `set_file` so RemoveModule completions stay current.
    pub fn set_file_tags(&mut self, file: &str, tags: Vec<ModuleTagDefinition>) {
        let entries = tags
            .iter()
            .map(|tag| {
                (
                    tag.object.to_ascii_lowercase(),
                    tag.name.clone(),
                    tag.is_reference,
                )
            })
            .collect::<Vec<_>>();
        let normalized = normalize_object_tags(
            &entries
                .iter()
                .filter(|(_, _, is_reference)| !is_reference)
                .map(|(object, name, _)| (object.clone(), name.clone()))
                .collect::<Vec<_>>(),
        );
        if self
            .file_tags
            .get(file)
            .map(|old| {
                normalize_object_tags(
                    &old.iter()
                        .filter(|(_, _, is_reference)| !is_reference)
                        .map(|(object, name, _)| (object.clone(), name.clone()))
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default()
            != normalized
        {
            self.generation += 1;
        }
        self.remove_tag_entries(file);
        for tag in tags {
            let object = tag.object.to_ascii_lowercase();
            let location = Location {
                file: file.to_string(),
                span: tag.span,
            };
            if tag.is_reference {
                self.object_tag_sites
                    .entry((object, tag.name.to_ascii_lowercase()))
                    .or_default()
                    .push(location);
            } else {
                self.object_tags
                    .entry(object)
                    .or_default()
                    .push(ModuleTagEntry {
                        name: tag.name,
                        location,
                    });
            }
        }
        if !entries.is_empty() {
            self.file_tags.insert(file.to_string(), entries);
        }
    }

    fn remove_tag_entries(&mut self, file: &str) {
        if let Some(old) = self.file_tags.remove(file) {
            for (object, name, _) in old {
                if let Some(tags) = self.object_tags.get_mut(&object) {
                    tags.retain(|tag| tag.location.file != file);
                    if tags.is_empty() {
                        self.object_tags.remove(&object);
                    }
                }
                let key = (object, name.to_ascii_lowercase());
                if let Some(sites) = self.object_tag_sites.get_mut(&key) {
                    sites.retain(|site| site.file != file);
                    if sites.is_empty() {
                        self.object_tag_sites.remove(&key);
                    }
                }
            }
        }
    }

    /// Module tags defined on `object_name` (case-insensitive) across all
    /// indexed files. Used to populate RemoveModule value completions.
    pub fn module_tags_for_object<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        self.object_tags
            .get(&name.to_ascii_lowercase())
            .into_iter()
            .flat_map(|tags| tags.iter().map(|tag| tag.name.as_str()))
    }

    pub fn module_tag_locations<'a>(&'a self, object: &str, tag: &str) -> Vec<&'a Location> {
        self.object_tags
            .get(&object.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter(|entry| entry.name.eq_ignore_ascii_case(tag))
            .map(|entry| &entry.location)
            .collect()
    }

    pub fn module_tag_reference_locations<'a>(
        &'a self,
        object: &str,
        tag: &str,
    ) -> Vec<&'a Location> {
        self.object_tag_sites
            .get(&(object.to_ascii_lowercase(), tag.to_ascii_lowercase()))
            .into_iter()
            .flatten()
            .collect()
    }

    pub fn is_new_override_object(&self, name: &str, file: &str) -> bool {
        is_override_layer(file)
            && !self
                .locations(RefKind::Object, name)
                .iter()
                .any(|location| !is_override_layer(&location.file))
    }

    /// Module tags visible to RemoveModule. New map/solo objects start as a
    /// copy of DefaultThingTemplate, while existing objects use their own tags.
    pub fn effective_module_tags_for_object<'a>(
        &'a self,
        name: &str,
        file: Option<&str>,
        before: Option<u32>,
    ) -> Vec<&'a str> {
        let mut out = self
            .object_tags
            .get(&name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter(|tag| {
                !file.zip(before).is_some_and(|(file, before)| {
                    tag.location.file == file && tag.location.span.start >= before
                })
            })
            .map(|tag| tag.name.as_str())
            .collect::<Vec<_>>();
        let is_new_override = file.is_some_and(|file| self.is_new_override_object(name, file));
        if is_new_override {
            out.extend(self.module_tags_for_object("DefaultThingTemplate"));
            let mut seen = std::collections::HashSet::new();
            out.retain(|tag| seen.insert(tag.to_ascii_lowercase()));
        }
        out
    }

    pub fn effective_module_tag_locations<'a>(
        &'a self,
        object: &str,
        tag: &str,
        file: Option<&str>,
        before: Option<u32>,
    ) -> Vec<&'a Location> {
        let mut out = self
            .module_tag_locations(object, tag)
            .into_iter()
            .filter(|location| {
                !file.zip(before).is_some_and(|(file, before)| {
                    location.file == file && location.span.start >= before
                })
            })
            .collect::<Vec<_>>();
        if out.is_empty() && file.is_some_and(|file| self.is_new_override_object(object, file)) {
            out = self.module_tag_locations("DefaultThingTemplate", tag);
        }
        out
    }

    /// Store string table keys parsed from the `.str` file co-located with `ini_file`.
    /// An empty `keys` list removes the entry.
    pub fn set_ini_string_keys(&mut self, ini_file: &str, keys: Vec<String>) {
        if keys.is_empty() {
            self.ini_str_keys.remove(ini_file);
        } else {
            self.ini_str_keys.insert(ini_file.to_string(), keys);
        }
    }

    /// String table keys available to `ini_file` from its companion `.str` file.
    pub fn string_keys_for_ini<'a>(&'a self, ini_file: &str) -> impl Iterator<Item = &'a str> {
        self.ini_str_keys
            .get(ini_file)
            .into_iter()
            .flat_map(|keys| keys.iter().map(|k| k.as_str()))
    }

    /// Whether any W3D model assets have been indexed.
    pub fn has_model_assets(&self) -> bool {
        !self.model_assets.is_empty()
    }

    /// Is a W3D model known from indexed assets?
    pub fn is_model_asset(&self, name: &str) -> bool {
        self.model_assets.contains_key(&name.to_ascii_lowercase())
    }

    /// Known W3D model names, in display casing.
    pub fn model_names(&self) -> impl Iterator<Item = &str> {
        self.model_assets
            .values()
            .filter_map(|contribs| contribs.last())
            .map(|(_, m)| m.name.as_str())
    }

    /// Last indexed contributor wins: base roots are indexed first and
    /// workspace roots last.
    pub fn effective_model_source(&self, model: &str) -> Option<&str> {
        self.model_assets
            .get(&model.to_ascii_lowercase())
            .and_then(|contribs| contribs.last())
            .map(|(source, _)| source.as_ref())
    }

    pub fn effective_texture_source(&self, texture: &str) -> Option<&FileAsset> {
        self.texture_assets
            .get(&asset_stem(texture))
            .and_then(|contribs| contribs.last())
            .map(|(_, asset)| asset)
    }

    /// User-addressable members (pivots/subobjects/meshes) for `model`,
    /// across every file that contributes the model. May repeat a member
    /// when several files define the same model; callers dedup or use `any`.
    pub fn model_members<'a>(&'a self, model: &str) -> impl Iterator<Item = &'a str> {
        self.model_assets
            .get(&model.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .flat_map(|(_, m)| m.members.iter().map(|b| b.as_str()))
    }

    /// Is `name` defined for `kind` anywhere in the workspace?
    /// Case-insensitive, like the engine's own name lookups.
    pub fn is_defined(&self, kind: RefKind, name: &str) -> bool {
        self.by_kind
            .get(&kind)
            .map(|n| n.contains_key(&name.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    /// All definition locations for `name` of `kind` (case-insensitive).
    pub fn locations(&self, kind: RefKind, name: &str) -> &[Location] {
        self.by_kind
            .get(&kind)
            .and_then(|n| n.get(&name.to_ascii_lowercase()))
            .map(|e| e.locations.as_slice())
            .unwrap_or(&[])
    }

    /// All known names for a kind (for reference completion), in their
    /// originally-written casing.
    pub fn names(&self, kind: RefKind) -> impl Iterator<Item = &str> {
        self.by_kind
            .get(&kind)
            .into_iter()
            .flat_map(|n| n.values().map(|e| e.display.as_str()))
    }

    /// Names that have at least one definition outside a map/solo override
    /// layer. These are valid targets for a map Object override; names that
    /// exist solely in an override layer are new map-local templates instead.
    pub fn override_target_names(&self, kind: RefKind) -> impl Iterator<Item = &str> {
        self.by_kind
            .get(&kind)
            .into_iter()
            .flat_map(|names| names.values())
            .filter(|entry| {
                entry
                    .locations
                    .iter()
                    .any(|loc| !is_override_layer(&loc.file))
            })
            .map(|entry| entry.display.as_str())
    }

    /// All reference sites for `name` of `kind` (case-insensitive).
    pub fn reference_sites(&self, kind: RefKind, name: &str) -> &[Location] {
        self.sites
            .get(&(kind, name.to_ascii_lowercase()))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Is `name` of `kind` referenced anywhere in the workspace?
    pub fn is_referenced(&self, kind: RefKind, name: &str) -> bool {
        !self.reference_sites(kind, name).is_empty()
    }

    /// Workspace symbols whose name contains `query` (case-insensitive; an
    /// empty query matches everything). Yields the display-cased name with
    /// each of its definition locations.
    pub fn symbols<'a>(
        &'a self,
        query: &'a str,
    ) -> impl Iterator<Item = (RefKind, &'a str, &'a Location)> + 'a {
        let q = query.to_ascii_lowercase();
        self.by_kind.iter().flat_map(move |(kind, names)| {
            let q = q.clone();
            names
                .iter()
                .filter(move |(lower, _)| q.is_empty() || lower.contains(&q))
                .flat_map(|(_, entry)| {
                    entry
                        .locations
                        .iter()
                        .map(|loc| (*kind, entry.display.as_str(), loc))
                })
        })
    }
}

fn dedup_case_insensitive(values: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    values.retain(|value| seen.insert(value.to_ascii_lowercase()));
}

fn normalized_model_assets(models: &[ModelAsset]) -> Vec<(String, Vec<String>)> {
    normalized_model_asset_refs(&models.iter().collect::<Vec<_>>())
}

fn normalized_model_asset_refs(models: &[&ModelAsset]) -> Vec<(String, Vec<String>)> {
    let mut out = models
        .iter()
        .map(|model| {
            let mut members = model
                .members
                .iter()
                .map(|member| member.to_ascii_lowercase())
                .collect::<Vec<_>>();
            members.sort();
            members.dedup();
            (model.name.to_ascii_lowercase(), members)
        })
        .collect::<Vec<_>>();
    out.sort();
    out
}

fn asset_stem(name: &str) -> String {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    file.rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file)
        .to_ascii_lowercase()
}

fn normalize_object_models(objects: &[(String, Vec<String>)]) -> Vec<(String, Vec<String>)> {
    let mut out = objects
        .iter()
        .map(|(name, models)| {
            let mut models = models
                .iter()
                .map(|m| m.to_ascii_lowercase())
                .collect::<Vec<_>>();
            models.sort();
            models.dedup();
            (name.to_ascii_lowercase(), models)
        })
        .collect::<Vec<_>>();
    out.sort();
    out
}

fn normalize_object_tags(tags: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = tags
        .iter()
        .map(|(object, tag)| (object.to_ascii_lowercase(), tag.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    out.sort();
    out
}

fn is_override_layer(file: &str) -> bool {
    file.rsplit(['/', '\\']).next().is_some_and(|name| {
        name.eq_ignore_ascii_case("map.ini") || name.eq_ignore_ascii_case("solo.ini")
    })
}

/// Collect the W3D models declared below every Object definition.
pub fn object_models_in(analyzer: &Analyzer, parse: &Parse) -> Vec<(String, Vec<String>)> {
    parse
        .syntax()
        .children()
        .filter_map(|node| {
            let block = Block(node.clone());
            if !block.keyword()?.text().eq_ignore_ascii_case("Object") {
                return None;
            }
            let name = block.name()?.text().to_string();
            let mut models = Vec::new();
            collect_object_models(analyzer, &node, &mut models);
            dedup_case_insensitive(&mut models);
            Some((name, models))
        })
        .collect()
}

pub fn object_parents_in(parse: &Parse) -> Vec<(String, String)> {
    parse
        .syntax()
        .children()
        .filter_map(|node| {
            let block = Block(node);
            if !block.keyword()?.text().eq_ignore_ascii_case("Object") {
                return None;
            }
            Some((
                block.name()?.text().to_string(),
                block.parent_name()?.text().to_string(),
            ))
        })
        .collect()
}

fn collect_object_models(analyzer: &Analyzer, node: &SyntaxNode, out: &mut Vec<String>) {
    let scope = scope_schema(analyzer, node);
    for child in node.children() {
        match child.kind() {
            SyntaxKind::FIELD => {
                let field = Field(child);
                let Some(schema_field) = field.key().and_then(|key| scope.field(key.text())) else {
                    continue;
                };
                let values = field.value_tokens();
                match schema_field.value_type {
                    ValueType::W3dModelList => out.extend(
                        values
                            .iter()
                            .map(|value| value.text().trim_matches('"').to_string()),
                    ),
                    ValueType::W3dModel => {
                        if let Some(value) = values.first() {
                            out.push(value.text().trim_matches('"').to_string());
                        }
                    }
                    _ => {}
                }
            }
            SyntaxKind::BLOCK | SyntaxKind::MODULE => collect_object_models(analyzer, &child, out),
            _ => {}
        }
    }
}

/// Collect every reference site in a parsed document: each value token of a
/// `Reference`/`ReferenceList`-typed field (including reference elements of
/// `token_list` fields). Null sentinels (`None`, audio `NoSound`) and engine
/// builtins are not sites — nothing to navigate to or rename.
pub fn references_in(analyzer: &Analyzer, parse: &Parse) -> Vec<ReferenceSite> {
    let mut out = Vec::new();
    for node in parse.syntax().children() {
        if node.kind() == SyntaxKind::BLOCK {
            collect_refs(analyzer, &node, &mut out);
        }
    }
    out
}

fn collect_refs(analyzer: &Analyzer, node: &SyntaxNode, out: &mut Vec<ReferenceSite>) {
    let scope = scope_schema(analyzer, node);
    for child in node.children() {
        match child.kind() {
            SyntaxKind::FIELD => collect_field_refs(analyzer, &child, &scope, out),
            SyntaxKind::MODULE | SyntaxKind::BLOCK => collect_refs(analyzer, &child, out),
            _ => {}
        }
    }
}

fn collect_field_refs(
    analyzer: &Analyzer,
    node: &SyntaxNode,
    scope: &ScopeSchema,
    out: &mut Vec<ReferenceSite>,
) {
    let field = Field(node.clone());
    let Some(key) = field.key() else { return };
    let Some(schema_field) = scope.field(key.text()) else {
        return;
    };
    let tokens = field.value_tokens();
    let mut push = |kind: RefKind, name: &str, span: Span| {
        if name.is_empty()
            || name.eq_ignore_ascii_case("None")
            || (kind == RefKind::AudioEvent && name.eq_ignore_ascii_case("NoSound"))
            || analyzer.is_builtin(kind, name)
        {
            return;
        }
        out.push(ReferenceSite {
            name: name.to_string(),
            kind,
            span,
        });
    };
    collect_refs_from_type(&schema_field.value_type, &tokens, &mut push);
}

fn collect_refs_from_type(
    ty: &ValueType,
    tokens: &[SyntaxToken],
    push: &mut impl FnMut(RefKind, &str, Span),
) {
    match ty {
        ValueType::OneOf { .. } => {
            if let Some(variant) =
                ty.variant_for_first_token(tokens.first().map(|t| t.text().trim_matches('"')))
            {
                collect_refs_from_type(variant, tokens, push);
            }
        }
        ValueType::Reference { ref_kind } => {
            if let Some(tok) = tokens.first() {
                push(
                    *ref_kind,
                    tok.text().trim_matches('"'),
                    tok.text_range().into(),
                );
            }
        }
        ValueType::ReferenceList { ref_kind } => {
            for tok in tokens {
                push(
                    *ref_kind,
                    tok.text().trim_matches('"'),
                    tok.text_range().into(),
                );
            }
        }
        ValueType::TokenList { .. } | ValueType::Prefixed { .. } => {
            let input = tokens
                .iter()
                .map(|token| token.text().trim_matches('"'))
                .collect::<Vec<_>>();
            for (index, tok) in tokens.iter().enumerate() {
                let Some(spec) = ty.token_type_at_input(&input, index) else {
                    continue;
                };
                match spec {
                    ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => {
                        push(
                            *ref_kind,
                            tok.text().trim_matches('"'),
                            tok.text_range().into(),
                        );
                    }
                    ValueType::Prefixed { prefix, value_type } => {
                        if let ValueType::Reference { ref_kind }
                        | ValueType::ReferenceList { ref_kind } = value_type.as_ref()
                        {
                            let text = tok.text().trim_matches('"');
                            let Some((actual, name)) = text.split_once(':') else {
                                continue;
                            };
                            if !actual.eq_ignore_ascii_case(prefix) {
                                continue;
                            }
                            if name.is_empty() {
                                continue;
                            }
                            let start = u32::from(tok.text_range().start())
                                + u32::from(tok.text().starts_with('"'))
                                + actual.len() as u32
                                + 1;
                            push(
                                *ref_kind,
                                name,
                                Span {
                                    start,
                                    end: start + name.len() as u32,
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Collect the named definitions a parsed document contributes (top-level
/// blocks whose keyword `defines` a reference kind).
pub fn definitions_in(analyzer: &Analyzer, parse: &Parse, _file: &str) -> Vec<Definition> {
    let mut out = Vec::new();
    let root = parse.syntax();
    for node in root.children().filter(|n| n.kind() == SyntaxKind::BLOCK) {
        let block = Block(node.clone());
        let Some(keyword) = block.keyword() else {
            continue;
        };
        let Some(schema_block) = analyzer.block(keyword.text()) else {
            continue;
        };
        let Some(kind) = schema_block.defines else {
            continue;
        };
        if let Some(name) = block.name() {
            out.push(Definition {
                name: name.text().to_string(),
                kind,
                span: name.text_range().into(),
            });
        }
    }
    out
}

/// Collect module-tag declarations and RemoveModule reference sites from all
/// Object blocks in a parsed file.
pub fn module_tags_in(_analyzer: &Analyzer, parse: &Parse) -> Vec<ModuleTagDefinition> {
    let mut out = Vec::new();
    for node in parse.syntax().children() {
        if node.kind() != SyntaxKind::BLOCK {
            continue;
        }
        let block = Block(node.clone());
        let Some(kw) = block.keyword() else { continue };
        if !kw.text().eq_ignore_ascii_case("Object") {
            continue;
        }
        let Some(name) = block.name() else { continue };
        let name_lower = name.text().to_ascii_lowercase();
        for child in node.children().filter(|n| n.kind() == SyntaxKind::MODULE) {
            if let Some(tag) = Module(child).tag() {
                out.push(ModuleTagDefinition {
                    object: name_lower.clone(),
                    name: tag.text().to_string(),
                    span: tag.text_range().into(),
                    is_reference: false,
                });
            }
        }
        for field in block.fields() {
            if !field
                .key()
                .is_some_and(|key| key.text().eq_ignore_ascii_case("RemoveModule"))
            {
                continue;
            }
            if let Some(tag) = field.value_tokens().first() {
                out.push(ModuleTagDefinition {
                    object: name_lower.clone(),
                    name: tag.text().trim_matches('"').to_string(),
                    span: tag.text_range().into(),
                    is_reference: true,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_named_definitions() {
        let a = Analyzer::embedded();
        let src = "Weapon AK47\nEnd\nObject Tank\nEnd\n";
        let parse = a.parse(src);
        let defs = definitions_in(&a, &parse, "f.ini");
        let mut idx = WorkspaceIndex::new();
        idx.set_file("f.ini", defs);
        assert!(idx.is_defined(RefKind::Weapon, "AK47"));
        assert!(idx.is_defined(RefKind::Object, "Tank"));
        // The engine resolves names case-insensitively (shipped data relies
        // on it: `MappedImage SAPathFinder1` vs `ButtonImage = SAPathfinder1`).
        assert!(idx.is_defined(RefKind::Weapon, "ak47"));
        assert!(idx.is_defined(RefKind::Object, "TANK"));
        assert!(!idx.is_defined(RefKind::Weapon, "Nonexistent"));
        // Completion shows the original casing.
        assert!(idx.names(RefKind::Weapon).any(|n| n == "AK47"));
    }

    #[test]
    fn generation_tracks_name_changes_only() {
        let a = Analyzer::embedded();
        let mut idx = WorkspaceIndex::new();
        let g0 = idx.generation();
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("Weapon AK47\nEnd\n"), "f.ini"),
        );
        let g1 = idx.generation();
        assert_ne!(g0, g1, "new definitions bump the generation");

        // Same names at shifted spans (e.g. a comment typed above): no bump.
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("; c\nWeapon AK47\nEnd\n"), "f.ini"),
        );
        assert_eq!(idx.generation(), g1);

        // Renaming a definition bumps.
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("Weapon M16\nEnd\n"), "f.ini"),
        );
        assert_ne!(idx.generation(), g1);

        // A file with no definitions, set repeatedly: no bumps after removal.
        let g2 = idx.generation();
        idx.set_file("empty.ini", vec![]);
        assert_eq!(idx.generation(), g2);
        idx.remove_file("empty.ini");
        assert_eq!(idx.generation(), g2);
    }

    #[test]
    fn module_tags_invalidate_diagnostics_and_remove_per_file() {
        let mut idx = WorkspaceIndex::new();
        let tag = vec![ModuleTagDefinition {
            object: "tank".into(),
            name: "ModuleTag_Physics".into(),
            span: Span::new(0, 17),
            is_reference: false,
        }];
        let g0 = idx.generation();
        idx.set_file_tags("base.ini", tag.clone());
        assert_ne!(idx.generation(), g0);

        idx.set_file_tags("patch.ini", tag);
        idx.remove_file("base.ini");
        assert_eq!(
            idx.module_tags_for_object("Tank").collect::<Vec<_>>(),
            vec!["ModuleTag_Physics"]
        );
        assert_eq!(
            idx.module_tag_locations("Tank", "ModuleTag_Physics")[0].file,
            "patch.ini"
        );
        idx.remove_file("patch.ini");
        assert_eq!(idx.module_tags_for_object("Tank").count(), 0);
    }

    #[test]
    fn collects_and_stores_reference_sites() {
        let a = Analyzer::embedded();
        let src = "Object Tank\n  Behavior = StatusBitsUpgrade ModuleTag_01\n    TriggeredBy = Upgrade_A Upgrade_B\n    FXListUpgrade = None\n  End\nEnd\n";
        let parse = a.parse(src);
        let refs = references_in(&a, &parse);
        // Both ReferenceList tokens are sites; `None` is not.
        assert_eq!(refs.len(), 2, "{refs:?}");
        assert!(refs.iter().all(|r| r.kind == RefKind::Upgrade));

        let mut idx = WorkspaceIndex::new();
        let g0 = idx.generation();
        idx.set_file_refs("f.ini", refs);
        assert_eq!(idx.generation(), g0, "sites must not bump the generation");
        assert!(idx.is_referenced(RefKind::Upgrade, "upgrade_a"));
        assert_eq!(idx.reference_sites(RefKind::Upgrade, "Upgrade_B").len(), 1);
        idx.set_file_refs("f.ini", Vec::new());
        assert!(!idx.is_referenced(RefKind::Upgrade, "Upgrade_A"));
    }

    #[test]
    fn object_models_follow_inheritance() {
        let a = Analyzer::embedded();
        let parse = a.parse("Object Parent\n  Draw = W3DModelDraw Tag\n    DefaultConditionState\n      Model = ParentModel\n    End\n  End\nEnd\nObject Child Parent\nEnd\n");
        let mut idx = WorkspaceIndex::new();
        idx.set_file_object_models("objects.ini", object_models_in(&a, &parse));
        idx.set_file_object_parents("objects.ini", object_parents_in(&parse));
        assert_eq!(idx.models_for_object("child"), vec!["ParentModel"]);
    }

    #[test]
    fn raw_assets_are_case_insensitive_and_track_effective_names() {
        let audio = |name: &str| FileAsset {
            kind: AssetKind::Audio,
            name: name.into(),
            uri: format!("file:///{name}"),
        };
        let mut idx = WorkspaceIndex::new();
        idx.set_file_assets("base", vec![audio("Click.WAV")]);
        let first = idx.generation();
        assert!(idx.is_asset(AssetKind::Audio, "click.wav"));

        idx.set_file_assets("mod", vec![audio("CLICK.wav")]);
        assert_eq!(
            idx.generation(),
            first,
            "duplicate contributor changes no effective name"
        );
        idx.remove_file("base");
        assert_eq!(
            idx.generation(),
            first,
            "removing one duplicate preserves the name"
        );
        idx.set_file_assets("mod", vec![audio("Other.wav")]);
        assert_ne!(idx.generation(), first);
        assert!(!idx.is_asset(AssetKind::Audio, "Click.wav"));
        assert!(idx.is_asset(AssetKind::Audio, "OTHER.WAV"));
    }

    #[test]
    fn effective_model_and_texture_use_last_contributor() {
        let texture = |name: &str, uri: &str| FileAsset {
            kind: AssetKind::Texture,
            name: name.into(),
            uri: uri.into(),
        };
        let mut idx = WorkspaceIndex::new();
        idx.set_file_models(
            "base.w3d",
            vec![ModelAsset {
                name: "Tank".into(),
                members: Vec::new(),
            }],
        );
        idx.set_file_models(
            "mod.w3d",
            vec![ModelAsset {
                name: "TANK".into(),
                members: Vec::new(),
            }],
        );
        idx.set_file_assets(
            "base.big",
            vec![texture("Tank.dds", "big:///base.big!/Tank.dds")],
        );
        idx.set_file_assets(
            "workspace",
            vec![texture("tank.tga", "file:///workspace/tank.tga")],
        );
        assert_eq!(idx.effective_model_source("tank"), Some("mod.w3d"));
        assert_eq!(
            idx.effective_texture_source("TANK.DDS")
                .map(|asset| asset.uri.as_str()),
            Some("file:///workspace/tank.tga")
        );
    }

    #[test]
    fn split_prefixed_reference_site_span_excludes_prefix() {
        let a = Analyzer::embedded();
        let src = "Object Tank\n  Behavior = TransitionDamageFX ModuleTag_01\n    DamagedParticleSystem1 = Bone: NONE RandomBone: No PSys: MissingParticle\n  End\nEnd\n";
        let refs = references_in(&a, &a.parse(src));
        let reference = refs
            .iter()
            .find(|reference| reference.name == "MissingParticle")
            .unwrap();
        assert_eq!(reference.kind, RefKind::ParticleSystem);
        assert_eq!(
            &src[reference.span.start as usize..reference.span.end as usize],
            "MissingParticle"
        );
    }

    #[test]
    fn workspace_symbols_match_by_substring() {
        let a = Analyzer::embedded();
        let mut idx = WorkspaceIndex::new();
        idx.set_file(
            "f.ini",
            definitions_in(
                &a,
                &a.parse("Weapon AK47\nEnd\nObject Tank\nEnd\n"),
                "f.ini",
            ),
        );
        let hits: Vec<_> = idx.symbols("ak").collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "AK47", "display casing is preserved");
        assert_eq!(idx.symbols("").count(), 2, "empty query matches all");
    }

    #[test]
    fn updating_a_file_replaces_its_symbols() {
        let a = Analyzer::embedded();
        let mut idx = WorkspaceIndex::new();
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("Weapon Old\nEnd\n"), "f.ini"),
        );
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("Weapon New\nEnd\n"), "f.ini"),
        );
        assert!(!idx.is_defined(RefKind::Weapon, "Old"));
        assert!(idx.is_defined(RefKind::Weapon, "New"));
    }

    #[test]
    fn model_asset_member_changes_bump_generation() {
        let mut idx = WorkspaceIndex::new();
        let original = vec![ModelAsset {
            name: "Tank".into(),
            members: vec!["Tire01".into()],
        }];
        idx.set_file_models("model.w3d", original.clone());
        let g1 = idx.generation();
        idx.set_file_models("model.w3d", original);
        assert_eq!(idx.generation(), g1);

        idx.set_file_models(
            "model.w3d",
            vec![ModelAsset {
                name: "Tank".into(),
                members: vec!["Tire02".into()],
            }],
        );
        assert_ne!(idx.generation(), g1);
    }

    #[test]
    fn prepared_model_insert_supports_lookup_override_and_removal() {
        let mut idx = WorkspaceIndex::new();
        idx.insert_file_models_prepared(
            "base.w3d",
            vec![ModelAsset {
                name: "Tank".into(),
                members: vec!["Tire01".into()],
            }],
        );
        idx.insert_file_models_prepared(
            "patch.w3d",
            vec![ModelAsset {
                name: "TANK".into(),
                members: vec!["Cargo01".into()],
            }],
        );

        assert!(idx.is_model_asset("tank"));
        assert_eq!(idx.effective_model_source("Tank"), Some("patch.w3d"));
        assert_eq!(
            idx.model_members("tank").collect::<Vec<_>>(),
            vec!["Tire01", "Cargo01"]
        );

        idx.remove_file("patch.w3d");
        assert_eq!(idx.effective_model_source("tank"), Some("base.w3d"));
        assert_eq!(
            idx.model_members("tank").collect::<Vec<_>>(),
            vec!["Tire01"]
        );
    }

    #[test]
    fn removing_one_file_keeps_other_files_models_of_same_name() {
        // Base game and a patch archive both ship a model called "Tank"
        // (patches overriding base models is the normal case).
        let mut idx = WorkspaceIndex::new();
        idx.set_file_models(
            "base.w3d",
            vec![ModelAsset {
                name: "Tank".into(),
                members: vec!["Tire01".into()],
            }],
        );
        idx.set_file_models(
            "patch.w3d",
            vec![ModelAsset {
                name: "TANK".into(),
                members: vec!["Cargo01".into()],
            }],
        );
        let members: Vec<_> = idx.model_members("tank").collect();
        assert!(members.contains(&"Tire01") && members.contains(&"Cargo01"));

        idx.remove_file("patch.w3d");
        assert!(idx.is_model_asset("Tank"));
        let members: Vec<_> = idx.model_members("Tank").collect();
        assert_eq!(members, vec!["Tire01"]);

        // Re-indexing the remaining file (a rescan) must not lose it either.
        idx.set_file_models(
            "base.w3d",
            vec![ModelAsset {
                name: "Tank".into(),
                members: vec!["Tire01".into()],
            }],
        );
        assert!(idx.is_model_asset("Tank"));
    }
}
