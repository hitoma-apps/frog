//! A set of IMAP UIDs, kept as bits. UIDs are small and mostly dense, so the set
//! takes about one bit per UID up to the largest one held. A hash set of the same
//! UIDs takes several times as much.

/// A set of UIDs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UidSet {
    words: Vec<u64>,
    len: usize,
}

/// Where a UID's bit lives: its word, and its bit within that word.
fn position(uid: u32) -> (usize, u32) {
    (uid as usize / 64, uid % 64)
}

impl UidSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a UID. Returns `false` if it was already in the set.
    pub fn insert(&mut self, uid: u32) -> bool {
        let (word, bit) = position(uid);
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let mask = 1u64 << bit;
        if self.words[word] & mask != 0 {
            return false;
        }
        self.words[word] |= mask;
        self.len += 1;
        true
    }

    /// Removes a UID. Returns `false` if it wasn't in the set.
    pub fn remove(&mut self, uid: u32) -> bool {
        let (word, bit) = position(uid);
        let mask = 1u64 << bit;
        match self.words.get_mut(word) {
            Some(w) if *w & mask != 0 => {
                *w &= !mask;
                self.len -= 1;
                true
            }
            _ => false,
        }
    }

    pub fn contains(&self, uid: u32) -> bool {
        let (word, bit) = position(uid);
        self.words.get(word).is_some_and(|w| w & (1u64 << bit) != 0)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The UIDs in the set, in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(i, &w)| {
            let base = (i * 64) as u32;
            (0..64u32)
                .filter(move |bit| w & (1u64 << bit) != 0)
                .map(move |bit| base + bit)
        })
    }
}

impl FromIterator<u32> for UidSet {
    fn from_iter<I: IntoIterator<Item = u32>>(iter: I) -> Self {
        let mut set = Self::new();
        for uid in iter {
            set.insert(uid);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_contains_and_remove() {
        let mut set = UidSet::new();
        assert!(set.insert(3));
        assert!(!set.insert(3), "a second insert changes nothing");
        assert!(set.insert(700));
        assert_eq!(set.len(), 2);
        assert!(set.contains(3) && set.contains(700));
        assert!(!set.contains(4) && !set.contains(100_000));
        assert!(set.remove(3));
        assert!(!set.remove(3));
        assert!(!set.contains(3));
        assert_eq!(set.len(), 1);
        assert!(!set.is_empty());
    }

    #[test]
    fn iterates_in_ascending_order_across_words() {
        let set: UidSet = [130, 1, 64, 63, 2].into_iter().collect();
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![1, 2, 63, 64, 130]);
        assert_eq!(set.len(), 5);
    }

    #[test]
    fn empty_set_is_empty() {
        let set = UidSet::new();
        assert!(set.is_empty());
        assert_eq!(set.iter().count(), 0);
        assert!(!set.contains(0));
    }
}
