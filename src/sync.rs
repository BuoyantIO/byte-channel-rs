use bytes::{Buf, Bytes};
use futures::*;
use std::collections::VecDeque;
use std::cmp;
use std::sync::{Arc, Mutex, Weak};

// XXX
// - should capacity reclamation be tied into Chunk?  It seems "correct" for capacity to
//   be reclaimed as the returned buffer is consumed.  But this might be overkill if the
//   consumer ensures it can act on the data immediately (i.e. by providing a size to
//   poll_chunk).

/// Buffers up to `capacity` bytes between a `ByteSender` and `ByteReceiver`.
pub fn new(capacity: usize) -> (ByteSender, ByteReceiver) {
    let state = Arc::new(Mutex::new(Some(State::Buffering {
        capacity,
        len: 0,
        buffers: VecDeque::new(),
        rx_blocked: None,
        tx_blocked: None,
    })));

    let tx = ByteSender(state.clone());
    let rx = ByteReceiver(state);
    (tx, rx)
}

// XXX big hairy mutex around the internal state.
type Shared = Arc<Mutex<Option<State>>>;

/// The shared state of the byte channel.
#[derive(Debug)]
enum State {
    Buffering {
        capacity: usize,
        len: usize,
        buffers: VecDeque<Bytes>,
        tx_blocked: Option<TxBlocked>,
        rx_blocked: Option<RxBlocked>,
    },

    Draining {
        len: usize,
        buffers: VecDeque<Bytes>,
    },
}

impl State {
    fn len(&self) -> usize {
        match *self {
            State::Buffering { len, .. } |
            State::Draining { len, .. } => len,
        }
    }

    fn available(&self) -> usize {
        match *self {
            State::Buffering { capacity, .. } => capacity,
            State::Draining { .. } => 0,
        }
    }

