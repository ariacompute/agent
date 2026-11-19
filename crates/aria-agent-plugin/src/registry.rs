//! Plugin registry: assembly (builder) and the frozen snapshot the runtime uses.
//!
//! Assembly is deliberately **not** a Cordis-style runtime DI container
//! (ADR-0014 §Decision): there is no hot swap and no prototype-chain context.
//! The builder collects providers, validates the graph, then [`Registry::freeze`]
//! produces an immutable [`FrozenRegistry`]. The agent resolves every seam once
//! at construction and keeps the `Arc`s in struct fields, so the hot path never
//! touches the registry.

use crate::error::{PluginError, PluginResult};
use crate::manifest::{PluginManifest, SeamKey};
use std::any::Any;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// A type-erased capability provider.
pub type Provider = Arc<dyn Any + Send + Sync>;

/// Sized wrapper so unsized trait objects (`Arc<dyn Trait>`) can be stored in a
/// `dyn Any` map and downcast back with the original type.
#[derive(Debug)]
pub struct Shared<T: ?Sized>(pub Arc<T>);

impl<T: ?Sized> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: ?Sized> Shared<T> {
    pub fn new(value: Arc<T>) -> Self {
        Self(value)
    }

    /// Clone the inner handle.
    pub fn get(&self) -> Arc<T> {
        self.0.clone()
    }
}

/// A plugin: declares what it provides / requires and installs its providers.
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;
    fn install(&self, reg: &mut Registry) -> PluginResult<()>;
}

#[derive(Debug, Clone)]
struct Entry {
    seam: SeamKey,
    name: &'static str,
    plugin_id: String,
    /// `std::any::type_name` of the capability, for actionable type errors.
    type_name: &'static str,
    value: Provider,
}

/// Mutable assembly graph.
#[derive(Default)]
pub struct Registry {
    entries: Vec<Entry>,
    defaults: BTreeMap<&'static str, &'static str>,
    owner: Option<String>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("providers", &self.entries.len())
            .field("defaults", &self.defaults)
            .finish()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider for `seam` under `name`.
    ///
    /// `value` is any shared handle (`Arc<dyn Trait>` included); callers resolve
    /// it back with the same type via [`Registry::resolve`] /
    /// [`FrozenRegistry::resolve`].
    pub fn provide<T: ?Sized + Send + Sync + 'static>(
        &mut self,
        seam: SeamKey,
        name: &'static str,
        value: Arc<T>,
    ) -> PluginResult<()> {
        if seam.is_service() {
            return Err(PluginError::ServiceIsNotAProvider { seam: seam.0 });
        }
        if self
            .entries
            .iter()
            .any(|e| e.seam == seam && e.name == name)
        {
            return Err(PluginError::DuplicateProvider { seam: seam.0, name });
        }
        self.entries.push(Entry {
            seam,
            name,
            plugin_id: self.owner.clone().unwrap_or_else(|| "manual".into()),
            type_name: std::any::type_name::<T>(),
            value: Arc::new(Shared::new(value)),
        });
        Ok(())
    }

    /// Mark `name` as the default provider for `seam` (must be registered).
    pub fn set_default(&mut self, seam: SeamKey, name: &'static str) -> PluginResult<()> {
        if !self
            .entries
            .iter()
            .any(|e| e.seam == seam && e.name == name)
        {
            return Err(PluginError::NoProvider { seam: seam.0 });
        }
        self.defaults.insert(seam.0, name);
        Ok(())
    }

    /// Resolve the default provider of `seam`.
    pub fn resolve<T: ?Sized + Send + Sync + 'static>(
        &self,
        seam: SeamKey,
    ) -> PluginResult<Arc<T>> {
        let entry = pick(&self.entries, &self.defaults, seam, None)?;
        downcast(entry, seam)
    }

    /// Resolve a named provider of `seam`.
    pub fn resolve_named<T: ?Sized + Send + Sync + 'static>(
        &self,
        seam: SeamKey,
        name: &'static str,
    ) -> PluginResult<Arc<T>> {
        let entry = pick(&self.entries, &self.defaults, seam, Some(name))?;
        downcast(entry, seam)
    }

    pub fn has(&self, seam: SeamKey) -> bool {
        self.entries.iter().any(|e| e.seam == seam)
    }

    pub fn provider_names(&self, seam: SeamKey) -> Vec<&'static str> {
        self.entries
            .iter()
            .filter(|e| e.seam == seam)
            .map(|e| e.name)
            .collect()
    }

    /// Sort by declared dependencies, then install every plugin and freeze.
    pub fn install_all(mut self, plugins: &[Arc<dyn Plugin>]) -> PluginResult<FrozenRegistry> {
        let manifests: Vec<PluginManifest> = plugins.iter().map(|p| p.manifest().clone()).collect();
        let order = validate_graph(&manifests)?;
        for idx in order {
            let manifest = &manifests[idx];
            self.owner = Some(manifest.id.clone());
            plugins[idx].install(&mut self)?;
            self.owner = None;
            for key in &manifest.provides {
                if !self
                    .entries
                    .iter()
                    .any(|e| e.seam == *key && e.plugin_id == manifest.id)
                {
                    return Err(PluginError::UnfulfilledProvide {
                        plugin: manifest.id.clone(),
                        seam: key.0,
                    });
                }
            }
        }
        self.freeze(&manifests)
    }

    /// Validate and freeze an already-installed registry.
    pub fn freeze(mut self, manifests: &[PluginManifest]) -> PluginResult<FrozenRegistry> {
        // Core seams are single-owner by definition.
        for key in crate::manifest::ALL {
            if !key.is_core() {
                continue;
            }
            let names: Vec<&'static str> = self.provider_names(*key);
            if names.len() > 1 {
                return Err(PluginError::CoreConflict {
                    seam: key.0,
                    count: names.len(),
                    providers: names.join(", "),
                });
            }
        }
        // Every declared provider is reachable, and every seam with providers has
        // a deterministic default (first registered unless overridden).
        for entry in &self.entries {
            self.defaults.entry(entry.seam.0).or_insert(entry.name);
        }
        for (seam, name) in &self.defaults {
            if !self
                .entries
                .iter()
                .any(|e| e.seam.0 == *seam && e.name == *name)
            {
                let key = SeamKey(seam);
                return Err(PluginError::MissingDefault { seam: key.0 });
            }
        }
        let providers = self.entries.len();
        let plugins = manifests.len();
        tracing::debug!(providers, plugins, "plugin registry frozen");
        Ok(FrozenRegistry {
            entries: self.entries,
            defaults: self.defaults,
        })
    }
}

