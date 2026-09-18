//! The gateway's **closed** route table (HORO-1144).
//!
//! # Why a closed table and not a path rewrite
//!
//! The single defining property of an open proxy is that the caller
//! chooses the destination. A proxy that takes the inbound path, appends
//! it to a configured base, and forwards is an open proxy against that
//! base: `/v1/messages/../../whatever` and every encoding trick around it
//! become somebody else's problem to normalise correctly.
//!
//! So no inbound path segment is ever concatenated into an outbound URL.
//! [`match_route`] compares the inbound path against a fixed list of
//! exact string literals and returns a [`Route`] — an enum, not a string.
//! The outbound URL is then built from the configured upstream base plus
//! that route's **own** [`Route::upstream_path`] constant. An inbound
//! path that is not in the list is a `404` and never reaches the
//! upstream.
//!
//! The consequence is that adding a proxied endpoint is a deliberate code
//! change with a review attached, which is the intended cost.

/// An endpoint the gateway is willing to proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `POST /v1/messages` — the metered endpoint. The only route that
    /// reserves, forwards under a reservation, and settles.
    Messages,
    /// `POST /v1/messages/count_tokens` — free, and returns no
    /// completion. Forwarded unmetered: reserving against a call that
    /// cannot cost anything would deny real work for no reason.
    CountTokens,
    /// `GET`/`HEAD /api/hello` — Claude Code's connectivity probe.
    /// Answered locally; it never touches the upstream, so a probe costs
    /// nothing and works even when the credential is unavailable.
    Hello,
}

impl Route {
    /// The canonical upstream path for this route — a constant, never
    /// derived from the request. See module docs.
    pub fn upstream_path(&self) -> &'static str {
        match self {
            Route::Messages => "/v1/messages",
            Route::CountTokens => "/v1/messages/count_tokens",
            // Never forwarded; present so the mapping is total rather
            // than having a panicking arm.
            Route::Hello => "/api/hello",
        }
    }

    /// Whether traffic on this route consumes provider resources and must
    /// therefore pass the reserve/settle cycle.
    pub fn is_metered(&self) -> bool {
        matches!(self, Route::Messages)
    }

    /// Whether the gateway answers this route itself instead of
    /// forwarding it.
    pub fn is_answered_locally(&self) -> bool {
        matches!(self, Route::Hello)
    }
}

/// Matches `(method, path)` against the closed route table.
///
/// `path` must already be the URI's path component with its query string
/// removed. Matching is on exact equality — no prefix match, no
/// normalisation, no trailing-slash tolerance. A caller that wants
/// `/v1/messages/` proxied must add it to this table on purpose.
pub fn match_route(method: &hyper::Method, path: &str) -> Option<Route> {
    match (method, path) {
        (&hyper::Method::POST, "/v1/messages") => Some(Route::Messages),
        (&hyper::Method::POST, "/v1/messages/count_tokens") => Some(Route::CountTokens),
        (&hyper::Method::GET, "/api/hello") | (&hyper::Method::HEAD, "/api/hello") => {
            Some(Route::Hello)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Method;

    #[test]
    fn the_three_supported_routes_match() {
        assert_eq!(
            match_route(&Method::POST, "/v1/messages"),
            Some(Route::Messages)
        );
        assert_eq!(
            match_route(&Method::POST, "/v1/messages/count_tokens"),
            Some(Route::CountTokens)
        );
        assert_eq!(match_route(&Method::GET, "/api/hello"), Some(Route::Hello));
        assert_eq!(match_route(&Method::HEAD, "/api/hello"), Some(Route::Hello));
    }

    #[test]
    fn the_method_is_part_of_the_match() {
        assert_eq!(match_route(&Method::GET, "/v1/messages"), None);
        assert_eq!(match_route(&Method::DELETE, "/v1/messages"), None);
        assert_eq!(match_route(&Method::POST, "/api/hello"), None);
    }

    #[test]
    fn traversal_and_near_miss_paths_are_refused_rather_than_normalised() {
        for path in [
            "/v1/messages/../../internal",
            "/v1/messages/",
            "//v1/messages",
            "/V1/MESSAGES",
            "/v1/messages/extra",
            "/v1/messages%2f..%2fadmin",
            "/",
            "",
            "/v1/complete",
            "/v1/models",
        ] {
            assert_eq!(
                match_route(&Method::POST, path),
                None,
                "{path} must not match the closed route table"
            );
        }
    }

    #[test]
    fn an_upstream_path_is_a_constant_not_the_inbound_path() {
        // The property that keeps this from being an open proxy: whatever
        // arrived, the outbound path is one of exactly these strings.
        for route in [Route::Messages, Route::CountTokens, Route::Hello] {
            assert!(route.upstream_path().starts_with('/'));
            assert!(!route.upstream_path().contains(".."));
        }
        assert_eq!(Route::Messages.upstream_path(), "/v1/messages");
        assert_eq!(
            Route::CountTokens.upstream_path(),
            "/v1/messages/count_tokens"
        );
    }

    #[test]
    fn only_messages_is_metered_and_only_hello_is_local() {
        assert!(Route::Messages.is_metered());
        assert!(
            !Route::CountTokens.is_metered(),
            "count_tokens is free — metering it would deny real work for no reason"
        );
        assert!(!Route::Hello.is_metered());

        assert!(Route::Hello.is_answered_locally());
        assert!(!Route::Messages.is_answered_locally());
        assert!(!Route::CountTokens.is_answered_locally());
    }
}
