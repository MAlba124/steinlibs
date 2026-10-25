//! stein-arena against plain models, under ASan: the arena, the growing list, text,
//! the interner and the bump allocator hold what was put in and refuse past their room.

#![no_main]

use std::collections::HashMap;

use libfuzzer_sys::fuzz_target;
use stein_arena::bump::Bump;
use stein_arena::interner::Interner;
use stein_arena::text::Text;
use stein_arena::{Arena, ArenaError, Grow, Span};

/// Bytes off the front of the input, zero once it runs out.
struct Ops<'a>(&'a [u8]);

impl<'a> Ops<'a> {
    fn u8(&mut self) -> u8 {
        let Some((&b, rest)) = self.0.split_first() else {
            return 0;
        };
        self.0 = rest;
        b
    }

    fn u16(&mut self) -> usize {
        usize::from(u16::from_le_bytes([self.u8(), self.u8()]))
    }

    fn bytes(&mut self, max: usize) -> &'a [u8] {
        let n = usize::from(self.u8()).min(max).min(self.0.len());
        let (b, rest) = self.0.split_at(n);
        self.0 = rest;
        b
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
    let cap = 1 + usize::from(which >> 3) * 1000;
    match which % 5 {
        0 => arena(&mut ops, cap),
        1 => grow(&mut ops, cap),
        2 => text(&mut ops, cap),
        3 => interner(&mut ops, cap),
        _ => bump(&mut ops, cap),
    }
});

fn arena(ops: &mut Ops<'_>, cap: usize) {
    let a = Arena::reserve(cap);
    assert!(a.is_ok(), "reserving {cap}");
    let Ok(mut a) = a else {
        return;
    };
    let cap = a.capacity();
    // A one byte list takes one page, which is the page size.
    let page = Grow::<u8>::reserve(1).map_or(4096, |g| g.capacity());
    // None where the arena holds bytes never written since the last reset.
    let mut model: Vec<Option<u8>> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    while ops.more() {
        match ops.u8() % 6 {
            0 | 1 => {
                let b = ops.bytes(255);
                match a.push_bytes(b) {
                    Ok(s) => {
                        assert_eq!(
                            s,
                            Span {
                                at: model.len(),
                                len: b.len()
                            }
                        );
                        model.extend(b.iter().map(|x| Some(*x)));
                        spans.push(s);
                    }
                    Err(e) => {
                        assert!(matches!(e, ArenaError::Full { .. }) && model.len() + b.len() > cap)
                    }
                }
            }
            2 => {
                let len = ops.u16();
                let align = ops.u16().max(1);
                let at = model.len().next_multiple_of(align);
                match a.alloc(len, align) {
                    Ok(s) => {
                        assert_eq!(s, Span { at, len });
                        let fill = ops.u8();
                        a.bytes_mut(s).fill(fill);
                        model.resize(at, None);
                        model.resize(at + len, Some(fill));
                        spans.push(s);
                    }
                    Err(_) => assert!(at + len > cap, "{len} at {align} refused with room"),
                }
            }
            3 => {
                let s = Span {
                    at: ops.u16(),
                    len: ops.u16(),
                };
                let got = a.bytes(s);
                match model.get(s.at..s.at + s.len) {
                    Some(want) => {
                        assert_eq!(got.len(), s.len);
                        assert!(got.iter().zip(want).all(|(g, w)| w.is_none_or(|w| w == *g)));
                    }
                    None => assert!(got.is_empty(), "{s:?} past the end read"),
                }
            }
            4 => {
                a.reset();
                model.clear();
                spans.clear();
            }
            _ => {
                let keep = ops.u16();
                a.shrink(keep);
                let keep = keep.min(cap).next_multiple_of(page);
                if keep < cap {
                    model.truncate(keep);
                }
                spans.retain(|s| s.at + s.len <= model.len());
            }
        }
        assert_eq!(a.len(), model.len());
        for s in &spans {
            let got = a.bytes(*s);
            let want = &model[s.at..s.at + s.len];
            assert!(
                got.iter().zip(want).all(|(g, w)| w.is_none_or(|w| w == *g)),
                "{s:?} changed"
            );
        }
    }
}

fn grow(ops: &mut Ops<'_>, cap: usize) {
    let g = Grow::<u32>::reserve(cap);
    assert!(g.is_ok(), "reserving {cap}");
    let Ok(mut g) = g else {
        return;
    };
    assert!(g.capacity() >= cap);
    let mut model: Vec<u32> = Vec::new();
    while ops.more() {
        match ops.u8() % 5 {
            0 | 1 => {
                let v = u32::from(ops.u8()) << 8 | u32::from(ops.u8());
                let fits = model.len() < g.capacity();
                assert_eq!(g.push(v).is_ok(), fits);
                if fits {
                    model.push(v);
                }
            }
            2 => {
                let n = ops.u16();
                let vs: Vec<u32> = (0..n as u32).collect();
                let fits = model.len() + n <= g.capacity();
                assert_eq!(g.extend_from_slice(&vs).is_ok(), fits);
                if fits {
                    model.extend_from_slice(&vs);
                }
            }
            3 => assert_eq!(g.pop(), model.pop()),
            _ => {
                let n = ops.u16();
                g.truncate(n);
                model.truncate(n);
            }
        }
        assert_eq!(g.as_slice(), model.as_slice());
    }
}

