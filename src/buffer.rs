use bytes::Bytes;
use futures::task::Task;
use std::collections::VecDeque;

/// The shared state of the byte channel.
//
/// TODO `buffers` should be stored as a Rope, which should also back Chunk.
#[derive(Debug)]
pub enum ChannelBuffer<E> {
    Sending {
        len: usize,
        buffers: VecDeque<Bytes>,
        awaiting_chunk: Option<Task>,
    },

    /// No more data may be added to the byte channel.
    SenderClosed {
        len: usize,
        buffers: VecDeque<Bytes>,
    },

    /// Indicates the sender has failed the stream and the next chunk read will fail with
    /// `E`.
    SenderFailed(E),

    LostReceiver,
}

impl<E> Default for ChannelBuffer<E> {
    fn default() -> Self {
        ChannelBuffer::Sending {
            len: 0,
            buffers: VecDeque::new(),
            awaiting_chunk: None,
        }
    }
}

impl<E> ChannelBuffer<E> {
    pub fn len(&self) -> usize {
        use self::ChannelBuffer::*;
        match *self {
            Sending { len, .. } => len,
            SenderClosed { len, .. } => len,
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
