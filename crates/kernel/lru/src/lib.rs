//! Least Recently Used (LRU) cache.

#![feature(allocator_api)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::{alloc::Global, collections::LinkedList, sync::Arc};
use core::{alloc::Allocator, ptr::NonNull, sync::atomic::AtomicUsize};

use mutex_api::Mutex;

/// Least Recently Used (LRU) cache.
#[derive(Debug)]
pub struct Lru<LruMutex>(LruMutex);

impl<LruMutex, K, V> Lru<LruMutex>
where
    LruMutex: Mutex<Data = LruMap<K, V>>,
{
    /// Creates a new LRU cache with the given size.
    #[must_use]
    pub fn new(size: usize) -> Self
    where
        V: Default,
    {
        Self(LruMutex::new(LruMap::new(size)))
    }
}

impl<LruMutex, K, V, A> Lru<LruMutex>
where
    LruMutex: Mutex<Data = LruMap<K, V, A>>,
    A: Allocator + Clone,
{
    /// Creates a new LRU cache with the given size and allocator.
    pub fn new_in(size: usize, alloc: A) -> Self
    where
        V: Default,
    {
        Self(LruMutex::new(LruMap::new_in(size, alloc)))
    }

    /// Returns a reference to the cached value associated with the key.
    ///
    /// If the value is cached, returns a reference to it.
    /// If the value is not cached, recycles the least recently used (LRU)
    /// unreferenced cache and returns a reference to it. If all values are
    /// referenced, returns `None`.
    ///
    /// When returned value is dropped, the key is promoted to the most recently
    /// used (MRU) position.
    pub fn get(&self, key: K) -> Option<LruValue<'_, LruMutex, K, V, A>>
    where
        K: PartialEq + Clone,
    {
        self.0.lock().get(key, &self.0)
    }
}

/// An key-value maps of Least Recently Used (LRU) cache.
#[expect(clippy::linkedlist)]
pub struct LruMap<K, V, A = Global>
where
    A: Allocator,
{
    list: LinkedList<(Option<K>, Arc<V, A>), A>,
}

/// Allocation layout for [`LruMap`].
///
/// This struct has same layout as `alloc::collections::linked_list::Node<T>`.
pub struct LruMapAllocLayout<K, V, A>
where
    A: Allocator,
{
    _next: Option<NonNull<Self>>,
    _prev: Option<NonNull<Self>>,
    _element: (Option<K>, Arc<V, A>),
}

unsafe impl<K, V, A> Send for LruMapAllocLayout<K, V, A> where A: Allocator {}

impl<K, V> Default for LruMap<K, V, Global> {
    fn default() -> Self {
        Self {
            list: LinkedList::default(),
        }
    }
}

impl<K, V> LruMap<K, V> {
    /// Creates a new `LruMap` with the given size.
    fn new(size: usize) -> Self
    where
        V: Default,
    {
        Self::new_in(size, Global)
    }
}

impl<K, V, A> LruMap<K, V, A>
where
    A: Allocator + Clone,
{
    /// Creates a new `LruMap` with the given size and allocator.
    fn new_in(size: usize, alloc: A) -> Self
    where
        V: Default,
    {
        assert!(size > 0, "size must be greater than 0");

        let mut list = LinkedList::new_in(alloc.clone());
        for _ in 0..size {
            list.push_back((None, Arc::new_in(V::default(), alloc.clone())));
        }
        Self { list }
    }

    /// Returns a reference to the cached value associated with the key.
    ///
    /// If the value is cached, returns a reference to the value.
    /// If the value is not cached, recycles the least recently used (LRU)
    /// value. If all buffers are in use, returns `None`.
    ///
    /// When returned value is dropped, the key is promoted to the most recently
    /// used (MRU) position.
    fn get<'a, LruMutex>(
        &mut self,
        key: K,
        list: &'a LruMutex,
    ) -> Option<LruValue<'a, LruMutex, K, V, A>>
    where
        LruMutex: Mutex<Data = Self>,
        K: PartialEq + Clone,
    {
        // Find the value with the key
        if let Some((_k, v)) = self.list.iter().find(|(k, _v)| k.as_ref() == Some(&key)) {
            return Some(LruValue {
                list,
                key,
                value: Arc::clone(v),
            });
        }

        // Not cached
        // Recycle the least recently used value.
        if let Some(buf) = self.list.iter_mut().rev().find_map(|(k, v)| {
            (Arc::strong_count(v) == 1).then(|| {
                *k = Some(key.clone());
                (k, v)
            })
        }) {
            return Some(LruValue {
                list,
                key,
                value: Arc::clone(buf.1),
            });
        }

        None
    }
}