fn text(ops: &mut Ops<'_>, cap: usize) {
    let t = Text::reserve(cap);
    assert!(t.is_ok(), "reserving {cap}");
    let Ok(mut t) = t else {
        return;
    };
    let room = t.capacity();
    let mut valid: Vec<u8> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    while ops.more() {
        match ops.u8() % 5 {
            0 | 1 => {
                let b = ops.bytes(64);
                let fits = valid.len() + pending.len() + b.len() <= room;
                assert_eq!(t.push(b).is_ok(), fits);
                if fits {
                    pending.extend_from_slice(b);
                }
            }
            2 => {
                let ok = core::str::from_utf8(&pending).is_ok();
                assert_eq!(t.commit().is_ok(), ok);
                if ok {
                    valid.append(&mut pending);
                }
            }
            3 => {
                t.discard();
                pending.clear();
            }
            _ => {
                let n = ops.u16() % (valid.len() + 2);
                let boundary = n > valid.len()
                    || core::str::from_utf8(&valid).is_ok_and(|s| s.is_char_boundary(n));
                assert_eq!(t.truncate(n), boundary);
                if boundary {
                    pending.clear();
                    valid.truncate(n);
                }
            }
        }
        assert_eq!(t.as_bytes(), valid.as_slice());
        assert_eq!(t.pending(), pending.as_slice());
        assert!(core::str::from_utf8(t.as_bytes()).is_ok());
    }
}

fn interner(ops: &mut Ops<'_>, cap: usize) {
    let i = Interner::reserve(cap);
    assert!(i.is_ok(), "reserving {cap}");
    let Ok(mut i) = i else {
        return;
    };
    let mut ids: HashMap<String, u32> = HashMap::new();
    let mut by_id: Vec<String> = Vec::new();
    while ops.more() {
        let b = ops.bytes(32);
        let s = String::from_utf8_lossy(b).into_owned();
        match i.intern(&s) {
            Ok(id) => {
                let want = *ids.entry(s.clone()).or_insert_with(|| {
                    by_id.push(s.clone());
                    by_id.len() as u32 - 1
                });
                assert_eq!(id, want, "{s:?}");
            }
            Err(_) => assert!(!ids.contains_key(&s), "a known string refused"),
        }
        assert_eq!(i.len(), by_id.len());
    }
    for (id, s) in by_id.iter().enumerate() {
        assert_eq!(i.get(id as u32), s);
    }
    assert_eq!(i.get(by_id.len() as u32), "");
}

fn bump(ops: &mut Ops<'_>, cap: usize) {
    let b = Bump::reserve(cap);
    assert!(b.is_ok(), "reserving {cap}");
    let Ok(mut b) = b else {
        return;
    };
    for _ in 0..4 {
        round(ops, &b);
        b.reset();
        assert!(b.is_empty());
        if !ops.more() {
            break;
        }
    }
}

/// Allocations held together until the round ends, each checked to be intact.
fn round(ops: &mut Ops<'_>, b: &Bump) {
    let mut copies: Vec<(&mut [u8], Vec<u8>)> = Vec::new();
    let mut words: Vec<(&mut [u64], u64)> = Vec::new();
    // Address of the cursor, the end of the latest allocation or growth.
    let mut cursor: Option<usize> = None;
    let mut steps = 0;
    while ops.more() && steps < 256 {
        steps += 1;
        let before = b.len();
        match ops.u8() % 5 {
            0 | 1 => {
                let src = ops.bytes(255);
                match b.copy(src) {
                    Ok(d) => {
                        assert_eq!(&*d, src);
                        cursor = Some(d.as_ptr_range().end.addr());
                        copies.push((d, src.to_vec()));
                    }
                    Err(_) => assert!(before + src.len() > b.capacity()),
                }
            }
            2 => {
                let n = usize::from(ops.u8());
                match b.alloc_bytes(n) {
                    Ok(d) => {
                        assert!(d.iter().all(|x| *x == 0), "reused bytes not cleared");
                        d.fill(0xee);
                        cursor = Some(d.as_ptr_range().end.addr());
                        copies.push((d, vec![0xee; n]));
                    }
                    Err(_) => assert!(before + n > b.capacity()),
                }
            }
            3 => {
                let n = usize::from(ops.u8() % 64);
                let init = u64::from(ops.u8()) * 0x0101_0101_0101_0101;
                match b.alloc_slice(n, init) {
                    Ok(d) => {
                        assert!(d.as_ptr().addr().is_multiple_of(8), "a u64 slice unaligned");
                        assert!(d.iter().all(|x| *x == init));
                        cursor = Some(d.as_ptr_range().end.addr());
                        words.push((d, init));
                    }
                    Err(_) => assert!(before.next_multiple_of(8) + n * 8 > b.capacity()),
                }
            }
            _ => {
                let more = usize::from(ops.u8());
                let Some((d, _)) = copies.last() else {
                    continue;
                };
                let end = d.as_ptr_range().end;
                let want = cursor == Some(end.addr()) && before + more <= b.capacity();
                assert_eq!(b.grow_last(end, more), want, "growing by {more}");
                if want {
                    cursor = cursor.map(|c| c + more);
                }
                assert_eq!(b.len(), before + if want { more } else { 0 });
            }
        }
    }
    for (d, want) in &copies {
        assert_eq!(
            &**d,
            want.as_slice(),
            "a copy changed under later allocations"
        );
    }
    for (w, init) in &words {
        assert!(
            w.iter().all(|x| x == init),
            "a slice changed under later allocations"
        );
    }
}
