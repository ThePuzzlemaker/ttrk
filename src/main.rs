use std::{
    cmp::Ordering,
    env,
    fmt::Write as _,
    fs::{self, File},
    io::{self, Write},
    path::PathBuf,
    process::Command,
};

use color_eyre::eyre::{self, bail, eyre, Context};

use clap::{Parser, Subcommand};
use dirs::home_dir;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tempfile::{tempdir, NamedTempFile};
use time::{format_description::FormatItem, macros::format_description, Duration, OffsetDateTime};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Clone, Debug, Parser)]
#[clap(propagate_version = true)]
#[clap(author, about, version)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
    /// The JSON log file to output sessions to.
    ///
    /// By default this is located at `~/.ttrk.json`.
    #[clap(short = 'l', long, parse(from_str))]
    logfile: Option<PathBuf>,
}

#[derive(Clone, Debug, Subcommand)]
enum Commands {
    /// Begin a session.
    Begin,
    /// End a session, giving a message of what was done.
    End {
        #[clap(value_parser)]
        message: String,
    },
    /// Cancel the current session.
    Cancel,
    /// Get the status of the current session and of the log overall.
    Status,
    /// Show all sessions, completed and current.
    List,
    /// Fix up the log file in your `$EDITOR`.
    Fixup,
    /// Export to CSV
    Csv,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Log {
    completed: Vec<Session>,
    current: Option<Session>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Session {
    start: Time,
    end: Option<Time>,
    message: Option<String>,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
struct Time(#[serde(with = "time::serde::rfc3339")] pub OffsetDateTime);

const TIMESTAMP_FMT: &[FormatItem] = format_description!("[month]-[day]-[year] [hour]:[minute]:[second] (UTC[offset_hour sign:mandatory]:[offset_second])");
const CSV_TIMESTAMP_FMT: &[FormatItem] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .without_time()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(concat!(env!("CARGO_PKG_NAME"), "=info").parse()?),
        )
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
    let logfile = cli.logfile.unwrap_or_else(|| {
        home_dir()
            .ok_or_else(|| eyre!("Failed to find home directory"))
            .unwrap()
            .join(".ttrk.json")
    });

    let mut old_content = String::new();
    let mut file;
    if !logfile.is_file() {
        file = File::options()
            .write(true)
            .create_new(true)
            .open(&logfile)
            .wrap_err(eyre!(
                "Failed to create log file at `{}`",
                logfile.display()
            ))?;
        info!("Created log file at `{}`", logfile.display());
    } else {
        old_content = fs::read_to_string(&logfile)
            .wrap_err(eyre!("Failed to read log file at `{}`", logfile.display()))?;
        file = File::options()
            .write(true)
            .open(&logfile)
            .wrap_err(eyre!("Failed to open log file at `{}`", logfile.display()))?;
        info!("Using log file at `{}`", logfile.display());
    };

    let mut log = if old_content.is_empty() {
        Log::default()
    } else {
        serde_json::from_str(&old_content).wrap_err(eyre!("Failed to parse log file"))?
    };

    let time_on_at_fmt = format_description!("on [month]-[day]-[year] at [hour]:[minute]:[second] (UTC[offset_hour sign:mandatory]:[offset_second])");

    match cli.command {
        Commands::Begin => match log.current {
            Some(ref sess) => {
                error!(
                    "There is already a current session, started {}.",
                    sess.start.0.format(time_on_at_fmt)?
                );
            }
            None => {
                log.current = Some(Session {
                    start: Time(get_time()?),
                    end: None,
                    message: None,
                });
                println!("Started a session.");
            }
        },
        Commands::End { message } => match log.current.take() {
            Some(mut sess) => {
                sess.end = Some(Time(get_time()?));
                if message.contains('\n') {
                    bail!("A message for a completed session must be one line.");
                }
                sess.message = Some(message);
                println!(
                    "Ended session started at {}.\nElapsed time: {}.",
                    sess.start.0.format(time_on_at_fmt)?,
                    display_duration(sess.end.unwrap().0 - sess.start.0)
                );
                log.completed.push(sess);
            }
            None => {
                error!("There is no current session.");
            }
        },
        Commands::Cancel => match log.current {
            Some(ref mut sess) => {
                let time = sess.start;
                log.current = None;
                println!(
                    "Canceled session that was started {}.",
                    time.0.format(time_on_at_fmt)?
                );
            }
            None => {
                error!("There is no current session.");
            }
        },
        Commands::Status => {
            println!(
                "=== Status ===\n- Logged {} completed session{}.",
                log.completed.len(),
                if log.completed.len() != 1 { "s" } else { "" }
            );
            let mut elapsed_total = Duration::default();
            let today = get_time()?.date();
            let thisweek = get_time()?.sunday_based_week();
            let mut elapsed_today = Duration::default();
            let mut elapsed_thisweek = Duration::default();
            for session in &log.completed {
                let end = session.end.unwrap().0;
                let start = session.start.0;
                elapsed_total += end - start;
                if start.date() == today && end.date() == today {
                    elapsed_today += end - start;
                }
                if start.sunday_based_week() == thisweek && end.sunday_based_week() == thisweek {
                    elapsed_thisweek += end - start;
                }
            }
            println!(
                "- Total elapsed time (completed only): {}\n- Total elapsed time today (completed only): {}\n- Total elapsed time this week (completed only): {}",
                display_duration(elapsed_total),
                display_duration(elapsed_today),
                display_duration(elapsed_thisweek),
            );
            if !log.completed.is_empty() {
                let last = log.completed.iter().last().unwrap();
                let start = last.start.0;
                let end = last.end.unwrap().0;
                println!(
                    "\n=== Most recent completed session ===\n- Began {}\n- Ended {}\n- Time elapsed: {}\n- Message: \"{}\"",
                    start.format(time_on_at_fmt)?,
                    end.format(time_on_at_fmt)?,
                    display_duration(end - start),
                    last.message.as_ref().unwrap()
                );
            }
            if let Some(ref sess) = log.current {
                println!(
                    "\n=== Current session ===\n- Began {}\n- Time elapsed: {}",
                    sess.start.0.format(time_on_at_fmt)?,
                    display_duration(get_time()? - sess.start.0)
                );
            }
        }
        Commands::List => {
            println!("{}", format_log(&log)?);
        }
        Commands::Fixup => {
            // This one is super hacky, but it works.
            let editor = env::var("EDITOR")
                .wrap_err(eyre!("Failed to get `$EDITOR` environment variable."))?;
            let tmpdir = tempdir()?;
            let mut tmpfile_path = tmpdir.path().to_owned();
            let mut tmpfile = NamedTempFile::new_in(&tmpdir)
                .wrap_err(eyre!("Failed to create a temporary file."))?;
            tmpfile_path.push(tmpfile.path());
            writeln!(
                tmpfile,
                r#"# Here you can fix up any entries of the log. One log entry per line.
#
# Empty lines or lines starting with `#` are ignored.
#
# Duration is ignored--don't worry about calculating it, just leave the
# duration within the parens untouched, or remove it (but keeping the parens).
#
# Current log entries, marked with an end time of `[now]`, cannot have a
# message.
#
# For current entries, do not worry about messing up the padding--
# it is ignored.
#
# Example format:
# 06-24-2022 16:55:46 (UTC-05:00) -> 06-24-2022 16:55:49 (UTC-05:00) (3 seconds): Message here
#
# For current entries:
# 06-24-2022 17:21:10 (UTC-05:00) -> [now]                           (35 minutes, 47 seconds)
"#
            )?;
            write!(tmpfile, "{}", format_log(&log)?)?;
            tmpfile.flush()?;

            let path = tmpfile.into_temp_path();
            Command::new(editor)
                .arg(&tmpfile_path)
                .spawn()
                .wrap_err(eyre!("Failed to open an editor."))?
                .wait()
                .wrap_err(eyre!("Failed to open an editor."))?;
            let s = fs::read_to_string(&tmpfile_path)?;
            path.close()?;
            tmpdir.close()?;
            log = parse_log_fmtd(s).wrap_err(eyre!("Failed to parse new log."))?;
            println!("Successfully edited the log.");
        }
        Commands::Csv => {
            let mut csv = csv::Writer::from_writer(io::stdout());
            csv.serialize((
                "UTC-Start",
                "UTC-End",
                "Hours",
                "Minutes",
                "Seconds",
                "Message",
            ))?;
            for session in &log.completed {
                let start = session.start.0;
                let end = session.end.unwrap().0;
                let seconds = (end - start).whole_seconds();
                let minutes = seconds / 60;
                let hours = seconds / (60 * 60);
                csv.serialize((
                    start
                        .to_offset(time::macros::offset!(+0))
                        .format(CSV_TIMESTAMP_FMT)?,
                    end.to_offset(time::macros::offset!(+0))
                        .format(CSV_TIMESTAMP_FMT)?,
                    hours,
                    minutes % 60,
                    seconds % 60,
                    session.message.as_ref().unwrap(),
                ))?;
            }
            csv.flush()?;
        }
    }

    file.set_len(0)?;
    serde_json::to_writer(&mut file, &log).wrap_err(eyre!("Failed to write to log file"))?;
    Ok(())
}

fn format_log(log: &Log) -> eyre::Result<String> {
    let mut s = String::new();
    for session in &log.completed {
        let start = session.start.0;
        let end = session.end.unwrap().0;
        writeln!(
            s,
            "{} -> {} ({}): {}",
            start.format(TIMESTAMP_FMT)?,
            end.format(TIMESTAMP_FMT)?,
            display_duration(end - start),
            session.message.as_ref().unwrap()
        )?;
    }
    if let Some(ref session) = log.current {
        let start = session.start.0;
        writeln!(
            s,
            "{} -> [now]                           ({})",
            start.format(TIMESTAMP_FMT)?,
            display_duration(get_time()? - start)
        )?;
    }
    Ok(s)
}

/// A very chonky regex that parses the log lines.
static LOG_LINE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?P<start>[0-9]{2}-[0-9]{2}-[0-9]{4} [0-9]{2}:[0-9]{2}:[0-9]{2} \(UTC[-+][0-9]{2}:[0-9]{2}\)) -> (?:(?P<end_time>[0-9]{2}-[0-9]{2}-[0-9]{4} [0-9]{2}:[0-9]{2}:[0-9]{2} \(UTC[-+][0-9]{2}:[0-9]{2}\))|(?P<end_current>\[now\](\s+))) \([0-9a-z, ]*\)(?:: (?P<message>.*))?"#).unwrap()
});

