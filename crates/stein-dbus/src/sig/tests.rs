use super::*;
use std::vec::Vec;

#[test]
fn ordinary_signatures_pass() {
    for s in [
        "",
        "s",
        "as",
        "a{sv}",
        "(ii)",
        "a(oa{sa{sv}})",
        "yyyyuua(yv)",
        "v",
        "aav",
        "a{s(ib)}",
    ] {
        assert_eq!(check(s.as_bytes()), Ok(()), "{s}");
    }
}

#[test]
fn malformed_signatures_fail() {
    assert_eq!(check(b"a"), Err(SigError::Truncated));
    assert_eq!(check(b"(i"), Err(SigError::Truncated));
    assert_eq!(check(b"()"), Err(SigError::EmptyStruct));
    assert_eq!(check(b"{sv}"), Err(SigError::LooseDictEntry));
    assert_eq!(check(b"a{vs}"), Err(SigError::BadDictEntry));
    assert_eq!(check(b"a{sss}"), Err(SigError::BadDictEntry));
    assert_eq!(check(b"a{s}"), Err(SigError::BadDictEntry));
    assert_eq!(check(b"z"), Err(SigError::Unknown(b'z')));
    assert_eq!(check(b")"), Err(SigError::Unknown(b')')));
    // A dict entry is only an array's direct element.
    assert_eq!(check(b"a({sv})"), Err(SigError::LooseDictEntry));
}

#[test]
fn nesting_limits_hold() {
    let mut deep = Vec::new();
    deep.resize(32, b'a');
    deep.push(b'i');
    assert_eq!(check(&deep), Ok(()));
    deep.insert(0, b'a');
    assert_eq!(check(&deep), Err(SigError::TooDeep));

    let mut s = Vec::new();
    s.resize(33, b'(');
    s.push(b'i');
    s.resize(s.len() + 33, b')');
    assert_eq!(check(&s), Err(SigError::TooDeep));
}

#[test]
fn length_limit_holds() {
    let long = [b'i'; 256];
    assert_eq!(check(&long), Err(SigError::TooLong(256)));
    assert_eq!(check(&long[..255]), Ok(()));
}

#[test]
fn types_split_a_signature() {
    let got: Vec<&[u8]> = Types::new(b"sa{sv}(ii)u")
        .map(|t| t.expect("type"))
        .collect();
    assert_eq!(got, [&b"s"[..], b"a{sv}", b"(ii)", b"u"]);
}

#[test]
fn single_means_one_complete_type() {
    assert_eq!(check_single(b"a{sv}"), Ok(()));
    assert_eq!(check_single(b"ss"), Err(SigError::NotSingle));
    assert_eq!(check_single(b""), Err(SigError::NotSingle));
}

#[test]
fn alignments_follow_the_spec() {
    assert_eq!(align(b'y'), 1);
    assert_eq!(align(b'g'), 1);
    assert_eq!(align(b'v'), 1);
    assert_eq!(align(b'n'), 2);
    assert_eq!(align(b's'), 4);
    assert_eq!(align(b'a'), 4);
    assert_eq!(align(b't'), 8);
    assert_eq!(align(b'{'), 8);
}
