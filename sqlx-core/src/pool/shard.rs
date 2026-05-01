use crate::rt;
use event_listener::{listener, Event, IntoNotification};
use futures_util::{future, stream, StreamExt};
use spin::lock_api::Mutex;
use std::future::Future;
use std::num::NonZero;
use std::ops::{Deref, DerefMut};
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{atomic, Arc};
use std::task::{ready, Poll};
use std::time::Duration;
use std::{array, iter};

type ShardId = usize;
type ConnectionIndex = usize;

/// Delay before a task waiting in a call to `acquire()` enters the global wait queue.
///
/// We want tasks to acquire from their local shards where possible, so they don't enter
/// the global queue immediately.
const GLOBAL_ACQUIRE_DELAY: Duration = Duration::from_millis(10);

/// Delay before attempting to acquire from a non-local shard,
/// as well as the backoff when iterating through shards.
const NON_LOCAL_ACQUIRE_DELAY: Duration = Duration::from_micros(100);

pub struct Sharded<T> {
    shards: Box<[ArcShard<T>]>,
    global: Arc<Global<T>>,
}

type ArcShard<T> = Arc<Shard<T, [Arc<Mutex<Option<T>>>]>>;

struct Global<T, F: ?Sized = dyn Fn(DisconnectedSlot<T>) + Send + Sync + 'static> {
    unlock_event: Event<SlotGuard<T>>,
    disconnect_event: Event<SlotGuard<T>>,
    min_connections: usize,
    num_shards: usize,
    do_reconnect: F,
}

type ArcMutexGuard<T> = lock_api::ArcMutexGuard<spin::Mutex<()>, Option<T>>;

struct SlotGuard<T> {
    // `Option` allows us to take the guard in the drop handler.
    locked: Option<ArcMutexGuard<T>>,
    shard: ArcShard<T>,
    index: ConnectionIndex,
    dropped: bool,
}

pub struct ConnectedSlot<T>(SlotGuard<T>);

pub struct DisconnectedSlot<T>(SlotGuard<T>);

// Align to cache lines.
// Simplified from https://docs.rs/crossbeam-utils/0.8.21/src/crossbeam_utils/cache_padded.rs.html#80
//
// Instead of listing every possible architecture, we just assume 64-bit architectures have 128-byte
// cache lines, which is at least true for newer versions of x86-64 and AArch64.
// A larger alignment isn't harmful as long as we make use of the space.
#[cfg_attr(target_pointer_width = "64", repr(align(128)))]
#[cfg_attr(not(target_pointer_width = "64"), repr(align(64)))]
struct Shard<T, Ts: ?Sized> {
    shard_id: ShardId,
    /// Bitset for all connection indices that are currently in-use.
    locked_set: AtomicUsize,
    /// Bitset for all connection indices that are currently connected.
    connected_set: AtomicUsize,
    /// Bitset for all connection indices that have been explicitly leaked.
    leaked_set: AtomicUsize,
    unlock_event: Event<SlotGuard<T>>,
    disconnect_event: Event<SlotGuard<T>>,
    leak_event: Event<ConnectionIndex>,
    global: Arc<Global<T>>,
    connections: Ts,
}

#[derive(Debug)]
struct Params {
    shards: usize,
    shard_size: usize,
    remainder: usize,
}

const MAX_SHARD_SIZE: usize = if usize::BITS > 64 {
    64
} else {
    usize::BITS as usize
};

impl<T> Sharded<T> {
    pub fn new(
        connections: usize,
        shards: NonZero<usize>,
        min_connections: usize,
        do_reconnect: impl Fn(DisconnectedSlot<T>) + Send + Sync + 'static,
    ) -> Sharded<T> {
        let params = Params::calc(connections, shards.get());

        let global = Arc::new(Global {
            unlock_event: Event::with_tag(),
            disconnect_event: Event::with_tag(),
            num_shards: params.shards,
            min_connections,
            do_reconnect,
        });

        let shards = params
            .shard_sizes()
            .enumerate()
            .map(|(shard_id, size)| Shard::new(shard_id, size, global.clone()))
            .collect::<Box<[_]>>();

        Sharded { shards, global }
    }

    #[inline]
    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    #[allow(clippy::cast_possible_truncation)] // This is only informational
    pub fn count_connected(&self) -> usize {
        atomic::fence(Ordering::Acquire);

        self.shards
            .iter()
            .map(|shard| shard.connected_set.load(Ordering::Relaxed).count_ones() as usize)
            .sum()
    }

