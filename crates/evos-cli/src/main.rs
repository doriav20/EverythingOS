//! `evos`: search the EverythingOS index from the command line.
//!
//! The options follow Everything's `es.exe`, including its single-dash
//! long options (`-sort size`, `-export-csv out.csv`) and DIR-style
//! switches (`/ad`, `/o-s`); see [`es`].

mod es;
mod filelist;
mod mount;
mod output;
mod servers;

use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use evos_proto::{Client, ClientError, Request, Row, RunChange, SearchOptions, Sort, SortKey};

use es::Column;
use output::{DateFormat, Format, SizeFormat, Style, Writer};

/// Instant file search for Linux.
///
/// With no subcommand, the arguments are a search in Everything syntax,
/// e.g. `evos ext:pdf report`, `evos "*.rs" !test`, `evos size:>1gb dm:today`.
/// Options may also be written es-style: `evos -sort size -dm report`.
#[derive(Parser)]
#[command(name = "evos", version, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Daemon socket.
    #[arg(long, env = "EVOS_SOCKET", default_value = evos_proto::DEFAULT_SOCKET, global = true)]
    socket: PathBuf,
    #[command(flatten)]
    search: SearchArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Search (the default when no subcommand is given).
    Search(Box<SearchArgs>),
    /// Show indexed volumes and their state.
    Status,
    /// File, folder and size totals per volume.
    Stats,
    /// Re-check a path's volume (or every volume) for changes.
    Rescan { path: Option<PathBuf> },
    /// Write every index to disk now.
    Save,
    /// Show, count or set how often a file was opened.
    RunCount {
        path: PathBuf,
        /// Count one more run.
        #[arg(long, conflicts_with = "set")]
        inc: bool,
        /// Set the run count.
        #[arg(long)]
        set: Option<u32>,
    },
    /// Manage mounted shares and folders.
    #[command(subcommand)]
    Mount(Box<mount::MountCommand>),
    /// Manage the ETP/FTP, HTTP and index servers.
    #[command(subcommand)]
    Server(Box<servers::ServerCommand>),
    /// Manage remote indexes: copies of other computers' indexes.
    #[command(subcommand)]
    Remote(Box<servers::RemoteCommand>),
}

#[derive(Copy, Clone, ValueEnum)]
enum FilterArg {
    Audio,
    Compressed,
    Document,
    Executable,
    Folder,
    Picture,
    Video,
}

#[derive(Args, Default)]
#[command(next_help_heading = "Search")]
struct SearchArgs {
    /// Search terms.
    query: Vec<String>,
    /// Treat the search as one regular expression.
    #[arg(short = 'r', long)]
    regex: bool,
    /// Match case.
    #[arg(short = 'i', long = "case", visible_alias = "match-case")]
    match_case: bool,
    /// Match whole words.
    #[arg(short = 'w', long = "whole-word", visible_aliases = ["ww", "whole-words"])]
    whole_word: bool,
    /// Match the full path and name.
    #[arg(short = 'p', long = "match-path")]
    match_path: bool,
    /// Match diacritical marks (é is not e).
    #[arg(short = 'a', long, visible_alias = "match-diacritics")]
    diacritics: bool,
    /// Match the start of words.
    #[arg(long)]
    prefix: bool,
    /// Match the end of words.
    #[arg(long)]
    suffix: bool,
    #[arg(long)]
    ignore_punctuation: bool,
    #[arg(long)]
    ignore_white_space: bool,

