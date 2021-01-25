// Copyright 2020 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use crate::{vertex::Vertex, MessageRef};

use bee_message::{Message, MessageId};

use async_trait::async_trait;
// use dashmap::{mapref::entry::Entry, DashMap};
use log::info;
use lru::LruCache;
use tokio::sync::{Mutex, RwLock as TRwLock, RwLockReadGuard as TRwLockReadGuard};
use fxhash::FxBuildHasher;

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    fmt::Debug,
    marker::PhantomData,
    ops::Deref,
    sync::atomic::{AtomicU64, Ordering},
};

const CACHE_LEN: usize = 100_000;

/// A trait used to provide hooks for a tangle. The tangle acts as an in-memory cache and will use hooks to extend its
/// effective volume. When an entry doesn't exist in the tangle cache and needs fetching, or when an entry gets
/// inserted, the tangle will call out to the hooks in order to fulfil these actions.
#[async_trait]
pub trait Hooks<T> {
    /// An error generated by these hooks.
    type Error: Debug;

    /// Fetch a message from some external storage medium.
    async fn get(&self, message_id: &MessageId) -> Result<Option<(Message, T)>, Self::Error>;
    /// Insert a message into some external storage medium.
    async fn insert(&self, message_id: MessageId, tx: Message, metadata: T) -> Result<(), Self::Error>;
    /// Fetch the approvers list for a given message.
    async fn fetch_approvers(&self, message_id: &MessageId) -> Result<Option<Vec<MessageId>>, Self::Error>;
    /// Insert a new approver for a given message.
    async fn insert_approver(&self, message_id: MessageId, approver: MessageId) -> Result<(), Self::Error>;
}

/// Phoney default hooks that do nothing.
pub struct NullHooks<T>(PhantomData<T>);

impl<T> Default for NullHooks<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

#[async_trait]
impl<T: Send + Sync> Hooks<T> for NullHooks<T> {
    type Error = ();

    async fn get(&self, _message_id: &MessageId) -> Result<Option<(Message, T)>, Self::Error> {
        Ok(None)
    }

    async fn insert(&self, _message_id: MessageId, _tx: Message, _metadata: T) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn fetch_approvers(&self, _message_id: &MessageId) -> Result<Option<Vec<MessageId>>, Self::Error> {
        Ok(None)
    }

