
use cmp::Cmp;
use key_types::{parse_internal_key, truncate_to_userkey, InternalKey, LookupKey, ValueType};
use merging_iter::MergingIter;
use snapshot::Snapshot;
use types::{Direction, LdbIterator, Shared};
use version_set::VersionSet;

use std::cmp::Ordering;
use std::rc::Rc;

use rand;

const READ_BYTES_PERIOD: isize = 1048576;

pub struct DBIterator {
    // A user comparator.
    cmp: Rc<Box<Cmp>>,
    vset: Shared<VersionSet>,
    iter: MergingIter,
    // By holding onto a snapshot, we make sure that the iterator iterates over the state at the
    // point of its creation.
    ss: Snapshot,
    dir: Direction,
    byte_count: isize,

    valid: bool,
    // temporarily stored user key.
    savedkey: Vec<u8>,
    // buffer for reading internal keys
    buf: Vec<u8>,
    savedval: Vec<u8>,
}

impl DBIterator {
    pub fn new(cmp: Rc<Box<Cmp>>,
               vset: Shared<VersionSet>,
               iter: MergingIter,
               ss: Snapshot)
               -> DBIterator {
        DBIterator {
            cmp: cmp,
            vset: vset,
            iter: iter,
            ss: ss,
            dir: Direction::Forward,
            byte_count: random_period(),

            valid: false,
            savedkey: vec![],
            buf: vec![],
            savedval: vec![],
        }
    }

    /// record_read_sample records a read sample using the current contents of self.buf, which
    /// should be an InternalKey.
    fn record_read_sample<'a>(&mut self) {
        if self.byte_count < 0 {
            let vset = self.vset.borrow().current();
            vset.borrow_mut().record_read_sample(&self.buf);
            self.byte_count += random_period();
        }
    }

    /// find_next_user_entry skips to the next user entry after the one saved in self.savedkey.
    fn find_next_user_entry(&mut self, mut skipping: bool) -> bool {
        assert!(self.iter.valid());
        assert!(self.dir == Direction::Forward);

        while self.iter.valid() {
            self.iter.current(&mut self.buf, &mut self.savedval);
            self.record_read_sample();
            let (typ, seq, ukey) = parse_internal_key(&self.buf);

            // Skip keys with a sequence number after our snapshot.
            if seq <= self.ss.sequence() {
                if typ == ValueType::TypeDeletion {
                    // Mark current (deleted) key to be skipped.
                    self.savedkey.clear();
                    self.savedkey.extend_from_slice(ukey);
                    skipping = true;
                } else if typ == ValueType::TypeValue {
                    if skipping && self.cmp.cmp(ukey, &self.savedkey) <= Ordering::Equal {
                        // Entry hidden, because it's smaller than the key to be skipped.
                    } else {
                        self.valid = true;
                        self.savedkey.clear();
                        return true;
                    }
                }
            }
            self.iter.advance();
        }
        self.savedkey.clear();
        self.valid = false;
        false
    }

    /// find_prev_user_entry finds the next smaller entry before self.savedkey.
    fn find_prev_user_entry(&mut self) -> bool {
        assert!(self.dir == Direction::Reverse);
        let mut value_type = ValueType::TypeDeletion;
        let mut newsavedval = vec![];

        // The iterator should be already set to the previous entry if this is a direction change
        // (i.e. first prev() call after advance()). savedkey is set to the key of that entry.
        //
        // We read the current entry, ignore it for comparison (because value_type is Deletion),
        // assign it to savedkey and savedval and go back another step (at the end of the loop).
        //
        // We then look at the entry one *before* the entry we want to return. We check it against
        // the saved key (still containing the key of the desired entry), see that it's less-than,
        // and break. The key and value of the desired entry are in savedkey and savedval.
        while self.iter.valid() {
            self.iter.current(&mut self.buf, &mut newsavedval);
            self.record_read_sample();
            let (typ, seq, ukey) = parse_internal_key(&self.buf);
            println!("current: {:?} / {:?}", ukey, self.savedval);

            if seq > 0 && seq <= self.ss.sequence() {
                if value_type != ValueType::TypeDeletion &&
                   self.cmp.cmp(ukey, &self.savedkey) == Ordering::Less {
                    println!("found previous key {:?} / {:?}", ukey, self.savedval);
                    // We found a non-deleted entry for a previous key (in the previous iteration)
                    break;
                }
                value_type = typ;
                if value_type == ValueType::TypeDeletion {
                    self.savedkey.clear();
                    self.savedval.clear();
                } else {
                    self.savedkey.clear();
                    self.savedkey.extend_from_slice(&ukey);
                    self.savedval.clear();
                    self.savedval.extend_from_slice(&newsavedval);
                }
            }
            self.iter.prev();
        }

        if value_type == ValueType::TypeDeletion {
            self.valid = false;
            self.savedkey.clear();
            self.savedval.clear();
            self.dir = Direction::Forward;
        } else {
            self.valid = true;
        }
        true
    }
}

