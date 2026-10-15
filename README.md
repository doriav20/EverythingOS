# EverythingOS

Instant file search for Linux, in the spirit of [Everything](https://www.voidtools.com/) by voidtools — including the network shares you mount.

- **Instant.** All file names live in a compact in-memory index; a substring search over a million entries takes about 5 ms.
- **Always current.** Local disks are followed live through fanotify filesystem-wide watches (the Linux counterpart of the NTFS journal Everything uses), with an inotify fallback.
- **Network shares, done properly.** EverythingOS mounts SMB, NFS and SSHFS shares (and folders) for you, from the window or the command line, then keeps even huge shares fresh without re-crawling them: SMB change notifications, adaptive parallel scanning and per-folder hot/cold scheduling.
- **Everything 1.5's search syntax**: operators, wildcards, every modifier and function from `ext:` and `size:` to `dupe:`, `content:`, `child:`, `width:` and `artist:`, with the same date and size forms. See [docs/everything-parity.md](docs/everything-parity.md).
- **Every language.** Names in Hebrew, Arabic, Greek, Cyrillic, Japanese or with emoji are searched with Unicode case folding, and diacritics are ignored by default as in Everything: `שלום` finds `שָׁלוֹם`, `resume` finds `Résumé`.
- **It only looks.** Indexing and searching never change a file — not even its access time.
- **A desktop window** with Everything's columns, highlighting, preview, bookmarks and command line, and an `es`-compatible command-line tool.

| Component  | Role                                                       |
|------------|------------------------------------------------------------|
| `evosd`    | System service: indexes, watches, mounts, answers queries  |
| `evos`     | Command-line search and administration (es-compatible)     |
| `evos-gtk` | Desktop search window (GTK4 / libadwaita)                  |

## Install

### Packages

Build the packages (in Docker, from any host) with `make package`; they land in `dist/`.

| Distribution                         | Install                                                                 |
|--------------------------------------|-------------------------------------------------------------------------|
| Ubuntu 24.04+, Debian 13+            | `sudo apt install ./dist/evos_*.deb ./dist/evos-gtk_*.deb`              |
| Fedora 40+, openSUSE                 | `sudo dnf install ./dist/evos-0*.rpm ./dist/evos-gtk-0*.rpm`            |
| Arch Linux                           | `sudo pacman -U dist/evos-*.pkg.tar.zst` (or `makepkg -si` in `packaging/arch`) |
| Any systemd distribution             | `tar xf dist/evos-*-linux.tar.gz && sudo ./evos-*/install.sh`           |

The Debian, RPM and tarball installs enable and start `evosd.service`; on Arch run `sudo systemctl enable --now evosd`. The `evos-gtk` package is optional on servers.

### Try it without installing (Docker)

On a Linux host:

```bash
make run
```

The daemon runs in a container but joins the host's mount namespace, so it indexes the host with real paths; the desktop client is served at <http://localhost:8080>. Search from the shell with `docker compose exec gui evos <query>`, and stop with `make stop`.

### From source

Rust 1.82+, GTK 4.14+ and libadwaita 1.5+ development files are needed (`libgtk-4-dev libadwaita-1-dev` on Debian/Ubuntu):

```bash
cargo build --release
sudo install -m755 target/release/{evosd,evos,evos-gtk} /usr/local/bin/
```

## The desktop window

Type to search; results update as you type and as files change, with the matched part of names and paths in bold. Click a column header to sort (again to reverse); right-click any header to choose columns: Name, Path, Size, Extension, Type, Date Modified, Date Created, Date Accessed, Attributes, Date Recently Changed, Run Count and Date Run.

| Key | Action |
|---|---|
| Enter, double-click | open (counts a run, as in Everything) |
| Ctrl+Enter | open the containing folder |
| Ctrl+C / Ctrl+Shift+C | copy the files / their full paths |
| Alt+Enter | properties: every date, attributes, run history |
| Alt+P | preview pane (images and text) |
| Ctrl+I, Ctrl+B, Ctrl+U, Ctrl+M, Ctrl+R | match case, whole words, path, diacritics, regex |
| Ctrl+D, Ctrl+Shift+B | add a bookmark, organize bookmarks |
| Ctrl+S | export the results (EFU, CSV, TSV, TXT, M3U) |
| Ctrl+N, Ctrl+W | new window, close window |
| Ctrl+F / F3, F5, Esc | search box, refresh, clear the search (twice: close) |

The main menu has every Search option (also match prefix/suffix and ignore punctuation/white space), the View options (preview, status bar, columns, sort), Recent Searches, Tools (statistics, rescan, save the index) and Preferences, where indexed volumes are listed and shares and folders are mounted. The right-click menu offers Open, Open With, Open Containing Folder, the copy commands and Properties. Results can be dragged into other applications. Settings, bookmarks and history are kept in `~/.config/evos/evos-gtk.ini`.

Everything's command line works too, and is passed to the running window:

```bash
evos-gtk -s "*.pdf" -sort size-descending   # search, sorted by size
evos-gtk -filter pictures -s holiday
evos-gtk -bookmark "Big videos"
evos-gtk -toggle-window                      # bind this to a hotkey
evos-gtk -help                               # every option
```

## Searching from the command line

`evos` follows Everything's `es.exe`, including its single-dash options:

```bash
evos report                                  # names containing "report"
evos "ext:jpg;png size:>2mb dm:lastweek"     # quote searches that use > | < !
evos '"two words"'                           # a phrase
evos -sort size-descending -size -dm -n 20 file:
evos /ad projects                            # DIR-style: folders only
evos -csv -name -path-column -size ext:pdf > pdfs.csv
evos -export-efu music.efu audio:            # an Everything file list
evos --filelist music.efu queen              # search a file list offline
evos --inc-run-count ~/notes.md; evos "runcount:>0"
evos --json pic: holiday                     # JSON lines for scripts
evos stats                                   # files, folders and sizes per volume
```

`evos --help` lists every option: columns (`-name`, `-path-column`, `-size`, `-dc`, `-dm`, `-da`, `-attributes`, `-run-count`, ...), formats (`-csv`, `-tsv`, `-efu`, `-m3u8`, `-json`, `-export-*`), size and date formats, highlighting, `-get-result-count`, `-get-total-size`, `-save-db` and `-reindex`. It exits with 1 when nothing matched, like grep.

### Search syntax

| Syntax | Meaning |
|---|---|
| `foo bar`, `foo\|bar`, `!foo`, `<foo bar>\|baz` | AND, OR, NOT, grouping |
| `"two words"`, `&sp:` `&vert:` ... | phrases; character entities |
| `*.txt`, `IMG_????.jpg`, `/src/*/main.rs`, `**/test/*` | wildcards match the whole name; in paths `*` stays in one folder, `**` crosses folders |
| `case:` `diacritics:` `ww:` `prefix:` `suffix:` `startwith:` `endwith:` `whole:` `path:` `regex:` `ignorepunc:` `ignorews:` | modifiers (and their `no` forms; `?mod:` applies one to the whole search) |
| `file:` `folder:` `ext:jpg;png` `type:"PDF Document"` `attrib:RH` | kind, extension, type description, attributes |
| `size:>10mb`, `size:1mb..5mb`, `size:huge` | size (folders too) |
| `dm:` `dc:` `da:` `rc:` `dr:` with `today`, `lastweek`, `last3days`, `2024-05`, `>=2023`, `mtd`, `unknown`... | modified, created, accessed, recently changed, run dates |
| `parent:` `under:` `root:` `depth:` `len:` `empty:` `child:` `child-count:` | structure |
| `dupe:` `sizedupe:` `stemdupe:` ... | duplicates |
| `content:text`, `regex:content:^TODO` | file content (UTF-8 and UTF-16) |
| `width:` `height:` `bitdepth:` `length:` `artist:` `album:` `title:` `genre:` `track:` `comment:` | image and media properties |
| `runcount:`, `count:100` | run history; result limit |
| `audio:` `video:` `pic:` `doc:` `zip:` `exe:` | type macros |

The full list, with every alias and value form, is in [docs/everything-parity.md](docs/everything-parity.md).

## Network shares

```bash
evos mount add nas --type smb --source //nas/media --user me   # prompts for the password
evos mount add backup --type nfs --source nas:/export/backup --max-ops 500
evos mount add laptop --type sshfs --source me@laptop:/home/me -o IdentityFile=/root/.ssh/id_ed25519
evos mount add projects --type bind --source /srv/projects
evos mount list
evos mount down nas && evos mount up nas
evos mount rm nas
```

The same is available in the desktop window under Preferences → Mounts, including a connection test. Mounts are kept in `/etc/evos/mounts.d/*.toml`; SMB passwords are stored root-only in `/etc/evos/credentials/` and never appear on command lines. Changing mounts requires root or a polkit authorization (an administrator password in desktop sessions). Shares mounted by other means are indexed too unless `index_unmanaged = false`.

### How big shares stay fresh

A network filesystem cannot be watched like a local disk, and re-reading millions of entries is far too slow. EverythingOS combines:

1. **Adaptive parallel scanning.** Scanning is limited by round trips, so many directories are listed in parallel. An AIMD controller (as in TCP) raises the concurrency while throughput grows and backs off when latency inflates or requests fail; an optional ops/sec cap protects busy servers. Interrupted first scans resume where they stopped.
2. **SMB change notifications.** On SMB mounts the server reports changed paths (CHANGE_NOTIFY through the kernel's `CIFS_IOC_NOTIFY_INFO`), and exactly those folders are re-listed — typically within a second.
3. **Directory-mtime sweeps.** Creating, deleting or renaming a file changes its folder's modification time, so a sweep only checks folders (about a tenth of all entries) and re-lists the changed ones, bypassing client attribute caches.
4. **Hot/cold scheduling.** Each folder's check interval doubles every time it is found unchanged and drops back to the minimum (30 s by default) when it changes, capped at the maximum staleness (24 h). Busy folders are checked often, archives rarely, and folders you open results in are refreshed first. A weekly re-list refreshes file sizes and dates.
5. **Offline handling.** Unreachable shares (and unmounted ones) go offline but stay searchable, then resume. Mounts use soft semantics so a dead server cannot hang the daemon.

Measured by the end-to-end suite (`make e2e`, 100,000 files per share, containers on one machine):

| Scenario | Result |
|---|---|
| First scan of an SMB share / NFS export | 5.3 s / 2.7 s |
| First scan with 5 ms added latency | 5.1 s (concurrency adapted to 18) |
| File created or deleted by another SMB client | found in 0.3 s |
| File created on the server's own disk (SMB / NFS) | found in 3.4 s / 4.7 s |
| Server becomes unreachable | offline after ~45 s, still searchable |
| Daemon restart | searchable again after 0.3 s |

## Searching never changes your files

EverythingOS only reads metadata and, when you ask for it, content:

- Folders are listed with `O_NOATIME` and entries are examined with `statx`, so indexing does not even update access times.
- `content:`, the property functions and the preview pane are served by the daemon, which reads with `O_NOATIME`, never follows symbolic links (a link cannot be used to read someone else's file), refuses FIFOs and devices, and only reads files the asking user could read themselves.
- The desktop window has no delete, rename or move: it lists files and opens them in the application you choose.

The end-to-end suite checks this on a filesystem mounted with `strictatime`, where any ordinary read would show: after scans, rescans, content and property searches, exports and previews, every access, modification and change time, the set of files and their content are unchanged.

## Permissions

`evosd` runs as root so it can index everything and use fanotify, but every user only gets results they could list themselves: an entry is returned only if each folder above it grants search permission and its folder grants read permission to the caller (mode bits; POSIX ACLs are not evaluated). Content searches and previews additionally need read permission on the file. Run history is kept per user. Set `single_user = true` on single-user machines to skip the check.

## Configuration

`/etc/evos/evosd.toml` documents every setting with its default — exclusions, tmpfs indexing, fanotify, network concurrency, ops/sec caps, recheck intervals, SMB notifications and more. Per-mount settings (`--max-concurrency`, `--max-ops`, `--exclude`, `--recheck`, `--staleness`, `--no-smb-notify`) override them. Snapshots of every index are kept in `/var/lib/evos` so restarts are instant.

`evos status` shows each volume's state, watch mode and, for network shares, the current concurrency, throughput and number of scheduled folders.

## Development

Everything builds and runs in Docker, so it works from any host:

| Command | Purpose |
|---|---|
| `make check` | rustfmt, clippy (`-D warnings`) and all unit and integration tests |
| `make test`, `make build`, `make shell` | the usual |
| `make e2e` | every end-to-end suite, including real Samba and nfs-ganesha servers (`E2E_FILES=1000000 make e2e` for a big share) |
| `make e2e-local` | the suites that need no servers: search features, read-only guarantees and the desktop client |
| `make ui-test` | the desktop client alone |
| `make package`, `make deb`, `make rpm`, `make arch`, `make tarball` | packages into `dist/` |
| `make run`, `make stop` | containerized daemon plus browser GUI |
| `docker compose run --rm dev cargo bench -p evos-core` | search, sort and snapshot benchmarks |

The end-to-end suites (`tests/e2e/run.sh [features] [readonly] [ui] [network]`):

- **features** — the search syntax through the real daemon and CLI; names in Hebrew, Arabic, Greek, Japanese and emoji; content and media properties; run history; exports and file lists; live updates; per-user filtering.
- **readonly** — nothing changes on a `strictatime` filesystem, not even access times.
- **ui** — the real `evos-gtk` window on Xvfb, driven like a user (typing, Hebrew input, shortcuts, clicks, right-click menu, clipboard, scrolling 5,000 results, dialogs, mounting a folder from Preferences, surviving a daemon restart) and read through the accessibility bus, so it checks what is actually on screen.
- **network** — SMB and NFS: scanning, change notifications, sweeps, latency, offline shares, restarts, names in other scripts and content over both, and an SMB share mounted with a login from the window's Preferences.

Workspace layout:

```
crates/evos-core    index store, query language, parallel search, sorting, permissions, snapshots, content/properties
crates/evos-scan    mount discovery, walker, sweeps, fanotify/inotify, SMB notify, AIMD gate, scheduler, volume drivers
crates/evos-mount   mount definitions, credentials, mount commands, supervision
crates/evos-proto   socket protocol, client library, file list writer
crates/evosd        the daemon
crates/evos-cli     `evos`
crates/evos-gtk     `evos-gtk`
packaging/          systemd unit, polkit policy, desktop entry, deb/rpm/arch/tarball packaging
docker/, tests/e2e  build, packaging, runtime and test containers
```

Benchmarks on 12 cores with 1M entries: substring search 5 ms, `*.ext` 3.5 ms, path substring 10 ms, regular expression 19 ms; sorting all results by name 150 ms; snapshot load 125 ms.

## Limitations

- Linux 5.9+ is needed for fanotify filesystem watches (5.17+ for single-event renames); other filesystems fall back to inotify or sweeps.
- POSIX ACLs are not considered when filtering results.
- Not implemented from Everything: the HTTP and ETP servers, thumbnail view, the folder and filter sidebars, result omission, tabs, user-defined filters, and opening file lists in the window (the CLI searches them). See [docs/everything-parity.md](docs/everything-parity.md).
- Wayland has no global hotkeys for applications; bind `evos-gtk -toggle-window` in your desktop's keyboard settings.

## License

MIT
