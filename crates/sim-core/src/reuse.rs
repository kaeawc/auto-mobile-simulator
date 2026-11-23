//! When a device connection may be reused. The iOS backend keeps each device's HID client and
//! display between input calls, keyed by the device's boot: a reboot (or a device whose boots it
//! can't tell apart) connects again, and so does any failure.

/// A value (a connection) kept while its key stays the same.
#[derive(Debug)]
pub struct Reuse<K, T> {
    slot: Option<(K, T)>,
}

impl<K, T> Default for Reuse<K, T> {
    fn default() -> Self {
        Self { slot: None }
    }
}

impl<K: PartialEq, T> Reuse<K, T> {
    /// Runs `use_it` with the value for `key`, making it first when there is none or it was made
    /// under another key. A `None` key means the caller can't tell one session from the next: the
    /// value is made for this use only and nothing is kept. An error from `make` or `use_it` drops
    /// the value, so the next use makes a new one.
    pub fn with<R, E>(
        &mut self,
        key: Option<K>,
        make: impl FnOnce() -> Result<T, E>,
        use_it: impl FnOnce(&T) -> Result<R, E>,
    ) -> Result<R, E> {
        let Some(key) = key else {
            self.slot = None;
            return use_it(&make()?);
        };
        let (_, value) = match self.slot.take() {
            Some((kept, value)) if kept == key => self.slot.insert((kept, value)),
            _ => self.slot.insert((key, make()?)),
        };
        let result = use_it(value);
        if result.is_err() {
            self.slot = None;
        }
        result
    }

    /// Drops the kept value.
    pub fn clear(&mut self) {
        self.slot = None;
    }

    /// The key of the kept value, if any.
    pub fn key(&self) -> Option<&K> {
        self.slot.as_ref().map(|(k, _)| k)
    }
}

/// One boot of a device: a connection made in it is reused until the device's state changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootKey {
    changes: Option<u64>,
    booted_at: Option<u64>,
}

impl BootKey {
    /// The key for a device now: none unless it's booted and something tells its boots apart, a
    /// count of its state changes (from a notification handler) or when its boot began.
    pub fn of(booted: bool, changes: Option<u64>, booted_at: Option<f64>) -> Option<Self> {
        if !booted || (changes.is_none() && booted_at.is_none()) {
            return None;
        }
        Some(Self { changes, booted_at: booted_at.map(f64::to_bits) })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn a_boot_key_needs_a_booted_device_whose_boots_can_be_told_apart() {
        assert_eq!(BootKey::of(false, Some(0), Some(1.0)), None, "not booted: nothing to reuse");
        assert_eq!(BootKey::of(true, None, None), None, "can't tell a reboot: connect per call");
        let watched = BootKey::of(true, Some(3), None);
        assert!(watched.is_some());
        assert_ne!(watched, BootKey::of(true, Some(4), None), "a state change is a new key");
        let dated = BootKey::of(true, None, Some(780_000_000.5));
        assert_eq!(dated, BootKey::of(true, None, Some(780_000_000.5)));
        assert_ne!(dated, BootKey::of(true, None, Some(780_000_100.0)), "a later boot is a new key");
        assert_ne!(BootKey::of(true, Some(3), Some(1.0)), BootKey::of(true, Some(3), Some(2.0)));
    }

    /// Makes "connections" numbered by when they were made.
    fn maker(made: &Cell<u32>) -> impl Fn() -> Result<u32, String> + '_ {
        || {
            made.set(made.get() + 1);
            Ok(made.get())
        }
    }

    /// Uses the connection for `key`; returns which one it was.
    fn use_with(cache: &mut Reuse<u64, u32>, made: &Cell<u32>, key: Option<u64>) -> Result<u32, String> {
        cache.with(key, maker(made), |c| Ok(*c))
    }

    #[test]
    fn reuses_the_value_while_the_key_holds() {
        let (mut cache, made) = (Reuse::default(), Cell::new(0));
        for _ in 0..3 {
            assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(1));
        }
        assert_eq!(made.get(), 1);
        assert_eq!(cache.key(), Some(&7));
    }

    #[test]
    fn a_new_key_makes_a_new_value() {
        let (mut cache, made) = (Reuse::default(), Cell::new(0));
        assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(1));
        assert_eq!(use_with(&mut cache, &made, Some(8)), Ok(2), "a reboot connects again");
        assert_eq!(use_with(&mut cache, &made, Some(8)), Ok(2));
        assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(3), "keys aren't remembered beyond the last");
    }

    #[test]
    fn no_key_makes_a_value_per_use_and_keeps_none() {
        let (mut cache, made) = (Reuse::default(), Cell::new(0));
        assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(1));
        assert_eq!(use_with(&mut cache, &made, None), Ok(2));
        assert_eq!(cache.key(), None, "a session that can't be told apart drops the kept value too");
        assert_eq!(use_with(&mut cache, &made, None), Ok(3));
        assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(4));
    }

    #[test]
    fn a_failed_use_drops_the_value() {
        let (mut cache, made) = (Reuse::default(), Cell::new(0));
        assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(1));
        let failed: Result<(), String> = cache.with(Some(7), maker(&made), |_| Err("send failed".into()));
        assert_eq!(failed, Err("send failed".into()));
        assert_eq!(cache.key(), None);
        assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(2));
    }

    #[test]
    fn a_failed_make_keeps_nothing_and_is_tried_again() {
        let (mut cache, made) = (Reuse::default(), Cell::new(0));
        assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(1));
        let failed: Result<u32, String> = cache.with(Some(8), || Err("not booted".into()), |c| Ok(*c));
        assert_eq!(failed, Err("not booted".into()));
        assert_eq!(cache.key(), None, "the old boot's value is gone too");
        assert_eq!(use_with(&mut cache, &made, Some(8)), Ok(2));
    }

    #[test]
    fn clear_drops_the_value() {
        let (mut cache, made) = (Reuse::default(), Cell::new(0));
        assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(1));
        cache.clear();
        assert_eq!(use_with(&mut cache, &made, Some(7)), Ok(2));
    }
}