    async fn insert_approver(&self, _message_id: MessageId, _approver: MessageId) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A foundational, thread-safe graph datastructure to represent the IOTA Tangle.
pub struct Tangle<T, H = NullHooks<T>>
where
    T: Clone,
{
    vertices: TRwLock<HashMap<MessageId, Vertex<T>, FxBuildHasher>>,
    children: TRwLock<HashMap<MessageId, (HashSet<MessageId, FxBuildHasher>, bool), FxBuildHasher>>,

    pub(crate) cache_counter: AtomicU64,
    pub(crate) cache_queue: Mutex<LruCache<MessageId, u64>>,

    pub(crate) hooks: H,
    // pub(crate) hooks: NullHooks<T>,
}

impl<T, H: Hooks<T>> Default for Tangle<T, H>
where
    T: Clone + Send + Sync,
    H: Default,
{
    fn default() -> Self {
        Self::new(H::default())
    }
}

impl<T, H: Hooks<T>> Tangle<T, H>
where
    T: Clone + Send + Sync,
{
    /// Creates a new Tangle.
    pub fn new(hooks: H) -> Self {
        Self {
            vertices: TRwLock::new(HashMap::default()),
            children: TRwLock::new(HashMap::default()),

            cache_counter: AtomicU64::new(0),
            cache_queue: Mutex::new(LruCache::new(CACHE_LEN + 1)),

            hooks: hooks,
            // hooks: NullHooks::default(),
        }
    }

    /// Create a new tangle with the given capacity.
    pub fn with_capacity(self, cap: usize) -> Self {
        Self {
            cache_queue: Mutex::new(LruCache::new(cap + 1)),
            ..self
        }
    }

    /// Return a reference to the storage hooks used by this tangle.
    pub fn hooks(&self) -> &H {
        &self.hooks
    }

    async fn insert_inner(&self, message_id: MessageId, message: Message, metadata: T) -> Option<MessageRef> {
        let r = match self.vertices.write().await.entry(message_id) {
            Entry::Occupied(_) => {},
            Entry::Vacant(entry) => {
                let parent1 = *message.parent1();
                let parent2 = *message.parent2();
                let vtx = Vertex::new(message, metadata);
                let tx = vtx.message().clone();
                self.add_children_inner(&[parent1, parent2], message_id).await;
                entry.insert(vtx);

                // Insert cache queue entry to track eviction priority
                self.cache_queue
                    .lock()
                    .await
                    .put(message_id, self.generate_cache_index());

                Some(tx)
            }
        };

        self.perform_eviction().await;

        r
    }

    /// Inserts a message, and returns a thread-safe reference to it in case it didn't already exist.
    pub async fn insert(&self, message_id: MessageId, message: Message, metadata: T) -> Option<MessageRef> {
        if self.contains_inner(&message_id).await {
            None
        } else {
            // Insert into backend using hooks
            self.hooks
                .insert(message_id, message.clone(), metadata.clone())
                .await
                .unwrap_or_else(|e| info!("Failed to insert message {:?}", e));

            self.insert_inner(message_id, message, metadata).await
        }
    }

    #[inline]
    async fn add_children_inner(&self, parents: &[MessageId], child: MessageId) {
        let mut children_map = self.children.write().await;
        for &parent in parents {
            children_map
                .entry(parent)
                .or_insert_with(|| (HashSet::default(), false))
                .0
                .insert(child);
        }
        drop(children_map);
        for &parent in parents {
            self.hooks
                .insert_approver(parent, child)
                .await
                .unwrap_or_else(|e| info!("Failed to update approvers for message {:?}", e));
        }
    }

    async fn get_inner(&self, message_id: &MessageId) -> Option<impl Deref<Target = Vertex<T>> + '_> {
        let res = TRwLockReadGuard::try_map(self.vertices.read().await, |m| m.get(message_id)).ok();

        if res.is_some() {
            let mut cache_queue = self.cache_queue.lock().await;
            // Update message_id priority
            let entry = cache_queue.get_mut(message_id);
            let entry = if entry.is_none() {
                cache_queue.put(*message_id, 0);
                cache_queue.get_mut(message_id)
            } else {
                entry
            };
            *entry.unwrap() = self.generate_cache_index();
        }

        res
    }

    /// Get the data of a vertex associated with the given `message_id`.
    pub async fn get(&self, message_id: &MessageId) -> Option<MessageRef> {
        self.pull_message(message_id).await;

        self.get_inner(message_id).await.map(|v| v.message().clone())
    }

    async fn contains_inner(&self, message_id: &MessageId) -> bool {
        self.vertices.read().await.contains_key(message_id)
    }

    /// Returns whether the message is stored in the Tangle.
    pub async fn contains(&self, message_id: &MessageId) -> bool {
        self.contains_inner(message_id).await || self.pull_message(message_id).await
    }

    /// Get the metadata of a vertex associated with the given `message_id`.
    pub async fn get_metadata(&self, message_id: &MessageId) -> Option<T> {
        self.pull_message(message_id).await;

        self.get_metadata_maybe(message_id).await
    }

    /// Get the metadata of a vertex associated with the given `message_id`, if it's in the cahce.
    pub async fn get_metadata_maybe(&self, message_id: &MessageId) -> Option<T> {
        self.get_inner(message_id).await.map(|v| v.metadata().clone())
    }

    /// Get the metadata of a vertex associated with the given `message_id`.
    pub async fn get_vertex(&self, message_id: &MessageId) -> Option<impl Deref<Target = Vertex<T>> + '_> {
        self.pull_message(message_id).await;

