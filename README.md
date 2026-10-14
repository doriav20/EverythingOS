# EverythingOS

Instant file search for Linux, in the spirit of [Everything](https://www.voidtools.com/) by voidtools — including the network shares you mount.

- **Instant.** All file names live in a compact in-memory index; a substring search over a million entries takes about 5 ms.
- **Always current.** Local disks are followed live through fanotify filesystem-wide watches (the Linux counterpart of the NTFS journal Everything uses), with an inotify fallback.
- **Network shares, done properly.** EverythingOS mounts SMB, NFS and SSHFS shares (and bind-mounts folders) for you, then keeps even huge shares fresh without re-crawling them: SMB change notifications, adaptive parallel scanning and per-folder hot/cold scheduling.
- **Everything's search syntax**, a fast GTK4 desktop window and an `es`-style command line.

| Component  | Role                                                       |
|------------|------------------------------------------------------------|
| `evosd`    | System service: indexes, watches, mounts, answers queries  |
| `evos`     | Command-line search and administration                     |
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

## Searching

```bash
evos report                      # names containing "report"
evos "*.rs" !test                # wildcards match the whole name; ! negates
evos ext:jpg;png size:>2mb dm:lastweek
evos "src/main"                  # a "/" matches against the full path
evos --sort size --desc --size -H -n 20 file:
evos --json pic: holiday         # JSON lines for scripts
```

`evos` exits with 1 when nothing matched, like grep. In the desktop window the same syntax applies; results update as you type and as files change. Double-click or Enter opens a file, Ctrl+Enter opens its folder, Ctrl+Shift+C copies paths, and rows can be dragged into other applications. Bind `evos-gtk --toggle` to a keyboard shortcut in your desktop settings to summon the window like Everything's hotkey.

### Query syntax

| Syntax | Meaning |
|---|---|
| `foo bar` | both terms (AND) |
| `foo\|bar` | either term (OR binds tighter than AND) |
| `!foo` | not foo |
| `<foo bar>\|baz` | grouping |
| `"two words"` | quotes protect spaces and operators |
| `*.txt`, `IMG_????.jpg` | wildcards; the whole name must match |
| `case:` `ww:` `path:` `nopath:` `regex:` | match case, whole words, full path, name only, regular expression |
| `file:` `folder:` | only files / only folders (combine: `file:report`) |
| `ext:jpg;png` | extensions |
| `size:>10mb`, `size:1mb..5mb`, `size:huge` | size (binary units; `empty tiny small medium large huge gigantic`) |
| `dm:today`, `dm:lastweek`, `dm:last3days`, `dm:2024-05`, `dm:>=2023` | date modified |
| `parent:/home/me` | direct children of a folder |
| `under:/srv/projects` | anything below a folder |
| `depth:2`, `len:>40` | path depth, name length |
| `empty:` | empty folders |
| `audio:` `video:` `pic:` `doc:` `zip:` `exe:` | type macros |

The Search menu toggles (and `-i`, `-w`, `-p`, `-r` on the command line) set defaults for case, whole words, path matching and regular expressions.

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

## Permissions

`evosd` runs as root so it can index everything and use fanotify, but every user only gets results they could list themselves: an entry is returned only if each folder above it grants search permission and its folder grants read permission to the caller (mode bits; POSIX ACLs are not evaluated). Set `single_user = true` on single-user machines to skip the check.

## Configuration

`/etc/evos/evosd.toml` documents every setting with its default — exclusions, tmpfs indexing, fanotify, network concurrency, ops/sec caps, recheck intervals, SMB notifications and more. Per-mount settings (`--max-concurrency`, `--max-ops`, `--exclude`, `--recheck`, `--staleness`, `--no-smb-notify`) override them. Snapshots of every index are kept in `/var/lib/evos` so restarts are instant.

`evos status` shows each volume's state, watch mode and, for network shares, the current concurrency, throughput and number of scheduled folders.

## Development

Everything builds and runs in Docker, so it works from any host:

| Command | Purpose |
|---|---|
| `make check` | rustfmt, clippy (`-D warnings`) and all tests |
| `make test`, `make build`, `make shell` | the usual |
| `make e2e` | SMB/NFS end-to-end suite against Samba and nfs-ganesha containers (`E2E_FILES=1000000 make e2e` for a big share) |
| `make package`, `make deb`, `make rpm`, `make arch`, `make tarball` | packages into `dist/` |
| `make run`, `make stop` | containerized daemon plus browser GUI |
| `docker compose run --rm dev cargo bench -p evos-core` | search, sort and snapshot benchmarks |

Workspace layout:

```
crates/evos-core    index store, query language, parallel search, sorting, permissions, snapshots
crates/evos-scan    mount discovery, walker, sweeps, fanotify/inotify, SMB notify, AIMD gate, scheduler, volume drivers
crates/evos-mount   mount definitions, credentials, mount commands, supervision
crates/evos-proto   socket protocol and client library
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
- Sorting result sets of many millions of entries takes a noticeable fraction of a second.
- Wayland has no global hotkeys for applications; bind `evos-gtk --toggle` in your desktop's keyboard settings.
- Content search (`content:`), duplicate finding and indexing a file server remotely (federation) are not implemented yet.

## License

MIT
