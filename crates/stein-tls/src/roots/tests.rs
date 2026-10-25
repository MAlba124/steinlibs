#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

/// A store with no roots would reject every connection, so building one must
/// never produce an empty result whatever the machine looks like.
#[test]
fn a_store_is_never_empty() {
    let (store, from) = root_store();
    assert!(!store.is_empty(), "no roots from {from:?}");
    assert!(store.len() > 10, "only {} roots", store.len());
}

/// The bundled list is the fallback, so it has to stand on its own.
#[test]
fn the_bundled_list_has_the_usual_number_of_roots() {
    let store = bundled_store();
    // Mozilla's list has been between roughly 100 and 200 for years. The range
    // is wide on purpose: this is checking the list was linked in at all, not
    // pinning a number that changes every release.
    assert!(
        (50..500).contains(&store.len()),
        "the bundled list has {} roots",
        store.len()
    );
}

#[test]
fn a_partial_system_read_still_counts_as_the_system_store() {
    assert!(Roots::System.is_system());
    assert!(Roots::SystemPartial(3).is_system());
    assert!(!Roots::Bundled.is_system());
}

/// Building twice gives the same thing, which catches a store that consumes
/// some global on the first call.
#[test]
fn building_twice_gives_the_same_store() {
    let (a, from_a) = root_store();
    let (b, from_b) = root_store();
    assert_eq!(a.len(), b.len());
    assert_eq!(from_a, from_b);
}

/// Whatever the platform gave, the result has to actually work: a config built
/// from it must be usable.
#[test]
fn the_store_produces_a_usable_config() {
    let cfg = crate::config().expect("a config");
    assert!(crate::Tls::connect(&cfg, "example.com").is_ok());
}
