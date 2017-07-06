extern crate bytes;
extern crate futures;

use bytes::Bytes;
use futures::*;
use std::collections::VecDeque;
use std::cmp;
use std::sync::{Arc, Mutex};

pub fn new(init_capacity: usize) -> (Tx, Rx) {
    let state = Arc::new(Mutex::new(Some(State::Buffering {
        capacity: init_capacity,
        len: 0,
        buffers: VecDeque::new(),
        rx_blocked: None,
        tx_blocked: None,
    })));
    let tx = Tx(state.clone());
    let rx = Rx(state);
    (tx, rx)
}

#[derive(Copy, Clone, Debug)]
pub struct Closed;

pub enum AsyncPush {
    Ready,
    NotReady { bytes: Bytes, available: usize },
}

pub type TryPush = Result<AsyncPush, Closed>;

#[derive(Debug)]
struct TxBlocked {
    requested: usize,
    task: task::Task,
}

#[derive(Debug)]
struct RxBlocked {
    task: task::Task,
}

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
        use State::*;
        match *self {
            Buffering { len, .. } |
            Draining { len, .. } => len,
        }
    }

    fn available(&self) -> usize {
        use State::*;
        match *self {
            Buffering { capacity, .. } => capacity,
            Draining { .. } => 0,
        }
    }
}

type Inner = Arc<Mutex<Option<State>>>;

fn ensure_peer(inner: &Inner, state: &mut Option<State>) -> Result<(), Closed> {
    if Arc::strong_count(inner) == 1 {
        // lost receiver
        *state = None;
        return Err(Closed);
    }
    Ok(())
}

#[derive(Debug)]
pub struct Tx(Inner);

impl Tx {
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

    /// Signals that no further data will be provided.
    pub fn close(&mut self) {
        let mut state = self.0.lock().expect("locking byte channel");

        // If there's no receiver, clear the internal state.
        if let Err(Closed) = ensure_peer(&self.0, &mut state) {
            *state = None;
            return;
        }

        match state.take() {
            None => {}

            Some(State::Buffering { len, buffers, .. }) |
            Some(State::Draining { len, buffers }) => {
                *state = Some(State::Draining { len, buffers });
            }
        }
    }

    pub fn poll_capacity(&mut self, sz: usize) -> Poll<usize, Closed> {
        let mut state = self.0.lock().expect("locking byte channel");
        ensure_peer(&self.0, &mut state)?;
        match *state {
            None |
            Some(State::Draining { .. }) => Err(Closed),
            Some(State::Buffering {
                     capacity,
                     ref mut tx_blocked,
                     ..
                 }) => {
                if capacity >= sz {
                    Ok(Async::Ready(capacity))
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

    pub fn try_push(&mut self, bytes: Bytes) -> TryPush {
        let mut state = self.0.lock().expect("locking byte channel");
        ensure_peer(&self.0, &mut state)?;
        match *state {
            None |
            Some(State::Draining { .. }) => Err(Closed),
            Some(State::Buffering {
                     ref mut capacity,
                     ref mut len,
                     ref mut rx_blocked,
                     ref mut tx_blocked,
                     ref mut buffers,
                 }) => {
                let sz = bytes.len();
                if *capacity >= sz {
                    buffers.push_back(bytes);
                    *capacity -= sz;
                    *len += sz;
                    if let Some(rx) = rx_blocked.take() {
                        rx.task.notify();
                    }
                    Ok(AsyncPush::Ready)
                } else {
                    *tx_blocked = Some(TxBlocked {
                        requested: sz,
                        task: task::current(),
                    });
                    let available = *capacity;
                    Ok(AsyncPush::NotReady { bytes, available })
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct Rx(Inner);

pub struct NoBytesRequested;

impl Rx {
    pub fn poll_chunk(&mut self, sz: usize) -> Result<AsyncChunk, NoBytesRequested> {
        if sz == 0 {
            return Err(NoBytesRequested);
        }

        let mut state = self.0.lock().expect("locking byte channel");
        match (*state).take() {
            None => Ok(Async::Ready(None)),

            Some(State::Buffering {
                     mut capacity,
                     mut len,
                     mut buffers,
                     mut tx_blocked,
                     ..
                 }) => {
                if len == 0 {
                    if ensure_peer(&self.0, &mut state).is_err() {
                        return Ok(Async::Ready(None));
                    }

                    *state = Some(State::Buffering {
                        capacity,
                        len,
                        buffers,
                        tx_blocked,
                        rx_blocked: Some(RxBlocked { task: task::current() }),
                    });
                    return Ok(Async::NotReady);
                }

                let sz = cmp::min(len, sz);
                debug_assert!(sz != 0);

                len -= sz;
                capacity += sz;
                let chunk = Self::take_chunk(&mut buffers, sz);

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

                Ok(Async::Ready(Some(chunk)))
            }

            Some(State::Draining {
                     mut buffers,
                     mut len,
                 }) => {
                if len == 0 {
                    *state = None;
                    return Ok(Async::Ready(None));
                }

                let sz = cmp::min(len, sz);
                debug_assert!(sz != 0);
                let chunk = Self::take_chunk(&mut buffers, sz);

                len -= sz;
                if len == 0 {
                    *state = None;
                }

                Ok(Async::Ready(Some(chunk)))
            }
        }
    }

    fn take_chunk(buffers: &mut VecDeque<Bytes>, mut sz: usize) -> Chunk {
        let mut chunk = VecDeque::new();
        loop {
            if sz == 0 {
                return Chunk(chunk);
            }

            match buffers.pop_front() {
                None => {
                    return Chunk(chunk);
                }
                Some(mut bytes) => {
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

pub type AsyncChunk = Async<Option<Chunk>>;

pub struct Chunk(VecDeque<Bytes>);

// TODO
//impl Buf for Chunk {}
