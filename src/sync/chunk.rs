use bytes::{Buf, Bytes};
use std::collections::VecDeque;
use std::sync::Arc;

use super::{SharedWindow, WeakWindow};

pub fn empty() -> Chunk {
    Chunk {
        bytes: ChunkBytes::Zero,
        window: None,
    }
}

pub fn from_bytes(w: &SharedWindow, bytes: Bytes) -> Chunk {
    if bytes.is_empty() {
        return empty();
    }

    Chunk {
        bytes: ChunkBytes::One(bytes),
        window: Some(Arc::downgrade(w)),
    }
}

pub fn from_vec(w: &SharedWindow, mut buffers: VecDeque<Bytes>) -> Chunk {
    let sz = buffers.len();
    if sz == 0 {
        return empty();
    } else if sz == 1 {
        return from_bytes(w, buffers.pop_front().unwrap());
    }

    let remaining = buffers.iter().fold(0, |sz, b| sz + b.len());
    if remaining == 0 {
        return empty();
    }

    Chunk {
        bytes: ChunkBytes::Many { remaining, buffers },
        window: Some(Arc::downgrade(w)),
    }
}

/// Stores an immutable byte sequence.  As the sequence is consumed, the window is opened.
#[derive(Debug)]
pub struct Chunk {
    bytes: ChunkBytes,
    window: Option<WeakWindow>,
}

impl Chunk {
    pub fn len(&self) -> usize {
        match self.bytes {
            ChunkBytes::Zero => 0,
            ChunkBytes::One(ref bytes) => bytes.len(),
            ChunkBytes::Many { ref remaining, .. } => *remaining,
        }
    }

    fn add_capacity(wref: &WeakWindow, sz: usize) {
        if sz == 0 {
            return;
        }
        // XXX this is probably a bit too heavyweight to do on every Buf::advance()?
        if let Some(ref wmut) = wref.upgrade() {
            wmut.lock().expect("locking window").advertise_increment(sz);
        }
    }
}

// TODO this should be a Rope.
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
    /// When a chunk is dropped, all of its bytes are returned to the underlying window.
    fn drop(&mut self) {
        if let Some(win) = self.window.take() {
            Self::add_capacity(&win, self.len());
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
                panic!("advance exceeds chunk size");
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
                if let Some(ref win) = self.window.as_ref() {
                    Self::add_capacity(win, sz);
                }
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

                        // Commit the change
                        *remaining -= orig_sz;
                        if let Some(ref win) = self.window.as_ref() {
                            Self::add_capacity(win, orig_sz);
                        }
                        return;
                    }

                    // Consume the entire buffer.
                    drop(bytes);
                    sz -= len;
                    if sz == 0 {
                        *remaining -= orig_sz;
                        if let Some(ref win) = self.window.as_ref() {
                            Self::add_capacity(win, orig_sz);
                        }
                        return;
                    }
                }
                panic!("advance exceeds chunk size");
            }
        }
    }
}
