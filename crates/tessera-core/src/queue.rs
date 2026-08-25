use crate::media::MediaItem;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueOrder {
    Shuffle,
    Name,
    Date,
    Size,
}

/// Cursor-based wrapping playback queue. Played history before the cursor is
/// preserved; the pending tail after the cursor can be re-ordered or shuffled.
pub struct PlaybackQueue {
    items: Vec<MediaItem>,
    cursor: Option<usize>,
    order: QueueOrder,
}

impl PlaybackQueue {
    pub fn new(order: QueueOrder) -> PlaybackQueue {
        PlaybackQueue { items: Vec::new(), cursor: None, order }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn order(&self) -> QueueOrder {
        self.order
    }

    pub fn set_order(&mut self, order: QueueOrder) {
        self.order = order;
        self.sort_pending();
    }

    pub fn add_items(&mut self, mut items: Vec<MediaItem>) {
        self.items.append(&mut items);
        self.sort_pending();
    }

    pub fn current(&self) -> Option<&MediaItem> {
        self.items.get(self.cursor.unwrap_or(0))
    }

    pub fn current_index(&self) -> usize {
        self.cursor.unwrap_or(0)
    }

    /// Advances the cursor, wrapping to the start. Returns the new current item.
    /// A never-advanced queue treats index 0 as current.
    pub fn advance(&mut self) -> Option<&MediaItem> {
        if self.items.is_empty() {
            return None;
        }
        let next = match self.cursor {
            Some(c) => (c + 1) % self.items.len(),
            None => 1 % self.items.len(),
        };
        self.cursor = Some(next);
        self.items.get(next)
    }

    /// Moves the cursor back, wrapping to the end. Returns the new current item.
    pub fn go_back(&mut self) -> Option<&MediaItem> {
        if self.items.is_empty() {
            return None;
        }
        let prev = match self.cursor {
            Some(0) | None => self.items.len() - 1,
            Some(c) => c - 1,
        };
        self.cursor = Some(prev);
        self.items.get(prev)
    }

    /// Next n items after the cursor, wrapping, for an "up next" strip.
    pub fn upcoming(&self, n: usize) -> Vec<&MediaItem> {
        let mut out = Vec::new();
        if self.items.is_empty() {
            return out;
        }
        let cur = self.cursor.unwrap_or(0);
        for i in 1..=n.min(self.items.len()) {
            out.push(&self.items[(cur + i) % self.items.len()]);
        }
        out
    }

    /// The pending tail: items after the cursor in load order, NO wrapping
    /// (what will actually load next). Returns (index, item) pairs so the
    /// caller can reference entries in management operations.
    pub fn pending(&self, n: usize) -> Vec<(usize, &MediaItem)> {
        let cur = self.current_index();
        let end = (cur + 1 + n).min(self.items.len());
        (cur + 1..end).map(|i| (i, &self.items[i])).collect()
    }

    /// Moves a pending item to the front of the pending tail: it loads next.
    pub fn move_pending_to_front(&mut self, index: usize) {
        let cur = self.current_index();
        if index > cur && index < self.items.len() {
            let item = self.items.remove(index);
            self.items.insert(cur + 1, item);
        }
    }

    /// Removes a pending item from the queue. History and the current item
    /// (index <= cursor) are refused — tiles hold clones of those.
    pub fn remove_pending(&mut self, index: usize) {
        let cur = self.current_index();
        if index > cur && index < self.items.len() {
            self.items.remove(index);
        }
    }

    /// Reorders within the pending tail: the item at `from` is inserted
    /// immediately before the item currently at `to`.
    pub fn move_pending(&mut self, from: usize, to: usize) {
        let cur = self.current_index();
        if from > cur && from < self.items.len() && to > cur && to < self.items.len() && from != to {
            let item = self.items.remove(from);
            // After removal, positions above `from` shift down by one;
            // "insert before original `to`" accounts for that.
            let insert_at = if to > from { to - 1 } else { to };
            self.items.insert(insert_at, item);
        }
    }

    pub fn items(&self) -> &[MediaItem] {
        &self.items
    }

    fn sort_pending(&mut self) {
        if self.items.len() < 2 {
            return;
        }
        // Items up to and including the current one stay put; only the pending
        // tail is re-ordered, so the cursor always tracks the same item.
        let split = self.cursor.map_or(0, |c| (c + 1).min(self.items.len()));
        let (_, pending) = self.items.split_at_mut(split);
        match self.order {
            QueueOrder::Name => pending.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase())),
            QueueOrder::Date => pending.sort_by_key(|i| i.modified.unwrap_or(0)),
            QueueOrder::Size => pending.sort_by_key(|i| i.size.unwrap_or(0)),
            QueueOrder::Shuffle => {}
        }
        if self.order == QueueOrder::Shuffle {
            shuffle(pending, &mut rand_u64());
        }
    }
}

