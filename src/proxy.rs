//! API proxy — FAST-FOLLOW MODULE, NOT IMPLEMENTED IN v1 (§16.1).
//!
//! This is a documented stub. The v1 auth, token, rate-limit, and logging layers
//! are deliberately built so this module can be added later with only:
//!   1. a `[proxy]` config section declaring named routes (fixed upstream base
//!      URL, allowed methods, allowed path prefix, secret-injection target),
//!   2. a new `proxy` grant role/scope in the grants model, and
//!   3. a handler mounted at `POST /v1/proxy/{name}/{suffix}`.
//!
//! Design constraints when it IS built (do not violate):
//! - It is NOT a generic `url=`-param proxy. The upstream base URL is fixed by
//!   operator config per named route. Forwarding arbitrary client-supplied URLs
//!   is SSRF-by-design and is explicitly forbidden.
//! - Secrets come from env/secret store, never from the client. The client's
//!   `Authorization` header is STRIPPED before forwarding; the upstream secret
//!   is injected server-side.
//! - Each configured upstream must resolve to a public IP (block loopback,
//!   link-local, RFC1918, and cloud metadata endpoints) to guard against
//!   misconfiguration and DNS rebinding.
//! - Apply the per-route rate limit and log `(principal, route, ts)`.
//
// Intentionally empty: no code ships in v1.
