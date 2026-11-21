use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use repeaternet_terrain::{FileSource, TiffError, TiffSource};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, ACCEPT_ENCODING, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE},
    StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::{response_error, ProviderError, Result};

use super::{
    atomic_write,
    downloader::{FetchContext, PROVIDER},
    hash,
};

const CHUNK: u64 = 256 * 1024;
const MAX_RANGE: u64 = 16 * 1024 * 1024;
const WHOLE_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct Object {
    length: u64,
    validator: Option<String>,
}

struct RemoteSource {
    directory: PathBuf,
    object: Object,
    url: String,
    client: Client,
    headers: HeaderMap,
    context: FetchContext,
    recent: Mutex<Option<(u64, Vec<u8>)>>,
}

enum Reply {
    Missing,
    Whole(Vec<u8>),
    Partial(Object, Vec<u8>),
}

pub(super) fn open(
    client: &Client,
    root: &Path,
    url: &str,
    headers: &HeaderMap,
    tiled: bool,
    context: &FetchContext,
) -> Result<Option<Arc<dyn TiffSource>>> {
    context.check()?;
    let directory = root.join("cog").join(&hash(url.as_bytes())[..20]);
    let marker = directory.join("missing");
    let whole = directory.join("whole.tif");
    let meta = directory.join("object.json");
    if tiled && marker.is_file() {
        return Ok(None);
    }
    if whole.is_file() {
        return file_source(&whole).map(Some);
    }
    let mut recent = None;
    let object = fs::read(&meta)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Object>(&bytes).ok())
        .filter(|object| object.length > 0 && directory.join("header.bin").is_file());
    let object = match object {
        Some(object) => object,
        None => {
            let request = RangeRequest {
                client,
                url,
                headers,
                context,
            };
            match request.fetch(0, CHUNK - 1, None, tiled)? {
                Reply::Missing => {
                    atomic_write(&marker, b"missing\n")?;
                    return Ok(None);
                }
                Reply::Whole(bytes) => {
                    atomic_write(&whole, &bytes)?;
                    return file_source(&whole).map(Some);
                }
                Reply::Partial(object, bytes) => {
                    atomic_write(&directory.join("header.bin"), &bytes)?;
                    atomic_write(
                        &meta,
                        &serde_json::to_vec(&object).expect("object serializes"),
                    )?;
                    recent = Some((0, bytes));
                    object
                }
            }
        }
    };
    Ok(Some(Arc::new(RemoteSource {
        directory,
        object,
        url: url.to_owned(),
        client: client.clone(),
        headers: headers.clone(),
        context: context.clone(),
        recent: Mutex::new(recent),
    })))
}

fn file_source(path: &Path) -> Result<Arc<dyn TiffSource>> {
    Ok(Arc::new(FileSource::open(path).map_err(|error| {
        response_error(PROVIDER, error.to_string())
    })?))
}

struct RangeRequest<'a> {
    client: &'a Client,
    url: &'a str,
    headers: &'a HeaderMap,
    context: &'a FetchContext,
}

impl RangeRequest<'_> {
    fn fetch(&self, first: u64, last: u64, known: Option<&Object>, tiled: bool) -> Result<Reply> {
        self.context.check()?;
        let mut request = self
            .client
            .get(self.url)
            .headers(self.headers.clone())
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, format!("bytes={first}-{last}"));
        if let Some(validator) = known.and_then(|object| object.validator.as_ref()) {
            request = request.header(IF_RANGE, validator);
        }
        let response = self.context.send(request)?;
        let status = response.status();
        if tiled && matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN) {
            return Ok(Reply::Missing);
        }
        if !status.is_success() {
            return Err(ProviderError::Http {
                provider: PROVIDER,
                status: status.as_u16(),
            });
        }
        super::downloader::check_tiff_type(&response)?;
        if status == StatusCode::OK && known.is_none() {
            if response
                .content_length()
                .is_some_and(|len| len > WHOLE_LIMIT)
            {
                return Err(response_error(
                    PROVIDER,
                    "server does not support range requests",
                ));
            }
            let mut bytes = Vec::new();
            self.context.stream(response, &mut bytes, WHOLE_LIMIT).map_err(|error| {
                if matches!(&error, ProviderError::InvalidResponse { detail, .. } if detail == "body exceeds the size limit") {
                    response_error(PROVIDER, "server does not support range requests")
                } else { error }
            })?;
            if bytes.is_empty() {
                return Err(response_error(PROVIDER, "empty TIFF response"));
            }
            return Ok(Reply::Whole(bytes));
        }
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(response_error(
                PROVIDER,
                "server does not support range requests or the TIFF object changed",
            ));
        }
        let (start, end, length) = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range)
            .ok_or_else(|| response_error(PROVIDER, "invalid Content-Range"))?;
        let validator = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.starts_with("W/"))
            .or_else(|| {
                response
                    .headers()
                    .get(LAST_MODIFIED)
                    .and_then(|v| v.to_str().ok())
            })
            .map(str::to_owned);
        if start != first
            || end != last.min(length - 1)
            || known.is_some_and(|known| {
                known.length != length
                    || known
                        .validator
                        .as_ref()
                        .is_some_and(|etag| validator.as_ref() != Some(etag))
            })
        {
            return Err(response_error(
                PROVIDER,
                "unexpected Content-Range or changed TIFF object",
            ));
        }
        let count = end - start + 1;
        let mut bytes = Vec::with_capacity(count as usize);
        self.context.stream(response, &mut bytes, count)?;
        if bytes.len() as u64 != count {
            return Err(response_error(PROVIDER, "truncated TIFF byte range"));
        }
        Ok(Reply::Partial(Object { length, validator }, bytes))
    }
}

