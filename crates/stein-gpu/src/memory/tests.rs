use super::*;

fn ring(bytes: u64, frames: u8) -> Ring {
    Ring::new(
        Heap(Handle::new(0, 0)),
        512,
        GpuPtr(0x10_0000),
        bytes,
        frames,
    )
    .unwrap()
}

#[test]
fn bad_rings_are_refused() {
    let h = Heap(Handle::new(0, 0));
    assert!(Ring::new(h, 0, GpuPtr(0), 4096, 0).is_err());
    assert!(Ring::new(h, 0, GpuPtr(0), 4096, crate::MAX_FRAMES as u8 + 1).is_err());
    assert!(
        Ring::new(h, 8, GpuPtr(0), 4096, 2).is_err(),
        "unaligned start"
    );
    assert!(
        Ring::new(h, 0, GpuPtr(0), 300, 2).is_err(),
        "under a grain per frame"
    );
}

#[test]
fn partitions_round_down_to_the_grain() {
    let r = ring(1000, 3);
    assert_eq!(r.partition(), 256);
}

#[test]
fn allocations_are_aligned_and_addressed_from_the_heap_base() {
    let mut r = ring(4096, 2);
    let (a, pa) = r.alloc(3, 1).unwrap();
    assert_eq!(a, 512);
    assert_eq!(pa, GpuPtr(0x10_0000 + 512));
    let (b, _) = r.alloc(8, 16).unwrap();
    assert_eq!(b, 512 + 16);
    assert!(r.alloc(1, 3).is_err(), "not a power of two");
    assert!(r.alloc(1, 512).is_err(), "above the grain");
}

#[test]
fn a_partition_fills_to_its_exact_edge() {
    let mut r = ring(2048, 2);
    r.alloc(1000, 8).unwrap();
    r.alloc(24, 8).unwrap();
    assert_eq!(r.used(), 1024);
    assert_eq!(r.alloc(1, 1), Err(GpuError::RingFull { asked: 1, left: 0 }));
    r.begin_frame(1).unwrap();
    let (at, _) = r.alloc(1024, 1).unwrap();
    assert_eq!(at, 512 + 1024, "slot 1 starts one partition in");
    r.begin_frame(0).unwrap();
    assert_eq!(r.alloc(8, 8).map(|(a, _)| a), Ok(512), "slot 0 was reset");
    assert!(r.begin_frame(2).is_err());
}

#[test]
fn push_packs_items_into_the_mapping() {
    let mut r = ring(4096, 2);
    let mut mapped = vec![0u8; 512 + 4096];
    let s = r.push(&mut mapped, &[1u32, 2, 3]).unwrap();
    assert_eq!((s.at, s.len), (512, 3));
    assert_eq!(&mapped[512..524], &[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
    assert_eq!(s.range().bytes, 12);
    assert_eq!(s.at(2), s.gpu.offset(8));
    let short = &mut mapped[..600];
    assert!(
        r.push(short, &[0u64; 64]).is_err(),
        "mapping shorter than the ring"
    );
}

/// Random allocations across frames against a model of which bytes are
/// live. Every allocation must sit in its slot's partition, be aligned and
/// overlap nothing else live in that slot.
#[test]
fn random_frames_never_overlap_live_bytes() {
    let frames = 3u8;
    let mut r = ring(3 * 4096, frames);
    let mut live: Vec<Vec<(u64, u64)>> = vec![Vec::new(); frames as usize];
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut slot = 0u8;
    for _ in 0..20_000 {
        if next() % 16 == 0 {
            slot = (slot + 1) % frames;
            r.begin_frame(slot).unwrap();
            live[slot as usize].clear();
            continue;
        }
        let bytes = next() % 300;
        let align = 1u64 << (next() % 9);
        let Ok((at, ptr)) = r.alloc(bytes, align) else {
            continue;
        };
        let lo = 512 + u64::from(slot) * 4096;
        assert!(at >= lo && at + bytes <= lo + 4096, "outside the partition");
        assert_eq!(at % align, 0);
        assert_eq!(ptr, GpuPtr(0x10_0000 + at));
        let clash = live[slot as usize]
            .iter()
            .any(|&(a, n)| at < a + n && a < at + bytes);
        assert!(!clash || bytes == 0, "overlaps a live allocation");
        live[slot as usize].push((at, bytes));
    }
}