    /// Search in this folder and its subfolders.
    #[arg(long = "path", value_name = "PATH", help_heading = "Scope")]
    paths: Vec<PathBuf>,
    /// Search in the parent of this path and its subfolders.
    #[arg(long, value_name = "PATH", help_heading = "Scope")]
    parent_path: Vec<PathBuf>,
    /// Search only directly in this folder.
    #[arg(long, value_name = "PATH", help_heading = "Scope")]
    parent: Vec<PathBuf>,
    /// Limit results to a kind of file.
    #[arg(long, value_enum, help_heading = "Scope")]
    filter: Option<FilterArg>,
    /// Search an Everything file list (.efu) instead of the index.
    #[arg(long, value_name = "FILE", help_heading = "Scope")]
    filelist: Option<PathBuf>,
    /// Search an ETP server instead: Everything or EverythingOS on another
    /// computer, as [user[:password]@]host[:port].
    #[arg(
        long,
        value_name = "SERVER",
        help_heading = "Scope",
        conflicts_with = "filelist"
    )]
    etp: Option<String>,

    /// Skip this many results.
    #[arg(short = 'o', long, default_value_t = 0, help_heading = "Results")]
    offset: u64,
    /// Show at most this many results.
    #[arg(
        short = 'n',
        long = "max-results",
        visible_alias = "max",
        help_heading = "Results"
    )]
    max: Option<u64>,
    /// Sort by full path.
    #[arg(short = 's', help_heading = "Results")]
    sort_path: bool,
    /// Sort by name, path, size, extension, type, date-created,
    /// date-modified, date-accessed, attributes, date-recently-changed,
    /// run-count or date-run, optionally with -ascending or -descending.
    #[arg(long, value_name = "NAME", help_heading = "Results")]
    sort: Option<String>,
    #[arg(long, help_heading = "Results")]
    sort_ascending: bool,
    #[arg(long, visible_alias = "desc", help_heading = "Results")]
    sort_descending: bool,

    // Columns: printed in the order given (see es::translate).
    /// Show the name column.
    #[arg(long, help_heading = "Columns")]
    name: bool,
    /// Show the path (folder) column.
    #[arg(long, help_heading = "Columns")]
    path_column: bool,
    /// Show the full path and name column (the default).
    #[arg(long, visible_alias = "filename-column", help_heading = "Columns")]
    full_path_and_name: bool,
    #[arg(long, visible_alias = "ext", help_heading = "Columns")]
    extension: bool,
    #[arg(long, help_heading = "Columns")]
    size: bool,
    #[arg(long, visible_alias = "dc", help_heading = "Columns")]
    date_created: bool,
    #[arg(long, visible_alias = "dm", help_heading = "Columns")]
    date_modified: bool,
    #[arg(long, visible_alias = "da", help_heading = "Columns")]
    date_accessed: bool,
    #[arg(long, visible_aliases = ["attribs", "attrib"], help_heading = "Columns")]
    attributes: bool,
    #[arg(long, help_heading = "Columns")]
    run_count: bool,
    #[arg(long, help_heading = "Columns")]
    date_run: bool,
    #[arg(long, visible_alias = "rc", help_heading = "Columns")]
    date_recently_changed: bool,
    #[arg(long = "type", help_heading = "Columns")]
    type_column: bool,

    #[arg(long, group = "format", help_heading = "Output")]
    csv: bool,
    #[arg(long, group = "format", help_heading = "Output")]
    tsv: bool,
    /// Everything file list.
    #[arg(long, group = "format", help_heading = "Output")]
    efu: bool,
    #[arg(long, group = "format", help_heading = "Output")]
    txt: bool,
    #[arg(long, group = "format", help_heading = "Output")]
    m3u: bool,
    #[arg(long, group = "format", help_heading = "Output")]
    m3u8: bool,
    /// JSON lines with every column.
    #[arg(long, group = "format", help_heading = "Output")]
    json: bool,
    #[arg(long, value_name = "FILE", group = "export", help_heading = "Output")]
    export_csv: Option<PathBuf>,
    #[arg(long, value_name = "FILE", group = "export", help_heading = "Output")]
    export_tsv: Option<PathBuf>,
    #[arg(long, value_name = "FILE", group = "export", help_heading = "Output")]
    export_efu: Option<PathBuf>,
    #[arg(long, value_name = "FILE", group = "export", help_heading = "Output")]
    export_txt: Option<PathBuf>,
    #[arg(long, value_name = "FILE", group = "export", help_heading = "Output")]
    export_m3u: Option<PathBuf>,
    #[arg(long, value_name = "FILE", group = "export", help_heading = "Output")]
    export_m3u8: Option<PathBuf>,
    #[arg(long, value_name = "FILE", group = "export", help_heading = "Output")]
    export_json: Option<PathBuf>,
    /// Leave out the header line of CSV, TSV and EFU output.
    #[arg(long, help_heading = "Output")]
    no_header: bool,
    /// Put names and paths in double quotes.
    #[arg(long, help_heading = "Output")]
    double_quote: bool,
    /// 0 automatic, 1 bytes, 2 KB, 3 MB.
    #[arg(long, value_name = "N", help_heading = "Output")]
    size_format: Option<u8>,
    /// 0 local, 1 ISO 8601, 2 FILETIME, 3 ISO 8601 UTC.
    #[arg(long, value_name = "N", help_heading = "Output")]
    date_format: Option<u8>,
    /// Human-readable sizes (-size-format 0).
    #[arg(short = 'H', long, help_heading = "Output")]
    human: bool,
    /// Highlight the search terms (on a terminal).
    #[arg(long, help_heading = "Output")]
    highlight: bool,
    /// Separate results with NUL instead of newline.
    #[arg(short = '0', long, help_heading = "Output")]
    null: bool,
    /// Only print the number of results.
    #[arg(long, visible_alias = "get-result-count", help_heading = "Output")]
    count: bool,
    /// Only print the total size of the files found.
    #[arg(long, help_heading = "Output")]
    get_total_size: bool,

    /// Print how often a file was opened.
    #[arg(long, value_name = "FILE", help_heading = "Actions")]
    get_run_count: Option<PathBuf>,
    /// Count one more run of a file.
    #[arg(long, value_name = "FILE", help_heading = "Actions")]
    inc_run_count: Option<PathBuf>,
    /// Set the run count of a file.
    #[arg(long, num_args = 2, value_names = ["FILE", "COUNT"], help_heading = "Actions")]
    set_run_count: Option<Vec<String>>,
    /// Write the index to disk.
    #[arg(long, help_heading = "Actions")]
    save_db: bool,
    /// Rescan every volume.
    #[arg(long, visible_alias = "rescan-all", help_heading = "Actions")]
    reindex: bool,
    /// Print the daemon's version.
    #[arg(long, help_heading = "Actions")]
    get_everything_version: bool,
    /// Write a file list of everything indexed under the given
    /// semicolon-separated folders.
    #[arg(long, num_args = 2, value_names = ["FILE", "PATHS"], help_heading = "Actions")]
    create_file_list: Option<Vec<String>>,
}