    fn add_capacity(&mut self, sz: usize) {
        match *self {
            State::Draining { .. } => {}
            State::Buffering {
                ref mut capacity,
                ref mut tx_blocked,
                ..
            } => {
                *capacity += sz;
                if let Some(tx) = tx_blocked.take() {
                    tx.task.notify();
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct LostPeer;

pub type AsyncChunk = Async<Option<Chunk>>;

#[derive(Debug)]
struct TxBlocked {
    requested: usize,
    task: task::Task,
}

#[derive(Debug)]
struct RxBlocked {
    task: task::Task,
}

/// Clears out the internal state of `sharded` if the peer has been lost.
fn ensure_peer(shared: &Shared, state: &mut Option<State>) -> Result<(), LostPeer> {
    if Arc::strong_count(shared) == 1 {
        *state = None;
        return Err(LostPeer);
    }
    Ok(())
}

#[derive(Debug)]
pub struct ByteSender(Shared);

impl ByteSender {
    pub fn available(&self) -> usize {
        (*self.0.lock().expect("locking byte channel"))
            .as_ref()
            .map(|s| s.available())
            .unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        (*self.0.lock().expect("locking byte channel"))
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Signals that no further data will be provided.  The `ByteReceiver` may continue to
    /// read from this channel until it is empty.
    pub fn close(mut self) {
        self.do_close();
    }

    fn do_close(&mut self) {
        let mut state = self.0.lock().expect("locking byte channel");

        // If there's no receiver, clear the internal state.
        if let Err(LostPeer) = ensure_peer(&self.0, &mut state) {
            *state = None;
            return;
        }

        // If there is another receiver,
        match state.take() {
            None => {}

            Some(draining @ State::Draining { .. }) => {
                *state = Some(draining);
            }

            Some(State::Buffering {
                     len,
                     buffers,
                     mut rx_blocked,
                     ..
                 }) => {
                *state = Some(State::Draining { len, buffers });
                drop(state);
                // If the receiver is waiting for data, inform it that the jig is up.
                if let Some(rx) = rx_blocked.take() {
                    rx.task.notify();
                }
            }
        }
    }

    /// Polls the channel to determine if `sz` bytes may be pushed.
    ///
    /// XXX this can't actually take a 'sz'.
    /// TODO This needs to be `poll_window_update(&mut self) -> Poll<usize, LostPeer>`
    pub fn poll_capacity(&mut self, sz: usize) -> Poll<(), LostPeer> {
        let mut state = self.0.lock().expect("locking byte channel");
        ensure_peer(&self.0, &mut state)?;
        match *state {
            None |
            Some(State::Draining { .. }) => {
                unreachable!();
            }

            Some(State::Buffering {
                     capacity,
                     ref mut tx_blocked,
                     ..
                 }) => {
                if capacity >= sz {
                    Ok(Async::Ready(()))
                } else {
                    *tx_blocked = Some(TxBlocked {
                        requested: sz,
                        task: task::current(),
                    });
                    Ok(Async::NotReady)
                }
            }
        }
    }

    /// Pushes bytes into the channel.
    ///
    /// ## Panics
    ///
    /// If called in a state when poll_capacity() will not returned a size greater than
    /// `bytes.len()`.
    pub fn push_bytes(&mut self, bytes: Bytes) -> Result<(), LostPeer> {
        let mut state = self.0.lock().expect("locking byte channel");
        ensure_peer(&self.0, &mut state)?;

        if let Some(State::Buffering {
                        ref mut capacity,
                        ref mut len,
                        ref mut rx_blocked,
                        ref mut buffers,
                        ..
                    }) = *state
        {
            let sz = bytes.len();
            if sz <= *capacity {
                buffers.push_back(bytes);
                *capacity -= sz;
                *len += sz;
                if let Some(rx) = rx_blocked.take() {
                    rx.task.notify();
                }
                return Ok(());
            }
        }

        panic!("ByteSender::push called in illegal state: {:?}", *state);
    }
}

impl Drop for ByteSender {
    fn drop(&mut self) {
        self.do_close();
    }
}

#[derive(Debug)]
pub struct ByteReceiver(Shared);

impl ByteReceiver {
    pub fn poll_chunk(&mut self, sz: usize) -> AsyncChunk {
        if sz == 0 {
            return Async::Ready(Some(Chunk::empty(&self.0)));
        }

        let mut state = self.0.lock().expect("locking byte channel");

        let chunk = match (*state).take() {
            None => {
                return Async::Ready(None);
            }

            Some(State::Buffering {
                     capacity,
                     mut len,
                     mut buffers,
                     mut tx_blocked,
                     ..
                 }) => {
                if len == 0 {
                    // If the buffer is empty and the sender has detached, there's no
                    // chance of
                    if let Err(LostPeer) = ensure_peer(&self.0, &mut state) {
                        return Async::Ready(None);
                    }

                    *state = Some(State::Buffering {
                        capacity,
                        len,
                        buffers,
                        tx_blocked,
                        rx_blocked: Some(RxBlocked { task: task::current() }),
                    });
                    return Async::NotReady;
                }

                let sz = cmp::min(len, sz);
                debug_assert!(sz != 0);

                // Capacity will be increased as the chunk is consumed.
                len -= sz;
                let chunk = Self::assemble_chunk(&self.0, &mut buffers, sz);

                *state = Some(State::Buffering {
                    capacity,
                    len,
                    buffers,
                    rx_blocked: None,
                    tx_blocked: tx_blocked.take().and_then(
                        |tx| if tx.requested <= capacity {
                            tx.task.notify();
                            None
                        } else {
                            Some(tx)
                        },
                    ),
                });

                chunk
            }

            Some(State::Draining {
                     mut buffers,
                     mut len,
                 }) => {
                if len == 0 {
                    *state = None;
                    return Async::Ready(None);
                }

                let sz = cmp::min(len, sz);
                debug_assert!(sz != 0);
                let chunk = Self::assemble_chunk(&self.0, &mut buffers, sz);

                len -= sz;
                *state = if len == 0 {
                    None
                } else {
                    Some(State::Draining { buffers, len })
                };

                chunk
            }
        };

        Async::Ready(Some(chunk))
    }

    fn assemble_chunk(chan: &Shared, buffers: &mut VecDeque<Bytes>, mut sz: usize) -> Chunk {
        let mut chunk = VecDeque::new();

        // Gather `sz` bytes from `buffers` into the returned `Chunk`.
        loop {
            if sz == 0 {
                return Chunk::from_vec(chan, chunk);
            }

            match buffers.pop_front() {
                None => {
                    return Chunk::from_vec(chan, chunk);
                }

                Some(mut bytes) => {
                    // If the buffer is larger than the needed number of bytes, save the
                    // beginning to be returned and put the rest of it back in the buffers
                    // queue.  If the entire buffer has been requested, just add it to be
                    // returned.
                    if sz < bytes.len() {
                        let rest = bytes.split_off(sz);
                        buffers.push_front(rest);
                    }
                    sz -= bytes.len();
                    chunk.push_back(bytes);
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct Chunk {
    bytes: ChunkBytes,
    channel: Weak<Mutex<Option<State>>>,
}

impl Chunk {
    fn empty(channel: &Shared) -> Chunk {
        let channel = Arc::downgrade(channel);
        Chunk {
            channel,
            bytes: ChunkBytes::Zero,
        }
    }

    fn from_bytes(channel: &Shared, bytes: Bytes) -> Chunk {
        if bytes.is_empty() {
            Self::empty(channel)
        } else {
            let channel = Arc::downgrade(channel);
            let bytes = ChunkBytes::One(bytes);
            Chunk { channel, bytes }
        }
    }

    fn from_vec(channel: &Shared, mut buffers: VecDeque<Bytes>) -> Chunk {
        match buffers.len() {
            0 => {
                return Self::empty(channel);
            }
            1 => {
                return Self::from_bytes(channel, buffers.pop_front().unwrap());
            }
            _ => {
                let remaining = buffers.iter().fold(0, |sz, b| sz + b.len());
                if remaining == 0 {
                    return Self::empty(channel);
                }

                let channel = Arc::downgrade(channel);
                Chunk {
                    channel,
                    bytes: ChunkBytes::Many { remaining, buffers },
                }
            }
        }
    }

    /// XXX this is probably a bit too heavyweight to do on every Buf::advance()?
    fn add_capacity(ch: &Weak<Mutex<Option<State>>>, sz: usize) {
        if let Some(ch) = ch.upgrade() {
            if let Some(ch) = ch.lock().expect("locking channel").as_mut() {
                ch.add_capacity(sz);
            }
        }
    }
}

#[derive(Debug)]
enum ChunkBytes {
    Zero,
    One(Bytes),
    Many {
        remaining: usize,
        buffers: VecDeque<Bytes>,
    },
}

impl Drop for Chunk {
    fn drop(&mut self) {
        match self.bytes {
            ChunkBytes::Zero => {}
            ChunkBytes::One(ref bytes) => {
                Self::add_capacity(&self.channel, bytes.len());
            }
            ChunkBytes::Many { ref remaining, .. } => {
                Self::add_capacity(&self.channel, *remaining);
            }
        }
        self.bytes = ChunkBytes::Zero;
    }
}

impl Buf for Chunk {
    fn remaining(&self) -> usize {
        match self.bytes {
            ChunkBytes::Zero => 0,
            ChunkBytes::One(ref bytes) => bytes.len(),
            ChunkBytes::Many { ref remaining, .. } => *remaining,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self.bytes {
            ChunkBytes::Zero => &[],
            ChunkBytes::One(ref bytes) => bytes.as_ref(),
            ChunkBytes::Many { ref buffers, .. } => {
                match buffers.front() {
                    None => &[],
                    Some(bytes) => bytes.as_ref(),
                }
            }
        }
    }

    fn advance(&mut self, sz: usize) {
        if sz == 0 {
            return;
        }

        match self.bytes {
            ChunkBytes::Zero => {
                return;
            }
            ChunkBytes::One(ref mut bytes) => {
                let len = bytes.len();
                if len < sz {
                    panic!("advance exceeds chunk size");
                } else if len == sz {
                    drop(bytes);
                } else {
                    drop(bytes.split_to(sz))
                };
                Self::add_capacity(&self.channel, sz);
                return;
            }

            ChunkBytes::Many {
                ref mut buffers,
                ref mut remaining,
            } => {
                if *remaining < sz {
                    panic!("advance exceeds chunk size");
                }
                let orig_sz = sz;
                let mut sz = sz;

                while let Some(mut bytes) = buffers.pop_front() {
                    let len = bytes.len();
                    if sz < len {
                        // Consume the beginning of the buffer.
                        let rest = bytes.split_to(sz);
                        drop(bytes);
                        buffers.push_front(rest);

                        *remaining -= orig_sz;
                        Self::add_capacity(&self.channel, orig_sz);
                        return;
                    }

                    // Consume the entire buffer.
                    drop(bytes);
                    sz -= len;
                    if sz == 0 {
                        *remaining -= orig_sz;
                        Self::add_capacity(&self.channel, orig_sz);
                        return;
                    }
                }
            }
        }

        panic!("advance exceeds chunk size");
    }
}
