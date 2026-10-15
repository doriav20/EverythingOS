//! End-to-end tests of the socket server over a real (tmpfs) volume.

use std::os::unix::ffi::OsStrExt;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use evos_mount::MountManager;
use evos_proto::{
    Client, ErrorCode, Request, RunChange, SearchOptions, SearchResult, Sort, SortKey,
};
use evosd::config::{Config, LocalConfig};
use evosd::history::RunHistory;
use evosd::server::{serve, Daemon};
use evosd::volumes::VolumeManager;

/// Each fixture indexes only its own tmpfs, chosen by skipping every other
/// mount; a fixture created concurrently would see the other's tmpfs, so
/// tests using fixtures run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    _tmp: tempfile::TempDir,
    disk: std::path::PathBuf,
    socket: std::path::PathBuf,
    daemon: Arc<Daemon>,
    _rt: tokio::runtime::Runtime,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.daemon.volumes.shutdown();
        let _ = Command::new("umount").arg(&self.disk).status();
    }
}

fn fixture() -> Option<Fixture> {
    let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let disk = tmp.path().join("disk");
    std::fs::create_dir_all(&disk).unwrap();
    let ok = Command::new("mount")
        .args(["-t", "tmpfs", "evos-test"])
        .arg(&disk)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return None;
    }
    for d in ["docs/reports", "photos"] {
        std::fs::create_dir_all(disk.join(d)).unwrap();
    }
    std::fs::write(disk.join("docs/reports/q1 report.pdf"), vec![0u8; 3000]).unwrap();
    std::fs::write(disk.join("docs/reports/q2 report.pdf"), vec![0u8; 1000]).unwrap();
    std::fs::write(disk.join("photos/cat.jpg"), vec![0u8; 2000]).unwrap();
    std::fs::write(disk.join("photos/מסמך חשוב.txt"), "שלום עולם").unwrap();

    let disk_bytes = disk.as_os_str().as_bytes().to_vec();
    let skip = evos_scan::mountinfo::read_mounts()
        .unwrap()
        .into_iter()
        .filter(|m| m.mount_point != disk_bytes)
        .map(|m| String::from_utf8_lossy(&m.mount_point).into_owned())
        .collect();
    let cfg = Arc::new(Config {
        state_dir: tmp.path().join("state"),
        local: LocalConfig {
            index_tmpfs: true,
            skip_mounts: skip,
            ..Default::default()
        },
        ..Default::default()
    });
    std::fs::create_dir_all(&cfg.state_dir).unwrap();
    let mounts =
        Arc::new(MountManager::open(tmp.path().join("etc"), tmp.path().join("run")).unwrap());
    let volumes = Arc::new(VolumeManager::new(cfg.clone(), mounts.clone()));
    volumes.sync();
    let (events, _) = tokio::sync::broadcast::channel(16);
    let history = Arc::new(RunHistory::load(cfg.state_dir.join("run-history")));
    let daemon = Arc::new(Daemon {
        cfg,
        volumes,
        mounts,
        history,
        events,
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let socket = tmp.path().join("evosd.sock");
    let listener = rt
        .block_on(async { tokio::net::UnixListener::bind(&socket) })
        .unwrap();
    rt.spawn(serve(daemon.clone(), listener));
    Some(Fixture {
        _tmp: tmp,
        disk,
        socket,
        daemon,
        _rt: rt,
        _serial: serial,
    })
}

fn wait_indexed(client: &mut Client, query: &str, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let SearchResult {
            id: result, total, ..
        } = client
            .search(query, SearchOptions::default(), Sort::default())
            .unwrap();
        client.release(result).unwrap();
        if total == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{query:?}: {total} hits, wanted {expected}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn names(client: &mut Client, result: u32) -> Vec<String> {
    client
        .fetch(result, 0, 100)
        .unwrap()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.name).into_owned())
        .collect()
}