    #[allow(clippy::cast_possible_truncation)] // This is only informational
    pub fn count_unlocked(&self, connected: bool) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.unlocked_mask(connected).count_ones())
            .sum()
    }

    pub async fn acquire_connected(&self) -> ConnectedSlot<T> {
        let guard = self.acquire(true).await;

        assert!(
            guard.get().is_some(),
            "BUG: expected slot {}/{} to be connected but it wasn't",
            guard.shard.shard_id,
            guard.index
        );

        ConnectedSlot(guard)
    }

    pub fn try_acquire_connected(&self) -> Option<ConnectedSlot<T>> {
        todo!()
    }

    pub async fn acquire_disconnected(&self) -> DisconnectedSlot<T> {
        let guard = self.acquire(false).await;

        assert!(
            guard.get().is_none(),
            "BUG: expected slot {}/{} NOT to be connected but it WAS",
            guard.shard.shard_id,
            guard.index
        );

        DisconnectedSlot(guard)
    }

    async fn acquire(&self, connected: bool) -> SlotGuard<T> {
        if self.shards.len() == 1 {
            return self.shards[0].acquire(connected).await;
        }

        let thread_id = current_thread_id();

        let mut acquire_local = pin!(self.shards[thread_id % self.shards.len()].acquire(connected));

        let mut acquire_nonlocal = pin!(async {
            let mut next_shard = thread_id;

            loop {
                rt::sleep(NON_LOCAL_ACQUIRE_DELAY).await;

                // Choose shards pseudorandomly by multiplying with a (relatively) large prime.
                next_shard = (next_shard.wrapping_mul(547)) % self.shards.len();

                if let Some(locked) = self.shards[next_shard].try_acquire(connected) {
                    return locked;
                }
            }
        });

        let mut acquire_global = pin!(async {
            rt::sleep(GLOBAL_ACQUIRE_DELAY).await;

            let event_to_listen = if connected {
                &self.global.unlock_event
            } else {
                &self.global.disconnect_event
            };

            event_listener::listener!(event_to_listen => listener);
            listener.await
        });

        // Hand-rolled `select!{}` because there isn't a great cross-runtime solution.
        //
        // `futures_util::select!{}` is a proc-macro.
        std::future::poll_fn(|cx| {
            if let Poll::Ready(locked) = acquire_local.as_mut().poll(cx) {
                return Poll::Ready(locked);
            }

            if let Poll::Ready(locked) = acquire_nonlocal.as_mut().poll(cx) {
                return Poll::Ready(locked);
            }

            if let Poll::Ready(locked) = acquire_global.as_mut().poll(cx) {
                return Poll::Ready(locked);
            }

            Poll::Pending
        })
        .await
    }

    pub fn iter_min_connections(&self) -> impl Iterator<Item = DisconnectedSlot<T>> + '_ {
        self.shards
            .iter()
            .flat_map(|shard| shard.iter_min_connections())
    }

    pub fn iter_idle(&self) -> impl Iterator<Item = ConnectedSlot<T>> + '_ {
        self.shards.iter().flat_map(|shard| shard.iter_idle())
    }

    pub async fn drain<F, Fut>(&self, close: F)
    where
        F: Fn(ConnectedSlot<T>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DisconnectedSlot<T>> + Send + 'static,
        T: Send + 'static,
    {
        let close = Arc::new(close);

        stream::iter(self.shards.iter())
            .for_each_concurrent(None, |shard| {
                let shard = shard.clone();
                let close = close.clone();

                rt::spawn(async move {
                    shard.drain(&*close).await;
                })
            })
            .await;
    }
}

impl<T> Shard<T, [Arc<Mutex<Option<T>>>]> {
    fn new(shard_id: ShardId, len: usize, global: Arc<Global<T>>) -> Arc<Self> {
        // There's no way to create DSTs like this, in `std::sync::Arc`, on stable.
        //
        // Instead, we coerce from an array.
        macro_rules! make_array {
            ($($n:literal),+) => {
                match len {
                    $($n => Arc::new(Shard {
                        shard_id,
                        locked_set: AtomicUsize::new(0),
                        connected_set: AtomicUsize::new(0),
                        leaked_set: AtomicUsize::new(0),
                        unlock_event: Event::with_tag(),
                        disconnect_event: Event::with_tag(),
                        leak_event: Event::with_tag(),
                        global,
                        connections: array::from_fn::<_, $n, _>(|_| Arc::new(Mutex::new(None)))
                    }),)*
                    _ => unreachable!("BUG: length not supported: {len}"),
                }
            }
        }

        make_array!(
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45,
            46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64
        )
    }

