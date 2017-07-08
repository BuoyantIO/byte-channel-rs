use bytes::{Buf, Bytes};
use futures::*;
use std::collections::VecDeque;
use std::cmp;
use std::sync::{Arc, Mutex, Weak};

use buffer::ChannelBuffer;
use window::Window;

/// Creates an asynchronous channel for transfering byte streams.
///
/// T
pub fn new<E>(initial_window_size: usize) -> (ByteSender<E>, ByteReceiver<E>, WindowIncrements) {
    let buffer = Arc::new(Mutex::new(Some(ChannelBuffer::Buffering {
        len: 0,
        buffers: VecDeque::new(),
        awaiting_chunk: None,
    })));

    let window = Arc::new(Mutex::new(Some(Window::new(initial_window_size))));

    let tx = ByteSender {
        buffer: buffer.clone(),
        window: window.clone(),
    };
    let rx = ByteReceiver {
        buffer,
        window: window.clone(),
    };
    let up = WindowIncrements(window);
    (tx, rx, up)
}

pub type PollChunk<E> = Result<Async<Option<Chunk>>, E>;

type SharedBuffer<E> = Arc<Mutex<Option<ChannelBuffer<E>>>>;
type SharedWindow = Arc<Mutex<Option<Window>>>;
type WeakWindow = Weak<Mutex<Option<Window>>>;

#[derive(Copy, Clone, Debug)]
pub struct LostPeer;

/// Clears out the internal state of `sharded` if the peer has been lost.
fn ensure_peer<T>(shared: &Arc<T>) -> Result<(), LostPeer> {
    if Arc::strong_count(shared) == 1 {
        return Err(LostPeer);
    }
    Ok(())
}

#[derive(Debug)]
pub struct ByteSender<E> {
    buffer: SharedBuffer<E>,
    window: SharedWindow,
}

impl<E> ByteSender<E> {
    pub fn available_window(&self) -> usize {
        (*self.window.lock().expect("locking byte channel window"))
            .as_ref()
            .map(|s| s.available())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        (*self.buffer.lock().expect("locking byte channel buffer"))
            .as_ref()
            .map(|s| s.is_empty())
            .unwrap_or(true)
    }

    pub fn len(&self) -> usize {
        (*self.buffer.lock().expect("locking byte channel buffer"))
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0)
    }

    fn return_buffer_to_window(&self, buffer: &Option<ChannelBuffer<E>>) {
        let sz = buffer.as_ref().map(|b| b.len()).unwrap_or(0);
        if sz == 0 {
            return;
        }
        let mut window = self.window.lock().expect("locking byte channel window");
        if let Some(ref mut w) = *window {
            w.push_increment(sz);
        }
    }

    /// Will cause the next receiver operation to fail with the provided error.
    pub fn reset(self, e: E) {
        let mut buffer = self.buffer.lock().expect("locking byte channel buffer");
        self.return_buffer_to_window(&buffer);
        *buffer = Some(ChannelBuffer::Failed(e));
    }

    /// Signals that no further data will be provided.  The `ByteReceiver` may continue to
    /// read from this channel until it is empty.
    pub fn close(mut self) {
        self.do_close();
    }

    fn do_close(&mut self) {
        let mut buffer = self.buffer.lock().expect("locking byte channel buffer");

        // If there's no receiver, clear the internal state.
        if let Err(LostPeer) = ensure_peer(&self.buffer) {
            self.return_buffer_to_window(&buffer);
            *buffer = None;
            return;
        }

        // If there is another receiver,
        match (*buffer).take() {
            None => {}

            Some(ChannelBuffer::Buffering {
                     len,
                     buffers,
                     mut awaiting_chunk,
                     ..
                 }) => {
                *buffer = Some(ChannelBuffer::Draining { len, buffers });

                // If the receiver is waiting for data, notify it so that the channel is
                // closed.
                if let Some(t) = awaiting_chunk.take() {
                    t.notify();
                }
            }

            Some(state) => {
                *buffer = Some(state);
            }
        }
    }

    /// Pushes bytes into the channel.
    ///
    /// ## Panics
    ///
    /// If the channel is not
    pub fn push_bytes(&mut self, bytes: Bytes) -> Result<(), LostPeer> {
        let mut buffer = self.buffer.lock().expect("locking byte channel buffer");

        if let Err(lost) = ensure_peer(&self.buffer) {
            // If there's no receiver, drop the entire buffer and error.
            self.return_buffer_to_window(&buffer);
            *buffer = None;
            return Err(lost);
        }

        if let Some(ChannelBuffer::Buffering {
                        ref mut len,
                        ref mut awaiting_chunk,
                        ref mut buffers,
                        ..
                    }) = *buffer
        {
            let sz = bytes.len();

            match *self.window.lock().expect("locking byte channel window") {
                None => panic!("byte channel missing window"),
                Some(ref mut window) => {
                    if sz <= window.available() {
                        *len += sz;
                        window.decrement(sz);
                        buffers.push_back(bytes);
                        if let Some(t) = awaiting_chunk.take() {
                            t.notify();
                        }
                        return Ok(());
                    }
                }
            }

            panic!("byte channel overflow");
        }

        panic!("ByteSender::push called in illegal buffer state");
    }
}

impl<E> Drop for ByteSender<E> {
    fn drop(&mut self) {
        self.do_close();
    }
}

#[derive(Debug)]
pub struct ByteReceiver<E> {
    buffer: Arc<Mutex<Option<ChannelBuffer<E>>>>,
    window: Arc<Mutex<Option<Window>>>,
}

