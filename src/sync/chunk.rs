use bytes::{Buf, Bytes};
use std::collections::VecDeque;
use std::sync::Arc;

use super::{SharedWindow, WeakWindow};

pub fn empty(w: &SharedWindow) -> Chunk {
    Chunk {
        bytes: ChunkBytes::Zero,
        window: Arc::downgrade(w),
    }
}

pub fn from_bytes(w: &SharedWindow, bytes: Bytes) -> Chunk {
    if bytes.is_empty() {
        return empty(w);
    }

    Chunk {
        bytes: ChunkBytes::One(bytes),
        window: Arc::downgrade(w),
    }
}

pub fn from_vec(w: &SharedWindow, mut buffers: VecDeque<Bytes>) -> Chunk {
    let sz = buffers.len();
    if sz == 0 {
        return empty(w);
    } else if sz == 1 {
        return from_bytes(w, buffers.pop_front().unwrap());
    }

    let remaining = buffers.iter().fold(0, |sz, b| sz + b.len());
    if remaining == 0 {
        return empty(w);
    }

    Chunk {
        bytes: ChunkBytes::Many { remaining, buffers },
        window: Arc::downgrade(w),
    }
}

#[derive(Debug)]
pub struct Chunk {
    bytes: ChunkBytes,
    window: WeakWindow,
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
        if let Some(wmut) = wref.upgrade() {
            if let Some(window) = wmut.lock().expect("locking window").as_mut() {
                window.push_increment(sz);
            }
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
    fn drop(&mut self) {
        Self::add_capacity(&self.window, self.len());
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
                Self::add_capacity(&self.window, sz);
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
                        Self::add_capacity(&self.window, orig_sz);
                        return;
                    }

                    // Consume the entire buffer.
                    drop(bytes);
                    sz -= len;
                    if sz == 0 {
                        *remaining -= orig_sz;
                        Self::add_capacity(&self.window, orig_sz);
                        return;
                    }
                }
            }
        }

        panic!("advance exceeds chunk size");
    }
}
