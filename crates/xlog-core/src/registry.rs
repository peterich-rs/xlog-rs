use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, Weak};

enum Slot<T> {
    Ready(Weak<T>),
    Pending(Arc<(Mutex<bool>, Condvar)>),
}

/// Registry of named instances plus one optional default instance.
///
/// Named entries are stored as [`Weak`] references so dead instances can be
/// dropped without explicit deregistration. Construction runs outside the map
/// lock. A pending slot makes concurrent callers wait for the same name.
pub struct InstanceRegistry<T> {
    instances: Mutex<HashMap<String, Slot<T>>>,
    default: Mutex<Option<Arc<T>>>,
}

impl<T> Default for InstanceRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> InstanceRegistry<T> {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            instances: Mutex::new(HashMap::new()),
            default: Mutex::new(None),
        }
    }

    /// Return the live named instance, or insert one produced by `new_value`.
    pub fn get_or_insert_with<F>(&self, name: &str, new_value: F) -> Arc<T>
    where
        F: FnOnce() -> Arc<T>,
    {
        match self
            .get_or_try_insert_with(name, || Ok::<Arc<T>, std::convert::Infallible>(new_value()))
        {
            Ok(value) => value,
            Err(err) => match err {},
        }
    }

    /// Fallible variant of [`Self::get_or_insert_with`].
    ///
    /// The map lock is not held while `new_value` runs. Concurrent callers for
    /// the same name wait, then reuse the winner.
    pub fn get_or_try_insert_with<F, E>(&self, name: &str, new_value: F) -> Result<Arc<T>, E>
    where
        F: FnOnce() -> Result<Arc<T>, E>,
    {
        loop {
            let claim = {
                let map = self.instances.lock().expect("instances lock poisoned");
                match map.get(name) {
                    Some(Slot::Ready(existing)) => {
                        if let Some(value) = existing.upgrade() {
                            return Ok(value);
                        }
                        Ok(())
                    }
                    Some(Slot::Pending(gate)) => Err(Arc::clone(gate)),
                    None => Ok(()),
                }
            };
            match claim {
                Ok(()) => {
                    let gate = Arc::new((Mutex::new(false), Condvar::new()));
                    {
                        let mut map = self.instances.lock().expect("instances lock poisoned");
                        if let Some(Slot::Ready(existing)) = map.get(name) {
                            if let Some(value) = existing.upgrade() {
                                return Ok(value);
                            }
                        }
                        if let Some(Slot::Pending(existing)) = map.get(name) {
                            let existing = Arc::clone(existing);
                            drop(map);
                            wait_for_build(&existing);
                            continue;
                        }
                        map.insert(name.to_string(), Slot::Pending(Arc::clone(&gate)));
                    }
                    return self.publish(name, new_value(), &gate);
                }
                Err(gate) => wait_for_build(&gate),
            }
        }
    }

    fn publish<E>(
        &self,
        name: &str,
        built: Result<Arc<T>, E>,
        gate: &Arc<(Mutex<bool>, Condvar)>,
    ) -> Result<Arc<T>, E> {
        let result = {
            let mut map = self.instances.lock().expect("instances lock poisoned");
            match built {
                Ok(value) => {
                    map.insert(name.to_string(), Slot::Ready(Arc::downgrade(&value)));
                    Ok(value)
                }
                Err(err) => {
                    map.remove(name);
                    Err(err)
                }
            }
        };
        let (lock, condvar) = &**gate;
        let mut done = lock.lock().expect("registry build lock poisoned");
        *done = true;
        condvar.notify_all();
        result
    }

    /// Return the live named instance if it still exists.
    pub fn get(&self, name: &str) -> Option<Arc<T>> {
        let map = self.instances.lock().expect("instances lock poisoned");
        match map.get(name)? {
            Slot::Ready(existing) => existing.upgrade(),
            Slot::Pending(_) => None,
        }
    }

    /// Set the default instance reference.
    pub fn set_default(&self, value: Arc<T>) {
        let mut default = self.default.lock().expect("default lock poisoned");
        *default = Some(value);
    }

    /// Return the current default instance, if any.
    pub fn default_instance(&self) -> Option<Arc<T>> {
        self.default.lock().expect("default lock poisoned").clone()
    }

    /// Clear the current default instance reference.
    pub fn clear_default(&self) {
        let mut default = self.default.lock().expect("default lock poisoned");
        *default = None;
    }

    /// Visit every currently live named instance.
    ///
    /// Dead weak entries are removed before iteration starts.
    pub fn for_each_live<F>(&self, mut f: F)
    where
        F: FnMut(Arc<T>),
    {
        let live: Vec<Arc<T>> = {
            let mut map = self.instances.lock().expect("instances lock poisoned");
            map.retain(|_, slot| match slot {
                Slot::Ready(value) => value.upgrade().is_some(),
                Slot::Pending(_) => true,
            });
            map.values()
                .filter_map(|slot| match slot {
                    Slot::Ready(value) => value.upgrade(),
                    Slot::Pending(_) => None,
                })
                .collect()
        };
        for instance in live {
            f(instance);
        }
    }
}

fn wait_for_build(gate: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, condvar) = &**gate;
    let mut done = lock.lock().expect("registry build lock poisoned");
    while !*done {
        done = condvar.wait(done).expect("registry build lock poisoned");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Weak};

    use super::InstanceRegistry;

    #[test]
    fn get_or_insert_reuses_live_entries_and_recreates_dropped_ones() {
        let registry = InstanceRegistry::new();
        let first = registry.get_or_insert_with("alpha", || Arc::new(1usize));
        let weak = Arc::downgrade(&first);
        let second = registry.get_or_insert_with("alpha", || Arc::new(2usize));

        assert!(Arc::ptr_eq(&first, &second));

        drop(first);
        drop(second);
        assert!(Weak::upgrade(&weak).is_none());

        let third = registry.get_or_insert_with("alpha", || Arc::new(3usize));
        assert_eq!(*third, 3);
    }

    #[test]
    fn get_or_try_insert_with_does_not_cache_errors() {
        let registry = InstanceRegistry::new();

        let err = registry
            .get_or_try_insert_with("beta", || -> Result<Arc<usize>, &'static str> {
                Err("boom")
            })
            .unwrap_err();
        assert_eq!(err, "boom");
        assert!(registry.get("beta").is_none());

        let inserted = registry
            .get_or_try_insert_with("beta", || Ok::<_, &'static str>(Arc::new(7usize)))
            .unwrap();
        assert!(Arc::ptr_eq(&inserted, &registry.get("beta").unwrap()));
    }

    #[test]
    fn default_instance_and_for_each_live_prune_dead_entries() {
        let registry = InstanceRegistry::new();
        let dropped = registry.get_or_insert_with("dropped", || Arc::new(1usize));
        let live = registry.get_or_insert_with("live", || Arc::new(2usize));

        registry.set_default(live.clone());
        assert!(Arc::ptr_eq(&registry.default_instance().unwrap(), &live));

        drop(dropped);
        let mut visited = Vec::new();
        registry.for_each_live(|value| visited.push(*value));

        assert_eq!(visited, vec![2]);
        assert!(registry.get("dropped").is_none());

        registry.clear_default();
        assert!(registry.default_instance().is_none());
    }
}
