# EverythingOS and Everything 1.5

How EverythingOS covers [Everything](https://www.voidtools.com/) 1.5 (1.5.0.1423b), feature by feature. ✓ works as in Everything, ◐ partly or differently, ✗ not implemented.

Linux has no NTFS journal, drive letters or Windows properties, so some features map to their Linux counterparts; those are described in the notes.

## Search syntax

### Operators, wildcards and entities

| Everything | | Notes |
|---|---|---|
| space (AND), `\|` (OR), `!` (NOT), `< >` (grouping), `" "` (escape) | ✓ | |
| `*` (within a folder), `**` (across folders), `?` | ✓ | a term with a wildcard matches the whole name, or the whole path when it contains `/` |
| `&sp:` `&vert:` `&excl:` `&lt:` `&gt:` `&quot:` `&#n:` `&#xn:` | ✓ | |

### Macros

| Everything | | Notes |
|---|---|---|
| `audio:` `zip:` `doc:` `exe:` `image:` `video:` | ✓ | also `pic:`; `exe:` also matches files with an execute permission bit |

### Modifiers

All modifiers have `no` forms (`nocase:`) and can be made global with `?` (`?case:`), as in Everything.

| Everything | | Notes |
|---|---|---|
| `case:` `diacritics:` `ww:` `prefix:` `suffix:` | ✓ | diacritics are ignored by default; Hebrew niqqud and Arabic harakat count as diacritics |
| `startwith:` `endwith:` `whole:` | ✓ | |
| `path:` `file:` `folder:` `regex:` | ✓ | |
| `ignorepunc:` `ignorews:` | ✓ | |
| `len:` | ✓ | as a function (`len:>40`), in characters |
| `highlight:` `fromdisk:` `tonumber:` `tostring:` `binary:` `hex:` | ◐ | accepted; highlighting is always on, content and properties are always read from disk |
| `wildcards:` (`#`, `[ ]`, `[! ]` in content) | ◐ | accepted, without the extra wildcard forms |

### Functions

| Everything | | Notes |
|---|---|---|
| `ext:` `filelist:` `type:` `attrib:` | ✓ | type descriptions such as "PDF Document"; attributes R (no write bit), H (dot name), D, A (any file), L (symbolic link), N |
| `name:` `stem:` `location:` | ✓ | also `namepart:` |
| `size:` | ✓ | named sizes `empty` … `gigantic`; folders by the total size of their contents |
| `dm:` `dc:` `da:` `rc:` `dr:` | ✓ | date created is the birth time where the filesystem records it (`dc:unknown` otherwise); date recently changed is when the index saw the change |
| `parent:` `infolder:` `nosubfolders:` `root:` `depth:` `parents:` `empty:` | ✓ | also `under:` (anything below a folder) |
| `child:` `child-count:` `child-file-count:` `child-folder-count:` | ✓ | |
| `count:` | ✓ | applied after sorting |
| `dupe:` and `sizedupe:` `stemdupe:` `extdupe:` `dmdupe:` `dcdupe:` | ✓ | |
| `content:` | ✓ | read on demand (UTF-8, and UTF-16 with a byte order mark); `regex:content:` works; no content index is kept |
| `width:` `height:` `bitdepth:` `length:` `track:` `album:` `artist:` `title:` `genre:` `comment:` | ✓ | read on demand from image headers and audio/video tags, cached until the file changes |
| `run-count:` | ✓ | |
| `rating:` `tag:` | ✗ | Windows properties without a common Linux equivalent |
| `si:` | ✗ | Windows Search query syntax |

### Values

| Everything | | |
|---|---|---|
| `f:v` `f:<=v` `f:<v` `f:=v` `f:==v` `f:>v` `f:>=v` `f:!=v` `f:!v` | ✓ | |
| `f:a..b` `f:a-b` `f:<a b>` `f:<a\|b>` `f:a;b` | ✓ | |
| dates: `today` `yesterday` `this/last/next week/month/year`, `lastNdays`, `pastNweeks`, month and weekday names, `mtd` `ytd`, `unknown`, `YYYY`, `YYYY-MM`, `YYYYMMDD[Thhmm]`, `d/m/y` | ✓ | |
| durations: `[[[DD:]HH:]MM:]SS`, `1h30m`, `10mins`, ISO 8601 `PT1H`, `very-short` … `very-long` | ✓ | |

## Search options

| Everything | | |
|---|---|---|
| Match Case, Match Whole Words, Match Path, Match Diacritics, Match Prefix, Match Suffix, Ignore Punctuation, Ignore White Space, Enable Regex | ✓ | Search menu, keyboard shortcuts, `evos-gtk -case` etc., `evos -i -w -p -a -r --prefix ...` |
| Filters: Everything, Audio, Compressed, Document, Executable, Folder, Picture, Video | ✓ | |
| Organize filters (user-defined) | ✗ | bookmarks cover saved searches |

## Results window

| Everything | | Notes |
|---|---|---|
| Columns: Name, Path, Size, Extension, Type, Date Modified, Date Created, Date Accessed, Attributes, Date Recently Changed, Run Count, Date Run | ✓ | chosen from View → Columns or a header's right-click menu |
| Sort by any column, ascending or descending | ✓ | click headers, View → Sort By, `-sort` |
| Highlighted search terms | ✓ | folded like the search |
| Status bar with result and selection count | ✓ | plus indexing state |
| Preview pane | ✓ | images and text, read through the daemon |
| Open, Open With, Open Path (containing folder) | ✓ | |
| Copy, Copy Full Path, Copy Name, Copy Path | ✓ | Copy places the files on the clipboard |
| Drag results to other applications | ✓ | |
| Properties | ✓ | EverythingOS's own dialog: all dates, attributes, type, size, run history |
| File → Export (EFU, CSV, TXT, M3U, M3U8) | ✓ | also TSV |
| Bookmarks (add, organize, open) | ✓ | they keep search, filter, options and sort |
| Search history | ✓ | View → Recent Searches |
| Run history (run count, date run) | ✓ | per user |
| Statistics | ✓ | files, folders, total size and index memory per volume |
| New Window | ✓ | |
| Delete, Rename, Cut, Move | ✗ | deliberately: EverythingOS only lists files |
| Thumbnails view | ✗ | |
| Folder and filter sidebars | ✗ | |
| Result omission | ✗ | |
| Tabs | ✗ | |
| File list editor, opening file lists in the window | ✗ | `evos -export-efu` writes them, `evos --filelist` searches them |

## Indexing

| Everything | | Notes |
|---|---|---|
| NTFS volumes, kept current through the USN journal | ✓ | every local filesystem, kept current through fanotify filesystem marks (inotify where unavailable) |
| Folder indexes, rescanned on a schedule | ✓ | folders are bind-mounted and followed live |
| Network drives | ✓ | mounted on request (SMB, NFS, SSHFS); SMB change notifications, adaptive scanning and hot/cold sweeps instead of periodic full rescans |
| Index folder sizes | ✓ | always |
| Index file content and properties | ◐ | read on demand rather than kept in the database |
| Exclude folders | ✓ | `/etc/evos/evosd.toml` and per mount |
| File list indexes | ◐ | searched from the command line |
| Database saved across restarts | ✓ | snapshots in `/var/lib/evos`; `-save-db`, Tools → Save Index Now |
| Force rebuild / rescan | ✓ | `-reindex`, Tools → Rescan All Volumes |
| ETP and HTTP servers | ✗ | |
| Run as a service, results limited by user permissions | ✓ | `evosd`; results are filtered by each user's permissions |

## Command lines

`evos-gtk` takes Everything.exe's options with one or two dashes, and passes them to the running window:
`-s` / `-search`, `-search*`, a bare search, `-filter`, `-sort` with `-ascending`/`-descending`, `-sort-ascending`, `-sort-descending`, `-case` `-diacritics` `-ww` `-regex` `-match-path` `-prefix` `-suffix` `-ignore-punctuation` `-ignore-white-space` and their `-no-` forms, `-p` / `-path`, `-parent`, `-bookmark`, `-columns`, `-add-columns`, `-remove-columns`, `-preview`, `-status-bar`, `-new-window`, `-toggle-window`, `-focus-results`, `-focus-search-edit`, `-maximize`, `-minimize`, `-fullscreen`, `-home`, `-close`, `-exit`, `-reindex`, `-rescan-all`, `-save-db`. Window placement (`-x`, `-y`, `-width`, `-height`), themes, sidebars, tabs, ETP and installation options are not supported.

`evos` follows es.exe: single-dash long options, `-r` `-i` `-w` `-p` `-a` and the other match options, `-o` `-n`, `-path` `-parent-path` `-parent`, DIR-style `/a[-]X` and `/o[-]X`, `-s`, `-sort NAME[-ascending|-descending]` and `-sort-NAME`, the column options in the order given, `-csv` `-tsv` `-efu` `-txt` `-m3u` `-m3u8` and their `-export-*` forms, `-no-header`, `-double-quote`, `-size-format`, `-date-format`, `-highlight`, `-get-result-count`, `-get-total-size`, `-get-run-count`, `-inc-run-count`, `-set-run-count`, `-save-db`, `-reindex`, `-get-everything-version`, `-create-file-list`. Not supported: `-instance`, `-timeout`, `-pause`, `-more`, colors and settings persistence.