#[test]
fn search_fetch_sort_and_live_updates() {
    let Some(fx) = fixture() else {
        eprintln!("skipping: cannot mount tmpfs here");
        return;
    };
    let mut client = Client::connect(&fx.socket, "test").unwrap();
    assert!(client.server.starts_with("evosd "));
    wait_indexed(&mut client, "report", 3);

    let by_size = Sort {
        key: SortKey::Size,
        descending: true,
    };
    let SearchResult {
        id: result, total, ..
    } = client
        .search("ext:pdf|ext:jpg", SearchOptions::default(), by_size)
        .unwrap();
    assert_eq!(total, 3);
    assert_eq!(
        names(&mut client, result),
        ["q1 report.pdf", "cat.jpg", "q2 report.pdf"]
    );
    let rows = client.fetch(result, 1, 1).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].size, Some(2000));
    assert_eq!(
        rows[0].path(),
        [fx.disk.as_os_str().as_bytes(), b"/photos/cat.jpg"].concat()
    );
    client
        .sort(
            result,
            Sort {
                key: SortKey::Name,
                descending: false,
            },
        )
        .unwrap();
    assert_eq!(
        names(&mut client, result),
        ["cat.jpg", "q1 report.pdf", "q2 report.pdf"]
    );

    let err = client
        .search("regex:(", SearchOptions::default(), Sort::default())
        .unwrap_err();
    assert!(matches!(
        err,
        evos_proto::ClientError::Server {
            code: ErrorCode::BadQuery,
            ..
        }
    ));
    let err = client.fetch(999, 0, 1).unwrap_err();
    assert!(matches!(
        err,
        evos_proto::ClientError::Server {
            code: ErrorCode::UnknownResult,
            ..
        }
    ));

    // Live update: a new file becomes searchable without a rescan.
    std::fs::write(fx.disk.join("docs/q3 report.pdf"), b"x").unwrap();
    wait_indexed(&mut client, "report", 4);

    let status = client.status().unwrap();
    assert_eq!(status.volumes.len(), 1);
    assert_eq!(status.volumes[0].phase, "live");
    assert!(status.volumes[0].entries >= 7);
    client.expect_ok(Request::Rescan { path: None }).unwrap();
    client
        .expect_ok(Request::Hint {
            path: fx.disk.as_os_str().as_bytes().to_vec(),
        })
        .unwrap();
    assert!(!client.mounts().unwrap().iter().any(|m| m.mounted));
}

#[test]
fn rejects_garbage_frames() {
    let Some(fx) = fixture() else { return };
    use std::io::{Read, Write};
    let mut raw = std::os::unix::net::UnixStream::connect(&fx.socket).unwrap();
    raw.write_all(&3u32.to_le_bytes()).unwrap();
    raw.write_all(&[0xff, 0xff, 0xff]).unwrap();
    let mut len = [0u8; 4];
    raw.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    raw.read_exact(&mut body).unwrap();
    let resp: evos_proto::Envelope<evos_proto::Response> = evos_proto::decode_body(&body).unwrap();
    assert!(matches!(
        resp.body,
        evos_proto::Response::Error {
            code: ErrorCode::BadRequest,
            ..
        }
    ));
}

fn search(client: &mut Client, query: &str, sort: Sort) -> (SearchResult, Vec<evos_proto::Row>) {
    let found = client
        .search(query, SearchOptions::default(), sort)
        .unwrap();
    let rows = client.fetch(found.id, 0, 100).unwrap();
    (found, rows)
}

