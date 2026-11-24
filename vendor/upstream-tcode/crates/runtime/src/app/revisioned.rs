use std::{
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, Ordering},
};

/// Invalidates derived projections on mutable access, including whole-value replacement.
/// Values used here must not mutate projection inputs through shared references.
pub struct Revisioned<T> {
    value: T,
    revision: u64,
}

impl<T> Revisioned<T> {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }
}

impl<T> From<T> for Revisioned<T> {
    fn from(value: T) -> Self {
        Self {
            value,
            revision: next_revision(),
        }
    }
}

impl<T: Default> Default for Revisioned<T> {
    fn default() -> Self {
        T::default().into()
    }
}

impl<T> Deref for Revisioned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Revisioned<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.revision = next_revision();
        &mut self.value
    }
}

fn next_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