fn shuffle<T>(slice: &mut [T], seed: &mut u64) {
    // xorshift64* PRNG; deterministic seeding keeps the crate dependency-free.
    let n = slice.len();
    for i in (1..n).rev() {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let r = ((*seed >> 33) as usize) % (i + 1);
        slice.swap(i, r);
    }
}

fn rand_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaItem;
    use std::path::Path;

    fn item(name: &str) -> MediaItem {
        MediaItem::from_path(Path::new(&format!("/tmp/{name}.mp4")))
    }

    #[test]
    fn empty_queue() {
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        assert!(q.is_empty());
        assert!(q.current().is_none());
        assert!(q.advance().is_none());
    }

    #[test]
    fn advance_wraps() {
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        q.add_items(vec![item("a"), item("b"), item("c")]);
        assert_eq!(q.current().unwrap().title, "a");
        q.advance();
        assert_eq!(q.current().unwrap().title, "b");
        q.advance();
        assert_eq!(q.current().unwrap().title, "c");
        q.advance();
        assert_eq!(q.current().unwrap().title, "a");
        q.go_back();
        assert_eq!(q.current().unwrap().title, "c");
    }

    #[test]
    fn upcoming_wraps() {
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        q.add_items(vec![item("a"), item("b")]);
        let up = q.upcoming(3);
        assert_eq!(up.len(), 2);
        assert_eq!(up[0].title, "b");
        assert_eq!(up[1].title, "a");
    }

    #[test]
    fn name_order_sorts_pending_only() {
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        q.add_items(vec![item("c"), item("a"), item("b")]);
        // all three were pending; sorted by name
        assert_eq!(q.items().iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
        assert_eq!(q.current().unwrap().title, "a");
        q.advance();
        // now "a" is history; adding "aa" should keep history intact
        q.add_items(vec![item("aa")]);
        assert_eq!(q.current().unwrap().title, "b");
        assert_eq!(q.items()[0].title, "a");
    }

    fn filled() -> PlaybackQueue {
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        q.add_items(vec![item("a"), item("b"), item("c"), item("d"), item("e")]);
        q.advance(); // cursor -> 1 ("b")
        q
    }

    #[test]
    fn pending_lists_tail_without_wrap() {
        let q = filled();
        let titles: Vec<&str> = q.pending(10).iter().map(|(_, m)| m.title.as_str()).collect();
        assert_eq!(titles, vec!["c", "d", "e"]);
        assert!(q.pending(0).is_empty());
    }

    #[test]
    fn move_pending_to_front() {
        let mut q = filled();
        q.move_pending_to_front(3); // "d" before "c"
        let titles: Vec<&str> = q.items().iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b", "d", "c", "e"]);
        assert_eq!(q.current().unwrap().title, "b"); // cursor still on "b"
        // history/current indices are refused
        q.move_pending_to_front(0);
        assert_eq!(q.items()[0].title, "a");
    }

    #[test]
    fn remove_pending_only() {
        let mut q = filled();
        q.remove_pending(2); // "c"
        let titles: Vec<&str> = q.items().iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b", "d", "e"]);
        q.remove_pending(0); // refused: history
        q.remove_pending(1); // refused: current
        assert_eq!(q.items().len(), 4);
        assert_eq!(q.current().unwrap().title, "b");
    }

    #[test]
    fn move_pending_both_directions() {
        // backward: drag a later item before an earlier one
        let mut q = filled();
        q.move_pending(4, 2); // "e" before "c"
        let titles: Vec<&str> = q.items().iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b", "e", "c", "d"]);
        // forward: drag an earlier item onto a later one
        let mut q = filled();
        q.move_pending(2, 4); // "c" before "e"
        let titles: Vec<&str> = q.items().iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b", "d", "c", "e"]);
        // adjacent no-op: drop on the entry right after the source
        let mut q = filled();
        q.move_pending(2, 3);
        let titles: Vec<&str> = q.items().iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b", "c", "d", "e"]);
        // history/current indices refused
        let mut q = filled();
        q.move_pending(1, 3);
        q.move_pending(2, 1);
        assert_eq!(q.items().iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn set_order_reshuffles_pending() {
        let mut q = filled();
        q.set_order(QueueOrder::Shuffle);
        let titles: Vec<String> = q.pending(10).iter().map(|(_, m)| m.title.clone()).collect();
        assert_eq!(titles.len(), 3);
        // membership preserved regardless of shuffle outcome
        let mut sorted = titles.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["c".to_string(), "d".to_string(), "e".to_string()]);
        // history and current untouched
        assert_eq!(q.items()[0].title, "a");
        assert_eq!(q.current().unwrap().title, "b");
    }
}
