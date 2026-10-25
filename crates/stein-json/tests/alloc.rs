//! `parse_vec` into a reused vector allocates only while the vector grows.
//! Once it has held the largest document, parsing allocates nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stein_arena::counting;
use stein_json::{Node, parse_vec};

#[global_allocator]
static ALLOC: stein_arena::Counting = stein_arena::Counting;

fn doc(items: usize) -> String {
    let mut s = String::from(r#"{"jsonrpc":"2.0","id":1,"result":["#);
    for i in 0..items {
        if i > 0 {
            s.push(',');
        }
        s.push_str(r#"{"label":"item","kind":3,"docs":"a\nb\"c","range":[1,2,3.5,-4e2]}"#);
    }
    s.push_str("]}");
    s
}

#[test]
fn a_warm_vector_does_not_allocate() {
    let big = doc(2000);
    let small = doc(3);
    let mut nodes: Vec<Node> = Vec::new();
    let first = parse_vec(big.as_bytes(), &mut nodes).unwrap().nodes().len();
    counting::arm();
    for _ in 0..50 {
        let n = parse_vec(big.as_bytes(), &mut nodes).unwrap().nodes().len();
        assert_eq!(n, first);
        let m = parse_vec(small.as_bytes(), &mut nodes)
            .unwrap()
            .nodes()
            .len();
        assert!(m < n);
        assert!(parse_vec(b"[1,", &mut nodes).is_err());
    }
    assert_eq!(counting::count(), 0, "allocations after warm-up");
}
