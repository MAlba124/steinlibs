fn main() {
    let (store, from) = stein_tls::root_store();
    println!("{} roots, from {from:?}", store.len());
}
