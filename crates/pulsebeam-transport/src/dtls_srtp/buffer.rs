use std::{collections::VecDeque, mem::size_of};

pub struct Reservation<T> {
    expanded: Option<VecDeque<T>>,
    bytes: usize,
}

impl<T> Reservation<T> {
    pub fn prepare(queue: &VecDeque<T>, required: usize, max_bytes: usize) -> Option<Self> {
        if required < queue.len() {
            return None;
        }
        let bytes = |capacity: usize| {
            capacity
                .checked_mul(size_of::<T>())
                .filter(|bytes| *bytes <= max_bytes)
        };
        if required == 0 {
            return Some(Self {
                expanded: Some(VecDeque::new()),
                bytes: 0,
            });
        }
        if required <= queue.capacity() {
            return Some(Self {
                expanded: None,
                bytes: bytes(queue.capacity())?,
            });
        }
        let doubled = queue
            .capacity()
            .checked_mul(2)
            .unwrap_or(required)
            .max(required);
        let capacity = if bytes(doubled).is_some() {
            doubled
        } else {
            required
        };
        bytes(capacity)?;
        let mut expanded = VecDeque::new();
        expanded.try_reserve_exact(capacity).ok()?;
        Some(Self {
            bytes: bytes(expanded.capacity())?,
            expanded: Some(expanded),
        })
    }

    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn commit(self, queue: &mut VecDeque<T>) {
        if let Some(mut expanded) = self.expanded {
            assert!(expanded.capacity() >= queue.len());
            expanded.append(queue);
            *queue = expanded;
        }
    }
}
