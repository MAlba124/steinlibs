//! stein-core containers against plain models: the handle pool, the keyed store and
//! the block pool, driven by one op stream each, agree with the model after every op.

#![no_main]

use std::collections::{HashMap, VecDeque};

use libfuzzer_sys::fuzz_target;
use stein_core::buf::{BlockId, BlockMeta, BlockPool, BufError};
use stein_core::handle::{Handle, Pool};
use stein_core::keyed::KeyedStore;

/// Bytes off the front of the input, zero once it runs out.
struct Ops<'a>(&'a [u8]);

impl Ops<'_> {
    fn u8(&mut self) -> u8 {
        let Some((&b, rest)) = self.0.split_first() else {
            return 0;
        };
        self.0 = rest;
        b
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }

    fn more(&self) -> bool {
        !self.0.is_empty()
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    let mut ops = Ops(rest);
    match which % 3 {
        0 => pool(&mut ops),
        1 => keyed(&mut ops),
        _ => blocks(&mut ops),
    }
});

fn pool(ops: &mut Ops<'_>) {
    let n = usize::from(ops.u8() % 33);
    let mut slots = vec![None; n];
    let mut gens = vec![0xdead_beef; n];
    let mut free = vec![7; n];
    let p = Pool::<u32>::new(&mut slots, &mut gens, &mut free);
    assert!(p.is_ok(), "equal lengths refused");
    let Ok(mut p) = p else {
        return;
    };
    let mut live: Vec<(Handle<u32>, u32)> = Vec::new();
    let mut dead: Vec<Handle<u32>> = Vec::new();
    while ops.more() {
        match ops.u8() % 6 {
            0 | 1 => {
                let v = ops.u32();
                match p.insert(v) {
                    Some(h) => {
                        assert!(live.len() < n, "an insert past capacity");
                        assert!(live.iter().all(|(l, _)| l.slot() != h.slot()));
                        live.push((h, v));
                    }
                    None => assert_eq!(live.len(), n, "an insert refused with room"),
                }
            }
            2 if !live.is_empty() => {
                let (h, v) = live.swap_remove(usize::from(ops.u8()) % live.len());
                assert_eq!(p.remove(h), Some(v));
                dead.push(h);
            }
            3 if !dead.is_empty() => {
                let h = dead[usize::from(ops.u8()) % dead.len()];
                assert_eq!(p.remove(h), None, "a stale handle removed");
                assert!(p.get(h).is_none());
            }
            4 if !live.is_empty() => {
                let i = usize::from(ops.u8()) % live.len();
                let v = ops.u32();
                let slot = p.get_mut(live[i].0);
                assert!(slot.is_some(), "a live handle not found");
                if let Some(slot) = slot {
                    *slot = v;
                }
                live[i].1 = v;
            }
            _ => {
                let h = Handle::<u32>::from_user_data(
                    u64::from(ops.u32()) << 32 | u64::from(ops.u32()),
                );
                let is_live = live.iter().any(|(l, _)| *l == h);
                assert_eq!(p.contains(h), is_live, "contains of {h:?}");
                assert_eq!(Handle::<u32>::from_user_data(h.to_user_data()), h);
            }
        }
        assert_eq!(p.len(), live.len());
        assert_eq!(p.capacity(), n);
        for (h, v) in &live {
            assert_eq!(p.get(*h), Some(v));
        }
    }
    let mut want: Vec<_> = live.iter().map(|(h, v)| (*h, *v)).collect();
    want.sort_by_key(|(h, _)| h.slot());
    let got: Vec<_> = p.iter().map(|(h, v)| (h, *v)).collect();
    assert_eq!(got, want);
    let mut walked = Vec::new();
    let mut slot = 0;
    while let Some(h) = p.next_live(slot) {
        slot = h.slot() + 1;
        walked.push(h);
    }
    assert_eq!(walked, want.iter().map(|(h, _)| *h).collect::<Vec<_>>());
}

