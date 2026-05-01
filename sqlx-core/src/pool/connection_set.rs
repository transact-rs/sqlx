use crate::ext::future::race;
use crate::rt;
use crate::sync::{AsyncMutex, AsyncMutexGuardArc};
use event_listener::{listener, Event, EventListener, IntoNotification};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use std::ops::{Deref, DerefMut, RangeInclusive};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::Instrument;

pub struct ConnectionSet<C> {
    global: Arc<Global<C>>,
    slots: Box<[Arc<Slot<C>>]>,
}

pub struct ConnectedSlot<C>(SlotGuard<C>);

pub struct DisconnectedSlot<C>(SlotGuard<C>);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum AcquirePreference {
    Connected,
    Disconnected,
    Either,
}

struct Global<C> {
    unlock_event: Event<ReleaseWithoutNotify<C>>,
    disconnect_event: Event<ReleaseWithoutNotify<C>>,
    locked_set: Box<[AtomicBool]>,
    num_connected: AtomicUsize,
    min_connections: usize,
    min_connections_event: Event<()>,
}

struct SlotGuard<C> {
    slot: Arc<Slot<C>>,
    // `Option` allows us to take the guard in the drop handler.
    locked: Option<AsyncMutexGuardArc<Option<C>>>,
}

struct ReleaseWithoutNotify<C>(SlotGuard<C>);

struct Slot<C> {
    // By having each `Slot` hold its own reference to `Global`, we can avoid extra contended clones
    // which would sap performance
    global: Arc<Global<C>>,
    index: usize,
    // I'd love to eliminate this redundant `Arc` but it's likely not possible without `unsafe`
    connection: Arc<AsyncMutex<Option<C>>>,
    unlock_event: Event,
    disconnect_event: Event,
    connected: AtomicBool,
    locked: AtomicBool,
    leaked: AtomicBool,
}

impl<C> ConnectionSet<C> {
    pub fn new(size: RangeInclusive<usize>) -> Self {
        let global = Arc::new(Global {
            unlock_event: Event::with_tag(),
            disconnect_event: Event::with_tag(),
            locked_set: (0..*size.end()).map(|_| AtomicBool::new(false)).collect(),
            num_connected: AtomicUsize::new(0),
            min_connections: *size.start(),
            min_connections_event: Event::with_tag(),
        });

        ConnectionSet {
            // `vec![<expr>; size].into()` clones `<expr>` instead of repeating it,
            // which is *no bueno* when wrapping something in `Arc`
            slots: (0..*size.end())
                .map(|index| {
                    Arc::new(Slot {
                        global: global.clone(),
                        index,
                        connection: Arc::new(AsyncMutex::new(None)),
                        unlock_event: Event::with_tag(),
                        disconnect_event: Event::with_tag(),
                        connected: AtomicBool::new(false),
                        locked: AtomicBool::new(false),
                        leaked: AtomicBool::new(false),
                    })
                })
                .collect(),
            global,
        }
    }

    #[inline(always)]
    pub fn num_connected(&self) -> usize {
        self.global.num_connected()
    }

