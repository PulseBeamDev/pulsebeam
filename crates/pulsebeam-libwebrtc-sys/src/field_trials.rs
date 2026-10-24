use std::{collections::BTreeMap, fmt, panic::AssertUnwindSafe, sync::Arc};

use crate::{ffi, provider_panic::catch_provider};

/// Immutable native field-trial provider.
///
/// Lookups may run concurrently on any engine thread. Results must remain
/// stable for the provider's lifetime. Copies must preserve that immutable
/// mapping and own their dependencies. No callback may require reentrant access
/// to an application session. Panics are contained at the native boundary:
/// lookup returns an empty group and copy returns an empty provider. The native
/// adapter memoizes the first result for each key across retaining copies, so
/// native consumers cannot observe a changed group between validation and use.
pub trait FieldTrialsView: Send + Sync + 'static {
    fn lookup(&self, key: &str) -> String;
    fn create_copy(&self) -> Arc<dyn FieldTrialsView>;
}

impl<T: FieldTrialsView + ?Sized> FieldTrialsView for Arc<T> {
    fn lookup(&self, key: &str) -> String {
        (**self).lookup(key)
    }

    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        (**self).create_copy()
    }
}

/// Owned field-trial values, prepared before immutable environment registration.
///
/// Names and groups are open strings, not a binding allowlist. Meaning and
/// availability of individual trials are version-specific native behavior.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FieldTrials {
    values: BTreeMap<String, String>,
}

impl FieldTrials {
    /// Parse native `Name/Group/OtherName/OtherGroup/` syntax. Empty input is
    /// valid; conflicting duplicate names, empty names/groups, and missing
    /// trailing separators are invalid. Validation uses the pinned parser.
    pub fn parse(configuration: &str) -> Result<Self, FieldTrialsError> {
        if !ffi::field_trials_validate(configuration) {
            return Err(FieldTrialsError::InvalidConfiguration);
        }
        let mut values = BTreeMap::new();
        let mut entries = configuration.split_terminator('/');
        while let Some(name) = entries.next() {
            if let Some(group) = entries.next() {
                values.insert(name.to_owned(), group.to_owned());
            }
        }
        Ok(Self { values })
    }

    /// Set a group, or remove the name when `group` is empty. Neither name nor
    /// group may contain `/`, and the name must not be empty.
    pub fn set(&mut self, name: &str, group: &str) -> Result<(), FieldTrialsError> {
        if name.is_empty() || name.contains('/') || group.contains('/') {
            return Err(FieldTrialsError::InvalidEntry);
        }
        if group.is_empty() {
            self.values.remove(name);
        } else {
            self.values.insert(name.to_owned(), group.to_owned());
        }
        Ok(())
    }

    /// Merge values; the other mapping takes precedence on conflicts.
    pub fn merge(&mut self, other: &Self) {
        self.values.extend(other.values.clone());
    }

    /// Missing values are the native empty string, not an invented default.
    pub fn lookup(&self, key: &str) -> &str {
        self.values.get(key).map_or("", String::as_str)
    }

    pub fn is_enabled(&self, key: &str) -> bool {
        self.lookup(key).starts_with("Enabled")
    }

    pub fn is_disabled(&self, key: &str) -> bool {
        self.lookup(key).starts_with("Disabled")
    }
}

impl FieldTrialsView for FieldTrials {
    fn lookup(&self, key: &str) -> String {
        self.lookup(key).to_owned()
    }

    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        Arc::new(self.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldTrialsError {
    InvalidConfiguration,
    InvalidEntry,
}

impl fmt::Display for FieldTrialsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid field-trial {}",
            match self {
                Self::InvalidConfiguration => "configuration",
                Self::InvalidEntry => "name or group",
            }
        )
    }
}

impl std::error::Error for FieldTrialsError {}

/// Owned native copy of an environment's field-trial view.
///
/// No environment borrow is retained. Native default views preserve the pinned
/// engine's process-global trial lookup semantics; supplied providers preserve
/// their own immutable copy contract.
pub struct FieldTrialsSnapshot {
    native: cxx::UniquePtr<ffi::NativeFieldTrials>,
}

// SAFETY: FieldTrialsView is immutable; custom Rust implementations require
// Send + Sync. Native-owned copies retain all provider dependencies.
unsafe impl Send for FieldTrialsSnapshot {}
// SAFETY: see Send rationale; all exposed native methods are const lookups.
unsafe impl Sync for FieldTrialsSnapshot {}

impl Clone for FieldTrialsSnapshot {
    fn clone(&self) -> Self {
        Self::from_native(ffi::field_trials_copy(self.native()))
    }
}

impl FieldTrialsSnapshot {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeFieldTrials>) -> Self {
        Self { native }
    }

    pub(crate) fn native(&self) -> &ffi::NativeFieldTrials {
        self.native.as_ref().expect("owned field-trial view")
    }

    pub fn lookup(&self, key: &str) -> String {
        ffi::field_trials_lookup(self.native(), key)
    }

    pub fn is_enabled(&self, key: &str) -> bool {
        ffi::field_trials_is_enabled(self.native(), key)
    }

    pub fn is_disabled(&self, key: &str) -> bool {
        ffi::field_trials_is_disabled(self.native(), key)
    }
}

impl FieldTrialsView for FieldTrialsSnapshot {
    fn lookup(&self, key: &str) -> String {
        self.lookup(key)
    }

    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        Arc::new(self.clone())
    }
}

pub(crate) struct RustFieldTrials(pub(crate) Option<Arc<dyn FieldTrialsView>>);
impl Drop for RustFieldTrials {
    fn drop(&mut self) {
        let provider = self.0.take();
        let _ = catch_provider(|| drop(provider));
    }
}

pub(crate) fn field_trials_present(view: &RustFieldTrials) -> bool {
    view.0.is_some()
}

pub(crate) fn rust_field_trials_lookup(view: &RustFieldTrials, key: &str) -> String {
    catch_provider(AssertUnwindSafe(|| {
        view.0
            .as_ref()
            .map_or_else(String::new, |view| view.lookup(key))
    }))
    .unwrap_or_default()
}

pub(crate) fn rust_field_trials_copy(view: &RustFieldTrials) -> Box<RustFieldTrials> {
    let copy = catch_provider(AssertUnwindSafe(|| {
        view.0.as_ref().map(|view| view.create_copy())
    }))
    .unwrap_or_else(|_| Some(Arc::new(FieldTrials::default())));
    Box::new(RustFieldTrials(copy))
}
