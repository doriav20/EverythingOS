//! The index server: other EverythingOS computers add this one as a
//! remote index and keep a live copy of its index (EverythingOS's
//! counterpart of the Everything Server).
//!
//! Clients log in with one of the configured accounts, list the volumes
//! they may copy, take a snapshot of each and then ask for deltas: just
//! the entries appended or changed since their last sync. Each account
//! sees what the "run as" Linux account may see, limited to its
//! include-only folders; hidden entries travel as nameless tombstones.
//! Copies of other computers' indexes are never shared on.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use evos_core::perm::{Caller, Visibility};
use evos_core::{persist, EntryId, Index};
use evos_proto::{
    decode_body, encode_frame, Envelope, ErrorCode, IndexUser, IndexVolume, Request, Response,
    MAX_FRAME, PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{files, included};
use crate::accounts::verify_password;
use crate::server::{blocking, Daemon};
use crate::volumes::VolumeHandle;

const MAX_CLIENTS: usize = 32;
const IDLE: Duration = Duration::from_secs(600);
/// Largest file read per request.
const MAX_READ: u32 = 16 << 20;

/// Mixed into reported epochs: a restarted daemon may have renumbered
/// entries, so copies taken from an earlier run must start over.
fn run_nonce() -> u64 {
    static NONCE: OnceLock<u64> = OnceLock::new();
    *NONCE.get_or_init(|| {
        use std::io::Read;
        let mut b = [0u8; 8];
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        u64::from_le_bytes(b) ^ t
    })
}

fn wire_epoch(ix: &Index) -> u64 {
    run_nonce().wrapping_add(ix.epoch())
}

pub async fn serve(daemon: Arc<Daemon>, listener: TcpListener) {
    super::accept_loop(listener, MAX_CLIENTS, b"", |stream, peer| {
        let daemon = daemon.clone();
        async move {
            if let Err(e) = connection(daemon, stream).await {
                tracing::debug!("index client {peer} disconnected: {e}");
            }
        }
    })
    .await
}

/// The folders an account's copy is limited to, as entry ids of a volume:
/// `None` means the whole volume.
fn include_ids(ix: &Index, include: &[String]) -> Option<Option<Vec<EntryId>>> {
    let folders: Vec<&String> = include.iter().filter(|f| !f.trim().is_empty()).collect();
    if folders.is_empty() || included(ix.root_path(), include) {
        return Some(None);
    }
    let ids: Vec<EntryId> = folders
        .iter()
        .filter_map(|f| ix.resolve(f.trim_end_matches('/').as_bytes()))
        .collect();
    (!ids.is_empty()).then_some(Some(ids))
}

/// Local volumes the account may copy.
fn offered(daemon: &Daemon, user: &IndexUser) -> Vec<Arc<VolumeHandle>> {
    daemon
        .volumes
        .volumes()
        .into_iter()
        .filter(|h| h.remote.is_none())
        .filter(|h| include_ids(&h.vol.index.read(), &user.include_only).is_some())
        .collect()
}

/// What the account may see in one volume.
fn keep_fn<'a>(
    ix: &'a Index,
    vol: u16,
    caller: &'a Caller,
    include: Option<Vec<EntryId>>,
) -> impl Fn(EntryId) -> bool + 'a {
    let vis = Visibility::new(caller.clone(), &[(vol, ix)]);
    move |id| {
        vis.is_visible(vol, ix, id)
            && include
                .as_ref()
                .is_none_or(|inc| inc.iter().any(|&i| ix.is_within(id, i)))
    }
}

fn error(code: ErrorCode, message: &str) -> Response {
    Response::Error {
        code,
        message: message.into(),
    }
}

async fn connection(daemon: Arc<Daemon>, stream: TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let (mut reader, mut writer) = stream.into_split();
    let mut user: Option<IndexUser> = None;
    loop {
        let len = match tokio::time::timeout(IDLE, reader.read_u32_le()).await {
            Ok(Ok(len)) => len as usize,
            _ => return Ok(()),
        };
        if len > MAX_FRAME {
            return Ok(());
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).await?;
        let (id, response) = match decode_body::<Envelope<Request>>(&body) {
            Ok(req) => (req.id, handle(&daemon, &mut user, req.body).await),
            Err(e) => (0, error(ErrorCode::BadRequest, &e.to_string())),
        };
        let frame = encode_frame(&Envelope { id, body: response })
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        writer.write_all(&frame).await?;
    }
}

