//! Mount management requests.
//!
//! Listing mounts is open to everyone. Changing them runs mount(8) as root,
//! so it requires authorization.

use std::sync::Arc;

use evos_mount::MountError;
use evos_proto::{ErrorCode, Event, Request, Response};

use crate::auth;
use crate::server::{blocking, Daemon, Peer};

/// Is the peer allowed to perform a polkit `action`? Root always is;
/// others need the action, usually an administrator password via their
/// agent.
pub(crate) async fn authorize(peer: &Peer, action: &str) -> Result<(), String> {
    if peer.caller.is_root() {
        return Ok(());
    }
    let Some(pid) = peer.pid else {
        return Err("cannot identify the calling process".into());
    };
    let what = if action == auth::MANAGE_SERVERS {
        "servers and remote indexes"
    } else {
        "mounts"
    };
    match auth::polkit_authorized(pid, peer.caller.uid, action).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "not authorized to manage {what} (run with sudo, or from a desktop \
             session with a polkit agent)"
        )),
        Err(e) => Err(format!(
            "cannot check authorization: {e}; run with sudo instead"
        )),
    }
}

fn error_response(e: MountError) -> Response {
    let code = match e {
        MountError::Invalid(_) => ErrorCode::BadRequest,
        MountError::NotFound(_) => ErrorCode::NotFound,
        MountError::Failed(_) | MountError::Io(_) => ErrorCode::Failed,
    };
    Response::Error {
        code,
        message: e.to_string(),
    }
}

/// Handles the mount family of requests.
pub async fn handle(daemon: &Arc<Daemon>, peer: &Peer, request: Request) -> Response {
    if !matches!(request, Request::Mounts) {
        if let Err(message) = authorize(peer, auth::MANAGE_MOUNTS).await {
            return Response::Error {
                code: ErrorCode::PermissionDenied,
                message,
            };
        }
    }
    let d = daemon.clone();
    blocking(move || {
        let mounts = &d.mounts;
        let result = match request {
            Request::Mounts => return Response::Mounts(mounts.list()),
            Request::MountTest { spec, credentials } => {
                return match mounts.test(&spec, credentials.as_ref()) {
                    Ok(()) => Response::Ok,
                    Err(e) => error_response(e),
                };
            }
            Request::MountAdd { spec, credentials } => mounts.add(spec, credentials),
            Request::MountRemove { name } => mounts.remove(&name),
            Request::MountUp { name } => mounts.up(&name),
            Request::MountDown { name } => mounts.down(&name),
            other => {
                return Response::Error {
                    code: ErrorCode::BadRequest,
                    message: format!("not a mount request: {other:?}"),
                }
            }
        };
        // Attach or detach the affected volumes right away.
        d.volumes.sync();
        let _ = d.events.send(Event::StatusChanged);
        match result {
            Ok(()) => Response::Ok,
            Err(e) => error_response(e),
        }
    })
    .await
}
