use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::{ConnectInfo, Form, FromRequest, Multipart, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header, uri::Authority},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use repeaternet_project_store::valid_workspace_key;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{api::problem, assets};

const ACCESS_COOKIE: &str = "planner_access";
const WORKSPACE_COOKIE: &str = "planner_workspace";

#[derive(Clone)]
pub struct Security {
    access_token_digest: Option<[u8; 32]>,
    loopback_only: bool,
    trusted_proxy: Option<IpAddr>,
}

impl Security {
    pub fn new(access_token: &str, loopback_only: bool, trusted_proxy: Option<IpAddr>) -> Self {
        Self {
            access_token_digest: (!access_token.is_empty()).then(|| token_digest(access_token)),
            loopback_only,
            trusted_proxy: trusted_proxy.map(|ip| ip.to_canonical()),
        }
    }

    fn accepts(&self, token: &str) -> bool {
        if let Some(digest) = &self.access_token_digest {
            digest.ct_eq(&token_digest(token)).into()
        } else {
            token.is_empty()
        }
    }
}

fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[derive(Clone, Copy, Default)]
pub struct Https(pub bool);

pub async fn enforce(
    State(security): State<Security>,
    mut request: Request,
    next: Next,
) -> Response {
    let trusted = security.trusted_proxy.is_some_and(|proxy| {
        request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|peer| peer.0.ip().to_canonical() == proxy)
    });
    let forwarded = trusted
        && (request.headers().contains_key("x-forwarded-proto")
            || request.headers().contains_key("x-forwarded-host"));
    let (authority, scheme) = if forwarded {
        let host = single_header(request.headers(), "x-forwarded-host")
            .and_then(|host| host.parse::<Authority>().ok())
            .filter(|host| !host.as_str().contains(['@', ',']));
        let scheme =
            single_header(request.headers(), "x-forwarded-proto").and_then(|scheme| match scheme {
                "http" => Some("http"),
                "https" => Some("https"),
                _ => None,
            });
        (host, scheme)
    } else {
        (request_authority(&request), Some("http"))
    };
    let path = request.uri().path();
    let response = if forwarded && (authority.is_none() || scheme.is_none()) {
        problem(StatusCode::BAD_REQUEST, "Invalid forwarded host or scheme")
    } else if security.loopback_only
        && !forwarded
        && !authority.as_ref().is_some_and(is_loopback_host)
    {
        problem(
            StatusCode::BAD_REQUEST,
            "Invalid Host for a loopback listener",
        )
    } else if !same_origin(&request, authority.as_ref(), scheme.unwrap_or("http")) {
        problem(StatusCode::FORBIDDEN, "Cross-origin request rejected")
    } else if path != "/login"
        && path != "/health"
        && security.access_token_digest.is_some()
        && !cookie(request.headers(), ACCESS_COOKIE).is_some_and(|token| security.accepts(&token))
    {
        if path.starts_with("/api/") || path == "/api" {
            problem(StatusCode::UNAUTHORIZED, "Sign in required")
        } else {
            assets::login()
        }
    } else {
        request
            .extensions_mut()
            .insert(Https(scheme == Some("https")));
        next.run(request).await
    };

    let mut response = response;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn request_authority(request: &Request) -> Option<Authority> {
    if let Some(authority) = request.uri().authority() {
        return Some(authority.clone());
    }
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts.next()?.to_str().ok()?;
    if hosts.next().is_some() {
        return None;
    }
    host.parse().ok()
}

fn is_loopback_host(authority: &Authority) -> bool {
    let host = authority.host();
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(crate::is_loopback)
}

fn same_origin(request: &Request, authority: Option<&Authority>, scheme: &str) -> bool {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return true;
    }
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    origins.next().is_none()
        && authority
            .is_some_and(|host| origin.as_bytes() == format!("{scheme}://{host}").as_bytes())
}

#[derive(Deserialize)]
struct Login {
    token: Option<String>,
}

pub async fn login(State(security): State<Security>, request: Request) -> Response {
    let https = request
        .extensions()
        .get::<Https>()
        .copied()
        .unwrap_or_default();
    let token = match login_token(request).await {
        Ok(token) => token,
        Err(status) => return problem(status, "Could not read the access token form"),
    };
    let Some(token) = token.filter(|token| security.accepts(token)) else {
        return problem(StatusCode::UNAUTHORIZED, "Incorrect access token");
    };
    set_cookie(
        Redirect::to("/").into_response(),
        ACCESS_COOKIE,
        &token,
        "",
        https,
    )
}