    pub fn count_idle(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_locked()).count()
    }

    pub async fn acquire_connected(&self) -> ConnectedSlot<C> {
        self.acquire_inner(AcquirePreference::Connected)
            .await
            .assert_connected()
    }

    /// Attempt to acquire the connection associated with the current thread.
    pub async fn acquire_any(&self) -> Result<ConnectedSlot<C>, DisconnectedSlot<C>> {
        self.acquire_inner(AcquirePreference::Either)
            .await
            .try_connected()
    }

    async fn acquire_inner(&self, pref: AcquirePreference) -> SlotGuard<C> {
        let span = tracing::trace_span!(
            target: "sqlx::pool::connection_set",
            "acquire_inner",
            preferred_slot = tracing::field::Empty,
            ?pref,
        );

        if self.slots.len() == 1 {
            span.record("preferred_slot", 0usize);
            return self.slots[0].acquire(pref).instrument(span).await;
        }

        let preferred_slot = self.choose_preferred_slot();
        span.record("preferred_slot", preferred_slot);

        let acquire_preferred = self.slots[preferred_slot].acquire(pref);

        let acquire_global = async {
            // Yielding actually improves performance here.
            rt::yield_now().await;

            // Since we know `preferred_slot` is locked, we offset our search by the number
            // of tasks interested in this slot, which is always at least 1.
            let search_offset = Arc::strong_count(&self.slots[preferred_slot]);

            if let Some(locked) = self.try_acquire(pref, preferred_slot.wrapping_add(search_offset))
            {
                tracing::trace!(
                    search_offset,
                    slot = locked.slot.index,
                    "acquired from try_acquire"
                );
                return locked;
            }

            // Since `acquire_global` is fair, we wait
            //rt::sleep(Duration::from_millis(50)).await;

            rt::yield_now().await;

            self.global.listen(pref).await
        };

        let res = race(acquire_preferred, acquire_global)
            .instrument(span.clone())
            .await;

        let _span = span.enter();
        match res {
            Ok(preferred) => {
                tracing::trace!(slot = preferred_slot, "acquired from acquire_preferred");
                preferred
            }
            Err(global) => {
                tracing::trace!(slot = global.slot.index, "acquired from acquire_global");
                global
            }
        }
    }

    pub fn try_acquire_connected(&self) -> Option<ConnectedSlot<C>> {
        Some(
            self.try_acquire(AcquirePreference::Connected, current_thread_id())?
                .assert_connected(),
        )
    }

    pub fn try_acquire_disconnected(&self) -> Option<DisconnectedSlot<C>> {
        Some(
            self.try_acquire(AcquirePreference::Disconnected, current_thread_id())?
                .assert_disconnected(),
        )
    }

    /// Find a non-leaked slot starting with the one associated with [`current_thread_id()`].
    fn choose_preferred_slot(&self) -> usize {
        // Always try to lock the connection associated with our thread ID first
        let starting_slot = current_thread_id() % self.slots.len();

        let search_slots = (starting_slot..self.slots.len()).chain(0..starting_slot);

        for slot in search_slots {
            if !self.slots[slot].is_leaked() {
                return slot;
            }
        }

        tracing::warn!(
            num_slots = self.slots.len(),
            "all slots have been leaked! all acquires will time out"
        );
        starting_slot
    }

    fn try_acquire(&self, pref: AcquirePreference, starting_slot: usize) -> Option<SlotGuard<C>> {
        let starting_slot = starting_slot % self.slots.len();

        let (slots_before, slots_after) = self.global.locked_set.split_at(starting_slot);

        for (index, locked) in slots_after.iter().chain(slots_before).enumerate() {
            if locked.load(Ordering::Relaxed) {
                continue;
            }

            let slot = (starting_slot + index) % self.slots.len();

            if let Some(locked) = self.slots[slot].try_acquire(pref) {
                return Some(locked);
            }
        }

        None
    }

    pub fn min_connections_listener(&self) -> EventListener {
        self.global.min_connections_event.listen()
    }

    pub fn iter_idle(&self) -> impl Iterator<Item = ConnectedSlot<C>> + '_ {
        self.slots.iter().filter_map(|slot| {
            Some(
                slot.try_acquire(AcquirePreference::Connected)?
                    .assert_connected(),
            )
        })
    }

    pub async fn drain(&self, close: impl AsyncFn(ConnectedSlot<C>) -> DisconnectedSlot<C>) {
        let mut closing = FuturesUnordered::new();

        // We could try to be more efficient by only populating the `FuturesUnordered` for
        // connected slots, but then we'd have to handle a disconnected slot becoming connected,
        // which could happen concurrently.
        //
        // However, we don't *need* to be efficient when shutting down the pool.
        for slot in &self.slots {
            closing.push(async {
                let locked = slot.lock().await;

                let slot = match locked.try_connected() {
                    Ok(connected) => close(connected).await,
                    Err(disconnected) => disconnected,
                };

                // The pool is shutting down; don't wake any tasks that might have been interested
                slot.leak();
            });
        }

        while closing.next().await.is_some() {}
    }
}

const EXPECT_LOCKED: &str = "BUG: `SlotGuard::locked` should not be `None` in normal operation";
const EXPECT_CONNECTED: &str = "BUG: `ConnectedSlot` expects `Slot::connection` to be `Some`";

impl<C> ConnectedSlot<C> {
    pub fn take(mut self) -> (C, DisconnectedSlot<C>) {
        let conn = self.0.get_mut().take().expect(EXPECT_CONNECTED);
        (conn, self.0.assert_disconnected())
    }
}

