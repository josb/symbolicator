use std::io::Cursor;
use std::sync::Arc;

use anyhow::Context;
use axum::body::Body;
use axum::extract;
use axum::http::{Method, Request, Response, StatusCode};

use symbolicator_sources::parse_symstore_path;

use crate::service::{FindObject, ObjectHandle, ObjectPurpose, RequestService, Scope};

use super::ResponseError;

async fn load_object(
    service: RequestService,
    path: String,
) -> anyhow::Result<Option<Arc<ObjectHandle>>> {
    let config = service.config();
    if !config.symstore_proxy {
        return Ok(None);
    }

    let (filetypes, object_id) = match parse_symstore_path(&path) {
        Some(tuple) => tuple,
        None => return Ok(None),
    };

    tracing::debug!("Searching for {:?} ({:?})", object_id, filetypes);

    let found_object = service
        .find_object(FindObject {
            filetypes,
            identifier: object_id,
            sources: config.default_sources(),
            scope: Scope::Global,
            purpose: ObjectPurpose::Debug,
        })
        .await
        .context("failed to download object")?;

    let object_meta = match found_object.meta {
        Some(meta) => meta,
        None => return Ok(None),
    };

    let object_handle = service
        .fetch_object(object_meta)
        .await
        .context("failed to download object")?;

    if object_handle.has_object() {
        Ok(Some(object_handle))
    } else {
        Ok(None)
    }
}

pub async fn proxy_symstore_request(
    extract::Extension(service): extract::Extension<RequestService>,
    extract::Path(path): extract::Path<String>,
    request: Request<Body>,
) -> Result<Response<Body>, ResponseError> {
    sentry::configure_scope(|scope| {
        scope.set_transaction(Some("GET /proxy"));
    });

    let object_handle = match load_object(service, path).await? {
        Some(handle) => handle,
        None => {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())?)
        }
    };

    let response = Response::builder()
        .header("content-length", object_handle.len())
        .header("content-type", "application/octet-stream");

    if *request.method() == Method::HEAD {
        return Ok(response.body(Body::empty())?);
    }

    let bytes = Cursor::new(object_handle.data());
    Ok(response.body(Body::wrap_stream(tokio_util::io::ReaderStream::new(bytes)))?)
}
