use bytes::Bytes;

use super::{ChannelBuffer, SharedBuffer, SharedWindow, LostPeer, ensure_peer};

pub fn new<E>(buffer: SharedBuffer<E>, window: SharedWindow) -> ByteSender<E> {
    ByteSender { buffer, window }
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