impl<K, V, A> LruMap<K, V, A>
where
    A: Allocator,
{
    /// Promotes the cached value associated with the key to the most recently
    /// used (MRU) position.
    fn promote(&mut self, key: &K)
    where
        K: PartialEq,
    {
        let buf = self
            .list
            .extract_if(|(k, _v)| k.as_ref() == Some(key))
            .next();
        if let Some((k, v)) = buf {
            self.list.push_front((k, v));
        }
    }
}

/// A reference to the cached value associated with the key.
#[derive(Debug)]
pub struct LruValue<'list, LruMutex, K, V, A>
where
    LruMutex: Mutex<Data = LruMap<K, V, A>>,
    K: PartialEq,
    A: Allocator,
{
    list: &'list LruMutex,
    key: K,
    value: Arc<V, A>,
}

/// Allocation layout for `LruValue`.
///
/// This struct has same layout as `ArcInner<T>`.
pub struct LruValueAllocLayout<V> {
    _strong_count: AtomicUsize,
    _weak_count: AtomicUsize,
    _value: V,
}

impl<LruMutex, K, V, A> Drop for LruValue<'_, LruMutex, K, V, A>
where
    LruMutex: Mutex<Data = LruMap<K, V, A>>,
    K: PartialEq,
    A: Allocator,
{
    fn drop(&mut self) {
        self.list.lock().promote(&self.key);
    }
}

impl<LruMutex, K, V, A> LruValue<'_, LruMutex, K, V, A>
where
    LruMutex: Mutex<Data = LruMap<K, V, A>>,
    K: PartialEq,
    A: Allocator,
{
    /// Returns the cache key.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Returns a reference to the cached value.
    pub fn value(&self) -> &V {
        &self.value
    }
}

impl<LruMutex, K, V, A> Clone for LruValue<'_, LruMutex, K, V, A>
where
    LruMutex: Mutex<Data = LruMap<K, V, A>>,
    K: PartialEq + Clone,
    A: Allocator + Clone,
{
    fn clone(&self) -> Self {
        Self {
            list: self.list,
            key: self.key.clone(),
            value: Arc::clone(&self.value),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn test_lru_new() {
        let lru: Lru<Mutex<LruMap<i32, i32>>> = Lru::new(3);
        assert_eq!(lru.0.lock().unwrap().list.len(), 3);
    }

    #[test]
    fn test_lru_get_and_evict() {
        let lru: Lru<Mutex<LruMap<i32, i32>>> = Lru::new(2);
        assert!(lru.get(1).is_some());
        assert!(lru.get(2).is_some());
        assert!(lru.get(3).is_some());
    }

    #[test]
    fn test_lru_get_never_evict_referenced() {
        let lru: Lru<Mutex<LruMap<i32, i32>>> = Lru::new(2);

        let _c1 = lru.get(1).unwrap();
        let c2 = lru.get(2).unwrap();
        assert!(lru.get(3).is_none());

        // after drop reference, new entry can be obtained
        drop(c2);
        assert!(lru.get(3).is_some());
    }

    #[test]
    fn test_lru_get_same_cache() {
        let lru: Lru<Mutex<LruMap<i32, i32>>> = Lru::new(2);
        let _c1 = lru.get(1).unwrap();
        let _c2 = lru.get(2).unwrap();
        assert!(lru.get(1).is_some());
        assert!(lru.get(2).is_some());
        assert!(lru.get(3).is_none());
    }

    #[test]
    fn test_lru_promote() {
        let lru: Lru<Mutex<LruMap<i32, i32>>> = Lru::new(3);
        let value1 = lru.get(1).unwrap();
        let value2 = lru.get(2).unwrap();
        let value3 = lru.get(3).unwrap();
        drop(value1);
        assert_eq!(
            lru.0.lock().unwrap().list.front().as_ref().unwrap().0,
            Some(1)
        );
        drop(value2);
        assert_eq!(
            lru.0.lock().unwrap().list.front().as_ref().unwrap().0,
            Some(2)
        );
        drop(value3);
        assert_eq!(
            lru.0.lock().unwrap().list.front().as_ref().unwrap().0,
            Some(3)
        );
    }
}