fn parse_range(text: &str) -> Option<(u64, u64, u64)> {
    let (range, length) = text.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, length) = (start.parse().ok()?, end.parse().ok()?, length.parse().ok()?);
    (start <= end && end < length).then_some((start, end, length))
}

impl RemoteSource {
    fn path(&self, offset: u64) -> PathBuf {
        if offset == 0 {
            return self.directory.join("header.bin");
        }
        let len = CHUNK.min(self.object.length - offset);
        self.directory
            .join("blocks")
            .join(format!("{offset}-{len}.bin"))
    }

    fn cached_chunk(&self, offset: u64) -> Option<Vec<u8>> {
        let path = self.path(offset);
        let expected = CHUNK.min(self.object.length - offset);
        if fs::metadata(&path).ok()?.len() != expected {
            return None;
        }
        let bytes = fs::read(path).ok()?;
        (bytes.len() as u64 == expected).then_some(bytes)
    }

    fn read(&self, offset: u64, len: usize, end: u64) -> Result<Vec<u8>> {
        self.context.check()?;
        let mut recent = self.recent.lock().expect("COG chunk cache lock");
        let mut result = Vec::with_capacity(len);
        let mut at = offset;
        while at < end {
            let chunk = at / CHUNK * CHUNK;
            if !recent
                .as_ref()
                .is_some_and(|(start, bytes)| *start <= at && at < *start + bytes.len() as u64)
            {
                *recent = if let Some(bytes) = self.cached_chunk(chunk) {
                    Some((chunk, bytes))
                } else {
                    // Coalesce consecutive missing chunks within this TIFF read.
                    let mut next = (chunk + CHUNK).min(self.object.length);
                    while next < end && next - chunk < MAX_RANGE && !self.path(next).is_file() {
                        next = (next + CHUNK).min(self.object.length);
                    }
                    let request = RangeRequest {
                        client: &self.client,
                        url: &self.url,
                        headers: &self.headers,
                        context: &self.context,
                    };
                    let Reply::Partial(_, bytes) =
                        request.fetch(chunk, next - 1, Some(&self.object), false)?
                    else {
                        return Err(response_error(PROVIDER, "expected a partial TIFF response"));
                    };
                    for (i, block) in bytes.chunks(CHUNK as usize).enumerate() {
                        atomic_write(&self.path(chunk + i as u64 * CHUNK), block)?;
                    }
                    Some((chunk, bytes))
                };
            }
            let (start, bytes) = recent.as_ref().expect("loaded COG range");
            let relative = (at - start) as usize;
            let count = (bytes.len() - relative).min((end - at) as usize);
            result.extend_from_slice(&bytes[relative..relative + count]);
            at += count as u64;
        }
        Ok(result)
    }
}

impl TiffSource for RemoteSource {
    fn len(&self) -> u64 {
        self.object.length
    }

    fn read_exact_at(&self, offset: u64, len: usize) -> std::result::Result<Vec<u8>, TiffError> {
        let end = offset
            .checked_add(len as u64)
            .filter(|end| *end <= self.len())
            .ok_or(TiffError::OffsetOutOfBounds {
                offset,
                length: len as u64,
                data_len: self.len(),
            })?;
        self.read(offset, len, end)
            .map_err(|err| TiffError::Io(io::Error::other(err), "COG byte range".into()))
    }
}
