use std::collections::VecDeque;
use std::sync::atomic::Ordering;

use portable_atomic::{AtomicBool, AtomicUsize};
use tokio::sync::{Mutex, Semaphore};
// roomler (#3755): a write that waits for room wakes the write loop first,
// and a begun message holds the lock while it queues its rest.
use tokio::sync::{mpsc, MutexGuard, SemaphorePermit, TryAcquireError};
use util::sync::RwLock;

use crate::chunk::chunk_payload_data::ChunkPayloadData;

// TODO: benchmark performance between multiple Atomic+Mutex vs one Mutex<PendingQueueInternal>

// Some tests push a lot of data before starting to process any data...
#[cfg(test)]
const QUEUE_BYTES_LIMIT: usize = 128 * 1024 * 1024;
/// Maximum size of the pending queue, in bytes.
#[cfg(not(test))]
const QUEUE_BYTES_LIMIT: usize = 128 * 1024;
/// Total user data size, beyond which the packet will be split into chunks. The chunks will be
/// added to the pending queue one by one.
const QUEUE_APPEND_LARGE: usize = (QUEUE_BYTES_LIMIT * 2) / 3;

/// Basic queue for either ordered or unordered chunks.
pub(crate) type PendingBaseQueue = VecDeque<ChunkPayloadData>;

/// A queue for both ordered and unordered chunks.
#[derive(Debug)]
pub(crate) struct PendingQueue {
    // These two fields limit appending bytes to the queue
    // This two step process is necessary because
    // A) We need backpressure which the semaphore applies by limiting the total amount of bytes via the permits
    // B) The chunks of one fragmented message need to be put in direct sequence into the queue which the lock guarantees
    //
    // The semaphore is not inside the lock because the permits need to be returned without needing a lock on the semaphore
    semaphore_lock: Mutex<()>,
    semaphore: Semaphore,
    // roomler (#3755): `QUEUE_APPEND_LARGE`, two thirds of the queue's size;
    // a field so that a test can make a queue small enough to fill.
    append_large_at: usize,
    // roomler (#3755): bytes queued without room by a begun message whose
    // append was dropped (`BegunMessage`); `pop` pays them back before it
    // gives room again. At rest: room + queued bytes - overdraft == the limit.
    overdraft: AtomicUsize,

    unordered_queue: RwLock<PendingBaseQueue>,
    ordered_queue: RwLock<PendingBaseQueue>,
    queue_len: AtomicUsize,
    n_bytes: AtomicUsize,
    selected: AtomicBool,
    unordered_is_selected: AtomicBool,
}

impl Default for PendingQueue {
    fn default() -> Self {
        PendingQueue::new()
    }
}

impl PendingQueue {
    pub(crate) fn new() -> Self {
        Self {
            semaphore_lock: Mutex::default(),
            semaphore: Semaphore::new(QUEUE_BYTES_LIMIT),
            append_large_at: QUEUE_APPEND_LARGE,
            overdraft: Default::default(), // roomler (#3755)
            unordered_queue: Default::default(),
            ordered_queue: Default::default(),
            queue_len: Default::default(),
            n_bytes: Default::default(),
            selected: Default::default(),
            unordered_is_selected: Default::default(),
        }
    }

    /// roomler (#3755): a queue of `bytes`, for the tests that need one full
    /// (`QUEUE_BYTES_LIMIT` is 128 MiB under `cfg(test)`).
    #[cfg(test)]
    pub(crate) fn with_limit(bytes: usize) -> Self {
        Self {
            semaphore: Semaphore::new(bytes),
            append_large_at: (bytes * 2) / 3,
            ..Self::new()
        }
    }