impl<C> Deref for ConnectedSlot<C> {
    type Target = C;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.0.get().as_ref().expect(EXPECT_CONNECTED)
    }
}

impl<C> DerefMut for ConnectedSlot<C> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.get_mut().as_mut().expect(EXPECT_CONNECTED)
    }
}

impl<C> DisconnectedSlot<C> {
    pub fn put(mut self, conn: C) -> ConnectedSlot<C> {
        *self.0.get_mut() = Some(conn);
        ConnectedSlot(self.0)
    }

    pub fn leak(mut self) {
        self.0.slot.connected.store(false, Ordering::Relaxed);
        self.0.slot.leaked.store(true, Ordering::Release);
        // Drop the guard without marking the connection as unlocked
        self.0.locked = None;
    }
}

impl AcquirePreference {
    #[inline(always)]
    fn wants_connected(&self, is_connected: bool) -> bool {
        matches!(
            (self, is_connected),
            (Self::Connected, true) | (Self::Disconnected, false) | (Self::Either, _)
        )
    }
}

impl<C> Slot<C> {
    #[inline(always)]
    fn matches_pref(&self, pref: AcquirePreference) -> bool {
        !self.is_leaked() && pref.wants_connected(self.is_connected())
    }

    #[inline(always)]
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    #[inline(always)]
    fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    #[inline(always)]
    fn is_leaked(&self) -> bool {
        self.leaked.load(Ordering::Relaxed)
    }

    #[inline(always)]
    fn set_is_connected(&self, connected: bool) {
        let was_connected = self.connected.swap(connected, Ordering::Acquire);

        match (connected, was_connected) {
            (false, true) => {
                // Ensure this is synchronized with `connected`
                self.global.num_connected.fetch_add(1, Ordering::Release);
            }
            (true, false) => {
                self.global.num_connected.fetch_sub(1, Ordering::Release);
            }
            _ => (),
        }
    }

    async fn acquire(self: &Arc<Self>, pref: AcquirePreference) -> SlotGuard<C> {
        loop {
            if self.matches_pref(pref) {
                tracing::trace!(slot_index=%self.index, "waiting for lock");

                let locked = self.lock().await;

                if locked.matches_pref(pref) {
                    return locked;
                }
            }

            match pref {
                AcquirePreference::Connected => {
                    listener!(self.unlock_event => listener);
                    listener.await;
                }
                AcquirePreference::Disconnected => {
                    listener!(self.disconnect_event => listener);
                    listener.await
                }
                AcquirePreference::Either => {
                    listener!(self.unlock_event => unlock_listener);
                    listener!(self.disconnect_event => disconnect_listener);
                    race(unlock_listener, disconnect_listener).await.ok();
                }
            }
        }
    }

    fn try_acquire(self: &Arc<Self>, pref: AcquirePreference) -> Option<SlotGuard<C>> {
        if self.matches_pref(pref) {
            let locked = self.try_lock()?;

            if locked.matches_pref(pref) {
                return Some(locked);
            }
        }

        None
    }

    async fn lock(self: &Arc<Self>) -> SlotGuard<C> {
        let locked = crate::sync::lock_arc(&self.connection).await;

        self.locked.store(true, Ordering::Relaxed);
        self.global.locked_set[self.index].store(true, Ordering::Relaxed);

        SlotGuard {
            slot: self.clone(),
            locked: Some(locked),
        }
    }

    fn try_lock(self: &Arc<Self>) -> Option<SlotGuard<C>> {
        let locked = crate::sync::try_lock_arc(&self.connection)?;

        self.locked.store(true, Ordering::Relaxed);
        self.global.locked_set[self.index].store(true, Ordering::Relaxed);

        Some(SlotGuard {
            slot: self.clone(),
            locked: Some(locked),
        })
    }
}

impl<C> SlotGuard<C> {
    #[inline(always)]
    fn get(&self) -> &Option<C> {
        self.locked.as_ref().expect(EXPECT_LOCKED)
    }

    #[inline(always)]
    fn get_mut(&mut self) -> &mut Option<C> {
        self.locked.as_mut().expect(EXPECT_LOCKED)
    }

    #[inline(always)]
    fn matches_pref(&self, pref: AcquirePreference) -> bool {
        !self.slot.is_leaked() && pref.wants_connected(self.is_connected())
    }