fn keyed(ops: &mut Ops<'_>) {
    let n = 4usize << (ops.u8() % 5);
    let mut keys = vec![0x5a5a; n];
    let mut values = vec![Some(9); n];
    let mut touched = vec![3; n];
    let s = KeyedStore::<u32>::new(&mut keys, &mut values, &mut touched);
    assert!(s.is_ok(), "a power of two of at least 4 refused");
    let Ok(mut s) = s else {
        return;
    };
    let limit = s.load_limit();
    assert_eq!(limit, n / 4 * 3);
    // Key to (value, last touched frame).
    let mut model: HashMap<u64, (u32, u32)> = HashMap::new();
    let mut frame = 0u32;
    let mut cursor = 0usize;
    // Few distinct keys so chains collide, now and then a wide one.
    let key = |ops: &mut Ops<'_>| match ops.u8() {
        k @ 0..=239 => u64::from(k % 24),
        _ => u64::from(ops.u32()) << 32 | u64::from(ops.u32()),
    };
    while ops.more() {
        match ops.u8() % 8 {
            0..=2 => {
                let k = key(ops);
                let v = ops.u32();
                let fresh = !model.contains_key(&k);
                match s.get_or_insert_with(k, frame, || v) {
                    Some(slot) => {
                        let e = model.entry(k).or_insert((v, frame));
                        assert_eq!(*slot, e.0, "the value of key {k}");
                        e.1 = frame;
                    }
                    None => assert!(fresh && model.len() == limit, "refused key {k}"),
                }
            }
            3 => {
                let k = key(ops);
                assert_eq!(s.remove(k), model.remove(&k).map(|(v, _)| v));
            }
            4 => {
                let k = key(ops);
                let v = ops.u32();
                if let (Some(slot), Some(e)) = (s.get_mut(k), model.get_mut(&k)) {
                    *slot = v;
                    e.0 = v;
                }
                assert_eq!(s.get(k), model.get(&k).map(|(v, _)| v));
            }
            5 => frame = frame.wrapping_add(u32::from(ops.u8() % 8)),
            6 => {
                let age = u32::from(ops.u8() % 16);
                let before = model.len();
                model.retain(|_, (_, t)| frame.saturating_sub(*t) <= age);
                assert_eq!(s.prune(frame, age), before - model.len());
            }
            _ => {
                let age = u32::from(ops.u8() % 16);
                let budget = usize::from(ops.u8() % 16);
                let (evicted, next) = s.prune_from(frame, age, cursor, budget);
                assert!(next < n || budget == 0);
                cursor = next;
                let kept: HashMap<u64, u32> = s.iter().map(|(k, v)| (k, *v)).collect();
                let gone: Vec<u64> = model
                    .keys()
                    .filter(|k| !kept.contains_key(k))
                    .copied()
                    .collect();
                assert_eq!(gone.len(), evicted);
                for k in gone {
                    let (_, t) = model.remove(&k).unwrap_or_default();
                    assert!(frame.saturating_sub(t) > age, "key {k} pruned while fresh");
                }
            }
        }
        assert_eq!(s.len(), model.len());
        assert!(s.len() <= limit);
    }
    let got: HashMap<u64, u32> = s.iter().map(|(k, v)| (k, *v)).collect();
    let want: HashMap<u64, u32> = model.iter().map(|(k, (v, _))| (*k, *v)).collect();
    assert_eq!(got, want);
}

fn blocks(ops: &mut Ops<'_>) {
    let count = usize::from(ops.u8() % 16) + 1;
    let size = usize::from(ops.u8() % 64) + 1;
    let mut storage = vec![0xa5; count * size];
    let mut meta = vec![BlockMeta::EMPTY; count];
    let mut free = vec![0; count];
    let p = BlockPool::new(&mut storage, &mut meta, &mut free);
    assert!(p.is_ok(), "{count} blocks of {size} refused");
    let Ok(mut p) = p else {
        return;
    };
    assert_eq!(p.block_size(), size);
    let mut held: Vec<(BlockId, VecDeque<u8>)> = Vec::new();
    while ops.more() {
        match ops.u8() % 5 {
            0 => match p.acquire() {
                Ok(id) => {
                    assert!(held.iter().all(|(h, _)| *h != id));
                    assert!(p.is_empty(id));
                    held.push((id, VecDeque::new()));
                }
                Err(e) => assert!(e == BufError::Exhausted && held.len() == count),
            },
            1 if !held.is_empty() => {
                let (id, _) = held.swap_remove(usize::from(ops.u8()) % held.len());
                p.release(id);
            }
            2 | 3 if !held.is_empty() => {
                let i = usize::from(ops.u8()) % held.len();
                let len = usize::from(ops.u8() % 80);
                let bytes: Vec<u8> = (0..len).map(|_| ops.u8()).collect();
                let (id, model) = &mut held[i];
                let fits = model.len() + len <= size;
                match p.append(*id, &bytes) {
                    Ok(()) => {
                        assert!(fits);
                        model.extend(&bytes);
                    }
                    Err(e) => assert!(e == BufError::BlockFull && !fits),
                }
            }
            _ if !held.is_empty() => {
                let i = usize::from(ops.u8()) % held.len();
                let (id, model) = &mut held[i];
                let n = usize::from(ops.u8()) % (model.len() + 1);
                p.consume(*id, n);
                model.drain(..n);
            }
            _ => {}
        }
        assert_eq!(p.free_blocks(), count - held.len());
        for (id, model) in &held {
            assert!(
                p.data(*id).iter().eq(model.iter()),
                "block {} bytes",
                id.index()
            );
            assert_eq!(p.len(*id), model.len());
            assert_eq!(p.writable(*id), size - model.len());
        }
    }
}