    /// roomler (#3755): the bytes a writer could queue now without waiting.
    #[cfg(test)]
    pub(crate) fn room(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// roomler (#3755): the bytes queued without room, not yet paid back.
    #[cfg(test)]
    pub(crate) fn overdraft(&self) -> usize {
        self.overdraft.load(Ordering::SeqCst)
    }

    /// roomler (#3716): lets go every writer parked in flow control. Their
    /// permits come back only when the peer acknowledges, so on a dead path a
    /// writer waited forever, holding `semaphore_lock` (which a stream's reset
    /// queues behind) and whatever the caller held across the write. The
    /// association calls this as it closes; a writer it lets go queues nothing.
    pub(crate) fn close(&self) {
        self.semaphore.close();
    }

    /// roomler (#3716): queues `c` only if no writer is parked in flow control
    /// and its permits are free now; otherwise hands it back. For a stream's
    /// reset, which must never wait behind a writer the peer may never ack.
    pub(crate) fn try_push(&self, c: ChunkPayloadData) -> std::result::Result<(), ChunkPayloadData> {
        let Ok(_sem_lock) = self.semaphore_lock.try_lock() else {
            return Err(c);
        };
        let user_data_len = c.user_data.len();
        let Ok(permits) = self.semaphore.try_acquire_many(user_data_len as u32) else {
            return Err(c);
        };
        permits.forget();
        if c.unordered {
            self.unordered_queue.write().push_back(c);
        } else {
            self.ordered_queue.write().push_back(c);
        }
        self.n_bytes.fetch_add(user_data_len, Ordering::SeqCst);
        self.queue_len.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Appends a chunk to the back of the pending queue. False when the queue
    /// was closed (#3716) before the chunk got its permits; nothing is queued.
    pub(crate) async fn push(&self, c: ChunkPayloadData) -> bool {
        let user_data_len = c.user_data.len();

        {
            let _sem_lock = self.semaphore_lock.lock().await;
            let permits = self.semaphore.acquire_many(user_data_len as u32).await;
            // roomler (#3716): closed by the association's close; was an unwrap.
            let Ok(permits) = permits else {
                return false;
            };
            permits.forget();

            if c.unordered {
                let mut unordered_queue = self.unordered_queue.write();
                unordered_queue.push_back(c);
            } else {
                let mut ordered_queue = self.ordered_queue.write();
                ordered_queue.push_back(c);
            }
        }

        self.n_bytes.fetch_add(user_data_len, Ordering::SeqCst);
        self.queue_len.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Appends chunks to the back of the pending queue. False when the queue
    /// was closed (#3716) before they got their permits.
    ///
    /// roomler (#3755): `wake` is the association's write loop, woken before
    /// the append waits for room ([`Self::take_room`]).
    ///
    /// roomler (#3755): `commit` is the writer's own state change (the
    /// stream's sequence number and buffered amount). It runs once the first
    /// chunk has room, with no await between it and that chunk's push, so an
    /// append dropped at any of its awaits before then (the lock, the room)
    /// leaves no trace, and one let go by the queue's close commits nothing.
    /// An empty append commits nothing: no chunk carries the number. It runs
    /// before the push, so no ack can release bytes not yet counted.
    ///
    /// # Panics
    ///
    /// If it's a mix of unordered and ordered chunks.
    pub(crate) async fn append(
        &self,
        chunks: Vec<ChunkPayloadData>,
        wake: &mpsc::Sender<()>,
        commit: impl FnOnce(),
    ) -> bool {
        if chunks.is_empty() {
            return true;
        }

        let total_user_data_len = chunks.iter().fold(0, |acc, c| acc + c.user_data.len());

        if total_user_data_len >= self.append_large_at {
            self.append_large(chunks, wake, commit).await
        } else {
            let _sem_lock = self.semaphore_lock.lock().await;
            // roomler (#3755): `take_room`, which wakes the write loop if it waits.
            // roomler (#3716): `None` = closed by the association's close; was an unwrap.
            let Some(permits) = self.take_room(total_user_data_len, wake).await else {
                return false;
            };
            permits.forget();
            // roomler (#3755): no await from here to the push.
            commit();
            self.append_unlimited(chunks, total_user_data_len);
            true
        }
    }

    /// roomler (#3755): permits for `n` bytes, or `None` once the queue was
    /// closed (#3716). An append that must wait for room wakes the write loop
    /// first: only the loop, sending what is queued, makes room, and on an
    /// association with nothing in flight nothing else wakes it (an ack, a
    /// timer, the end of an append). A message larger than the room left waited
    /// there forever, its first chunks queued and never sent.
    async fn take_room(&self, n: usize, wake: &mpsc::Sender<()>) -> Option<SemaphorePermit<'_>> {
        match self.semaphore.try_acquire_many(n as u32) {
            Ok(permits) => Some(permits),
            Err(TryAcquireError::Closed) => None,
            Err(TryAcquireError::NoPermits) => {
                let _ = wake.try_send(());
                self.semaphore.acquire_many(n as u32).await.ok()
            }
        }
    }

    // If this is a very large message we append chunks one by one to allow progress while we are appending
    //
    // roomler (#3755): the first chunk is queued with the writer's commit, and
    // from then on the message is begun: dropped before its last chunk is
    // queued, it queues its rest at once (`BegunMessage`).
    async fn append_large(
        &self,
        chunks: Vec<ChunkPayloadData>,
        wake: &mpsc::Sender<()>,
        commit: impl FnOnce(),
    ) -> bool {
        // lock this for the whole duration
        let sem_lock = self.semaphore_lock.lock().await;

        // roomler (#3755): the first chunk, then the begun message's rest.
        let mut chunks = chunks.into_iter();
        let Some(first) = chunks.next() else {
            return true;
        };
        // roomler (#3755): `take_room`, which wakes the write loop if it waits.
        // roomler (#3716): `None` = closed by the association's close; was an unwrap.
        let Some(permits) = self.take_room(first.user_data.len(), wake).await else {
            return false;
        };
        permits.forget();
        // roomler (#3755): no await from here to the push of the first chunk.
        commit();
        self.push_chunk(first);
        let mut begun = BegunMessage {
            rest: chunks,
            queue: self,
            wake,
            _sem_lock: sem_lock,
        };

        while let Some(user_data_len) = begun.rest.as_slice().first().map(|c| c.user_data.len()) {
            // roomler (#3755): `take_room`, which wakes the write loop if it waits.
            // roomler (#3716): `None` = closed by the association's close; was an unwrap.
            // The chunks queued so far are the association's to drop.
            let Some(permits) = self.take_room(user_data_len, wake).await else {
                return false;
            };
            permits.forget();
            if let Some(chunk) = begun.rest.next() {
                self.push_chunk(chunk);
            }
        }
        true
    }

    /// roomler (#3755): queues one chunk. The caller holds `semaphore_lock`
    /// and has accounted for its bytes: permits, or the overdraft.
    fn push_chunk(&self, chunk: ChunkPayloadData) {
        let user_data_len = chunk.user_data.len();
        if chunk.unordered {
            let mut unordered_queue = self.unordered_queue.write();
            unordered_queue.push_back(chunk);
        } else {
            let mut ordered_queue = self.ordered_queue.write();
            ordered_queue.push_back(chunk);
        }
        self.n_bytes.fetch_add(user_data_len, Ordering::SeqCst);
        self.queue_len.fetch_add(1, Ordering::SeqCst);
    }

    /// Assumes that A) enough permits have been acquired and forget from the semaphore and that the semaphore_lock is held
    fn append_unlimited(&self, chunks: Vec<ChunkPayloadData>, total_user_data_len: usize) {
        let chunks_len = chunks.len();
        let unordered = chunks
            .first()
            .expect("chunks to not be empty because of the above check")
            .unordered;
        if unordered {
            let mut unordered_queue = self.unordered_queue.write();
            assert!(
                chunks.iter().all(|c| c.unordered),
                "expected all chunks to be unordered"
            );
            unordered_queue.extend(chunks);
        } else {
            let mut ordered_queue = self.ordered_queue.write();
            assert!(
                chunks.iter().all(|c| !c.unordered),
                "expected all chunks to be ordered"
            );
            ordered_queue.extend(chunks);
        }

        self.n_bytes
            .fetch_add(total_user_data_len, Ordering::SeqCst);
        self.queue_len.fetch_add(chunks_len, Ordering::SeqCst);
    }

    pub(crate) fn peek(&self) -> Option<ChunkPayloadData> {
        if self.selected.load(Ordering::SeqCst) {
            if self.unordered_is_selected.load(Ordering::SeqCst) {
                let unordered_queue = self.unordered_queue.read();
                return unordered_queue.front().cloned();
            } else {
                let ordered_queue = self.ordered_queue.read();
                return ordered_queue.front().cloned();
            }
        }

        let c = {
            let unordered_queue = self.unordered_queue.read();
            unordered_queue.front().cloned()
        };

        if c.is_some() {
            return c;
        }

        let ordered_queue = self.ordered_queue.read();
        ordered_queue.front().cloned()
    }

    pub(crate) fn pop(
        &self,
        beginning_fragment: bool,
        unordered: bool,
    ) -> Option<ChunkPayloadData> {
        let popped = if self.selected.load(Ordering::SeqCst) {
            let popped = if self.unordered_is_selected.load(Ordering::SeqCst) {
                let mut unordered_queue = self.unordered_queue.write();
                unordered_queue.pop_front()
            } else {
                let mut ordered_queue = self.ordered_queue.write();
                ordered_queue.pop_front()
            };
            if let Some(p) = &popped {
                if p.ending_fragment {
                    self.selected.store(false, Ordering::SeqCst);
                }
            }
            popped
        } else {
            if !beginning_fragment {
                return None;
            }
            if unordered {
                let popped = {
                    let mut unordered_queue = self.unordered_queue.write();
                    unordered_queue.pop_front()
                };
                if let Some(p) = &popped {
                    if !p.ending_fragment {
                        self.selected.store(true, Ordering::SeqCst);
                        self.unordered_is_selected.store(true, Ordering::SeqCst);
                    }
                }
                popped
            } else {
                let popped = {
                    let mut ordered_queue = self.ordered_queue.write();
                    ordered_queue.pop_front()
                };
                if let Some(p) = &popped {
                    if !p.ending_fragment {
                        self.selected.store(true, Ordering::SeqCst);
                        self.unordered_is_selected.store(false, Ordering::SeqCst);
                    }
                }
                popped
            }
        };

        if let Some(p) = &popped {
            let user_data_len = p.user_data.len();
            self.n_bytes.fetch_sub(user_data_len, Ordering::SeqCst);
            self.queue_len.fetch_sub(1, Ordering::SeqCst);
            // roomler (#3755): the overdraft is paid first, and only the rest
            // becomes room again. One load while nothing is owed.
            let owed = self
                .overdraft
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |d| {
                    (d > 0).then_some(d - d.min(user_data_len))
                })
                .map_or(0, |d| d.min(user_data_len));
            self.semaphore.add_permits(user_data_len - owed);
        }

        popped
    }

    pub(crate) fn get_num_bytes(&self) -> usize {
        self.n_bytes.load(Ordering::SeqCst)
    }

    pub(crate) fn len(&self) -> usize {
        self.queue_len.load(Ordering::SeqCst)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// roomler (#3755): a large message whose first chunk is queued, so its
/// writer's commit (the stream's sequence number) is spent. Dropped before its
/// last chunk is queued (a timeout, a `select!` arm, a task abort), it queues
/// the rest at once, in place, while it still holds `semaphore_lock`, so
/// nothing interleaves and the peer reads the message whole, once. Without
/// it, the receiver of an ordered stream waits for the missing fragments
/// forever, and every later message of the stream waits behind them (RFC 4960
/// §6.6); a half-popped message also stalls the whole association's queue
/// (`pop` keeps it selected).
///
/// The rest takes the room that is free and owes the shortfall
/// (`PendingQueue::overdraft`). No writer gets room until `pop` has paid it
/// back, so at most one message is ever owed. Nothing is queued once the queue
/// was closed (#3716): the association is going away. The drop never awaits,
/// needs no runtime and does not panic, short of a poisoned queue lock, as
/// `pop`.
struct BegunMessage<'a> {
    rest: std::vec::IntoIter<ChunkPayloadData>,
    queue: &'a PendingQueue,
    wake: &'a mpsc::Sender<()>,
    // A field, so it is released after `drop` has queued the rest.
    _sem_lock: MutexGuard<'a, ()>,
}

impl Drop for BegunMessage<'_> {
    fn drop(&mut self) {
        if self.rest.len() == 0 {
            return;
        }
        let n: usize = self.rest.as_slice().iter().map(|c| c.user_data.len()).sum();
        // The free room is taken first. This holds `semaphore_lock`, so no
        // writer takes room meanwhile and the room can only grow (a `pop`):
        // what is counted here is there to take.
        let free =
            u32::try_from(self.queue.semaphore.available_permits().min(n)).unwrap_or(u32::MAX);
        let taken = match self.queue.semaphore.try_acquire_many(free) {
            Ok(permits) => {
                permits.forget();
                free as usize
            }
            Err(TryAcquireError::Closed) => return,
            Err(TryAcquireError::NoPermits) => 0,
        };
        // Owed before it is queued, so a `pop` of the rest can never return
        // room for bytes that never had it.
        self.queue
            .overdraft
            .fetch_add(n.saturating_sub(taken), Ordering::SeqCst);
        for chunk in self.rest.by_ref() {
            self.queue.push_chunk(chunk);
        }
        let _ = self.wake.try_send(());
    }
}
