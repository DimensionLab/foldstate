// SPDX-License-Identifier: Apache-2.0
//! A list of numbers that lives on the stack while it is short. Most steps touch a handful
//! of states and transitions; this keeps them free of heap allocation.

use alloc::vec::Vec;

const INLINE: usize = 12;

#[derive(Debug, Clone, Default)]
pub(crate) struct Small {
    inline: [u32; INLINE],
    len: usize,
    /// Used instead of `inline` once the list has outgrown it.
    spill: Vec<u32>,
}

impl Small {
    pub fn as_slice(&self) -> &[u32] {
        if self.spill.is_empty() {
            &self.inline[..self.len]
        } else {
            &self.spill
        }
    }

    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn spilled(&mut self) -> &mut Vec<u32> {
        if self.spill.is_empty() {
            self.spill.extend_from_slice(&self.inline[..self.len]);
            self.len = 0;
        }
        &mut self.spill
    }

    pub fn push(&mut self, v: u32) {
        if self.spill.is_empty() && self.len < INLINE {
            self.inline[self.len] = v;
            self.len += 1;
        } else {
            self.spilled().push(v);
        }
    }

    pub fn extend(&mut self, vs: &[u32]) {
        for &v in vs {
            self.push(v);
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(u32) -> bool) {
        if self.spill.is_empty() {
            let mut n = 0;
            for i in 0..self.len {
                if keep(self.inline[i]) {
                    self.inline[n] = self.inline[i];
                    n += 1;
                }
            }
            self.len = n;
        } else {
            self.spill.retain(|&v| keep(v));
        }
    }

    pub fn sort_dedup(&mut self) {
        if self.spill.is_empty() {
            let s = &mut self.inline[..self.len];
            s.sort_unstable();
            let mut n = 0;
            for i in 0..s.len() {
                if i == 0 || s[i] != s[n - 1] {
                    s[n] = s[i];
                    n += 1;
                }
            }
            self.len = n;
        } else {
            self.spill.sort_unstable();
            self.spill.dedup();
        }
    }

    /// Inserts into a sorted list, keeping it sorted and without duplicates.
    pub fn insert_sorted(&mut self, v: u32) {
        let i = match self.as_slice().binary_search(&v) {
            Ok(_) => return,
            Err(i) => i,
        };
        if self.spill.is_empty() && self.len < INLINE {
            self.inline.copy_within(i..self.len, i + 1);
            self.inline[i] = v;
            self.len += 1;
        } else {
            self.spilled().insert(i, v);
        }
    }

    /// Removes from a sorted list.
    pub fn remove_sorted(&mut self, v: u32) {
        let Ok(i) = self.as_slice().binary_search(&v) else {
            return;
        };
        if self.spill.is_empty() {
            self.inline.copy_within(i + 1..self.len, i);
            self.len -= 1;
        } else {
            self.spill.remove(i);
        }
    }

    /// Whether a sorted list holds a number in `lo ..= hi`.
    pub fn any_in(&self, lo: u32, hi: u32) -> bool {
        let s = self.as_slice();
        let i = s.partition_point(|&x| x < lo);
        s.get(i).is_some_and(|&x| x <= hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every operation behaves like the same operation on a sorted `Vec`, on both sides of
    /// the inline limit.
    #[test]
    fn behaves_like_a_vec() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..2000 {
            let mut small = Small::default();
            let mut model: Vec<u32> = Vec::new();
            let sorted = round % 2 == 0;
            for _ in 0..(next() % 40) {
                let v = (next() % 30) as u32;
                match next() % 6 {
                    0..=2 if sorted => {
                        small.insert_sorted(v);
                        if let Err(i) = model.binary_search(&v) {
                            model.insert(i, v);
                        }
                    }
                    3 if sorted => {
                        small.remove_sorted(v);
                        if let Ok(i) = model.binary_search(&v) {
                            model.remove(i);
                        }
                    }
                    4 if sorted => {
                        let (lo, hi) = (v, v + (next() % 5) as u32);
                        assert_eq!(
                            small.any_in(lo, hi),
                            model.iter().any(|&x| lo <= x && x <= hi)
                        );
                    }
                    5 => {
                        small.retain(|x| x % 3 != v % 3);
                        model.retain(|x| x % 3 != v % 3);
                    }
                    _ => {
                        if sorted {
                            continue;
                        }
                        small.push(v);
                        model.push(v);
                    }
                }
                assert_eq!(small.as_slice(), model.as_slice());
                assert_eq!(
                    (small.len(), small.is_empty()),
                    (model.len(), model.is_empty())
                );
            }
            if !sorted {
                small.sort_dedup();
                model.sort_unstable();
                model.dedup();
                assert_eq!(small.as_slice(), model.as_slice());
                small.extend(&[7, 7]);
                model.extend_from_slice(&[7, 7]);
                assert_eq!(small.as_slice(), model.as_slice());
            }
        }
    }
}
