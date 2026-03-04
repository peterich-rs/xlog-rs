use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

pub struct InstanceRegistry<T> {
    instances: Mutex<HashMap<String, Weak<T>>>,
    default: Mutex<Option<Arc<T>>>,
}

impl<T> Default for InstanceRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> InstanceRegistry<T> {
    pub fn new() -> Self {
        Self {
            instances: Mutex::new(HashMap::new()),
            default: Mutex::new(None),
        }
    }

    pub fn get_or_insert_with<F>(&self, name: &str, new_value: F) -> Arc<T>
    where
        F: FnOnce() -> Arc<T>,
    {
        let mut map = self.instances.lock().expect("instances lock poisoned");
        if let Some(existing) = map.get(name).and_then(Weak::upgrade) {
            return existing;
        }
        let value = new_value();
        map.insert(name.to_string(), Arc::downgrade(&value));
        value
    }

    pub fn get_or_try_insert_with<F, E>(&self, name: &str, new_value: F) -> Result<Arc<T>, E>
    where
        F: FnOnce() -> Result<Arc<T>, E>,
    {
        let mut map = self.instances.lock().expect("instances lock poisoned");
        if let Some(existing) = map.get(name).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        let value = new_value()?;
        map.insert(name.to_string(), Arc::downgrade(&value));
        Ok(value)
    }

    pub fn get(&self, name: &str) -> Option<Arc<T>> {
        let map = self.instances.lock().ok()?;
        map.get(name)?.upgrade()
    }

    pub fn set_default(&self, value: Arc<T>) {
        let mut default = self.default.lock().expect("default lock poisoned");
        *default = Some(value);
    }

    pub fn default_instance(&self) -> Option<Arc<T>> {
        self.default.lock().ok()?.clone()
    }

    pub fn clear_default(&self) {
        let mut default = self.default.lock().expect("default lock poisoned");
        *default = None;
    }

    pub fn for_each_live<F>(&self, mut f: F)
    where
        F: FnMut(Arc<T>),
    {
        let live: Vec<Arc<T>> = {
            let mut map = self.instances.lock().expect("instances lock poisoned");
            map.retain(|_, v| v.upgrade().is_some());
            map.values().filter_map(Weak::upgrade).collect()
        };
        for instance in live {
            f(instance);
        }
    }
}