    #[inline(always)]
    fn is_connected(&self) -> bool {
        self.get().is_some()
    }

    fn try_connected(self) -> Result<ConnectedSlot<C>, DisconnectedSlot<C>> {
        if self.is_connected() {
            Ok(ConnectedSlot(self))
        } else {
            Err(DisconnectedSlot(self))
        }
    }

    fn assert_connected(self) -> ConnectedSlot<C> {
        assert!(self.is_connected());
        ConnectedSlot(self)
    }

    fn assert_disconnected(self) -> DisconnectedSlot<C> {
        assert!(!self.is_connected());

        DisconnectedSlot(self)
    }

    fn release_without_notify(&mut self) -> Option<ReleaseWithoutNotify<C>> {
        self.locked.take().map(|locked| {
            ReleaseWithoutNotify(SlotGuard {
                slot: self.slot.clone(),
                locked: Some(locked),
            })
        })
    }
}

impl<C> Drop for SlotGuard<C> {
    fn drop(&mut self) {
        let Some(mut guard) = self.release_without_notify() else {
            return;
        };

        let connected = guard.is_connected();

        let event = if guard.is_connected() {
            &self.slot.global.unlock_event
        } else {
            &self.slot.global.disconnect_event
        };

        if event.notify(
            1.tag_with(|| ReleaseWithoutNotify(guard.take()))
                .additional(),
        ) != 0
        {
            return;
        }

        if connected {
            self.slot.unlock_event.notify(1);
            return;
        }

        if self.slot.disconnect_event.notify(1) != 0 {
            return;
        }

        if self.slot.global.num_connected() < self.slot.global.min_connections {
            self.slot.global.min_connections_event.notify(1);
        }
    }
}

impl<C> ReleaseWithoutNotify<C> {
    fn take(&mut self) -> SlotGuard<C> {
        SlotGuard {
            slot: self.0.slot.clone(),
            locked: Some(
                self.0
                    .locked
                    .take()
                    .expect("BUG: `SlotGuard.locked` should not be `None` here"),
            ),
        }
    }

    fn is_connected(&self) -> bool {
        self.0
            .locked
            .as_ref()
            .expect("BUG: `SlotGuard.locked` should not be `None` here")
            .is_some()
    }
}

impl<C> Drop for ReleaseWithoutNotify<C> {
    fn drop(&mut self) {
        let Some(locked) = self.0.locked.take() else {
            return;
        };

        self.0.slot.set_is_connected(locked.is_some());
        self.0.slot.locked.store(false, Ordering::Release);
        self.0.slot.global.locked_set[self.0.slot.index].store(false, Ordering::Relaxed);
    }
}

impl<C> Global<C> {
    #[inline(always)]
    fn num_connected(&self) -> usize {
        self.num_connected.load(Ordering::Relaxed)
    }

    async fn listen(&self, pref: AcquirePreference) -> SlotGuard<C> {
        match pref {
            AcquirePreference::Either => race(self.listen_unlocked(), self.listen_disconnected())
                .await
                .unwrap_or_else(|slot| slot),
            AcquirePreference::Connected => self.listen_unlocked().await,
            AcquirePreference::Disconnected => self.listen_disconnected().await,
        }
    }

    async fn listen_unlocked(&self) -> SlotGuard<C> {
        listener!(self.unlock_event => listener);
        listener.await.take()
    }

    async fn listen_disconnected(&self) -> SlotGuard<C> {
        listener!(self.disconnect_event => listener);
        listener.await.take()
    }
}

fn current_thread_id() -> usize {
    // FIXME: this can be replaced when this is stabilized:
    // https://doc.rust-lang.org/stable/std/thread/struct.ThreadId.html#method.as_u64
    static THREAD_ID: AtomicUsize = AtomicUsize::new(0);

    thread_local! {
        // `SeqCst` is possibly too strong since we don't need synchronization with
        // any other variable. I'm not confident enough in my understanding of atomics to be certain,
        // especially with regards to weakly ordered architectures.
        //
        // However, this is literally only done once on each thread, so it doesn't really matter.
        static CURRENT_THREAD_ID: usize = THREAD_ID.fetch_add(1, Ordering::SeqCst);
    }

    CURRENT_THREAD_ID.with(|i| *i)
}
