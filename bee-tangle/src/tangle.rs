// Copyright 2020 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use crate::{vertex::Vertex, MessageRef};
use crate::shashmap::HashMap;

use bee_message::{Message, MessageId};

use async_trait::async_trait;
// use dashmap::{mapref::entry::Entry, DashMap};
use log::info;
use lru::LruCache;
use tokio::sync::{RwLock as TRwLock, RwLockReadGuard as TRwLockReadGuard};
use spin::Mutex;

use std::{
    // collections::HashSet,
    fmt::Debug,
    marker::PhantomData,
    ops::Deref,
    sync::{Arc, atomic::{AtomicU64, Ordering}},
};
use hashbrown::HashSet;

const CACHE_LEN: usize = 1_000_000;

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
    /// Update the approvers list for a given message.
    async fn update_approvers(&self, message_id: MessageId, approvers: &Vec<MessageId>) -> Result<(), Self::Error>;
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

    async fn update_approvers(&self, _message_id: MessageId, _approvers: &Vec<MessageId>) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A foundational, thread-safe graph datastructure to represent the IOTA Tangle.
pub struct Tangle<T, H = NullHooks<T>>
where
    T: Clone,
{
    // Global Tangle Lock. Remove this as and when it is deemed correct to do so.
    // gtl: RwLock<()>,
    // vertices: TRwLock<HashMap<MessageId, Vertex<T>>>,
    // children: TRwLock<HashMap<MessageId, (HashSet<MessageId>, bool)>>,
    vertices: Arc<HashMap<MessageId, Vertex<T>>>,
    children: Arc<HashMap<MessageId, (HashSet<MessageId>, bool)>>,

    pub(crate) cache_counter: AtomicU64,
    pub(crate) cache_queue: Mutex<LruCache<MessageId, u64>>,

    pub(crate) hooks: H,
}

impl<T, H: Hooks<T>> Default for Tangle<T, H>
where
    T: Clone + Send + Sync + 'static,
    H: Default,
{
    fn default() -> Self {
        Self::new(H::default())
    }
}

