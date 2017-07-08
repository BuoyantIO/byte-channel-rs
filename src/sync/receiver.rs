use bytes::Bytes;
use futures::*;
use std::collections::VecDeque;
use std::cmp;

use super::{ChannelBuffer, SharedBuffer, SharedWindow, return_buffer_to_window};
use super::chunk::{self, Chunk};

pub type PollChunk<E> = Result<Async<Option<Chunk>>, E>;

pub fn new<E>(buffer: SharedBuffer<E>, window: SharedWindow) -> ByteReceiver<E> {
    ByteReceiver { buffer, window }
}

#[derive(Debug)]
pub struct ByteReceiver<E> {
    buffer: SharedBuffer<E>,
    window: SharedWindow,
}

impl<E> ByteReceiver<E> {
    pub fn shrink_window(&self, sz: usize) {
        (*self.window.lock().expect("locking byte channel window")).shrink(sz);
    }
}

impl<E> Drop for ByteReceiver<E> {
    fn drop(&mut self) {
        if let Ok(mut buffer) = self.buffer.lock() {
            let b = (*buffer).take();
            if b.is_none() {
                return;
            }
            return_buffer_to_window(&buffer, &self.window);
            *buffer = Some(ChannelBuffer::LostReceiver)
        }
    }
}

impl<E> ByteReceiver<E> {
    /// Poll at most `max_sz` bytes from the channel.
    pub fn poll_chunk(&mut self, max_sz: usize) -> PollChunk<E> {
        if max_sz == 0 {
            return Ok(Async::Ready(Some(chunk::empty())));
        }

        let chunk = {
            let mut buffer = self.buffer.lock().expect("locking byte channel buffer");
            match (*buffer).take() {
                None => {
                    return Ok(Async::Ready(None));
                }

                Some(ChannelBuffer::LostReceiver) => unreachable!(),
                Some(ChannelBuffer::SenderFailed(e)) => {
                    return Err(e);
                }

                Some(ChannelBuffer::Sending {
                         mut len,
                         mut buffers,
                         ..
                     }) => {
                    // If there's no data, wait for some.
                    if len == 0 {
                        *buffer = Some(ChannelBuffer::Sending {
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

                    *buffer = Some(ChannelBuffer::Sending {
                        len,
                        buffers,
                        awaiting_chunk: None,
                    });

                    chunk
                }

                Some(ChannelBuffer::SenderClosed {
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
                            Some(ChannelBuffer::SenderClosed { buffers, len })
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
        chunk::from_vec(window, chunk)
    }
}
