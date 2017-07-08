use bytes::Bytes;
use futures::task::Task;
use std::collections::VecDeque;

/// The shared state of the byte channel.
//
/// TODO `buffers` should be stored as a Rope, which should also back Chunk.
#[derive(Debug)]
pub enum ChannelBuffer<E> {
    Buffering {
        len: usize,
        buffers: VecDeque<Bytes>,
        awaiting_chunk: Option<Task>,
    },

    /// No more data may be added to the byte channel.
    Draining {
        len: usize,
        buffers: VecDeque<Bytes>,
    },

    /// Indicates the sender has failed the stream and the next chunk read will fail with
    /// `E`.
    Failed(E),
}

impl<E> Default for ChannelBuffer<E> {
    fn default() -> Self {
        ChannelBuffer::Buffering {
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
            Buffering { len, .. } => len,
            Draining { len, .. } => len,
            Failed(_) => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