    #[inline]
    fn unlocked_mask(&self, connected: bool) -> Mask {
        let locked_set = self.locked_set.load(Ordering::Acquire);
        let connected_set = self.connected_set.load(Ordering::Relaxed);

        let connected_mask = if connected {
            connected_set
        } else {
            !connected_set
        };

        Mask(!locked_set & connected_mask)
    }

    async fn acquire(self: &Arc<Self>, connected: bool) -> SlotGuard<T> {
        // Attempt an unfair acquire first, before we modify the waitlist.
        if let Some(locked) = self.try_acquire(connected) {
            return locked;
        }

        let event_to_listen = if connected {
            &self.unlock_event
        } else {
            &self.disconnect_event
        };

        event_listener::listener!(event_to_listen => listener);

        let mut listener = pin!(listener);

        loop {
            // We need to check again after creating the event listener,
            // because in the meantime, a concurrent task may have seen that there were no listeners
            // and just unlocked its connection.
            match rt::timeout(NON_LOCAL_ACQUIRE_DELAY, listener.as_mut()).await {
                Ok(slot) => return slot,
                Err(_) => {
                    if let Some(slot) = self.try_acquire(connected) {
                        return slot;
                    }
                }
            }
        }
    }

    fn try_acquire(self: &Arc<Self>, connected: bool) -> Option<SlotGuard<T>> {
        // If `locked_set` is constantly changing, don't loop forever.
        for index in self.unlocked_mask(connected) {
            if let Some(slot) = self.try_lock(index) {
                return Some(slot);
            }

            std::hint::spin_loop();
        }

        None
    }

    fn try_lock(self: &Arc<Self>, index: ConnectionIndex) -> Option<SlotGuard<T>> {
        let locked = self.connections.get(index)?.try_lock_arc()?;

        // The locking of the connection itself must use an `Acquire` fence,
        // so additional synchronization is unnecessary.
        atomic_set(&self.locked_set, index, true, Ordering::Relaxed);

        Some(SlotGuard {
            locked: Some(locked),
            shard: self.clone(),
            index,
            dropped: false,
        })
    }

    fn iter_min_connections(self: &Arc<Self>) -> impl Iterator<Item = DisconnectedSlot<T>> + '_ {
        self.unlocked_mask(false)
            .filter_map(|index| {
                let slot = self.try_lock(index)?;

                // Guard against some weird bug causing this to already be connected
                slot.get().is_none().then_some(DisconnectedSlot(slot))
            })
            .take(self.global.shard_min_connections(self.shard_id))
    }

    fn iter_idle(self: &Arc<Self>) -> impl Iterator<Item = ConnectedSlot<T>> + '_ {
        self.unlocked_mask(true).filter_map(|index| {
            let slot = self.try_lock(index)?;

            // Guard against some weird bug causing this to already be connected
            slot.get().is_some().then_some(ConnectedSlot(slot))
        })
    }

    fn all_leaked(&self) -> bool {
        let all_leaked_mask = (1usize << self.connections.len()) - 1;
        let leaked_set = self.leaked_set.load(Ordering::Acquire);

        leaked_set == all_leaked_mask
    }

    async fn drain<F, Fut>(self: &Arc<Self>, close: F)
    where
        F: Fn(ConnectedSlot<T>) -> Fut,
        Fut: Future<Output = DisconnectedSlot<T>>,
    {
        let mut drain_connected = pin!(async {
            loop {
                let connected = self.acquire(true).await;
                DisconnectedSlot::leak(close(ConnectedSlot(connected)).await);
            }
        });

        let mut drain_disconnected = pin!(async {
            loop {
                let disconnected = DisconnectedSlot(self.acquire(false).await);
                DisconnectedSlot::leak(disconnected);
            }
        });

        let mut drain_leaked = pin!(async {
            loop {
                listener!(self.leak_event => leaked);
                leaked.await;
            }
        });

        std::future::poll_fn(|cx| {
            // The connection set is drained once all slots are leaked.
            if self.all_leaked() {
                return Poll::Ready(());
            }

            // These futures shouldn't return `Ready`
            let _ = drain_connected.as_mut().poll(cx);
            let _ = drain_disconnected.as_mut().poll(cx);
            let _ = drain_leaked.as_mut().poll(cx);

            // Check again after driving the `drain` futures forward.
            if self.all_leaked() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

impl<T> Deref for ConnectedSlot<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
            .get()
            .as_ref()
            .expect("BUG: expected slot to be populated, but it wasn't")
    }
}

impl<T> DerefMut for ConnectedSlot<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .get_mut()
            .as_mut()
            .expect("BUG: expected slot to be populated, but it wasn't")
    }
}