        self.get_inner(message_id).await
    }

    // /// Updates the metadata of a particular vertex.
    // pub async fn set_metadata(&self, message_id: &MessageId, metadata: T) {
    //     self.pull_message(message_id).await;
    //     let to_write = if let Some(vtx) = self.vertices.write().await.get_mut(message_id) {
    //         let message = (&**vtx.message()).clone();
    //         let meta = vtx.metadata().clone();
    //         *vtx.metadata_mut() = metadata;
    //         Some((message, meta))
    //     } else {
    //         None
    //     };

    //     if let Some((message, meta)) = to_write {
    //         self.hooks
    //             .insert(*message_id, message, meta)
    //             .await
    //             .unwrap_or_else(|e| info!("Failed to update metadata for message {:?}", e));
    //     }
    // }

    /// Updates the metadata of a vertex.
    pub async fn update_metadata<R, Update>(&self, message_id: &MessageId, mut update: Update) -> Option<R>
    where
        Update: FnMut(&mut T) -> R,
    {
        self.pull_message(message_id).await;
        let r = if let Some(vtx) = self.vertices.write().await.get_mut(message_id) {
            let message = (&**vtx.message()).clone();
            let metadata = vtx.metadata().clone();
            let r = update(vtx.metadata_mut());

            Some((r, message, metadata))
        } else {
            None
        };

        if let Some((r, message, metadata)) = r {
            self.hooks
                .insert(*message_id, message, metadata)
                .await
                .unwrap_or_else(|e| info!("Failed to update metadata for message {:?}", e));
            Some(r)
        } else {
            None
        }
    }

    /// Returns the number of messages in the Tangle cache.
    pub async fn len(&self) -> usize {
        // Does not take GTL because this is effectively atomic
        self.vertices.read().await.len()
    }

    /// Returns the maximum number of messages the Tangle cache can hold.
    pub async fn capacity(&self) -> usize {
        CACHE_LEN
    }

    /// Checks if the tangle is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    async fn children_inner(&self, message_id: &MessageId) -> Option<impl Deref<Target = HashSet<MessageId, FxBuildHasher>> + '_> {
        // struct Children<'a> {
        //     children: dashmap::mapref::one::Ref<'a, MessageId, (HashSet<MessageId>, bool)>,
        // }

        // impl<'a> Deref for Children<'a> {
        //     type Target = HashSet<MessageId>;

        //     fn deref(&self) -> &Self::Target {
        //         &self.children.deref().0
        //     }
        // }

        struct Wrapper<'a> {
            children: HashSet<MessageId, FxBuildHasher>,
            phantom: PhantomData<&'a ()>,
        }

        impl<'a> Deref for Wrapper<'a> {
            type Target = HashSet<MessageId, FxBuildHasher>;

            fn deref(&self) -> &Self::Target {
                &self.children
            }
        }

        let children_map = self.children.read().await;
        let children = children_map
            .get(message_id)
            // Skip approver lists that are not exhaustive
            .filter(|children| children.1);

        let children = match children {
            Some(children) => children.0.clone(),
            None => {
                let current_children = children.map(|c| c.0.clone());
                drop(children_map);

                let mut to_insert = match self.hooks.fetch_approvers(message_id).await {
                    Err(e) => {
                        info!("Failed to update approvers for message message {:?}", e);
                        Vec::new()
                    }
                    Ok(None) => Vec::new(),
                    Ok(Some(approvers)) => approvers,
                };

                if let Some(current_children) = current_children {
                    to_insert.extend(current_children.into_iter());
                }

                let to_insert: HashSet<_, _> = to_insert.into_iter().collect();
                let to_insert2 = to_insert.clone();
                self.children
                    .write()
                    .await
                    .insert(*message_id, (to_insert2, true));

                to_insert
            }
        };

        Some(/* Children { children } */ Wrapper {
            children,
            phantom: PhantomData,
        })
    }

    /// Returns the children of a vertex, if we know about them.
    pub async fn get_children(&self, message_id: &MessageId) -> Option<HashSet<MessageId, FxBuildHasher>> {
        // Effectively atomic
        self.children_inner(message_id).await.map(|approvers| approvers.clone())
    }

    /// Returns the number of children of a vertex.
    pub async fn num_children(&self, message_id: &MessageId) -> usize {
        // Effectively atomic
        self.children_inner(message_id)
            .await
            .map_or(0, |approvers| approvers.len())
    }

    #[cfg(test)]
    pub async fn clear(&mut self) {
        self.vertices.write().await.clear();
        self.children.write().await.clear();
    }

    // Attempts to pull the message from the storage, returns true if successful.
    async fn pull_message(&self, message_id: &MessageId) -> bool {
        // If the tangle already contains the tx, do no more work
        if self.vertices.read().await.contains_key(message_id) {
            true
        } else {
            if let Ok(Some((tx, metadata))) = self.hooks.get(message_id).await {
                self.insert_inner(*message_id, tx, metadata).await;
                true
            } else {
                false
            }
        }
    }

    fn generate_cache_index(&self) -> u64 {
        self.cache_counter.fetch_add(1, Ordering::Relaxed)
    }

    async fn perform_eviction(&self) {
        loop {
            let len = self.len().await;
            let mut cache = self.cache_queue.lock().await;

            if len < cache.cap() {
                break;
            }

            let remove = if cache.len() == cache.cap() {
                let (message_id, _) = cache.pop_lru().expect("Cache capacity is zero");
                Some(message_id)
            } else {
                None
            };

            drop(cache);

            if let Some(message_id) = remove {
                self.vertices
                    .write()
                    .await
                    .remove(&message_id)
                    .expect("Expected vertex entry to exist");
                self.children.write().await.remove(&message_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_test::message::create_random_tx;
    use pollster::block_on;

    #[test]
    fn new_tangle() {
        let _: Tangle<u8> = Tangle::default();
    }

    #[test]
    fn insert_and_contains() {
        let tangle = Tangle::<()>::default();

        let (message_id, tx) = create_random_tx();

        let insert1 = block_on(tangle.insert(message_id, tx.clone(), ()));

        assert!(insert1.is_some());
        assert_eq!(1, tangle.len());
        assert!(block_on(tangle.contains(&message_id)));

        let insert2 = block_on(tangle.insert(message_id, tx, ()));

        assert!(insert2.is_none());
        assert_eq!(1, tangle.len());
        assert!(block_on(tangle.contains(&message_id)));
    }

    #[test]
    fn eviction_cap() {
        let tangle = Tangle::<()>::default().with_capacity(5);

        let txs = (0..10).map(|_| create_random_tx()).collect::<Vec<_>>();

        for (message_id, tx) in txs.iter() {
            let _ = block_on(tangle.insert(*message_id, tx.clone(), ()));
        }

        assert_eq!(tangle.len(), 5);
    }

    #[test]
    fn eviction_update() {
        let tangle = Tangle::<()>::default().with_capacity(5);

        let txs = (0..8).map(|_| create_random_tx()).collect::<Vec<_>>();

        for (message_id, tx) in txs.iter().take(4) {
            let _ = block_on(tangle.insert(*message_id, tx.clone(), ()));
        }

        assert!(block_on(tangle.get(&txs[0].0)).is_some());

        for (message_id, tx) in txs.iter().skip(4) {
            let _ = block_on(tangle.insert(*message_id, tx.clone(), ()));
        }

        assert!(block_on(tangle.contains(&txs[0].0)));

        for entry in tangle.vertices.iter() {
            assert!(entry.key() == &txs[0].0 || txs[4..].iter().any(|(h, _)| entry.key() == h));
        }
    }
}
