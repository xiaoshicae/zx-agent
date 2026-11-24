//! The HTTP client a server's transport sends with: rmcp's own (`default_http_client`), which
//! takes no proxy, made again with the server's.

use zx_ai_http::client::with_proxy;

use crate::ai::Proxy;

/// Redirects one request may follow (rmcp: `MAX_REDIRECTS`).
const MAX_REDIRECTS: usize = 10;

/// The client of a server reached through `proxy`. It keeps no idle connection, which a
/// response not read to its end would stall, and follows a redirect only to the same origin
/// with the method and body kept (307, 308). Fails for a proxy that is not the `http://` URL
/// of one.
pub(super) fn http_client(proxy: &Proxy) -> Result<reqwest::Client, String> {
    let builder = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(same_origin_redirects());
    with_proxy(builder, proxy)?
        .build()
        .map_err(|error| format!("cannot build the HTTP client: {error}"))
}

/// Follow 307 and 308 to the same origin, at most [`MAX_REDIRECTS`] of them, and answer any
/// other redirect with itself: a 301, 302 or 303 would turn the message into a `GET` without
/// it, and another origin would get the session's ID and the configured headers (rmcp:
/// `same_origin_redirect_policy`).
fn same_origin_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let keeps_body = matches!(
            attempt.status(),
            reqwest::StatusCode::TEMPORARY_REDIRECT | reqwest::StatusCode::PERMANENT_REDIRECT
        );
        let same_origin = attempt
            .previous()
            .first()
            .is_some_and(|original| original.origin() == attempt.url().origin());
        if !keeps_body || !same_origin {
            attempt.stop()
        } else if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error(format!("more than {MAX_REDIRECTS} redirects"))
        } else {
            attempt.follow()
        }
    })
}
