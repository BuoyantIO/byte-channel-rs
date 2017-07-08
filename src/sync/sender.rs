use bytes::Bytes;

use super::{ChannelBuffer, SharedBuffer, SharedWindow, return_buffer_to_window};

#[derive(Copy, Clone, Debug)]
pub struct LostReceiver;

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
        (*self.window.lock().expect("locking byte channel window")).advertised()
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

    /// Will cause the next receiver operation to fail with the provided error.
    pub fn reset(self, e: E) {
        let mut buffer = self.buffer.lock().expect("locking byte channel buffer");
        return_buffer_to_window(&buffer, &self.window);
        *buffer = Some(ChannelBuffer::SenderFailed(e));
    }

    /// Signals that no further data will be provided.  The `ByteReceiver` may continue to
    /// read from this channel until it is empty.
    pub fn close(mut self) {
        self.do_close();
    }

    fn do_close(&mut self) {
        let mut buffer = self.buffer.lock().expect("locking byte channel buffer");

        // If there is another receiver,
        if let Some(state) = (*buffer).take() {
            match state {
                ChannelBuffer::Sending {
                    len,
                    buffers,
                    mut awaiting_chunk,
                    ..
                } => {
                    *buffer = Some(ChannelBuffer::SenderClosed { len, buffers });

                    // If the receiver is waiting for data, notify it so that the channel is
                    // closed.
                    if let Some(t) = awaiting_chunk.take() {
                        t.notify();
                    }
                }

                state => {
                    *buffer = Some(state);
                }
            }
        }
    }

    /// Pushes bytes into the channel.
    ///
    /// ## Panics
    ///
    /// If the channel is not
    pub fn push_bytes(&mut self, bytes: Bytes) -> Result<(), LostReceiver> {
        let mut buffer = self.buffer.lock().expect("locking byte channel buffer");

        if let Some(ChannelBuffer::LostReceiver) = *buffer {
            // If there's no receiver, drop the entire buffer and error.
            // The receiver has already returned the buffer to the window.
            *buffer = None;
            return Err(LostReceiver);
        }

        if let Some(ChannelBuffer::Sending {
                        ref mut len,
                        ref mut awaiting_chunk,
                        ref mut buffers,
                        ..
                    }) = *buffer
        {
            let sz = bytes.len();

            let mut window = self.window.lock().expect("locking byte channel window");
            if sz <= (*window).advertised() {
                *len += sz;
                (*window).claim_advertised(sz);
                buffers.push_back(bytes);
                if let Some(t) = awaiting_chunk.take() {
                    t.notify();
                }
                return Ok(());
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