impl<T, H: Hooks<T>> Tangle<T, H>
where
    T: Clone + Send + Sync + 'static,
{
    /// Creates a new Tangle.
    pub fn new(hooks: H) -> Self {
        Self {
            // gtl: RwLock::new(()),
            vertices: Arc::new(HashMap::new()),
            children: Arc::new(HashMap::new()),

            cache_counter: AtomicU64::new(0),
            cache_queue: Mutex::new(LruCache::new(CACHE_LEN + 1)),

            hooks,
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
        let parents = [*message.parent1(), *message.parent2()];

        let vtx = Vertex::new(message, metadata);
        let tx = vtx.message().clone();

        if self.vertices.insert(message_id, vtx).await.is_none() {
            // Insert cache queue entry to track eviction priority
            self.cache_queue
                .lock()
                .put(message_id, self.generate_cache_index());

            self.add_children_inner(&parents, message_id).await;
        }

        self.perform_eviction().await;

        Some(tx)
    }

    /// Inserts a message, and returns a thread-safe reference to it in case it didn't already exist.
    pub async fn insert(&self, message_id: MessageId, message: Message, metadata: T) -> Option<MessageRef> {
        // if self.contains_inner(&message_id).await {
        //     None
        // } else {
            // let _gtl_guard = self.gtl.write().await;
            let r = self.insert_inner(message_id, message.clone(), metadata.clone()).await;

            // Insert into backend using hooks
            self.hooks
                .insert(message_id, message, metadata)
                .await
                .unwrap_or_else(|e| info!("Failed to insert message {:?}", e));

            r
        // }
    }

    #[inline]
    async fn add_children_inner(&self, parents: &[MessageId], child: MessageId) {
        for &parent in parents {
            if self.children.do_for_mut(&parent, |map| map.0.insert(child)).await.is_none() {
                let mut set = HashSet::new();
                set.insert(child);
                self.children.insert(parent, (set, false)).await;
            }
        }

        for &parent in parents {
            self.hooks
                .insert_approver(parent, child)
                .await
                .unwrap_or_else(|e| info!("Failed to update approvers for message {:?}", e));
            // self.hooks
            // .update_approvers(parent, &children.iter().copied().collect::<Vec<_>>())
            // .await
            // .unwrap_or_else(|e| info!("Failed to update approvers for message message {:?}", e));
        }
    }

    async fn get_inner(&self, message_id: &MessageId) -> Option<impl Deref<Target = Vertex<T>> + '_> {
        let res = self.vertices.get(message_id).await;

        if res.is_some() {
            let mut cache_queue = self.cache_queue.lock();
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
        self.vertices.contains_key(message_id).await
    }

    /// Returns whether the message is stored in the Tangle.
    pub async fn contains(&self, message_id: &MessageId) -> bool {
        self.contains_inner(message_id).await || self.pull_message(message_id).await
    }

    /// Get the metadata of a vertex associated with the given `message_id`.
    pub async fn get_metadata(&self, message_id: &MessageId) -> Option<T> {
        self.pull_message(message_id).await;

        self.get_inner(message_id).await.map(|v| v.metadata().clone())
    }

    /// Get the metadata of a vertex associated with the given `message_id`.
    pub async fn get_vertex(&self, message_id: &MessageId) -> Option<impl Deref<Target = Vertex<T>> + '_> {
        self.pull_message(message_id).await;

        self.get_inner(message_id).await
    }

    /// Updates the metadata of a particular vertex.
    pub async fn set_metadata(&self, message_id: &MessageId, metadata: T) {
        self.pull_message(message_id).await;
        if let Some(mut vtx) = self.vertices.get_cloned(message_id).await {
            // let _gtl_guard = self.gtl.write().await;

            *vtx.metadata_mut() = metadata;

            let message = (&**vtx.message()).clone();
            let metadata = vtx.metadata().clone();
            self.vertices.insert(*message_id, vtx).await;
            self.hooks
                .insert(*message_id, message, metadata)
                .await
                .unwrap_or_else(|e| info!("Failed to update metadata for message {:?}", e));
        }
    }

    /// Updates the metadata of a vertex.
    pub async fn update_metadata<Update>(&self, message_id: &MessageId, mut update: Update)
    where
        Update: FnMut(&mut T),
    {
        self.pull_message(message_id).await;
        if let Some(mut vtx) = self.vertices.get_cloned(message_id).await {
            // let _gtl_guard = self.gtl.write().await;

            update(vtx.metadata_mut());

            let message = (&**vtx.message()).clone();
            let metadata = vtx.metadata().clone();
            self.vertices.insert(*message_id, vtx).await;
            self.hooks
                .insert(*message_id, message, metadata)
                .await
                .unwrap_or_else(|e| info!("Failed to update metadata for message {:?}", e));
        }
    }

    /// Returns the number of messages in the Tangle.
    pub async fn len(&self) -> usize {
        // Does not take GTL because this is effectively atomic
        self.vertices.len().await
    }

    /// Checks if the tangle is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    async fn children_inner(&self, message_id: &MessageId) -> Option<impl Deref<Target = HashSet<MessageId>> + '_> {
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
            children: HashSet<MessageId>,
            phantom: PhantomData<&'a ()>,
        }

        impl<'a> Deref for Wrapper<'a> {
            type Target = HashSet<MessageId>;

            fn deref(&self) -> &Self::Target {
                &self.children
            }
        }

        let children = self.children
            .get_cloned(message_id)
            .await
            // Skip approver lists that are not exhaustive
            .filter(|children| children.1);

        let children = match children {
            Some(children) => children.0,
            None => {
                // let _gtl_guard = self.gtl.write().await;

                let to_insert = match self.hooks.fetch_approvers(message_id).await {
                    Err(e) => {
                        info!("Failed to update approvers for message message {:?}", e);
                        Vec::new()
                    }
                    Ok(None) => Vec::new(),
                    Ok(Some(approvers)) => approvers,
                };

                self.children
                    .insert(*message_id, (to_insert.into_iter().collect(), true))
                    .await;

                self.children
                    .get_cloned(message_id)
                    .await
                    .expect("Approver list inserted and immediately evicted")
                    .0
            }
        };

        Some(/* Children { children } */ Wrapper {
            children,
            phantom: PhantomData,
        })
    }

    /// Returns the children of a vertex, if we know about them.
    pub async fn get_children(&self, message_id: &MessageId) -> Option<HashSet<MessageId>> {
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
        // let _gtl_guard = self.gtl.write().await;

        self.vertices.write().await.clear();
        self.children.write().await.clear();
    }

    // Attempts to pull the message from the storage, returns true if successful.
    async fn pull_message(&self, message_id: &MessageId) -> bool {
        // If the tangle already contains the tx, do no more work
        if self.vertices.contains_key(message_id).await {
            true
        } else {
            // let _gtl_guard = self.gtl.write().await;

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
        const CACHE_THRESHOLD: usize = 1024;

        if self.len().await < self.cache_queue.lock().cap() + CACHE_THRESHOLD {
            return;
        }

        let mut to_remove = Vec::new();
        loop {
            let len = self.len().await;
            let mut cache = self.cache_queue.lock();

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
                to_remove.push(message_id);
            }
        }

        let vertices = self.vertices.clone();
        let children = self.children.clone();
        tokio::task::spawn(async move {
            for message_id in to_remove {
                vertices
                    .remove(&message_id)
                    .await
                    .expect("Expected vertex entry to exist");
                children.remove(&message_id).await;
            }
        });
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