async fn handle(daemon: &Arc<Daemon>, user: &mut Option<IndexUser>, request: Request) -> Response {
    let settings = daemon.servers.stored().index;
    let Ok(caller) = super::run_as(&settings.run_as) else {
        return error(ErrorCode::Failed, "the server has no account to run as");
    };
    match request {
        Request::Hello { .. } => {
            return Response::Hello {
                server: format!("evosd {} index server", env!("CARGO_PKG_VERSION")),
                protocol: PROTOCOL_VERSION,
            }
        }
        Request::IndexLogin {
            user: name,
            password,
        } => {
            let found = settings.users.iter().find(|u| u.name == name).cloned();
            return match found {
                Some(u) if verify_password(u.password.as_deref().unwrap_or(""), &password) => {
                    *user = Some(u);
                    Response::Ok
                }
                _ => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    error(ErrorCode::PermissionDenied, "login or password incorrect")
                }
            };
        }
        _ => {}
    }
    let Some(account) = user.clone() else {
        return error(ErrorCode::PermissionDenied, "log in first");
    };
    let d = daemon.clone();
    match request {
        Request::IndexVolumes => {
            let volumes = blocking(move || {
                offered(&d, &account)
                    .iter()
                    .map(|h| {
                        let ix = h.vol.index.read();
                        IndexVolume {
                            id: h.id,
                            root: ix.root_path().to_vec(),
                            fs_type: h.vol.fs_type().to_owned(),
                            epoch: wire_epoch(&ix),
                            generation: ix.generation(),
                        }
                    })
                    .collect()
            })
            .await;
            Response::IndexVolumes(volumes)
        }
        Request::IndexSnapshot { volume } => {
            blocking(move || {
                let Some(h) = offered(&d, &account).into_iter().find(|h| h.id == volume) else {
                    return error(ErrorCode::NotFound, "no such volume");
                };
                let ix = h.vol.index.read();
                let include = include_ids(&ix, &account.include_only).flatten();
                let copy = ix.filtered_copy(keep_fn(&ix, h.id, &caller, include));
                drop(ix);
                let mut bytes = Vec::new();
                match persist::write_snapshot(&copy, &[], &mut bytes) {
                    Ok(()) => Response::IndexData(bytes),
                    Err(e) => error(ErrorCode::Failed, &e.to_string()),
                }
            })
            .await
        }
        Request::IndexDelta {
            volume,
            epoch,
            len,
            since,
        } => {
            blocking(move || {
                let Some(h) = offered(&d, &account).into_iter().find(|h| h.id == volume) else {
                    return error(ErrorCode::NotFound, "no such volume");
                };
                let ix = h.vol.index.read();
                if wire_epoch(&ix) != epoch || len as usize > ix.len() {
                    return Response::IndexResync;
                }
                let include = include_ids(&ix, &account.include_only).flatten();
                let delta = ix.delta(len as usize, since, keep_fn(&ix, h.id, &caller, include));
                drop(ix);
                match delta.encode() {
                    Ok(bytes) => Response::IndexData(bytes),
                    Err(e) => error(ErrorCode::Failed, &e.to_string()),
                }
            })
            .await
        }
        Request::ReadFile {
            path,
            offset,
            limit,
        } => {
            if !settings.allow_file_download {
                return error(ErrorCode::PermissionDenied, "file downloads are disabled");
            }
            blocking(move || {
                use std::io::{Read, Seek};
                match files::open(&d, &caller, &account.include_only, &path) {
                    Ok((mut file, _)) => {
                        let mut data = Vec::new();
                        let read = file.seek(std::io::SeekFrom::Start(offset)).and_then(|_| {
                            file.take(limit.min(MAX_READ) as u64).read_to_end(&mut data)
                        });
                        match read {
                            Ok(_) => Response::FileContent(data),
                            Err(e) => error(ErrorCode::Failed, &e.to_string()),
                        }
                    }
                    Err(e) => error(ErrorCode::PermissionDenied, &e),
                }
            })
            .await
        }
        _ => error(ErrorCode::BadRequest, "not an index server request"),
    }
}
