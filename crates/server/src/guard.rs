//! Request checks that keep other websites from driving the app.
//!
//! The app only listens on loopback, but any page open in the user's browser
//! can still send it requests. Two checks stop that:
//!
//! - `Host` must name this server, which defeats DNS rebinding (a hostile
//!   domain re-pointed at 127.0.0.1 would send its own name).
//! - A state-changing request must come from this app's own pages: if the
//!   browser says where it came from (`Sec-Fetch-Site`, `Origin`), that must
//!   be here. Otherwise a cross-site form could, say, point a provider's base
//!   URL at an attacker and leak the API key.

use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub(crate) async fn check(port: u16, req: Request, next: Next) -> Response {
    if !host_allowed(req.headers(), port) {
        return (StatusCode::FORBIDDEN, "unexpected Host header").into_response();
    }
    let safe = matches!(*req.method(), Method::GET | Method::HEAD);
    if !safe && !same_origin(req.headers(), port) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    next.run(req).await
}

fn local_authorities(port: u16) -> [String; 3] {
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
}

fn host_allowed(headers: &HeaderMap, port: u16) -> bool {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|host| local_authorities(port).iter().any(|a| a == host))
}

fn same_origin(headers: &HeaderMap, port: u16) -> bool {
    let fetch_site = headers.get("sec-fetch-site").and_then(|h| h.to_str().ok());
    if fetch_site.is_some_and(|site| site != "same-origin" && site != "none") {
        return false;
    }
    match headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
        Some(origin) => local_authorities(port)
            .iter()
            .any(|a| origin == format!("http://{a}")),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, v.parse().unwrap());
        }
        map
    }

    #[test]
    fn host_must_be_local_with_our_port() {
        assert!(host_allowed(&headers(&[("host", "127.0.0.1:8787")]), 8787));
        assert!(host_allowed(&headers(&[("host", "localhost:8787")]), 8787));
        assert!(!host_allowed(&headers(&[("host", "evil.test:8787")]), 8787));
        assert!(!host_allowed(&headers(&[("host", "127.0.0.1:9999")]), 8787));
        assert!(!host_allowed(&HeaderMap::new(), 8787));
    }

    #[test]
    fn cross_site_posts_are_refused() {
        assert!(same_origin(
            &headers(&[
                ("origin", "http://127.0.0.1:8787"),
                ("sec-fetch-site", "same-origin")
            ]),
            8787
        ));
        assert!(!same_origin(
            &headers(&[("origin", "https://evil.test")]),
            8787
        ));
        assert!(!same_origin(
            &headers(&[("sec-fetch-site", "cross-site")]),
            8787
        ));
        // Non-browser clients send neither header.
        assert!(same_origin(&HeaderMap::new(), 8787));
    }
}
