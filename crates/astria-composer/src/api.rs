use std::net::SocketAddr;

use axum::{
    extract::{
        FromRef,
        State,
    },
    response::{
        IntoResponse,
        Response,
    },
    routing::{
        get,
        IntoMakeService,
    },
    Router,
};
use hyper::server::conn::AddrIncoming;
use serde::Serialize;
use tokio::sync::watch;
use tracing::{
    debug,
    instrument,
};

use crate::composer;

pub(super) type ApiServer = axum::Server<AddrIncoming, IntoMakeService<Router>>;

type ComposerStatus = watch::Receiver<composer::Status>;

/// `AppState` is an axum extractor
#[derive(Clone)]
struct AppState {
    composer_status: ComposerStatus,
}

impl FromRef<AppState> for ComposerStatus {
    fn from_ref(app_state: &AppState) -> Self {
        app_state.composer_status.clone()
    }
}

pub(super) fn start(listen_addr: SocketAddr, composer_status: ComposerStatus) -> ApiServer {
    let app = Router::new()
        .route("/readyz", get(readyz))
        .with_state(AppState {
            composer_status,
        });
    axum::Server::bind(&listen_addr).serve(app.into_make_service())
}

enum Readyz {
    Ok,
    NotReady,
}

impl IntoResponse for Readyz {
    fn into_response(self) -> Response {
        #[derive(Debug, Serialize)]
        struct ReadyBody {
            status: &'static str,
        }
        let (status, msg) = match self {
            Self::Ok => (axum::http::StatusCode::OK, "ok"),
            Self::NotReady => (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not ready"),
        };
        let mut response = axum::Json(ReadyBody {
            status: msg,
        })
        .into_response();
        *response.status_mut() = status;
        response
    }
}

// axum does not allow non-async handlers. This attribute can be removed
// once this method contains `await` statements.
#[allow(clippy::unused_async)]
#[instrument(skip_all)]
async fn readyz(State(composer_status): State<ComposerStatus>) -> Readyz {
    debug!("received readyz request");
    if composer_status.borrow().is_ready() {
        Readyz::Ok
    } else {
        Readyz::NotReady
    }
}
