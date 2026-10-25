# h3 test identity

Identity C of profluens' `http/tests/tls`: an Ed25519 CA and a leaf for
`DNS:origin.test`, `DNS:localhost` and `IP:127.0.0.1`, valid 100 years.
Test-only, the key is public.

- `ca_c.der`: the CA, the root the client trusts.
- `cert_c.der` / `key_c.der`: the leaf and its PKCS#8 key, served by the peer.