impl<T> ConnectedSlot<T> {
    pub fn take(mut this: Self) -> (T, DisconnectedSlot<T>) {
        let conn = this
            .0
            .get_mut()
            .take()
            .expect("BUG: expected slot to be populated, but it wasn't");

        atomic_set(
            &this.0.shard.connected_set,
            this.0.index,
            false,
            Ordering::AcqRel,
        );

        (conn, DisconnectedSlot(this.0))
    }
}

impl<T> DisconnectedSlot<T> {
    pub fn put(mut self, connection: T) -> ConnectedSlot<T> {
        *self.0.get_mut() = Some(connection);

        atomic_set(
            &self.0.shard.connected_set,
            self.0.index,
            true,
            Ordering::AcqRel,
        );

        ConnectedSlot(self.0)
    }

    pub fn leak(mut self: Self) {
        self.0.locked = None;

        atomic_set(
            &self.0.shard.connected_set,
            self.0.index,
            false,
            Ordering::Relaxed,
        );
        atomic_set(
            &self.0.shard.leaked_set,
            self.0.index,
            true,
            Ordering::AcqRel,
        );

        self.0.shard.leak_event.notify(usize::MAX.tag(self.0.index));
    }

    pub fn should_reconnect(&self) -> bool {
        self.0.should_reconnect()
    }
}

impl<T> SlotGuard<T> {
    fn get(&self) -> &Option<T> {
        self.locked
            .as_deref()
            .expect("BUG: `SlotGuard.locked` taken")
    }

    fn get_mut(&mut self) -> &mut Option<T> {
        self.locked
            .as_deref_mut()
            .expect("BUG: `SlotGuard.locked` taken")
    }

    fn should_reconnect(&self) -> bool {
        let min_connections = self.shard.global.shard_min_connections(self.shard.shard_id);

        let num_connected = self
            .shard
            .connected_set
            .load(Ordering::Acquire)
            .count_ones() as usize;

        num_connected < min_connections
    }
}

impl<T> Drop for SlotGuard<T> {
    fn drop(&mut self) {
        let Some(locked) = self.locked.take() else {
            return;
        };

        let connected = locked.is_some();

        // Updating the connected flag shouldn't require a fence.
        atomic_set(
            &self.shard.connected_set,
            self.index,
            connected,
            Ordering::Relaxed,
        );

        // We don't actually unlock the connection unless there's no receivers to accept it.
        // If another receiver is waiting for a connection, we can directly pass them the lock.
        //
        // This prevents drive-by tasks from acquiring connections before waiting tasks
        // at high contention, while requiring little synchronization otherwise.
        //
        // We *could* just pass them the shard ID and/or index, but then we have to handle
        // the situation when a receiver was passed a connection that was still marked as locked,
        // but was cancelled before it could complete the acquisition. Otherwise, the connection
        // would be marked as locked forever, effectively being leaked.
        let mut locked = Some(locked);

        // This is a code smell, but it's necessary because `event-listener` has no way to specify
        // that a message should *only* be sent once. This means tags either need to be `Clone`
        // or provided by a `FnMut()` closure.
        //
        // Note that there's no guarantee that this closure won't be called more than once by the
        // implementation, but the code as of writing should not.
        let mut self_as_tag = || {
            let locked = locked
                .take()
                .expect("BUG: notification sent more than once");

            SlotGuard {
                locked: Some(locked),
                shard: self.shard.clone(),
                index: self.index,
                // To avoid infinite recursion or deadlock, don't send another notification
                // if this guard was already dropped once: just unlock it.
                dropped: true,
            }
        };

        if !self.dropped && connected {
            // Check for global waiters first.
            if self
                .shard
                .global
                .unlock_event
                .notify(1.tag_with(&mut self_as_tag))
                > 0
            {
                return;
            }

            if self.shard.unlock_event.notify(1.tag_with(&mut self_as_tag)) > 0 {
                return;
            }
        } else if !self.dropped {
            if self
                .shard
                .global
                .disconnect_event
                .notify(1.tag_with(&mut self_as_tag))
                > 0
            {
                return;
            }

            if self
                .shard
                .disconnect_event
                .notify(1.tag_with(&mut self_as_tag))
                > 0
            {
                return;
            }

            if self.should_reconnect() {
                (self.shard.global.do_reconnect)(DisconnectedSlot(self_as_tag()));
                return;
            }
        }

        // Be sure to drop the lock guard if it's still held,
        // *before* we semantically release the lock in the bitset.
        //
        // Otherwise, another task could check and see the connection is free,
        // but then fail to lock the mutex for it.
        drop(locked);

        atomic_set(&self.shard.locked_set, self.index, false, Ordering::AcqRel);
    }
}