async fn login_token(request: Request) -> Result<Option<String>, StatusCode> {
    let media_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    match media_type {
        Some("application/x-www-form-urlencoded") => {
            let Form(Login { token }) = Form::from_request(request, &())
                .await
                .map_err(|error| error.status())?;
            Ok(token)
        }
        Some("multipart/form-data") => {
            let mut form = Multipart::from_request(request, &())
                .await
                .map_err(|error| error.status())?;
            let mut token = None;
            while let Some(field) = form.next_field().await.map_err(|error| error.status())? {
                if field.name() == Some("token") && field.file_name().is_none() {
                    token = Some(field.text().await.map_err(|error| error.status())?);
                }
            }
            Ok(token)
        }
        _ => Ok(None),
    }
}

pub fn workspace(headers: &HeaderMap) -> Option<String> {
    cookie(headers, WORKSPACE_COOKIE).filter(|key| valid_workspace_key(key))
}

pub fn initialize_workspace(mut response: Response, headers: &HeaderMap, https: Https) -> Response {
    if workspace(headers).is_none() {
        let mut bytes = [0; 24];
        if getrandom::fill(&mut bytes).is_err() {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not initialize a workspace",
            );
        }
        let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        response = set_cookie(
            response,
            WORKSPACE_COOKIE,
            &key,
            "; Max-Age=31536000",
            https,
        );
    }
    response
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|line| line.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(key, _)| key.trim() == name)
        .filter_map(|(_, value)| decode_cookie(value.trim()))
        .next_back()
}

fn set_cookie(
    mut response: Response,
    name: &str,
    value: &str,
    extra: &str,
    https: Https,
) -> Response {
    let value = encode_cookie(value);
    let secure = if https.0 { "; Secure" } else { "" };
    let header = format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict{extra}{secure}");
    // The name and attributes are static, and encode_cookie emits printable ASCII only.
    let header = HeaderValue::from_str(&header).expect("cookie header contains valid ASCII");
    response.headers_mut().append(header::SET_COOKIE, header);
    response
}

fn encode_cookie(value: &str) -> String {
    if value.bytes().all(is_cookie_octet) {
        return value.to_owned();
    }
    let mut result = String::from("\"");
    for byte in value.bytes() {
        if is_cookie_octet(byte) {
            result.push(char::from(byte));
        } else {
            // Python's SimpleCookie uses backslash-octal escapes in quoted cookie values.
            use std::fmt::Write;
            write!(result, "\\{byte:03o}").expect("writing to a String cannot fail");
        }
    }
    result.push('"');
    result
}

fn is_cookie_octet(byte: u8) -> bool {
    byte.is_ascii_graphic() && !matches!(byte, b'"' | b',' | b';' | b'\\')
}

fn decode_cookie(value: &str) -> Option<String> {
    let Some(quoted) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return Some(value.to_owned());
    };
    let mut remaining = quoted.as_bytes();
    let mut decoded = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        match remaining {
            [
                b'\\',
                a @ b'0'..=b'3',
                b @ b'0'..=b'7',
                c @ b'0'..=b'7',
                rest @ ..,
            ] => {
                decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
                remaining = rest;
            }
            [b'\\', escaped, rest @ ..] | [escaped, rest @ ..] => {
                decoded.push(*escaped);
                remaining = rest;
            }
            [] => break,
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::{decode_cookie, encode_cookie};

    #[test]
    fn cookie_encoding_round_trips_tokens_without_raw_delimiters() {
        for token in [
            "",
            "regular-key=123",
            "spaces; commas, and \\",
            "héllo 世界",
        ] {
            let encoded = encode_cookie(token);
            assert!(!encoded.contains([';', ',']));
            assert_eq!(decode_cookie(&encoded).as_deref(), Some(token));
        }
        assert_eq!(
            decode_cookie("\"a\\054b\\073c\\\\d\"").as_deref(),
            Some("a,b;c\\d")
        );
    }
}
