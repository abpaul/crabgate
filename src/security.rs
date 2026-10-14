use axum::{
    extract::Request,
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

#[derive(Clone)]
pub(crate) struct SelectorFraming(crate::settings::EmbedOrigins);

impl SelectorFraming {
    pub(crate) fn new(origins: crate::settings::EmbedOrigins) -> Self {
        Self(origins)
    }
}

pub async fn protect(request: Request, next: Next) -> Response {
    let forbidden = unsafe_method(request.method()) && !same_origin(&request);
    let path = request.uri().path();
    let selector = matches!(*request.method(), Method::GET | Method::HEAD)
        && path
            .strip_prefix("/embed/events/")
            .or_else(|| path.strip_prefix("/embed/series/"))
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok_and(|value| value.to_string() == id));
    let static_asset = path.starts_with("/static/");
    let private = ["/office", "/operator", "/orders", "/checkout"]
        .iter()
        .any(|prefix| {
            path.strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        });
    let mut response = if forbidden {
        StatusCode::FORBIDDEN.into_response()
    } else {
        next.run(request).await
    };
    if static_asset
        && response.status() == StatusCode::OK
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"text/html"))
    {
        *response.status_mut() = StatusCode::NOT_FOUND;
    }
    let ancestors = if selector
        && matches!(
            response.status(),
            StatusCode::OK
                | StatusCode::BAD_REQUEST
                | StatusCode::NOT_FOUND
                | StatusCode::CONFLICT
                | StatusCode::INTERNAL_SERVER_ERROR
        ) {
        response
            .extensions_mut()
            .remove::<SelectorFraming>()
            .and_then(|value| value.0.ancestors())
    } else {
        None
    };
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; font-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'"),
    );
    if let Some(ancestors) = ancestors {
        let policy = format!("default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; font-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors {ancestors}");
        if let Ok(policy) = HeaderValue::from_str(&policy) {
            headers.remove(header::X_FRAME_OPTIONS);
            headers.insert(header::CONTENT_SECURITY_POLICY, policy);
        }
    }
    if private || selector {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

fn unsafe_method(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn same_origin(request: &Request) -> bool {
    if request
        .headers()
        .get("sec-fetch-site")
        .is_some_and(|value| value == "cross-site")
    {
        return false;
    }
    let Some(origin) = request.headers().get(header::ORIGIN) else {
        return request
            .headers()
            .get("sec-fetch-site")
            .is_none_or(|value| value == "same-origin" || value == "none");
    };
    let Some(host) = request.headers().get(header::HOST) else {
        return false;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if origin == "null" {
        return request
            .headers()
            .get("sec-fetch-site")
            .is_some_and(|value| value == "same-origin");
    }
    let Ok(uri) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http" | "https"))
        && uri.authority().is_some_and(|authority| {
            authority
                .as_str()
                .eq_ignore_ascii_case(host.to_str().unwrap_or_default())
        })
        && (uri.path().is_empty() || uri.path() == "/")
        && uri.query().is_none()
}
