//! Bounded VecDeque ring with newest-wins overflow.
//!
//! Used by `EscalationQueue`. Capacity is fixed at construction. On push
//! beyond capacity the oldest element is dropped and `PushOutcome::Overflowed`
//! is returned so the caller can emit a warn-severity log.

use std::collections::VecDeque;

#[derive(Debug)]
pub struct Ring<T> {
    buf: VecDeque<T>,
    capacity: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome<T> {
    Inserted,
    Overflowed { dropped: T },
}

impl<T> Ring<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Ring capacity must be > 0");
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, item: T) -> PushOutcome<T> {
        if self.buf.len() == self.capacity {
            let dropped = self.buf.pop_front().expect("len == capacity > 0");
            self.buf.push_back(item);
            PushOutcome::Overflowed { dropped }
        } else {
            self.buf.push_back(item);
            PushOutcome::Inserted
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.buf.iter()
    }

    pub fn retain<F: FnMut(&T) -> bool>(&mut self, mut f: F) {
        self.buf.retain(|t| f(t));
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_within_capacity_inserts() {
        let mut r = Ring::new(3);
        assert_eq!(r.push(1), PushOutcome::Inserted);
        assert_eq!(r.push(2), PushOutcome::Inserted);
        assert_eq!(r.push(3), PushOutcome::Inserted);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn push_at_capacity_drops_oldest() {
        let mut r = Ring::new(2);
        r.push(1);
        r.push(2);
        assert_eq!(r.push(3), PushOutcome::Overflowed { dropped: 1 });
        let snap: Vec<_> = r.iter().copied().collect();
        assert_eq!(snap, vec![2, 3]);
    }

    #[test]
    fn iter_yields_oldest_first() {
        let mut r = Ring::new(4);
        r.push("a");
        r.push("b");
        r.push("c");
        let snap: Vec<_> = r.iter().copied().collect();
        assert_eq!(snap, vec!["a", "b", "c"]);
    }

    #[test]
    fn retain_drops_matching_entries() {
        let mut r = Ring::new(4);
        r.push(1);
        r.push(2);
        r.push(3);
        r.retain(|n| *n != 2);
        let snap: Vec<_> = r.iter().copied().collect();
        assert_eq!(snap, vec![1, 3]);
    }

    #[test]
    #[should_panic]
    fn zero_capacity_panics() {
        let _: Ring<i32> = Ring::new(0);
    }
}