/// Immutable post-assembly snapshot.
pub struct FrozenRegistry {
    entries: Vec<Entry>,
    defaults: BTreeMap<&'static str, &'static str>,
}

impl std::fmt::Debug for FrozenRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut by_seam: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
        for e in &self.entries {
            by_seam.entry(e.seam.0).or_default().push(e.name);
        }
        f.debug_struct("FrozenRegistry")
            .field("seams", &by_seam)
            .finish()
    }
}

impl FrozenRegistry {
    pub fn resolve<T: ?Sized + Send + Sync + 'static>(
        &self,
        seam: SeamKey,
    ) -> PluginResult<Arc<T>> {
        downcast(pick(&self.entries, &self.defaults, seam, None)?, seam)
    }

    pub fn resolve_named<T: ?Sized + Send + Sync + 'static>(
        &self,
        seam: SeamKey,
        name: &'static str,
    ) -> PluginResult<Arc<T>> {
        downcast(pick(&self.entries, &self.defaults, seam, Some(name))?, seam)
    }

    pub fn has(&self, seam: SeamKey) -> bool {
        self.entries.iter().any(|e| e.seam == seam)
    }

    pub fn provider_names(&self, seam: SeamKey) -> Vec<&'static str> {
        self.entries
            .iter()
            .filter(|e| e.seam == seam)
            .map(|e| e.name)
            .collect()
    }

    /// Providers of `seam` in **assembly order**, which is the precedence chain
    /// dsh calls "registration order is priority": the first entry is the
    /// default, later entries are fallbacks a consumer may try in order.
    pub fn precedence(&self, seam: SeamKey) -> Vec<&'static str> {
        self.provider_names(seam)
    }

    pub fn default_name(&self, seam: SeamKey) -> Option<&'static str> {
        self.defaults.get(seam.0).copied()
    }

    /// Release every provider in reverse assembly order, so a dependent plugin
    /// drops before the seams it consumed (the reverse of [`Self::seams`]'s
    /// topological install order).
    pub fn dispose(mut self) -> usize {
        let count = self.entries.len();
        // Dropping one entry at a time *is* the release order; logging here would
        // allocate lazily but ordering must not depend on a subscriber.
        while let Some(entry) = self.entries.pop() {
            drop(entry);
        }
        self.defaults.clear();
        count
    }

    /// Every registered seam (sorted), for startup logging / diagnostics.
    pub fn seams(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = self.entries.iter().map(|e| e.seam.0).collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

fn pick<'a>(
    entries: &'a [Entry],
    defaults: &BTreeMap<&'static str, &'static str>,
    seam: SeamKey,
    name: Option<&'static str>,
) -> PluginResult<&'a Entry> {
    let want = match name {
        Some(n) => n,
        None => *defaults
            .get(seam.0)
            .ok_or(PluginError::MissingDefault { seam: seam.0 })?,
    };
    entries
        .iter()
        .find(|e| e.seam == seam && e.name == want)
        .ok_or(PluginError::NoProvider { seam: seam.0 })
}