fn main() -> ExitCode {
    let translated = es::translate(std::env::args());
    let cli = Cli::parse_from(translated.args);
    let columns = translated.columns;
    let result = match cli.command {
        Some(Command::Search(args)) => run_search(&cli.socket, *args, columns),
        None => run_search(&cli.socket, cli.search, columns),
        Some(Command::Status) => status(&cli.socket),
        Some(Command::Stats) => stats(&cli.socket),
        Some(Command::Rescan { path }) => rescan(&cli.socket, path),
        Some(Command::Save) => simple(&cli.socket, Request::Save),
        Some(Command::RunCount { path, inc, set }) => {
            let change = match (inc, set) {
                (true, _) => RunChange::Opened,
                (_, Some(n)) => RunChange::Set(n),
                _ => RunChange::Get,
            };
            run_count(&cli.socket, &path, change, true)
        }
        Some(Command::Mount(cmd)) => mount::run(&cli.socket, *cmd),
        Some(Command::Server(cmd)) => servers::run_server(&cli.socket, *cmd),
        Some(Command::Remote(cmd)) => servers::run_remote(&cli.socket, *cmd),
    };
    match result {
        Ok(code) => code,
        Err(Failure::Client(e)) => {
            eprintln!("evos: {e}");
            ExitCode::from(2)
        }
        Err(Failure::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(Failure::Io(e)) => {
            eprintln!("evos: {e}");
            ExitCode::from(2)
        }
        Err(Failure::Usage(msg)) => {
            eprintln!("evos: {msg}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
pub enum Failure {
    Client(ClientError),
    Io(io::Error),
    Usage(String),
}

impl From<ClientError> for Failure {
    fn from(e: ClientError) -> Self {
        Failure::Client(e)
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure::Io(e)
    }
}

pub fn connect(socket: &PathBuf) -> Result<Client, Failure> {
    Ok(Client::connect(socket, "evos-cli")?)
}

fn absolute(path: &Path) -> Result<Vec<u8>, Failure> {
    Ok(std::path::absolute(path)?.as_os_str().as_bytes().to_vec())
}

/// A path as a quoted query value.
fn quoted(path: &[u8]) -> String {
    format!("\"{}\"", String::from_utf8_lossy(path))
}

impl SearchArgs {
    /// The search text: the terms, the scope options and the filter. The
    /// terms are joined as they are: on Linux a whole search is usually
    /// quoted to keep `>`, `|` and `!` from the shell, so `evos "ext:pdf
    /// size:>1mb"` is two terms; a phrase is written `evos '"a b"'`.
    fn query(&self) -> Result<String, Failure> {
        let mut parts: Vec<String> = self.query.clone();
        let mut scopes = Vec::new();
        for p in &self.paths {
            scopes.push(format!("under:{}", quoted(&absolute(p)?)));
        }
        for p in &self.parent_path {
            let abs = std::path::absolute(p)?;
            let parent = abs.parent().unwrap_or(&abs);
            scopes.push(format!("under:{}", quoted(parent.as_os_str().as_bytes())));
        }
        for p in &self.parent {
            scopes.push(format!("parent:{}", quoted(&absolute(p)?)));
        }
        match scopes.len() {
            0 => {}
            1 => parts.push(scopes.remove(0)),
            _ => parts.push(format!("<{}>", scopes.join("|"))),
        }
        if let Some(f) = self.filter {
            parts.push(
                match f {
                    FilterArg::Audio => "audio:",
                    FilterArg::Compressed => "zip:",
                    FilterArg::Document => "doc:",
                    FilterArg::Executable => "exe:",
                    FilterArg::Folder => "folder:",
                    FilterArg::Picture => "pic:",
                    FilterArg::Video => "video:",
                }
                .into(),
            );
        }
        Ok(parts.join(" "))
    }

    fn options(&self) -> SearchOptions {
        SearchOptions {
            match_case: self.match_case,
            whole_word: self.whole_word,
            match_path: self.match_path,
            regex: self.regex,
            match_diacritics: self.diacritics,
            match_prefix: self.prefix,
            match_suffix: self.suffix,
            ignore_punctuation: self.ignore_punctuation,
            ignore_white_space: self.ignore_white_space,
        }
    }

    fn sort(&self) -> Result<Sort, Failure> {
        let (key, dir) = match (&self.sort, self.sort_path) {
            (Some(s), _) => {
                es::parse_sort(s).ok_or_else(|| Failure::Usage(format!("unknown sort {s:?}")))?
            }
            (None, true) => (SortKey::Path, None),
            (None, false) => (SortKey::Name, None),
        };
        let descending = dir.unwrap_or(self.sort_descending && !self.sort_ascending);
        Ok(Sort { key, descending })
    }

    /// The output format and, for exports, the file to write.
    fn format(&self) -> (Format, Option<&PathBuf>) {
        let exports = [
            (&self.export_csv, Format::Csv),
            (&self.export_tsv, Format::Tsv),
            (&self.export_efu, Format::Efu),
            (&self.export_txt, Format::Txt),
            (&self.export_m3u, Format::M3u),
            (&self.export_m3u8, Format::M3u8),
            (&self.export_json, Format::Json),
        ];
        if let Some((file, format)) = exports
            .into_iter()
            .find_map(|(f, fmt)| f.as_ref().map(|f| (f, fmt)))
        {
            return (format, Some(file));
        }
        let format = [
            (self.csv, Format::Csv),
            (self.tsv, Format::Tsv),
            (self.efu, Format::Efu),
            (self.m3u, Format::M3u),
            (self.m3u8, Format::M3u8),
            (self.json, Format::Json),
        ]
        .into_iter()
        .find_map(|(on, fmt)| on.then_some(fmt))
        .unwrap_or(Format::Txt);
        (format, None)
    }

    fn style(
        &self,
        format: Format,
        mut columns: Vec<Column>,
        terms: Vec<String>,
        to_terminal: bool,
    ) -> Style {
        let named = |c: &Column| matches!(c, Column::Name | Column::Path | Column::FullPath);
        if !columns.iter().any(named) {
            columns.push(Column::FullPath);
        }
        Style {
            format,
            columns,
            size_format: match (self.size_format, self.human) {
                (Some(0), _) | (None, true) => SizeFormat::Auto,
                (Some(2), _) => SizeFormat::Kb,
                (Some(3), _) => SizeFormat::Mb,
                _ => SizeFormat::Bytes,
            },
            date_format: match self.date_format {
                Some(1) => DateFormat::Iso,
                Some(2) => DateFormat::FileTime,
                Some(3) => DateFormat::IsoUtc,
                _ => DateFormat::Local,
            },
            header: !self.no_header,
            double_quote: self.double_quote,
            null: self.null,
            highlight: (self.highlight && to_terminal).then_some(terms),
        }
    }
}

/// Runs the action options (run counts, save, reindex, ...). Returns
/// `None` if none was given.
fn actions(socket: &PathBuf, args: &SearchArgs) -> Result<Option<ExitCode>, Failure> {
    if let Some(p) = &args.get_run_count {
        return run_count(socket, p, RunChange::Get, true).map(Some);
    }
    if let Some(p) = &args.inc_run_count {
        return run_count(socket, p, RunChange::Opened, false).map(Some);
    }
    if let Some(v) = &args.set_run_count {
        let count = v[1]
            .parse()
            .map_err(|_| Failure::Usage(format!("not a run count: {:?}", v[1])))?;
        return run_count(socket, Path::new(&v[0]), RunChange::Set(count), false).map(Some);
    }
    if args.save_db {
        return simple(socket, Request::Save).map(Some);
    }
    if args.reindex {
        return simple(socket, Request::Rescan { path: None }).map(Some);
    }
    if args.get_everything_version {
        let client = connect(socket)?;
        println!("{}", client.server);
        return Ok(Some(ExitCode::SUCCESS));
    }
    Ok(None)
}

/// Where rows come from: the daemon or a file list.
enum Source {
    Daemon { client: Client, result: u32 },
    List(std::vec::IntoIter<Row>),
}

impl Source {
    /// The next batch of rows from `offset`, at most `count`.
    fn next(&mut self, offset: u64, count: u64) -> Result<Vec<Row>, Failure> {
        match self {
            Source::Daemon { client, result } => {
                Ok(client.fetch(*result, offset as u32, count.min(10_000) as u32)?)
            }
            Source::List(rows) => Ok(rows.take(count.min(10_000) as usize).collect()),
        }
    }
}

fn run_search(
    socket: &PathBuf,
    mut args: SearchArgs,
    columns: Vec<Column>,
) -> Result<ExitCode, Failure> {
    if let Some(code) = actions(socket, &args)? {
        return Ok(code);
    }
    if let Some(v) = args.create_file_list.take() {
        let paths: Vec<PathBuf> = v[1]
            .split(';')
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect();
        args.paths.extend(paths);
        args.export_efu = Some(PathBuf::from(&v[0]));
    }
    let query = args.query()?;
    let sort = args.sort()?;
    let (format, export) = args.format();

    let (mut source, total, terms) = match &args.filelist {
        Some(file) => {
            let ix = filelist::load(file)?;
            let parse_opts = evos_core::query::ParseOptions {
                match_case: args.match_case,
                whole_word: args.whole_word,
                match_path: args.match_path,
                regex: args.regex,
                match_diacritics: args.diacritics,
                match_prefix: args.prefix,
                match_suffix: args.suffix,
                ignore_punctuation: args.ignore_punctuation,
                ignore_white_space: args.ignore_white_space,
                now: None,
            };
            let spec = evos_core::sort::SortSpec {
                key: core_sort_key(sort.key),
                descending: sort.descending,
            };
            let (rows, terms) =
                filelist::search(&ix, &query, &parse_opts, spec).map_err(Failure::Usage)?;
            let total = rows.len() as u64;
            // Skip the offset up front; the daemon does it per fetch.
            let rows: Vec<Row> = rows.into_iter().skip(args.offset as usize).collect();
            (Source::List(rows.into_iter()), total, terms)
        }
        None => {
            let mut client = connect(socket)?;
            if let Some(server) = &args.etp {
                let target = evos_proto::RemoteSearch::parse(server)
                    .ok_or_else(|| Failure::Usage(format!("not a server: {server:?}")))?;
                client.connect_remote(target)?;
            }
            let found = client.search(&query, args.options(), sort)?;
            (
                Source::Daemon {
                    client,
                    result: found.id,
                },
                found.total,
                found.highlight,
            )
        }
    };

    let end = args.max.map_or(total, |m| (args.offset + m).min(total));
    if args.count {
        println!("{}", end.saturating_sub(args.offset));
        return Ok(exit_for(total));
    }

    let to_terminal = export.is_none() && io::stdout().is_terminal();
    let style = args.style(format, columns, terms, to_terminal);
    let out: Box<dyn Write> = match export {
        Some(file) => Box::new(File::create(file)?),
        None => Box::new(io::stdout().lock()),
    };
    let mut writer = Writer::new(BufWriter::with_capacity(1 << 16, out), style);
    let mut total_size = 0u64;
    let mut offset = args.offset;
    while offset < end {
        let rows = source.next(offset, end - offset)?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            if args.get_total_size {
                if !row.is_dir {
                    total_size += row.size.unwrap_or(0);
                }
            } else {
                writer.row(row)?;
            }
        }
        offset += rows.len() as u64;
    }
    if args.get_total_size {
        println!("{total_size}");
    } else {
        writer.finish()?;
    }
    if let Source::Daemon { mut client, result } = source {
        let _ = client.release(result);
    }
    Ok(exit_for(total))
}

fn core_sort_key(key: SortKey) -> evos_core::sort::SortKey {
    use evos_core::sort::SortKey as K;
    match key {
        SortKey::Name => K::Name,
        SortKey::Path => K::Path,
        SortKey::Size => K::Size,
        SortKey::Modified => K::Modified,
        SortKey::Extension => K::Extension,
        SortKey::Type => K::Type,
        SortKey::Created => K::Created,
        SortKey::Accessed => K::Accessed,
        SortKey::Attributes => K::Attributes,
        SortKey::RecentlyChanged => K::RecentlyChanged,
        SortKey::RunCount => K::RunCount,
        SortKey::DateRun => K::DateRun,
    }
}

/// Like grep: 1 when nothing matched.
fn exit_for(total: u64) -> ExitCode {
    if total == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn simple(socket: &PathBuf, request: Request) -> Result<ExitCode, Failure> {
    connect(socket)?.expect_ok(request)?;
    Ok(ExitCode::SUCCESS)
}

fn run_count(
    socket: &PathBuf,
    path: &Path,
    change: RunChange,
    print: bool,
) -> Result<ExitCode, Failure> {
    let info = connect(socket)?.run_count(&absolute(path)?, change)?;
    if print {
        println!("{}", info.count);
    }
    Ok(ExitCode::SUCCESS)
}

fn status(socket: &PathBuf) -> Result<ExitCode, Failure> {
    let status = connect(socket)?.status()?;
    output::write_status(&mut io::stdout().lock(), &status)?;
    Ok(ExitCode::SUCCESS)
}

fn stats(socket: &PathBuf) -> Result<ExitCode, Failure> {
    let stats = connect(socket)?.statistics()?;
    output::write_statistics(&mut io::stdout().lock(), &stats)?;
    Ok(ExitCode::SUCCESS)
}

fn rescan(socket: &PathBuf, path: Option<PathBuf>) -> Result<ExitCode, Failure> {
    let path = path.map(|p| absolute(&p)).transpose()?;
    simple(socket, Request::Rescan { path })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> (SearchArgs, Vec<Column>) {
        let t = es::translate(
            std::iter::once("evos")
                .chain(args.iter().copied())
                .map(String::from),
        );
        (Cli::parse_from(t.args).search, t.columns)
    }

    #[test]
    fn es_style_command_lines() {
        let (a, cols) = parse(&["-sort", "size-descending", "-dm", "-size", "report", "/ad"]);
        assert_eq!(a.query().unwrap(), "report folder:");
        assert_eq!(
            a.sort().unwrap(),
            Sort {
                key: SortKey::Size,
                descending: true
            }
        );
        assert_eq!(cols, [Column::DateModified, Column::Size]);
        let style = a.style(Format::Txt, cols, vec![], false);
        assert_eq!(
            style.columns,
            [Column::DateModified, Column::Size, Column::FullPath],
            "the full path is added when no name column is asked for"
        );

        let (a, _) = parse(&["-i", "-ww", "-diacritics", "-prefix", "x"]);
        let o = a.options();
        assert!(o.match_case && o.whole_word && o.match_diacritics && o.match_prefix);
        assert!(!o.regex && !o.match_path);
    }

    #[test]
    fn quoted_searches_and_scopes() {
        let (a, _) = parse(&[
            "ext:pdf size:>1mb",
            "--parent",
            "/tmp/a b",
            "--filter",
            "document",
        ]);
        assert_eq!(
            a.query().unwrap(),
            "ext:pdf size:>1mb parent:\"/tmp/a b\" doc:"
        );
        let (a, _) = parse(&["--path", "/x", "--path", "/y", "z"]);
        assert_eq!(a.query().unwrap(), "z <under:\"/x\"|under:\"/y\">");
    }

    #[test]
    fn exports_and_formats() {
        let (a, _) = parse(&["-export-csv", "out.csv", "x"]);
        assert_eq!(a.format(), (Format::Csv, Some(&PathBuf::from("out.csv"))));
        let (a, _) = parse(&["-m3u8", "x"]);
        assert_eq!(a.format(), (Format::M3u8, None));
        let (a, _) = parse(&["-size-format", "2", "-date-format", "3", "x"]);
        let style = a.style(Format::Txt, vec![], vec![], false);
        assert_eq!(
            (style.size_format, style.date_format),
            (SizeFormat::Kb, DateFormat::IsoUtc)
        );
    }
}
