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

    /// Adds items and re-sorts the pending tail into the queue's order —
    /// a shuffled queue interleaves them through what is still to play.
    /// This is the bulk "add to queue" from the library browser.
    pub fn add_items(&mut self, mut items: Vec<MediaItem>) {
        self.items.append(&mut items);
        self.sort_pending();
    }

    /// Appends items to the very end WITHOUT re-sorting. A deliberate
    /// single add ("play this next-ish") must not rewrite the order the
    /// queue already has — same distinction the Quest app draws between
    /// enqueue() and enqueueAppend().
    pub fn append_items(&mut self, mut items: Vec<MediaItem>) {
        self.items.append(&mut items);
    }

    /// Empties the queue outright and forgets the cursor. Tiles keep
    /// playing whatever they already loaded — they own their `MediaItem` —
    /// but nothing new is fed to them until the queue refills.
    pub fn clear(&mut self) {
        self.items.clear();
        self.cursor = None;
    }

    /// True if an item with this id is already queued. The browser uses it
    /// to mark rows that are in the queue already.
    pub fn contains(&self, id: &str) -> bool {
        self.items.iter().any(|i| i.id == id)
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

    /// Removes the item with this id from anywhere in the queue (history,
    /// current, or pending), adjusting the cursor so the next advance still
    /// yields a fresh item. Returns the removed item.
    pub fn remove_item(&mut self, id: &str) -> Option<MediaItem> {
        let idx = self.items.iter().position(|m| m.id == id)?;
        let item = self.items.remove(idx);
        if let Some(c) = self.cursor.as_mut() {
            if *c > idx {
                *c -= 1;
            } else if *c == idx {
                // Point at the item before the removed one so the next
                // advance lands on the item that followed it.
                *c = if idx == 0 { self.items.len().saturating_sub(1) } else { idx - 1 };
            }
        }
        Some(item)
    }

    /// Reorders within the pending tail: the item at `from` is inserted
    /// immediately before the item currently at `to`.
    ///
    /// `to == len()` means "after everything". Drag-and-drop only ever
    /// names an occupied index, but a move-one-later button has to be able
    /// to reach the last slot — and note that "one later" is `from + 2`,
    /// not `from + 1`: inserting before the item that already follows you
    /// is where you started (see the adjacent-no-op test below).
    pub fn move_pending(&mut self, from: usize, to: usize) {
        let cur = self.current_index();
        if from > cur && from < self.items.len() && to > cur && to <= self.items.len() && from != to {
            let item = self.items.remove(from);
            // After removal, positions above `from` shift down by one;
            // "insert before original `to`" accounts for that.
            let insert_at = if to > from { to - 1 } else { to };
            self.items.insert(insert_at, item);
        }
    }

    /// A snapshot for persistence: the items and where the cursor sits.
    pub fn snapshot(&self) -> (Vec<MediaItem>, usize) {
        (self.items.clone(), self.current_index())
    }

    /// Restores a snapshot verbatim — no re-sorting, or a saved shuffle
    /// would come back in a different order than it was left.
    pub fn restore(&mut self, items: Vec<MediaItem>, cursor: usize) {
        self.cursor = (!items.is_empty()).then(|| cursor.min(items.len() - 1));
        self.items = items;
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
    fn add_items_sorts_the_pending_tail_but_append_items_does_not() {
        // Bulk "add to queue" folds new items into the queue's order...
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        q.add_items(vec![item("b"), item("d")]);
        q.add_items(vec![item("a"), item("c")]);
        let titles: Vec<&str> = q.items().iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, ["a", "b", "c", "d"]);

        // ...while a deliberate single append lands at the end, untouched.
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        q.add_items(vec![item("b"), item("d")]);
        q.append_items(vec![item("a")]);
        let titles: Vec<&str> = q.items().iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, ["b", "d", "a"]);
    }

    #[test]
    fn appending_never_disturbs_what_is_already_playing() {
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        q.add_items(vec![item("b"), item("c"), item("d")]);
        q.advance(); // cursor on "c"
        let playing = q.current().unwrap().title.clone();
        q.add_items(vec![item("a")]);
        // "a" sorts first, but only the pending tail moves — the cursor
        // still points at the same item.
        assert_eq!(q.current().unwrap().title, playing);
        q.append_items(vec![item("e")]);
        assert_eq!(q.current().unwrap().title, playing);
    }

    #[test]
    fn snapshot_round_trips_including_the_cursor() {
        let mut q = PlaybackQueue::new(QueueOrder::Shuffle);
        q.add_items(vec![item("a"), item("b"), item("c"), item("d")]);
        q.advance();
        q.advance();
        let want = q.current().unwrap().title.clone();
        let (items, cursor) = q.snapshot();

        let mut back = PlaybackQueue::new(QueueOrder::Name);
        back.restore(items, cursor);
        // Restored verbatim: a saved shuffle must not be re-sorted by the
        // order the new queue happens to have been built with.
        assert_eq!(back.current().unwrap().title, want);
        assert_eq!(back.len(), 4);
    }

    #[test]
    fn restoring_a_bad_cursor_or_an_empty_list_is_safe() {
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        q.restore(vec![item("a")], 99);
        assert_eq!(q.current().unwrap().title, "a");
        q.restore(Vec::new(), 5);
        assert!(q.is_empty());
        assert!(q.current().is_none());
    }

    #[test]
    fn clear_empties_the_queue_and_resets_the_cursor() {
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        q.add_items(vec![item("a"), item("b"), item("c")]);
        q.advance();
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert!(q.current().is_none());
        assert!(q.advance().is_none());
        // Refilling starts clean rather than resuming an old cursor.
        q.add_items(vec![item("z")]);
        assert_eq!(q.current().unwrap().title, "z");
    }

    #[test]
    fn contains_finds_queued_items_by_id() {
        let mut q = PlaybackQueue::new(QueueOrder::Name);
        let a = item("a");
        let id = a.id.clone();
        q.add_items(vec![a]);
        assert!(q.contains(&id));
        assert!(!q.contains("nope"));
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
    fn move_one_place_later_reaches_the_last_slot() {
        // The queue tab's ⏷ button. `to` is "insert before whatever is at
        // this index", so one-place-later is from + 2 — and for the
        // second-to-last item that index is len(), past the end.
        let mut q = filled(); // [a b c d e], cursor on "b"
        q.move_pending(2, 4); // "c" one place later
        assert_eq!(q.items().iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), vec!["a", "b", "d", "c", "e"]);
        q.move_pending(3, 5); // "c" again, now into the final slot
        assert_eq!(q.items().iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), vec!["a", "b", "d", "e", "c"]);
        // Past the end is still refused, and the cursor never moves.
        q.move_pending(3, 6);
        assert_eq!(q.items().iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), vec!["a", "b", "d", "e", "c"]);
        assert_eq!(q.current().unwrap().title, "b");
    }

    #[test]
    fn remove_item_adjusts_cursor() {
        let mut q = filled(); // [a b c d e], cursor=1 ("b")
        // remove pending "d": cursor unchanged
        let it = q.remove_item(&"local|/tmp/d.mp4".to_string()).map(|m| m.title).unwrap();
        assert_eq!(it, "d");
        assert_eq!(q.items().iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c", "e"]);
        assert_eq!(q.current().unwrap().title, "b");
        // remove history "a": cursor shifts down
        q.remove_item(&"local|/tmp/a.mp4".to_string());
        assert_eq!(q.items().iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), vec!["b", "c", "e"]);
        assert_eq!(q.current().unwrap().title, "b");
        // remove current "b": cursor points at previous, next advance -> "c"
        q.remove_item(&"local|/tmp/b.mp4".to_string());
        assert_eq!(q.advance().unwrap().title, "c");
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