#[test]
fn everything_columns_history_and_statistics() {
    let Some(fx) = fixture() else {
        eprintln!("skipping: cannot mount tmpfs here");
        return;
    };
    let mut client = Client::connect(&fx.socket, "test").unwrap();
    wait_indexed(&mut client, "report", 3);
    let disk = fx.disk.as_os_str().as_bytes().to_vec();

    // Columns: type, attributes, dates; folders carry their total size.
    let (found, rows) = search(&mut client, "q1", Sort::default());
    assert_eq!(found.total, 1);
    assert_eq!(found.highlight, ["q1"]);
    let q1 = &rows[0];
    assert_eq!(q1.type_name, "PDF Document");
    assert_eq!(q1.attributes, "A");
    assert!(q1.mtime > 0 && q1.accessed > 0 && q1.changed > 0);
    let (_, rows) = search(&mut client, "folder: reports", Sort::default());
    assert_eq!(rows[0].size, Some(4000), "q1 + q2");
    assert_eq!(rows[0].type_name, "Folder");
    assert_eq!(rows[0].attributes, "D");
    let (found, _) = search(&mut client, "folder: size:>3kb", Sort::default());
    assert_eq!(found.total, 2, "reports and docs, by folder size");

    // count: keeps the first results in sort order; root: lists the volume.
    let by_size = Sort {
        key: SortKey::Size,
        descending: true,
    };
    let (found, rows) = search(&mut client, "ext:pdf;jpg count:2", by_size);
    assert_eq!(found.total, 2);
    assert_eq!(rows[0].name, b"q1 report.pdf");
    let (found, rows) = search(&mut client, "root:", Sort::default());
    assert_eq!(found.total, 1);
    assert_eq!(rows[0].path(), disk);
    assert!(rows[0].folder.is_empty() && rows[0].is_dir);

    // Hebrew names and content, with or without niqqud in the query.
    let (found, rows) = search(&mut client, "מִסְמָךְ", Sort::default());
    assert_eq!(found.total, 1, "niqqud in the query is ignored");
    assert_eq!(rows[0].name, "מסמך חשוב.txt".as_bytes());
    assert_eq!(rows[0].type_name, "Text Document");
    let note = fx.disk.join("photos/מסמך חשוב.txt");
    // Searching content must not even change the access time.
    assert!(Command::new("touch")
        .args(["-a", "-d", "2001-01-01"])
        .arg(&note)
        .status()
        .unwrap()
        .success());
    let atime = std::fs::metadata(&note).unwrap().accessed().unwrap();
    let (found, _) = search(&mut client, "content:שלום", Sort::default());
    assert_eq!(found.total, 1);
    let (found, _) = search(&mut client, "content:goodbye", Sort::default());
    assert_eq!(found.total, 0);
    assert_eq!(
        std::fs::metadata(&note).unwrap().accessed().unwrap(),
        atime,
        "content search changed the access time"
    );

    // Run history: counted per user, shown in rows, searchable, sortable.
    let cat = [disk.as_slice(), b"/photos/cat.jpg"].concat();
    assert_eq!(client.run_count(&cat, RunChange::Opened).unwrap().count, 1);
    let info = client.run_count(&cat, RunChange::Opened).unwrap();
    assert_eq!(info.count, 2);
    assert!(info.last > 0);
    assert_eq!(client.run_count(&cat, RunChange::Get).unwrap(), info);
    let (found, rows) = search(&mut client, "runcount:2", Sort::default());
    assert_eq!(found.total, 1);
    assert_eq!((rows[0].run_count, rows[0].date_run), (2, info.last));
    let (found, _) = search(&mut client, "dr:today", Sort::default());
    assert_eq!(found.total, 1);
    let by_runs = Sort {
        key: SortKey::RunCount,
        descending: true,
    };
    let (_, rows) = search(&mut client, "file:", by_runs);
    assert_eq!(rows[0].name, b"cat.jpg");
    client.run_count(&cat, RunChange::Set(0)).unwrap();
    let (found, _) = search(&mut client, "runcount:>0", Sort::default());
    assert_eq!(found.total, 0);

    // Statistics and saving.
    let stats = client.statistics().unwrap();
    assert_eq!(stats.len(), 1);
    let s = &stats[0];
    assert_eq!((s.files, s.folders), (4, 3));
    assert_eq!(s.total_size, 6000 + "שלום עולם".len() as u64);
    assert!(s.index_bytes > 0);
    client.expect_ok(Request::Save).unwrap();
    assert!(fx.daemon.cfg.state_dir.join("run-history").exists());
    let snapshots = std::fs::read_dir(&fx.daemon.cfg.state_dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().extension() == Some("idx".as_ref()))
        .count();
    assert_eq!(snapshots, 1);
}
