use std::sync::Arc;

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use chrono::{SecondsFormat, TimeZone, Utc};
use loco_rs::prelude::*;

use crate::{
    controllers::{
        checkout,
        hosted::{marketing_host, GUIDES, LANDING_PAGES},
    },
    events, hosted,
    organizers::{Edition, OrganizerDb, OrganizerError, OrganizerRegistry},
};

const SHOP_DISALLOW: [&str; 4] = ["/office", "/checkout", "/orders", "/door"];
const MARKETING_DISALLOW: [&str; 4] = ["/signup", "/verify", "/office", "/operator"];

enum Site {
    Marketing,
    Shop(OrganizerDb),
}

async fn site(
    headers: &HeaderMap,
    registry: &OrganizerRegistry,
) -> std::result::Result<Site, Box<Response>> {
    if Edition::from_environment() == Edition::Hosted && marketing_host(headers) {
        return Ok(Site::Marketing);
    }
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    match registry.resolve(host).await {
        Ok(scope) => Ok(Site::Shop(scope)),
        Err(OrganizerError::NotFound | OrganizerError::InvalidSubdomain) => {
            Err(Box::new(crate::app::not_found()))
        }
        Err(OrganizerError::Suspended) => {
            Err(Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))
        }
        Err(error) => {
            tracing::error!(%error, "Organizer lookup failed");
            Err(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))
        }
    }
}

async fn sitemap(
    State(ctx): State<AppContext>,
    headers: HeaderMap,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
) -> Result<Response> {
    let entries = match site(&headers, &registry).await {
        Err(response) => return Ok(*response),
        Ok(Site::Marketing) => {
            let origin = hosted::origin(None);
            std::iter::once("/")
                .chain(LANDING_PAGES.iter().map(|(path, _)| *path))
                .chain(GUIDES.iter().map(|guide| guide.path))
                .map(|path| (format!("{origin}{path}"), None))
                .collect::<Vec<_>>()
        }
        Ok(Site::Shop(OrganizerDb { organizer, db })) => {
            let origin = checkout::origin(&ctx, &organizer);
            let now = Utc::now().timestamp();
            let all = events::list(&db)
                .await
                .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
            std::iter::once((format!("{origin}/"), None))
                .chain(
                    all.into_iter()
                        .filter(|event| event.status == "Published" && event.ends_at > now)
                        .map(|event| {
                            (
                                format!("{origin}/events/{}", event.id),
                                Utc.timestamp_opt(event.updated_at, 0)
                                    .single()
                                    .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true)),
                            )
                        }),
                )
                .collect()
        }
    };
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    for (location, lastmod) in entries {
        xml.push_str("<url><loc>");
        xml.push_str(&escape_xml(&location));
        xml.push_str("</loc>");
        if let Some(lastmod) = lastmod {
            xml.push_str("<lastmod>");
            xml.push_str(&lastmod);
            xml.push_str("</lastmod>");
        }
        xml.push_str("</url>\n");
    }
    xml.push_str("</urlset>\n");
    Ok((
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

async fn robots(
    State(ctx): State<AppContext>,
    headers: HeaderMap,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
) -> Response {
    let (origin, disallow) = match site(&headers, &registry).await {
        Err(response) => return *response,
        Ok(Site::Marketing) => (hosted::origin(None), MARKETING_DISALLOW),
        Ok(Site::Shop(OrganizerDb { organizer, .. })) => {
            (checkout::origin(&ctx, &organizer), SHOP_DISALLOW)
        }
    };
    let mut body = String::from("User-agent: *\nAllow: /\n");
    for path in disallow {
        body.push_str(&format!("Disallow: {path}\n"));
    }
    body.push_str(&format!("\nSitemap: {origin}/sitemap.xml\n"));
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

async fn favicon() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/x-icon"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("../../assets/static/shared/favicon.ico").as_slice(),
    )
        .into_response()
}

async fn apple_touch_icon() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("../../assets/static/shared/apple-touch-icon.png").as_slice(),
    )
        .into_response()
}

fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

pub fn routes() -> Routes {
    Routes::new()
        .add("/sitemap.xml", get(sitemap))
        .add("/robots.txt", get(robots))
        .add("/favicon.ico", get(favicon))
        .add("/apple-touch-icon.png", get(apple_touch_icon))
}