impl<T> Global<T> {
    fn shard_min_connections(&self, shard_id: ShardId) -> usize {
        let min_connections_per_shard = self.min_connections / self.num_shards;

        if (self.min_connections % self.num_shards) < shard_id {
            min_connections_per_shard + 1
        } else {
            min_connections_per_shard
        }
    }
}

impl Params {
    fn calc(connections: usize, mut shards: usize) -> Params {
        assert_ne!(shards, 0);

        let mut shard_size = connections / shards;
        let mut remainder = connections % shards;

        if shard_size == 0 {
            tracing::debug!(connections, shards, "more shards than connections; clamping shard size to 1, shard count to connections");
            shards = connections;
            shard_size = 1;
            remainder = 0;
        } else if shard_size >= MAX_SHARD_SIZE {
            let new_shards = connections.div_ceil(MAX_SHARD_SIZE);

            tracing::debug!(
                connections,
                shards,
                "shard size exceeds {MAX_SHARD_SIZE}, clamping shard count to {new_shards}"
            );

            shards = new_shards;
            shard_size = connections / shards;
            remainder = connections % shards;
        }

        Params {
            shards,
            shard_size,
            remainder,
        }
    }

    fn shard_sizes(&self) -> impl Iterator<Item = usize> {
        iter::repeat_n(self.shard_size + 1, self.remainder).chain(iter::repeat_n(
            self.shard_size,
            self.shards - self.remainder,
        ))
    }
}

fn atomic_set(atomic: &AtomicUsize, index: usize, value: bool, ordering: Ordering) {
    if value {
        let bit = 1 << index;
        atomic.fetch_or(bit, ordering);
    } else {
        let bit = !(1 << index);
        atomic.fetch_and(bit, ordering);
    }
}

fn current_thread_id() -> usize {
    // FIXME: this can be replaced when this is stabilized:
    // https://doc.rust-lang.org/stable/std/thread/struct.ThreadId.html#method.as_u64
    static THREAD_ID: AtomicUsize = AtomicUsize::new(0);

    thread_local! {
        static CURRENT_THREAD_ID: usize = THREAD_ID.fetch_add(1, Ordering::SeqCst);
    }

    CURRENT_THREAD_ID.with(|i| *i)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Mask(usize);

impl Mask {
    pub fn count_ones(&self) -> usize {
        self.0.count_ones() as usize
    }
}

impl Iterator for Mask {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0 == 0 {
            return None;
        }

        let index = self.0.trailing_zeros() as usize;
        self.0 &= !(1 << index);

        Some(index)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let count = self.0.count_ones() as usize;
        (count, Some(count))
    }
}

#[cfg(test)]
mod tests {
    use super::{Mask, Params, MAX_SHARD_SIZE};

    #[test]
    fn test_params() {
        for connections in 0..100 {
            for shards in 1..32 {
                let params = Params::calc(connections, shards);

                let mut sum = 0;

                for (i, size) in params.shard_sizes().enumerate() {
                    assert!(size <= MAX_SHARD_SIZE, "Params::calc({connections}, {shards}) exceeded MAX_SHARD_SIZE at shard #{i}, size {size}");

                    sum += size;

                    assert!(sum <= connections, "Params::calc({connections}, {shards}) exceeded connections at shard #{i}, size {size}");
                }

                assert_eq!(
                    sum, connections,
                    "Params::calc({connections}, {shards}) does not add up ({params:?}"
                );
            }
        }
    }

    #[test]
    fn test_mask() {
        let inputs: &[(usize, &[usize])] = &[
            (0b0, &[]),
            (0b1, &[0]),
            (0b11, &[0, 1]),
            (0b111, &[0, 1, 2]),
            (0b1000, &[3]),
            (0b1001, &[0, 3]),
            (0b1001001, &[0, 3, 6]),
        ];

        for (mask, expected_indices) in inputs {
            let actual_indices = Mask(*mask).collect::<Vec<_>>();

            assert_eq!(
                actual_indices[..],
                expected_indices[..],
                "invalid mask: {mask:b}"
            );
        }
    }
}
