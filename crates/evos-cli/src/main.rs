//! `evos`: search the EverythingOS index from the command line.

mod mount;
mod output;

use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use evos_proto::{Client, ClientError, Request, SearchOptions, Sort, SortKey};

/// Instant file search for Linux.
///
/// With no subcommand, the arguments are a search query in Everything
/// syntax, e.g. `evos ext:pdf report`, `evos "*.rs" !test`, `evos size:>1gb`.
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
    Search(SearchArgs),
    /// Show indexed volumes and their state.
    Status,
    /// Re-check a path's volume (or every volume) for changes.
    Rescan { path: Option<PathBuf> },
    /// Manage mounted shares and folders.
    #[command(subcommand)]
    Mount(mount::MountCommand),
}

#[derive(Copy, Clone, ValueEnum)]
enum SortArg {
    Name,
    Path,
    Size,
    /// Date modified.
    Dm,
    Ext,
}

#[derive(Args, Default)]
struct SearchArgs {
    /// Search terms.
    #[arg(trailing_var_arg = true, allow_hyphen_values = false)]
    query: Vec<String>,
    /// Stop after this many results.
    #[arg(short = 'n', long = "max")]
    max: Option<u64>,
    /// Skip this many results.
    #[arg(long, default_value_t = 0)]
    offset: u64,
    #[arg(short, long, value_enum)]
    sort: Option<SortArg>,
    /// Sort descending.
    #[arg(long)]
    desc: bool,
    /// Match case.
    #[arg(short = 'i', long = "case")]
    match_case: bool,
    /// Match whole words.
    #[arg(short = 'w', long)]
    whole_word: bool,
    /// Match against the full path.
    #[arg(short = 'p', long = "match-path")]
    match_path: bool,
    /// Treat the query as one regular expression.
    #[arg(short = 'r', long)]
    regex: bool,
    /// Show sizes.
    #[arg(long)]
    size: bool,
    /// Show modification dates.
    #[arg(long)]
    dm: bool,
    /// Human-readable sizes.
    #[arg(short = 'H', long)]
    human: bool,
    /// Only print the number of results.
    #[arg(long, conflicts_with_all = ["json", "null"])]
    count: bool,
    /// Print results as JSON lines.
    #[arg(long)]
    json: bool,
    /// Separate results with NUL instead of newline.
    #[arg(short = '0', long)]
    null: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Search(args)) => search(&cli.socket, args),
        None => search(&cli.socket, cli.search),
        Some(Command::Status) => status(&cli.socket),
        Some(Command::Rescan { path }) => rescan(&cli.socket, path),
        Some(Command::Mount(cmd)) => mount::run(&cli.socket, cmd),
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

fn search(socket: &PathBuf, args: SearchArgs) -> Result<ExitCode, Failure> {
    let mut client = connect(socket)?;
    let sort = Sort {
        key: match args.sort {
            None | Some(SortArg::Name) => SortKey::Name,
            Some(SortArg::Path) => SortKey::Path,
            Some(SortArg::Size) => SortKey::Size,
            Some(SortArg::Dm) => SortKey::Modified,
            Some(SortArg::Ext) => SortKey::Extension,
        },
        descending: args.desc,
    };
    let options = SearchOptions {
        match_case: args.match_case,
        whole_word: args.whole_word,
        match_path: args.match_path,
        regex: args.regex,
        ..Default::default()
    };
    let found = client.search(&args.query.join(" "), options, sort)?;
    let (result, total) = (found.id, found.total);
    let end = args.max.map_or(total, |m| (args.offset + m).min(total));
    if args.count {
        println!("{}", end.saturating_sub(args.offset));
        return Ok(exit_for(total));
    }
    let style = output::Style {
        size: args.size,
        dm: args.dm,
        human: args.human,
        json: args.json,
        null: args.null,
    };
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 16, stdout.lock());
    let mut offset = args.offset;
    while offset < end {
        let count = (end - offset).min(10_000) as u32;
        let rows = client.fetch(result, offset as u32, count)?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            output::write_row(&mut out, row, &style)?;
        }
        offset += rows.len() as u64;
    }
    out.flush()?;
    let _ = client.release(result);
    Ok(exit_for(total))
}

/// Like grep: 1 when nothing matched.
fn exit_for(total: u64) -> ExitCode {
    if total == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn status(socket: &PathBuf) -> Result<ExitCode, Failure> {
    let mut client = connect(socket)?;
    let status = client.status()?;
    let mut out = io::stdout().lock();
    output::write_status(&mut out, &status)?;
    Ok(ExitCode::SUCCESS)
}

fn rescan(socket: &PathBuf, path: Option<PathBuf>) -> Result<ExitCode, Failure> {
    use std::os::unix::ffi::OsStrExt;
    let mut client = connect(socket)?;
    let path = path
        .map(|p| std::path::absolute(&p).map(|a| a.as_os_str().as_bytes().to_vec()))
        .transpose()?;
    client.expect_ok(Request::Rescan { path })?;
    Ok(ExitCode::SUCCESS)
}
