//! The `boxit:` protocol: a look at a file of an open archive, handed to
//! the interface as bytes instead of megabytes of JSON.
//!
//! `/preview?s=<archive>&n=<row>` answers with the content of the file as
//! [`boxit_engine::preview`] prepares it. The type of the content comes
//! from the table of that module and never from the archive, and the
//! browser is told not to guess another: what is inside an archive is data
//! from a stranger. An error is answered with its code and values as JSON.

use std::sync::Arc;

use tauri::http::{header, HeaderValue, Request, Response, StatusCode};
use tauri::{AppHandle, Manager, UriSchemeResponder, Url};

use boxit_core::{Error, NoInteraction, NoProgress, OpContext, Result};
use boxit_engine::browse::NodeId;
use boxit_engine::budget;
use boxit_engine::preview::{self, Preview, PreviewKind};

use crate::app::Core;

pub const SCHEME: &str = "boxit";

/// The pages that may read the answers: the interface as the program
/// carries it on each system, and the server of the development mode.
const ORIGINS: &[&str] = &[
    "http://tauri.localhost",
    "https://tauri.localhost",
    "tauri://localhost",
    "http://localhost:1420",
];

/// What the file is shown as, and whether it is longer than what is shown.
const KIND_HEADER: &str = "x-boxit-kind";
const TRUNCATED_HEADER: &str = "x-boxit-truncated";

/// A request taken apart: the archive and the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Query {
    session: u64,
    node: NodeId,
}

fn parse(uri: &str) -> Option<Query> {
    let url = Url::parse(uri).ok()?;
    let mut segments = url.path_segments()?.filter(|segment| !segment.is_empty());
    if segments.next() != Some("preview") || segments.next().is_some() {
        return None;
    }
    let mut session = None;
    let mut node = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "s" => session = value.parse().ok(),
            "n" => node = value.parse().ok(),
            _ => {}
        }
    }
    Some(Query {
        session: session?,
        node: node?,
    })
}

fn kind_name(kind: PreviewKind) -> &'static str {
    match kind {
        PreviewKind::Image => "image",
        PreviewKind::Text => "text",
        PreviewKind::Pdf => "pdf",
    }
}

fn serve(core: &Core, uri: &str) -> Result<Preview> {
    let query = parse(uri).ok_or_else(|| Error::new("notFound"))?;
    let session = core.engine.sessions.get(query.session)?;
    let ctx = OpContext {
        // A look at another file stops the reading of this one.
        control: core.begin_preview(),
        progress: Arc::new(NoProgress),
        interaction: Arc::new(NoInteraction),
        budget: budget::system(0),
    };
    preview::read(&session, query.node, &ctx)
}

fn respond(responder: UriSchemeResponder, origin: Option<HeaderValue>, result: Result<Preview>) {
    let mut response = Response::builder()
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            format!("{KIND_HEADER}, {TRUNCATED_HEADER}"),
        );
    if let Some(origin) = origin {
        response = response.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    let response = match result {
        Ok(preview) => response
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, preview.mime)
            .header(KIND_HEADER, kind_name(preview.kind))
            .header(TRUNCATED_HEADER, if preview.truncated { "1" } else { "0" })
            .body(preview.content),
        Err(error) => {
            let status = match error.code.as_str() {
                "notFound" | "archiveClosed" => StatusCode::NOT_FOUND,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            // The same shape a command fails with: a code and its values.
            let body =
                serde_json::to_vec(&error).unwrap_or_else(|_| br#"{"code":"internal"}"#.to_vec());
            response
                .status(status)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .body(body)
        }
    };
    if let Ok(response) = response {
        responder.respond(response);
    }
}

/// Answers a request of the web view. Called on the main thread, so the
/// reading goes to the worker pool.
pub fn handle(app: &AppHandle, request: &Request<Vec<u8>>, responder: UriSchemeResponder) {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .filter(|origin| {
            ORIGINS
                .iter()
                .any(|allowed| origin.as_bytes() == allowed.as_bytes())
        })
        .cloned();
    let Some(core) = app.try_state::<Arc<Core>>().map(|core| Arc::clone(&core)) else {
        respond(responder, origin, Err(Error::new("notFound")));
        return;
    };
    let uri = request.uri().to_string();
    tauri::async_runtime::spawn_blocking(move || respond(responder, origin, serve(&core, &uri)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_taken_apart() {
        assert_eq!(
            parse("http://boxit.localhost/preview?s=3&n=42"),
            Some(Query {
                session: 3,
                node: 42
            })
        );
        assert_eq!(
            parse("boxit://localhost/preview?n=0&s=18446744073709551615&v=7"),
            Some(Query {
                session: u64::MAX,
                node: 0
            })
        );
        for wrong in [
            "not a url",
            "boxit://localhost/preview",
            "boxit://localhost/preview?s=1",
            "boxit://localhost/preview?s=1&n=-1",
            "boxit://localhost/preview?s=one&n=1",
            "boxit://localhost/other?s=1&n=1",
            "boxit://localhost/preview/more?s=1&n=1",
            "boxit://localhost/?s=1&n=1",
        ] {
            assert_eq!(parse(wrong), None, "{wrong}");
        }
    }

    #[test]
    fn every_kind_has_a_name_for_the_interface() {
        assert_eq!(kind_name(PreviewKind::Image), "image");
        assert_eq!(kind_name(PreviewKind::Text), "text");
        assert_eq!(kind_name(PreviewKind::Pdf), "pdf");
    }
}
