use std::sync::{Arc, Mutex, Weak};

use buffer::ChannelBuffer;
use window::Window;

mod chunk;
mod receiver;
mod sender;
mod window;

pub use self::chunk::Chunk;
pub use self::sender::ByteSender;
pub use self::receiver::ByteReceiver;
pub use self::window::WindowAdvertiser;

/// Creates an asynchronous channel for transfering byte streams.
pub fn new<E>(initial_window_size: usize) -> (ByteSender<E>, ByteReceiver<E>, WindowAdvertiser) {
    let buffer = Arc::new(Mutex::new(Some(ChannelBuffer::default())));
    let window = Arc::new(Mutex::new(Some(Window::new(initial_window_size))));

    let tx = sender::new(buffer.clone(), window.clone());
    let rx = receiver::new(buffer, window.clone());
    let up = window::new(window);
    (tx, rx, up)
}

type SharedBuffer<E> = Arc<Mutex<Option<ChannelBuffer<E>>>>;
type SharedWindow = Arc<Mutex<Option<Window>>>;
type WeakWindow = Weak<Mutex<Option<Window>>>;

fn return_buffer_to_window<E>(buffer: &Option<ChannelBuffer<E>>, window: &SharedWindow) {
    let sz = buffer.as_ref().map(|b| b.len()).unwrap_or(0);
    if sz == 0 {
        return;
    }
    let mut window = window.lock().expect("locking byte channel window");
    if let Some(ref mut w) = *window {
        w.push_increment(sz);
    }
}