fn downcast<T: ?Sized + Send + Sync + 'static>(
    entry: &Entry,
    seam: SeamKey,
) -> PluginResult<Arc<T>> {
    entry
        .value
        .clone()
        .downcast::<Shared<T>>()
        .map(|s| s.0.clone())
        .map_err(|_| PluginError::TypeMismatch {
            seam: seam.0,
            name: entry.type_name,
        })
}

/// Validate manifests and return an install order (dependencies first).
fn validate_graph(manifests: &[PluginManifest]) -> PluginResult<Vec<usize>> {
    let mut seen: HashSet<&str> = HashSet::new();
    for m in manifests {
        m.validate()?;
        if !seen.insert(m.id.as_str()) {
            return Err(PluginError::DuplicatePlugin { id: m.id.clone() });
        }
    }

    // seam -> plugins that provide it
    let mut providers: HashMap<&'static str, Vec<usize>> = HashMap::new();
    for (i, m) in manifests.iter().enumerate() {
        for key in &m.provides {
            providers.entry(key.0).or_default().push(i);
        }
    }

    // edges[i] = plugins i must be installed after
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); manifests.len()];
    for (i, m) in manifests.iter().enumerate() {
        for key in &m.requires {
            let provs = providers.get(key.0).cloned().unwrap_or_default();
            if provs.is_empty() {
                return Err(PluginError::MissingDependency {
                    plugin: m.id.clone(),
                    seam: key.0,
                });
            }
            for p in provs {
                if p != i && !edges[i].contains(&p) {
                    edges[i].push(p);
                }
            }
        }
    }

    // Kahn's algorithm, stable by declaration order.
    let mut installed = vec![false; manifests.len()];
    let mut order = Vec::with_capacity(manifests.len());
    loop {
        let mut progressed = false;
        for i in 0..manifests.len() {
            if installed[i] || edges[i].iter().any(|p| !installed[*p]) {
                continue;
            }
            installed[i] = true;
            order.push(i);
            progressed = true;
        }
        if order.len() == manifests.len() {
            return Ok(order);
        }
        if !progressed {
            let chain = manifests
                .iter()
                .enumerate()
                .filter(|(i, _)| !installed[*i])
                .map(|(_, m)| m.id.clone())
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(PluginError::Cycle { chain });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{SeamRole, FS, LLM, OTEL, SANDBOX_POLICY, STORAGE, WEB};

    trait Other: Send + Sync {}

    trait Dummy: Send + Sync {
        fn id(&self) -> &'static str;
    }

    struct DummyImpl(&'static str);
    impl Dummy for DummyImpl {
        fn id(&self) -> &'static str {
            self.0
        }
    }

    struct Fixed {
        manifest: PluginManifest,
        installs: Vec<(SeamKey, &'static str, Arc<dyn Dummy>)>,
    }

    impl Fixed {
        fn new(manifest: PluginManifest) -> Self {
            Self {
                manifest,
                installs: Vec::new(),
            }
        }

        fn with(mut self, seam: SeamKey, name: &'static str, id: &'static str) -> Self {
            self.installs.push((seam, name, Arc::new(DummyImpl(id))));
            self
        }
    }

    impl Plugin for Fixed {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn install(&self, reg: &mut Registry) -> PluginResult<()> {
            for (seam, name, value) in &self.installs {
                reg.provide::<dyn Dummy>(*seam, name, value.clone())?;
            }
            Ok(())
        }
    }

    struct BrokenInstall {
        manifest: PluginManifest,
    }

    impl Plugin for BrokenInstall {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn install(&self, _reg: &mut Registry) -> PluginResult<()> {
            Ok(())
        }
    }

    fn plugin(reg: Registry, plugins: Vec<Arc<dyn Plugin>>) -> PluginResult<FrozenRegistry> {
        reg.install_all(&plugins)
    }

    #[test]
    fn installs_in_dependency_order_and_resolves() {
        let consumer = Arc::new(
            Fixed::new(
                PluginManifest::new("consumer", SeamRole::Bundle)
                    .requires([STORAGE])
                    .provides([WEB]),
            )
            .with(WEB, "http", "web"),
        );
        let store = Arc::new(
            Fixed::new(PluginManifest::new("store", SeamRole::Seam).provides([STORAGE]))
                .with(STORAGE, "json", "storage"),
        );
        // Declaration order puts the consumer first; assembly must reorder.
        let frozen = plugin(Registry::new(), vec![consumer, store]).unwrap();
        let web: Arc<dyn Dummy> = frozen.resolve(WEB).unwrap();
        assert_eq!(web.id(), "web");
        let storage: Arc<dyn Dummy> = frozen.resolve(STORAGE).unwrap();
        assert_eq!(storage.id(), "storage");
        assert_eq!(frozen.default_name(STORAGE), Some("json"));
        assert_eq!(frozen.default_name(WEB), Some("http"));
        assert!(frozen.has(STORAGE));
        assert!(!frozen.has(LLM));
        assert_eq!(frozen.seams(), vec!["storage", "web"]);
    }

    #[test]
    fn missing_dependency_fails_closed() {
        let orphan = Arc::new(Fixed::new(
            PluginManifest::new("orphan", SeamRole::Bundle).requires([LLM]),
        ));
        let err = plugin(Registry::new(), vec![orphan]).unwrap_err();
        assert!(
            matches!(err, PluginError::MissingDependency { seam, .. } if seam == "llm"),
            "{err:?}"
        );
    }

    #[test]
    fn dependency_cycle_is_reported() {
        let a = Arc::new(
            Fixed::new(
                PluginManifest::new("a", SeamRole::Seam)
                    .provides([FS])
                    .requires([STORAGE]),
            )
            .with(FS, "local", "fs"),
        );
        let b = Arc::new(
            Fixed::new(
                PluginManifest::new("b", SeamRole::Seam)
                    .provides([STORAGE])
                    .requires([FS]),
            )
            .with(STORAGE, "json", "storage"),
        );
        let err = plugin(Registry::new(), vec![a, b]).unwrap_err();
        assert!(
            matches!(err, PluginError::Cycle { ref chain } if chain.contains('a') && chain.contains('b')),
            "{err:?}"
        );
    }

    #[test]
    fn duplicate_plugin_id_is_rejected() {
        let a = Arc::new(Fixed::new(PluginManifest::new("dup", SeamRole::Seam)));
        let b = Arc::new(Fixed::new(PluginManifest::new("dup", SeamRole::Seam)));
        let err = plugin(Registry::new(), vec![a, b]).unwrap_err();
        assert!(
            matches!(err, PluginError::DuplicatePlugin { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn core_seam_cannot_have_two_providers() {
        let a =
            Arc::new(
                Fixed::new(PluginManifest::new("a", SeamRole::Core).provides([SANDBOX_POLICY]))
                    .with(SANDBOX_POLICY, "a", "a"),
            );
        let b =
            Arc::new(
                Fixed::new(PluginManifest::new("b", SeamRole::Core).provides([SANDBOX_POLICY]))
                    .with(SANDBOX_POLICY, "b", "b"),
            );
        let err = plugin(Registry::new(), vec![a, b]).unwrap_err();
        assert!(
            matches!(err, PluginError::CoreConflict { seam, count: 2, .. } if seam == "sandbox_policy"),
            "{err:?}"
        );
    }

    #[test]
    fn declared_provide_without_install_is_rejected() {
        let lazy = Arc::new(BrokenInstall {
            manifest: PluginManifest::new("lazy", SeamRole::Seam).provides([FS]),
        });
        let err = plugin(Registry::new(), vec![lazy]).unwrap_err();
        assert!(
            matches!(err, PluginError::UnfulfilledProvide { seam, .. } if seam == "fs"),
            "{err:?}"
        );
    }

    #[test]
    fn duplicate_provider_name_is_rejected() {
        let a = Arc::new(
            Fixed::new(PluginManifest::new("a", SeamRole::Seam).provides([WEB]))
                .with(WEB, "http", "a"),
        );
        let b = Arc::new(
            Fixed::new(PluginManifest::new("b", SeamRole::Seam).provides([WEB]))
                .with(WEB, "http", "b"),
        );
        let err = plugin(Registry::new(), vec![a, b]).unwrap_err();
        assert!(
            matches!(err, PluginError::DuplicateProvider { seam, name } if seam == "web" && name == "http"),
            "{err:?}"
        );
    }

    #[test]
    fn type_mismatch_is_reported() {
        let a = Arc::new(
            Fixed::new(PluginManifest::new("a", SeamRole::Seam).provides([WEB]))
                .with(WEB, "http", "a"),
        );
        let frozen = plugin(Registry::new(), vec![a]).unwrap();
        // Resolving the same seam with a different capability type must fail
        // instead of returning a corrupt handle.
        let err = frozen
            .resolve::<dyn Other>(WEB)
            .err()
            .expect("wrong capability type must fail");
        assert!(matches!(err, PluginError::TypeMismatch { .. }), "{err:?}");
    }

    #[test]
    fn missing_seam_resolution_is_an_error() {
        let frozen = plugin(Registry::new(), vec![]).unwrap();
        let err = frozen
            .resolve::<dyn Dummy>(STORAGE)
            .err()
            .expect("missing provider must fail");
        assert!(matches!(err, PluginError::MissingDefault { .. }), "{err:?}");
        assert!(frozen.provider_names(STORAGE).is_empty());
    }

    #[test]
    fn explicit_named_resolution_and_default_override() {
        let mut reg = Registry::new();
        // Nothing installed yet: the default cannot point at a missing provider.
        assert_eq!(
            reg.set_default(WEB, "http").unwrap_err(),
            PluginError::NoProvider { seam: "web" }
        );
        let multi = Arc::new(
            Fixed::new(PluginManifest::new("a", SeamRole::Seam).provides([WEB]))
                .with(WEB, "http", "http")
                .with(WEB, "stub", "stub"),
        );
        let frozen = reg.install_all(&[multi]).unwrap();
        // First registered wins by default.
        assert_eq!(frozen.default_name(WEB), Some("http"));
        let named: Arc<dyn Dummy> = frozen.resolve_named(WEB, "stub").unwrap();
        assert_eq!(named.id(), "stub");
        let unknown = frozen
            .resolve_named::<dyn Dummy>(WEB, "nope")
            .err()
            .expect("unknown provider name must fail");
        assert!(
            matches!(unknown, PluginError::NoProvider { .. }),
            "{unknown:?}"
        );
    }

    #[test]
    fn self_provisioned_dependency_is_not_a_cycle() {
        let solo = Arc::new(
            Fixed::new(
                PluginManifest::new("solo", SeamRole::Seam)
                    .provides([STORAGE])
                    .requires([STORAGE]),
            )
            .with(STORAGE, "json", "solo"),
        );
        let frozen = plugin(Registry::new(), vec![solo]).unwrap();
        assert!(frozen.has(STORAGE));
    }

    #[test]
    fn invalid_manifest_is_rejected_before_install() {
        let bad = Arc::new(Fixed::new(PluginManifest::new("", SeamRole::Seam)));
        let err = plugin(Registry::new(), vec![bad]).unwrap_err();
        assert_eq!(err, PluginError::EmptyPluginId);
    }

    #[test]
    fn shared_wrapper_clones_handle() {
        let d: Arc<dyn Dummy> = Arc::new(DummyImpl("x"));
        let shared = Shared::new(d.clone());
        assert_eq!(shared.get().id(), "x");
        assert_eq!(shared.clone().get().id(), "x");
    }

    #[test]
    fn builder_api_resolves_named_defaults_and_reports_state() {
        let mut reg = Registry::new();
        assert!(format!("{reg:?}").contains("providers: 0"));
        assert!(!reg.has(WEB));
        assert!(reg.provider_names(WEB).is_empty());
        assert!(matches!(
            reg.set_default(WEB, "http"),
            Err(PluginError::NoProvider { .. })
        ));
        reg.provide::<dyn Dummy>(WEB, "http", Arc::new(DummyImpl("web")))
            .unwrap();
        reg.provide::<dyn Dummy>(WEB, "backup", Arc::new(DummyImpl("alt")))
            .unwrap();
        reg.set_default(WEB, "backup").unwrap();
        assert!(reg.has(WEB));
        assert_eq!(reg.provider_names(WEB), vec!["http", "backup"]);
        assert!(format!("{reg:?}").contains("providers: 2"));
        let default: Arc<dyn Dummy> = reg.resolve(WEB).unwrap();
        assert_eq!(default.id(), "alt");
        let named: Arc<dyn Dummy> = reg.resolve_named(WEB, "http").unwrap();
        assert_eq!(named.id(), "web");

        let manifest = PluginManifest::new("p", SeamRole::Seam).provides([WEB]);
        let frozen = reg.freeze(&[manifest]).unwrap();
        assert!(format!("{frozen:?}").contains("FrozenRegistry"));
        assert_eq!(frozen.provider_names(WEB), vec!["http", "backup"]);
        assert_eq!(frozen.default_name(WEB), Some("backup"));
    }

    #[test]
    fn freeze_rejects_a_default_with_no_matching_provider() {
        let mut reg = Registry::new();
        reg.provide::<dyn Dummy>(WEB, "http", Arc::new(DummyImpl("web")))
            .unwrap();
        reg.defaults.insert(WEB.0, "ghost");
        assert!(matches!(
            reg.freeze(&[]),
            Err(PluginError::MissingDefault { .. })
        ));
    }

    /// Provider that records its drop order, so `dispose` can be asserted.
    struct Probe {
        id: &'static str,
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.log.lock().unwrap().push(self.id);
        }
    }

    impl Dummy for Probe {
        fn id(&self) -> &'static str {
            self.id
        }
    }

    #[test]
    fn service_seams_reject_provider_registration() {
        let mut reg = Registry::new();
        let err = reg
            .provide::<dyn Dummy>(OTEL, "otel", Arc::new(DummyImpl("otel")))
            .unwrap_err();
        assert_eq!(err, PluginError::ServiceIsNotAProvider { seam: "otel" });
        assert!(!reg.has(OTEL));
    }

    #[test]
    fn precedence_is_registration_order() {
        let mut reg = Registry::new();
        reg.provide::<dyn Dummy>(WEB, "primary", Arc::new(DummyImpl("p")))
            .unwrap();
        reg.provide::<dyn Dummy>(WEB, "fallback", Arc::new(DummyImpl("f")))
            .unwrap();
        let frozen = reg.freeze(&[]).unwrap();
        assert_eq!(frozen.precedence(WEB), vec!["primary", "fallback"]);
        assert_eq!(frozen.default_name(WEB), Some("primary"));
    }

    #[test]
    fn dispose_releases_providers_in_reverse_assembly_order() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut reg = Registry::new();
        reg.provide::<dyn Dummy>(
            LLM,
            "first",
            Arc::new(Probe {
                id: "first",
                log: log.clone(),
            }),
        )
        .unwrap();
        reg.provide::<dyn Dummy>(
            FS,
            "second",
            Arc::new(Probe {
                id: "second",
                log: log.clone(),
            }),
        )
        .unwrap();
        let frozen = reg.freeze(&[]).unwrap();
        assert_eq!(frozen.resolve::<dyn Dummy>(FS).unwrap().id(), "second");
        assert_eq!(frozen.dispose(), 2);
        assert_eq!(*log.lock().unwrap(), vec!["second", "first"]);
    }
}
