use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
    time::Duration,
};
struct Inner<T> {
    items: VecDeque<(T, usize)>,
    bytes: usize,
    closed: bool,
}
pub struct Queue<T> {
    inner: Mutex<Inner<T>>,
    wake: Condvar,
    limit: usize,
}
impl<T> Queue<T> {
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                items: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            wake: Condvar::new(),
            limit,
        }
    }
    pub fn push(&self, item: T, bytes: usize, terminal: bool) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed || (!terminal && inner.bytes.saturating_add(bytes) > self.limit) {
            return false;
        }
        inner.bytes += bytes;
        inner.items.push_back((item, bytes));
        self.wake.notify_one();
        true
    }
    pub fn pop(&self, timeout: Duration) -> Option<T> {
        let mut inner = self.inner.lock().unwrap();
        if inner.items.is_empty() && !inner.closed {
            inner = self.wake.wait_timeout(inner, timeout).unwrap().0;
        }
        let (item, bytes) = inner.items.pop_front()?;
        inner.bytes -= bytes;
        Some(item)
    }
    pub fn drained(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.closed && inner.items.is_empty()
    }
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.wake.notify_all();
    }
    pub fn bytes(&self) -> usize {
        self.inner.lock().unwrap().bytes
    }
}