impl<E> ByteReceiver<E> {
    /// Poll at most `max_sz` bytes from the channel.
    pub fn poll_chunk(&mut self, max_sz: usize) -> PollChunk<E> {
        if max_sz == 0 {
            return Ok(Async::Ready(Some(Chunk::empty(&self.window))));
        }

        let chunk = {
            let mut buffer = self.buffer.lock().expect("locking byte channel buffer");
            match (*buffer).take() {
                None => {
                    return Ok(Async::Ready(None));
                }

                Some(ChannelBuffer::Failed(e)) => {
                    return Err(e);
                }

                Some(ChannelBuffer::Buffering {
                         mut len,
                         mut buffers,
                         ..
                     }) => {
                    if len == 0 {
                        // If the buffer is empty and the sender has detached, there's no
                        // chance of producing anythign further.
                        if let Err(LostPeer) = ensure_peer(&self.buffer) {
                            *buffer = None;
                            return Ok(Async::Ready(None));
                        }

                        // Otherwise, wiat for another chunk to be pushed.
                        *buffer = Some(ChannelBuffer::Buffering {
                            len,
                            buffers,
                            awaiting_chunk: Some(task::current()),
                        });
                        return Ok(Async::NotReady);
                    }

                    let sz = cmp::min(len, max_sz);
                    debug_assert!(sz != 0);

                    // Capacity will be increased as the chunk is consumed.
                    len -= sz;
                    let chunk = Self::assemble_chunk(&self.window, &mut buffers, sz);

                    *buffer = Some(ChannelBuffer::Buffering {
                        len,
                        buffers,
                        awaiting_chunk: None,
                    });

                    chunk
                }

                Some(ChannelBuffer::Draining {
                         mut buffers,
                         mut len,
                     }) => {
                    if len == 0 {
                        *buffer = None;
                        return Ok(Async::Ready(None));
                    }

                    let sz = cmp::min(len, max_sz);
                    debug_assert!(sz != 0);
                    let chunk = Self::assemble_chunk(&self.window, &mut buffers, sz);

                    len -= sz;
                    *buffer = {
                        if len == 0 {
                            None
                        } else {
                            Some(ChannelBuffer::Draining { buffers, len })
                        }
                    };

                    chunk
                }
            }
        };

        Ok(Async::Ready(Some(chunk)))
    }

    fn assemble_chunk(
        window: &SharedWindow,
        buffers: &mut VecDeque<Bytes>,
        mut sz: usize,
    ) -> Chunk {
        let mut chunk = VecDeque::new();
        while sz != 0 {
            match buffers.pop_front() {
                None => break,
                Some(mut bytes) => {
                    if sz < bytes.len() {
                        // If the buffer is larger than the needed number of bytes, save the
                        // beginning to be returned and put the rest of it back in the buffers
                        // queue.
                        let rest = bytes.split_off(sz);
                        buffers.push_front(rest);
                    }
                    sz -= bytes.len();
                    chunk.push_back(bytes);
                }
            }
        }
        Chunk::from_vec(window, chunk)
    }
}

#[derive(Debug)]
pub struct Chunk {
    bytes: ChunkBytes,
    window: WeakWindow,
}

impl Chunk {
    fn empty(w: &SharedWindow) -> Chunk {
        Chunk {
            bytes: ChunkBytes::Zero,
            window: Arc::downgrade(w),
        }
    }

    fn from_bytes(w: &SharedWindow, bytes: Bytes) -> Chunk {
        if bytes.is_empty() {
            return Self::empty(w);
        }

        Chunk {
            bytes: ChunkBytes::One(bytes),
            window: Arc::downgrade(w),
        }
    }

    fn from_vec(w: &SharedWindow, mut buffers: VecDeque<Bytes>) -> Chunk {
        let sz = buffers.len();
        if sz == 0 {
            return Self::empty(w);
        } else if sz == 1 {
            return Self::from_bytes(w, buffers.pop_front().unwrap());
        }

        let remaining = buffers.iter().fold(0, |sz, b| sz + b.len());
        if remaining == 0 {
            return Self::empty(w);
        }

        Chunk {
            bytes: ChunkBytes::Many { remaining, buffers },
            window: Arc::downgrade(w),
        }
    }

    /// XXX this is probably a bit too heavyweight to do on every Buf::advance()?
    fn add_capacity(wref: &Weak<Mutex<Option<Window>>>, sz: usize) {
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
        match self.bytes {
            ChunkBytes::Zero => {}
            ChunkBytes::One(ref bytes) => {
                Self::add_capacity(&self.window, bytes.len());
            }
            ChunkBytes::Many { ref remaining, .. } => {
                Self::add_capacity(&self.window, *remaining);
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


#[derive(Debug)]
pub struct WindowIncrements(Arc<Mutex<Option<Window>>>);

impl WindowIncrements {
    fn is_orphaned(&self) -> bool {
        Arc::strong_count(&self.0) == 1 && Arc::weak_count(&self.0) == 0
    }
}

impl Stream for WindowIncrements {
    type Item = usize;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<usize>, ()> {
        let mut window = self.0.lock().expect("locking byte channel");

        if let Some(ref mut w) = *window {
            // If the window isn't closed, return either a new increment or indicate that
            // an increment isn't ready.  When poll_increment is not ready, it saves the task to be notified by a channel
            match w.poll_increment()? {
                Async::Ready(incr) => {
                    return Ok(Async::Ready(Some(incr)));
                }

                Async::NotReady => {
                    if !self.is_orphaned() {
                        return Ok(Async::NotReady);
                    }
                }
            }
        }

        // The window is no
        *window = None;
        Ok(Async::Ready(None))
    }
}
