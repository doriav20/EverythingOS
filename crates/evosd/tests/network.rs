//! The network servers (ETP/FTP, HTTP, index server), remote indexes and
//! searching an ETP server from a session, all against one daemon over a
//! tmpfs volume: the daemon's remote indexes copy its own servers.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use common::{fixture, wait_indexed, Fixture};
use evos_etp::{EtpClient, EtpError, Query};
use evos_proto::{
    Client, IndexUser, RemoteIndexSpec, RemoteKind, RemoteSearch, Request, Row, SearchOptions,
    Sort, SortKey,
};

const T: Duration = Duration::from_secs(10);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn disk(fx: &Fixture) -> String {
    fx.disk.to_str().unwrap().to_owned()
}

/// Adds a folder only root may enter, with a "report" inside: remote users
/// (who act as `nobody`) must never see it.
fn add_secret(fx: &Fixture) {
    let secret = fx.disk.join("secret");
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("hidden report.txt"), "private").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn search(client: &mut Client, query: &str) -> (u64, Vec<Row>) {
    let r = client
        .search(query, SearchOptions::default(), Sort::default())
        .unwrap();
    let rows = client.fetch(r.id, 0, 100).unwrap();
    (r.total, rows)
}

fn names(rows: &[Row]) -> Vec<String> {
    let mut n: Vec<String> = rows
        .iter()
        .map(|r| String::from_utf8_lossy(&r.name).into_owned())
        .collect();
    n.sort();
    n
}

/// Reads one FTP reply: its code and text.
fn ftp_reply(r: &mut impl BufRead) -> (u16, String) {
    let mut text = String::new();
    loop {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        text.push_str(&line);
        if line.len() >= 4 && line.as_bytes()[3] == b' ' {
            return (line[..3].parse().unwrap(), text);
        }
    }
}

