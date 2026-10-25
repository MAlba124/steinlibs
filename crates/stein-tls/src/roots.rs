//! Where the trusted roots come from.
//!
//! The platform store first, Mozilla's compiled-in list as a fallback.
//!
//! Neither alone is right. Only the platform store means a machine whose store
//! is empty, unreadable or simply absent cannot make a connection at all, and
//! a container image with no CA bundle is a common way to arrive there. Only
//! the bundled list means a CA the user or their employer added is not
//! trusted, so a proxy every other program on the machine accepts is rejected
//! here and nowhere else, which reads as a bug in this program.
//!
//! So: the platform store when it yields anything, the bundled list when it
//! does not, and the caller can always ask which happened.

/// Which trust store a configuration ended up using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Roots {
    /// The platform's own store.
    System,
    /// Mozilla's list, compiled in, because the platform gave nothing.
    Bundled,
    /// The platform gave some certificates and some were unparseable. The
    /// usable ones are loaded; the count is how many were not.
    SystemPartial(usize),
}

impl Roots {
    /// Whether this came from the platform at all.
    pub fn is_system(&self) -> bool {
        matches!(self, Roots::System | Roots::SystemPartial(_))
    }
}

/// Build a root store, and say where it came from.
///
/// Never fails. A store with no roots would reject every connection, so the
/// fallback is unconditional rather than an error the caller has to handle at
/// a point where they can do nothing about it.
pub fn root_store() -> (rustls::RootCertStore, Roots) {
    let mut store = rustls::RootCertStore::empty();
    let found = load_system(&mut store);
    match found {
        Some(bad) if bad > 0 => (store, Roots::SystemPartial(bad)),
        Some(_) => (store, Roots::System),
        None => {
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            (store, Roots::Bundled)
        }
    }
}

/// Add the platform's certificates. `None` when it produced none at all.
fn load_system(store: &mut rustls::RootCertStore) -> Option<usize> {
    let found = rustls_native_certs::load_native_certs();
    // A partial read is still worth using. The usual reason a single
    // certificate fails to parse is that it is expired or uses an algorithm
    // this does not support, and throwing away the rest over it would be worse
    // than trusting one fewer CA.
    let (added, ignored) = store.add_parsable_certificates(found.certs);
    if added == 0 {
        return None;
    }
    Some(ignored)
}

/// The bundled list, for a caller that wants it regardless of the platform.
///
/// Worth reaching for when connecting to something whose CA is known and the
/// machine's store is not to be trusted, which is not the common case.
pub fn bundled_store() -> rustls::RootCertStore {
    let mut store = rustls::RootCertStore::empty();
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    store
}

#[cfg(test)]
mod tests;