impl LdbIterator for DBIterator {
    fn advance(&mut self) -> bool {
        if !self.valid() {
            self.seek_to_first();
            return self.valid();
        }

        if self.dir == Direction::Reverse {
            self.dir = Direction::Forward;
            if !self.iter.valid() {
                self.iter.seek_to_first();
            } else {
                self.iter.advance();
            }
            if !self.iter.valid() {
                self.valid = false;
                self.savedkey.clear();
                return false;
            }
        } else {
            // Save current user key.
            assert!(self.iter.current(&mut self.buf, &mut self.savedval));
            let ukey = parse_internal_key(&self.buf).2;
            self.savedkey.clear();
            self.savedkey.extend_from_slice(ukey);
        }
        self.find_next_user_entry(// skipping=
                                  true)
    }
    fn current(&self, key: &mut Vec<u8>, val: &mut Vec<u8>) -> bool {
        if !self.valid() {
            return false;
        }
        if self.dir == Direction::Forward {
            self.iter.current(key, val);
            truncate_to_userkey(key);
            true
        } else {
            key.clear();
            key.extend_from_slice(&self.savedkey);
            val.clear();
            val.extend_from_slice(&self.savedval);
            true
        }
    }
    fn prev(&mut self) -> bool {
        if !self.valid() {
            return false;
        }

        let mut newsavedkey = vec![];

        if self.dir == Direction::Forward {
            // scan backwards until we hit a different key; then use the normal scanning procedure.
            self.iter.current(&mut self.buf, &mut self.savedval);
            self.savedkey.clear();
            self.savedkey.extend_from_slice(parse_internal_key(&self.buf).2);
            loop {
                self.iter.prev();
                if !self.iter.valid() {
                    self.valid = false;
                    self.savedkey.clear();
                    self.savedval.clear();
                    return false;
                }
                // Scan until we hit the next-smaller key.
                self.iter.current(&mut self.buf, &mut self.savedval);
                newsavedkey.clear();
                newsavedkey.extend_from_slice(parse_internal_key(&self.buf).2);
                if self.cmp.cmp(&newsavedkey, &self.savedkey) == Ordering::Less {
                    println!("breaking with {:?} / {:?}", newsavedkey, self.savedval);
                    break;
                }
            }
            self.dir = Direction::Reverse;
        }
        self.find_prev_user_entry()
    }
    fn valid(&self) -> bool {
        self.valid
    }
    fn seek(&mut self, to: &[u8]) {
        self.dir = Direction::Forward;
        self.savedkey.clear();
        self.savedval.clear();
        self.buf.clear();
        self.buf.extend_from_slice(LookupKey::new(to, self.ss.sequence()).internal_key());
        self.iter.seek(&self.savedkey);
        if self.iter.valid() {
            self.find_next_user_entry(// skipping=
                                      false);
        } else {
            self.valid = false;
        }
    }
    fn seek_to_first(&mut self) {
        self.dir = Direction::Forward;
        self.savedval.clear();
        self.iter.seek_to_first();
        if self.iter.valid() {
            self.find_next_user_entry(// skipping=
                                      false);
        } else {
            self.valid = false;
        }
    }
    fn reset(&mut self) {
        self.iter.reset();
        self.valid = false;
        self.savedkey.clear();
        self.savedval.clear();
        self.buf.clear();
    }
}

fn random_period() -> isize {
    rand::random::<isize>() % 2 * READ_BYTES_PERIOD
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{current_key_val, Direction};
    use db_impl::testutil::*;

    #[test]
    fn db_iter_basic_test() {
        let mut db = build_db();
        let mut iter = db.new_iter().unwrap();

        // keys and values come from make_version(); they are each the latest entry.
        let keys: &[&[u8]] = &[b"aaa", b"aab", b"aax", b"aba", b"bab", b"bba", b"cab", b"cba"];
        let vals: &[&[u8]] = &[b"val0", b"val2", b"val1", b"val3", b"val2", b"val3", b"val1",
                               b"val3"];

        for (k, v) in keys.iter().zip(vals.iter()) {
            assert!(iter.advance());
            assert_eq!((k.to_vec(), v.to_vec()), current_key_val(&iter).unwrap());
        }
    }

    #[test]
    fn db_iter_test_fwd_backwd() {
        let mut db = build_db();
        let mut iter = db.new_iter().unwrap();

        // keys and values come from make_version(); they are each the latest entry.
        let keys: &[&[u8]] = &[b"aaa", b"aab", b"aax", b"aba", b"bab", b"bba", b"cab", b"cba"];
        let vals: &[&[u8]] = &[b"val0", b"val2", b"val1", b"val3", b"val2", b"val3", b"val1",
                               b"val3"];

        // This specifies the direction that the iterator should move to. Based on this, an index
        // into keys/vals is incremented/decremented so that we get a nice test checking iterator
        // move correctness.
        let dirs: &[Direction] = &[Direction::Forward,
                                   Direction::Forward,
                                   Direction::Forward,
                                   Direction::Reverse,
                                   Direction::Reverse,
                                   Direction::Forward,
                                   Direction::Forward,
                                   Direction::Reverse,
                                   Direction::Forward,
                                   Direction::Forward,
                                   Direction::Forward,
                                   Direction::Forward];
        let mut i = 0;
        iter.advance();
        for d in dirs {
            println!("i = {}", i);
            assert_eq!((keys[i].to_vec(), vals[i].to_vec()),
                       current_key_val(&iter).unwrap());
            match *d {
                Direction::Forward => {
                    assert!(iter.advance());
                    i += 1;
                }
                Direction::Reverse => {
                    assert!(iter.prev());
                    i -= 1;
                }
            }
        }
    }
}
