#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn the_same_string_gets_the_same_id_once() {
    let mut i = Interner::reserve(1 << 20).expect("interner");
    let a = i.intern("alpha").unwrap();
    let b = i.intern("beta").unwrap();
    let a2 = i.intern("alpha").unwrap();
    assert_eq!(a, a2);
    assert_ne!(a, b);
    assert_eq!(i.len(), 2);
    assert_eq!(i.get(a), "alpha");
    assert_eq!(i.get(b), "beta");
}

#[test]
fn ids_are_dense_and_first_sight_ordered() {
    let mut i = Interner::reserve(1 << 20).expect("interner");
    for (n, s) in ["z", "a", "m"].iter().enumerate() {
        assert_eq!(i.intern(s).unwrap(), n as u32);
    }
}

#[test]
fn an_unknown_id_reads_empty() {
    let i = Interner::reserve(1 << 20).expect("interner");
    assert_eq!(i.get(123), "");
}

#[test]
fn unicode_round_trips() {
    let mut i = Interner::reserve(1 << 20).expect("interner");
    let id = i.intern("smörgåsbord::<核心>").unwrap();
    assert_eq!(i.get(id), "smörgåsbord::<核心>");
    let empty = i.intern("").unwrap();
    assert_eq!(i.get(empty), "");
    assert_ne!(id, empty);
}

#[test]
fn a_hundred_thousand_symbols_stay_resolvable() {
    let mut i = Interner::reserve(1 << 24).expect("interner");
    let mut ids = Vec::new();
    for n in 0..100_000u32 {
        ids.push(
            i.intern(&format!("symbol::number_{n}<u{}>", n % 7))
                .unwrap(),
        );
    }
    assert_eq!(i.len(), 100_000);
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(i.get(*id), format!("symbol::number_{n}<u{}>", n as u32 % 7));
    }
    // Re-interning finds, not duplicates.
    for n in 0..100_000u32 {
        let again = i
            .intern(&format!("symbol::number_{n}<u{}>", n % 7))
            .unwrap();
        assert_eq!(again, ids[n as usize]);
    }
    assert_eq!(i.len(), 100_000);
}