/// A dirt-simple formatted (with format_log) log parser.
fn parse_log_fmtd(fmtd: String) -> eyre::Result<Log> {
    let mut log = Log {
        completed: vec![],
        current: None,
    };
    for line in fmtd.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let caps = LOG_LINE_REGEX
            .captures(line)
            .ok_or_else(|| eyre!("Failed to parse log line"))?;
        let start = caps.name("start").unwrap();
        let end_time = caps.name("end_time");
        let end_current = caps.name("end_current");
        let message = caps.name("message");

        if end_current.is_some() && message.is_some() {
            bail!("Log lines must not have a message if they are current.")
        }

        // cannot have both without some serious regex bugs
        if end_current.is_some() {
            if log.current.is_some() {
                bail!("There can only be one current log line.");
            }

            log.current = Some(Session {
                start: Time(OffsetDateTime::parse(start.as_str(), TIMESTAMP_FMT)?),
                end: None,
                message: None,
            });
        } else if let Some(end_time) = end_time {
            if message.is_none() {
                bail!("A completed log must have a message.");
            }

            log.completed.push(Session {
                start: Time(OffsetDateTime::parse(start.as_str(), TIMESTAMP_FMT)?),
                end: Some(Time(OffsetDateTime::parse(
                    end_time.as_str(),
                    TIMESTAMP_FMT,
                )?)),
                message: Some(message.unwrap().as_str().to_string()),
            });
        }
    }

    Ok(log)
}

fn display_duration(duration: Duration) -> String {
    let seconds = duration.whole_seconds();
    let minutes = seconds / 60;
    let hours = seconds / (60 * 60);
    let mut components = vec![];
    match hours.cmp(&1) {
        Ordering::Equal => components.push("1 hour".to_string()),
        Ordering::Greater => components.push(format!("{} hours", hours)),
        _ => {}
    }
    match minutes.cmp(&1) {
        Ordering::Equal => components.push("1 minute".to_string()),
        Ordering::Greater => components.push(format!("{} minutes", minutes % 60)),
        _ => {}
    }
    match seconds.cmp(&1) {
        Ordering::Equal => components.push("1 second".to_string()),
        Ordering::Greater => components.push(format!("{} seconds", seconds % 60)),
        _ => {}
    }
    if duration.is_negative() || duration.is_zero() {
        components.push("N/A".to_string());
    }
    components.join(", ")
}

fn get_time() -> eyre::Result<OffsetDateTime> {
    // We don't need a lot of precision.
    Ok(OffsetDateTime::now_local()?.replace_nanosecond(0)?)
}
