use std::collections::BTreeMap;

use super::RtpConfigurationError;

#[derive(Clone)]
pub(super) struct Usage<V> {
    value: V,
    owners: usize,
}

pub(super) struct UsageChanges<K, V>(BTreeMap<K, Option<Usage<V>>>);

impl<K, V> Default for UsageChanges<K, V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<K: Ord + Clone, V: Clone + Eq> UsageChanges<K, V> {
    pub(super) fn release(
        &mut self,
        current: &BTreeMap<K, Usage<V>>,
        key: &K,
    ) -> Result<(), RtpConfigurationError> {
        let entry = self
            .0
            .entry(key.clone())
            .or_insert_with(|| current.get(key).cloned());
        let usage = entry
            .as_mut()
            .ok_or(RtpConfigurationError::ConnectionState)?;
        usage.owners = usage
            .owners
            .checked_sub(1)
            .ok_or(RtpConfigurationError::ConnectionState)?;
        if usage.owners == 0 {
            *entry = None;
        }
        Ok(())
    }

    pub(super) fn claim(
        &mut self,
        current: &BTreeMap<K, Usage<V>>,
        key: K,
        value: V,
    ) -> Result<(), RtpConfigurationError> {
        let entry = self
            .0
            .entry(key.clone())
            .or_insert_with(|| current.get(&key).cloned());
        if let Some(usage) = entry {
            if usage.value != value {
                return Err(RtpConfigurationError::Invalid);
            }
            usage.owners = usage
                .owners
                .checked_add(1)
                .ok_or(RtpConfigurationError::Capacity)?;
        } else {
            *entry = Some(Usage { value, owners: 1 });
        }
        Ok(())
    }

    pub(super) fn keys<'a>(
        &'a self,
        current: &'a BTreeMap<K, Usage<V>>,
    ) -> impl Iterator<Item = &'a K> {
        current
            .keys()
            .filter(|key| !self.0.contains_key(*key))
            .chain(
                self.0
                    .iter()
                    .filter_map(|(key, value)| value.as_ref().map(|_| key)),
            )
    }

    pub(super) fn commit(self, current: &mut BTreeMap<K, Usage<V>>) {
        for (key, usage) in self.0 {
            if let Some(usage) = usage {
                current.insert(key, usage);
            } else {
                current.remove(&key);
            }
        }
    }
}