/// Sends one HTTP request and returns the status line and the body.
fn http(port: u16, path: &str, headers: &str) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(T)).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{headers}\r\n"
    )
    .unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("headers end");
    let head = String::from_utf8_lossy(&out[..split]).into_owned();
    (
        head.lines().next().unwrap_or("").to_owned(),
        out[split + 4..].to_vec(),
    )
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn enable_etp(client: &mut Client) -> u16 {
    let port = free_port();
    let mut s = client.servers().unwrap();
    s.etp.enabled = true;
    s.etp.bind = vec!["127.0.0.1".into()];
    s.etp.port = port;
    s.etp.username = "remote".into();
    s.etp.password = Some("secret".into());
    client.set_servers(s).unwrap();
    port
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn etp_server_searches_and_downloads_read_only() {
    let Some(fx) = fixture() else {
        eprintln!("skipping: cannot mount tmpfs here");
        return;
    };
    add_secret(&fx);
    let mut client = Client::connect(&fx.socket, "test").unwrap();
    wait_indexed(&mut client, "report", 4);
    let port = enable_etp(&mut client);

    let public = client.servers().unwrap();
    assert!(public.etp.enabled);
    assert!(public.etp.password.is_none(), "password hashes stay inside");
    assert!(public.etp.password_set);

    assert!(matches!(
        EtpClient::connect("127.0.0.1", port, "remote", "wrong", T),
        Err(EtpError::Login(_))
    ));
    let mut etp = EtpClient::connect("127.0.0.1", port, "remote", "secret", T).unwrap();
    assert!(etp.welcome.contains("Welcome to Everything ETP/FTP"));
    let reply = etp
        .query(&Query {
            search: "report".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reply.total, 3, "the 0700 folder is hidden from nobody");
    let q1 = reply
        .items
        .iter()
        .find(|i| i.name == "q1 report.pdf")
        .expect("q1 report.pdf");
    assert_eq!(q1.size, Some(3000));
    assert_eq!(
        q1.path.as_deref(),
        Some(format!("{}/docs/reports", disk(&fx)).as_str())
    );
    assert!(reply
        .items
        .iter()
        .any(|i| i.name == "reports" && i.is_folder));

    // Windows of a long list, and the options Everything's client sends.
    let page = etp
        .query(&Query {
            search: "report".into(),
            sort: "NAME_DESCENDING".into(),
            offset: 1,
            count: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!((page.total, page.items.len()), (3, 1));
    assert_eq!(page.items[0].name, "q2 report.pdf");
    let cased = etp
        .query(&Query {
            search: "REPORT".into(),
            match_case: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(cased.total, 0);

    // Hebrew names and content.
    let he = etp
        .query(&Query {
            search: "חשוב".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(he.total, 1);
    assert_eq!(he.items[0].name, "מסמך חשוב.txt");
    let text = etp
        .read_file(&format!("{}/photos/מסמך חשוב.txt", disk(&fx)), 0, 1000)
        .unwrap();
    assert_eq!(String::from_utf8(text).unwrap(), "שלום עולם");

    let pdf = format!("{}/docs/reports/q1 report.pdf", disk(&fx));
    assert_eq!(etp.read_file(&pdf, 0, 1 << 20).unwrap().len(), 3000);
    assert_eq!(
        etp.read_file(&pdf, 2990, 1 << 20).unwrap().len(),
        10,
        "REST"
    );
    assert!(etp
        .read_file(&format!("{}/secret/hidden report.txt", disk(&fx)), 0, 100)
        .is_err());
    etp.quit();

    // Plain FTP: browsing works, nothing can be changed.
    let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(T)).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut w = s;
    assert_eq!(ftp_reply(&mut r).0, 220);
    let mut cmd = |line: &str| {
        write!(w, "{line}\r\n").unwrap();
        ftp_reply(&mut r)
    };
    assert_eq!(cmd("USER remote").0, 331);
    assert_eq!(cmd("PASS secret").0, 230);
    assert_eq!(cmd("SYST").0, 215);
    assert!(cmd("FEAT").1.contains("EVERYTHING"));
    assert_eq!(cmd(&format!("CWD {}/docs", disk(&fx))).0, 250);
    assert!(cmd("PWD").1.contains("/docs"));
    assert_eq!(
        cmd("SIZE reports/q1 report.pdf"),
        (213, "213 3000\r\n".into())
    );
    for write in [
        "DELE reports/q1 report.pdf",
        "RNFR reports/q1 report.pdf",
        "MKD new",
        "RMD reports",
        "STOR x.txt",
        "APPE reports/q1 report.pdf",
        "SITE CHMOD 777 reports",
    ] {
        let (code, text) = cmd(write);
        assert_eq!(code / 100, 5, "{write}: {text}");
    }
    // Active mode only connects back to the client itself.
    assert_eq!(cmd("PORT 10,1,2,3,4,5").0 / 100, 5);
    assert_eq!(cmd("QUIT").0, 221);
    assert!(fx.disk.join("docs/reports/q1 report.pdf").exists());
    assert!(!fx.disk.join("docs/new").exists());

    // Turning the server off closes the port.
    let mut s = client.servers().unwrap();
    s.etp.enabled = false;
    client.set_servers(s).unwrap();
    wait_for("the port to close", || {
        EtpClient::connect(
            "127.0.0.1",
            port,
            "remote",
            "secret",
            Duration::from_secs(1),
        )
        .is_err()
    });
}

#[test]
fn http_server_pages_json_and_downloads() {
    let Some(fx) = fixture() else {
        eprintln!("skipping: cannot mount tmpfs here");
        return;
    };
    add_secret(&fx);
    let mut client = Client::connect(&fx.socket, "test").unwrap();
    wait_indexed(&mut client, "report", 4);
    let port = free_port();
    let mut s = client.servers().unwrap();
    s.http.enabled = true;
    s.http.bind = vec!["127.0.0.1".into()];
    s.http.port = port;
    s.http.username = "web".into();
    s.http.password = Some("pw".into());
    client.set_servers(s).unwrap();
    // base64("web:pw")
    let auth = "Authorization: Basic d2ViOnB3\r\n";

    let (status, _) = http(port, "/?search=report", "");
    assert!(status.contains("401"), "{status}");
    let (status, _) = http(port, "/?search=report", "Authorization: Basic d2ViOnd4\r\n");
    assert!(status.contains("401"), "{status}");

    let (status, body) = http(
        port,
        "/?search=report&json=1&path_column=1&size_column=1",
        auth,
    );
    assert!(status.contains("200"), "{status}");
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("\"totalResults\":3"), "{body}");
    assert!(body.contains("\"name\":\"q1 report.pdf\""), "{body}");
    assert!(
        body.contains("\"size\":\"3000\"") || body.contains("\"size\":3000"),
        "{body}"
    );
    assert!(!body.contains("hidden"), "{body}");

    let (_, body) = http(port, &format!("/?s={}&j=1", url_escape("חשוב")), auth);
    assert!(String::from_utf8(body).unwrap().contains("מסמך חשוב.txt"));

    let (status, body) = http(port, "/?search=report", auth);
    assert!(status.contains("200"));
    let page = String::from_utf8(body).unwrap();
    assert!(
        page.contains("<html") || page.contains("<!DOCTYPE"),
        "{page}"
    );
    assert!(page.contains("q1 <b>report</b>.pdf") || page.contains("q1 report.pdf"));

    let pdf = url_escape(&format!("{}/docs/reports/q1 report.pdf", disk(&fx)));
    let (status, body) = http(port, &pdf, auth);
    assert!(status.contains("200"), "{status}");
    assert_eq!(body.len(), 3000);
    let (status, body) = http(port, &pdf, &format!("{auth}Range: bytes=0-9\r\n"));
    assert!(status.contains("206"), "{status}");
    assert_eq!(body.len(), 10);
    let (status, body) = http(port, &url_escape(&format!("{}/docs", disk(&fx))), auth);
    assert!(status.contains("200"));
    assert!(String::from_utf8(body).unwrap().contains("reports"));
    let secret = url_escape(&format!("{}/secret/hidden report.txt", disk(&fx)));
    let (status, _) = http(port, &secret, auth);
    assert!(!status.contains("200"), "{status}");
}

#[test]
fn remote_indexes_copy_index_and_etp_servers() {
    let Some(fx) = fixture() else {
        eprintln!("skipping: cannot mount tmpfs here");
        return;
    };
    add_secret(&fx);
    let mut client = Client::connect(&fx.socket, "test").unwrap();
    wait_indexed(&mut client, "report", 4);
    let etp_port = enable_etp(&mut client);
    let index_port = free_port();
    let mut s = client.servers().unwrap();
    s.index.enabled = true;
    s.index.bind = vec!["127.0.0.1".into()];
    s.index.port = index_port;
    s.index.users = vec![IndexUser {
        name: "laptop".into(),
        password: Some("pw".into()),
        ..Default::default()
    }];
    client.set_servers(s).unwrap();

    // The index server's own protocol.
    let mut ix = Client::connect_tcp("127.0.0.1", index_port, "test", T).unwrap();
    assert!(ix.index_volumes().is_err(), "log in first");
    assert!(ix.index_login("laptop", "nope").is_err());
    ix.index_login("laptop", "pw").unwrap();
    let vols = ix.index_volumes().unwrap();
    assert_eq!(vols.len(), 1);
    assert_eq!(vols[0].root, fx.disk.to_str().unwrap().as_bytes());
    let stale = ix
        .index_data(Request::IndexDelta {
            volume: vols[0].id,
            epoch: vols[0].epoch.wrapping_add(1),
            len: 0,
            since: 0,
        })
        .unwrap();
    assert!(stale.is_none(), "a copy of another epoch starts over");
    drop(ix);

    // A copy of the index server, kept current.
    client
        .remote_index_add(
            RemoteIndexSpec {
                name: "office".into(),
                host: "127.0.0.1".into(),
                port: Some(index_port),
                username: "laptop".into(),
                refresh_secs: Some(1),
                ..Default::default()
            },
            Some("pw".into()),
        )
        .unwrap();
    // Local: 2 files, the folder and the secret one; the copy: 3.
    wait_indexed(&mut client, "report", 7);
    let (_, rows) = search(&mut client, "\"q1 report.pdf\"");
    let copy = rows.iter().find(|r| r.remote).expect("the copy's row");
    let copied = format!("/net/office{}/docs/reports/q1 report.pdf", disk(&fx));
    assert_eq!(String::from_utf8_lossy(&copy.path()), copied);
    assert_eq!(copy.size, Some(3000));
    assert!(rows.iter().any(|r| !r.remote), "and the local one");
    assert_eq!(
        client
            .read_file(copied.as_bytes(), 0, 1 << 20)
            .unwrap()
            .len(),
        3000
    );
    assert_eq!(
        client
            .read_file(copied.as_bytes(), 2000, 100)
            .unwrap()
            .len(),
        100
    );

    std::fs::write(fx.disk.join("photos/new report.txt"), "new").unwrap();
    wait_indexed(&mut client, "report", 9);
    std::fs::remove_file(fx.disk.join("photos/new report.txt")).unwrap();
    wait_indexed(&mut client, "report", 7);

    let infos = client.remote_indexes().unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].phase, "live");
    assert!(infos[0].entries >= 6);
    let status = client.status().unwrap();
    assert!(status
        .volumes
        .iter()
        .any(|v| v.watch == "remote" && v.root.starts_with(b"/net/office")));

    // The server goes away: the copy stays searchable, shown offline, and
    // catches up when the server is back.
    let mut s = client.servers().unwrap();
    s.index.enabled = false;
    client.set_servers(s.clone()).unwrap();
    wait_for("the copy to go offline", || {
        client.remote_indexes().unwrap()[0].phase == "offline"
    });
    let (total, rows) = search(&mut client, "\"q1 report.pdf\"");
    assert_eq!(total, 2);
    assert!(rows.iter().any(|r| r.remote && r.offline));
    std::fs::write(fx.disk.join("photos/later report.txt"), "later").unwrap();
    s.index.enabled = true;
    client.set_servers(s).unwrap();
    wait_indexed(&mut client, "report", 9);
    assert_eq!(client.remote_indexes().unwrap()[0].phase, "live");
    std::fs::remove_file(fx.disk.join("photos/later report.txt")).unwrap();
    wait_indexed(&mut client, "report", 7);

    // A copy of an ETP server (as of Everything on Windows), refreshed by
    // copying the whole list.
    client
        .remote_index_add(
            RemoteIndexSpec {
                name: "winpc".into(),
                kind: RemoteKind::Etp,
                host: "127.0.0.1".into(),
                port: Some(etp_port),
                username: "remote".into(),
                refresh_secs: Some(1),
                ..Default::default()
            },
            Some("secret".into()),
        )
        .unwrap();
    wait_indexed(&mut client, "report", 10);
    let etp_copy = format!("/net/winpc{}/photos/מסמך חשוב.txt", disk(&fx));
    let (_, rows) = search(&mut client, "חשוב");
    assert!(rows
        .iter()
        .any(|r| r.remote && String::from_utf8_lossy(&r.path()) == etp_copy));
    assert_eq!(
        client.read_file(etp_copy.as_bytes(), 0, 100).unwrap(),
        "שלום עולם".as_bytes()
    );

    client.remote_index_remove("office").unwrap();
    client.remote_index_remove("winpc").unwrap();
    wait_indexed(&mut client, "report", 4);
    assert!(client.remote_indexes().unwrap().is_empty());
}

#[test]
fn sessions_search_an_etp_server_directly() {
    let Some(fx) = fixture() else {
        eprintln!("skipping: cannot mount tmpfs here");
        return;
    };
    add_secret(&fx);
    let mut client = Client::connect(&fx.socket, "test").unwrap();
    wait_indexed(&mut client, "report", 4);
    let port = enable_etp(&mut client);

    let mut gui = Client::connect(&fx.socket, "test").unwrap();
    let bad = RemoteSearch {
        host: "127.0.0.1".into(),
        port,
        username: "remote".into(),
        password: "wrong".into(),
    };
    assert!(gui.connect_remote(bad).is_err());
    let welcome = gui
        .connect_remote(RemoteSearch::parse(&format!("remote:secret@127.0.0.1:{port}")).unwrap())
        .unwrap();
    assert!(welcome.contains("Everything"));

    let (total, rows) = search(&mut gui, "report");
    assert_eq!(total, 3, "as the server's account sees it");
    assert!(rows.iter().all(|r| r.remote));
    assert_eq!(names(&rows), ["q1 report.pdf", "q2 report.pdf", "reports"]);
    let r = gui
        .search("report", SearchOptions::default(), Sort::default())
        .unwrap();
    gui.sort(
        r.id,
        Sort {
            key: SortKey::Name,
            descending: true,
        },
    )
    .unwrap();
    let sorted = gui.fetch(r.id, 0, 10).unwrap();
    assert_eq!(sorted[0].name, b"reports");
    let tail = gui.fetch(r.id, 2, 10).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].name, b"q1 report.pdf");

    let q1 = rows.iter().find(|r| r.name == b"q1 report.pdf").unwrap();
    assert_eq!(gui.read_file(&q1.path(), 0, 1 << 20).unwrap().len(), 3000);

    gui.disconnect_remote().unwrap();
    assert!(gui.fetch(r.id, 0, 10).is_err(), "remote results are gone");
    let (total, rows) = search(&mut gui, "report");
    assert_eq!(total, 4);
    assert!(rows.iter().all(|r| !r.remote));
}
