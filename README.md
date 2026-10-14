# EverythingOS

Instant file search for Linux, inspired by [Everything](https://www.voidtools.com/) by voidtools.

- **Instant search** over every local filesystem, with a compact in-memory index and Everything-style query syntax.
- **Always up to date**: fanotify filesystem-wide watches (with an inotify fallback) keep the index live.
- **Network shares**: mounts SMB, NFS, SSHFS and bind directories itself, then keeps very large shares fresh with adaptive parallel scanning, SMB change notifications and directory-mtime-guided sweeps.

Components:

| Binary     | Role                                              |
|------------|---------------------------------------------------|
| `evosd`    | System daemon: indexer, query engine, mount manager |
| `evos`     | Command-line search client (`es`-style)           |
| `evos-gtk` | GTK4 desktop client                               |

> Work in progress.
