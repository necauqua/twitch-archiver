use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::Utc;
use clap::Parser;
use file_rotate::{compression::Compression, suffix::AppendCount, ContentLimit, FileRotate};
use neca_cmd::CommandMessage;
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    num::NonZero,
    path::PathBuf,
};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use twitch_irc::{
    login::StaticLoginCredentials,
    message::{AsRawIRC, IRCMessage, IRCPrefix},
    ClientConfig, SecureTCPTransport, TwitchIRCClient,
};
use ureq::{
    http::{HeaderValue, Request, StatusCode},
    middleware::MiddlewareNext,
    SendBody,
};
use uuid::Uuid;

#[derive(Parser, Clone)]
enum OutputFormat {
    /// Write output in an IRCv3-compatible format, mostly what Twitch gives
    /// you with some things removed.
    Irc {
        /// The file to write logs to, will be rotated and compressed.
        /// If not specified, logs will be written to stdout.
        #[arg()]
        file: Option<PathBuf>,
        /// The size (in bytes) that has to be surpassed for the file to be rotated
        /// Default value is 16 MiB (2^24 bytes)
        #[arg(long)]
        rotation_limit: Option<usize>,
    },
    /// Write output in newline-delimited JSON format (the same format that's
    /// used for ES).
    Json {
        /// The file to write logs to, will be rotated and compressed.
        /// If not specified, logs will be written to stdout.
        #[arg()]
        file: Option<PathBuf>,
        /// The size (in bytes) that has to be surpassed for the file to be rotated
        /// Default value is 16 MiB (2^24 bytes)
        #[arg(long)]
        rotation_limit: Option<usize>,
    },
    /// Index messages into given Elasticsearch instance.
    Elastic {
        /// The address of the Elasticsearch instance to index messages into.
        #[arg()]
        address: String,
        /// The file containing the API key to use for authentication.
        #[arg()]
        api_key_file: String,
        /// The indices to index messages into. If one is given (the minimum
        /// requirement), all messages are indexed into that with `*` symbol
        /// being replaced by the channel, otherwise a 1-to-1 mapping of
        /// channels to indices is used.
        #[arg(required = true, num_args = 1..)]
        indices: Vec<String>,
    },
}

#[derive(Parser)]
struct ArchiveArgs {
    /// The channels to read from
    #[arg(short, long)]
    channels: Vec<String>,
    /// What nick to use for auth, defaults to an anonymous Twitch user
    #[arg(short, long)]
    nick: Option<String>,
    /// Whas password to use for auth, Twitch accepts the string
    /// "oauth:$OAUTH_TOKEN" here
    #[arg(short, long)]
    pass: Option<String>,
    /// Dont filter out any messages (except PING).
    /// By default, Twitch server welcome messages and JOIN/PART are filtered
    /// away
    #[arg(long)]
    dont_filter: bool,
    /// The file to write logs to, will be rotated and compressed.
    /// By default logs are just printed to stdout.
    /// If no name is given the file will be called twitch.log
    #[command(subcommand)]
    output: OutputFormat,
}

#[derive(Parser)]
struct BackfillArgs {
    /// The file to read IRC logs from (stdin by default)
    input: Option<PathBuf>,
    /// The file pattern to write Elastic bulk ndjson to.
    /// `%` in the given string is replaced with the chunk index.
    #[arg(default_value = "backfill-%.ndjson")]
    output: String,
    /// The Elastic index target for the backfilling.
    #[arg(long, default_value = "twitch-logs")]
    index: String,
    /// Dont filter out any messages (except PING).
    /// By default, Twitch server welcome messages and JOIN/PART are filtered
    /// away
    #[arg(long)]
    dont_filter: bool,
    /// The size (in bytes) of chunks to split the output into.
    #[arg(long)]
    chunk_size: Option<usize>,
}

#[derive(Parser)]
enum Args {
    Archive(ArchiveArgs),
    Backfill(BackfillArgs),
}

#[rustfmt::skip]
const IGNORED_CMDS: &[&str] = &[
    "001", "002", "003", "004",
    "353", "366", "372", "375", "376",
    "CAP", "JOIN", "PONG", "PING", "RECONNECT",
];

trait LogOutput {
    fn write(&mut self, message: &IRCMessage) -> Result<()>;
}

struct IrcLogOutput<W>(W);

impl<W: Write> LogOutput for IrcLogOutput<W> {
    fn write(&mut self, message: &IRCMessage) -> Result<()> {
        self.0.write_all(message.as_raw_irc().as_bytes())?;
        self.0.write_all(b"\n")?;
        Ok(())
    }
}

struct JsonLogOutput<W>(W);

impl<W: Write> LogOutput for JsonLogOutput<W> {
    fn write(&mut self, message: &IRCMessage) -> Result<()> {
        writeln!(
            &mut self.0,
            "{}",
            serde_json::to_string(&to_json(message)).unwrap()
        )?;
        Ok(())
    }
}

struct ElasticLogOutput {
    client: ureq::Agent,
    address: String,
    indices: HashMap<String, String>,
}

impl ElasticLogOutput {
    fn new(address: &str, api_key_file: &str, indices: HashMap<String, String>) -> Self {
        let key = std::fs::read_to_string(api_key_file)
            .expect("Failed to read ES API key from the given file");
        let key = key.trim();

        let auth_header = HeaderValue::from_str(&format!("ApiKey {key}")).unwrap();

        let client = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .middleware(move |mut req: Request<SendBody>, next: MiddlewareNext| {
                req.headers_mut()
                    .append("Authorization", auth_header.clone());
                req.headers_mut()
                    .append("Content-Type", HeaderValue::from_static("application/json"));
                next.handle(req)
            })
            .build()
            .new_agent();

        Self {
            client,
            address: address.to_owned(),
            indices,
        }
    }
}

impl LogOutput for ElasticLogOutput {
    fn write(&mut self, message: &IRCMessage) -> Result<()> {
        let mut json = to_json(message);

        let channel = json
            .channel
            .as_ref()
            .with_context(|| format!("No channel in message: {message:?}"))?;

        let index = self
            .indices
            .get(channel)
            .with_context(|| format!("No index mapping for channel {channel}"))?;
        // ^ should never happen

        let id = json.id.take().unwrap();
        let endpoint = format!("{}/{index}/_create/{id}", self.address);

        let body = serde_json::to_string(&json)?;

        let res = self.client.post(&endpoint).send(&body)?;

        if !res.status().is_success() {
            if res.status() == StatusCode::CONFLICT {
                tracing::info!(id, "Message already exists in ES");
            } else {
                tracing::error!(
                    id,
                    message = body,
                    "Failed to send log to ES (status {}): {}",
                    res.status(),
                    res.into_body()
                        .read_to_string()
                        .unwrap_or_else(|_| "<failed to read response body>".into())
                );
            };
        }
        Ok(())
    }
}

fn compress(msg: &mut IRCMessage) {
    // it's only twitch logins, irc user/host are redundant
    let nick = match &mut msg.prefix {
        None | Some(IRCPrefix::HostOnly { .. }) => "",
        Some(IRCPrefix::Full { nick, user, host }) => {
            if host
                .as_deref()
                .is_some_and(|h| h.ends_with(".tmi.twitch.tv"))
            {
                *host = None;
                *user = None;
            }
            nick
        }
    };
    msg.tags.0.retain(|k, v| {
        // client-nonce is a useless nonce that takes up 46 bytes total and display-name is redundant if equal to nick
        if k == "client-nonce" || k == "display-name" && v.as_deref() == Some(nick) {
            return false;
        }
        // otherwise just cleanup empty tags
        !v.as_deref().is_none_or(|s| s.is_empty())
    });
}

#[serde_with::skip_serializing_none]
#[derive(serde::Serialize)]
struct Json {
    #[serde(rename = "_id")]
    id: Option<String>,
    #[serde(rename = "@timestamp")]
    timestamp: i64,
    channel: Option<String>,
    name: Option<String>,
    message: Option<String>,
    tags: serde_json::Map<String, Value>,
    #[serde(rename = "irc.nick")]
    irc_nick: Option<String>,
    #[serde(rename = "irc.cmd")]
    irc_cmd: String,
    #[serde(rename = "irc.extras", skip_serializing_if = "Vec::is_empty")]
    irc_extras: Vec<String>,
    #[serde(rename = "commands.only")]
    commands_only: Option<bool>,
    #[serde(rename = "commands.count")]
    commands_count: Option<NonZero<u32>>,
}

fn to_json(message: &IRCMessage) -> Json {
    let mut tags = serde_json::Map::new();

    let mut id = None;
    let mut timestamp = None;

    for (k, v) in &message.tags.0 {
        let k = (*k).to_owned();
        let v = v.as_deref().unwrap_or_default();
        if k == "badges" || k == "badge-info" {
            let data = v
                .split(",")
                .map(|b| {
                    let (k, v) = b.split_once("/").unwrap_or((b, ""));
                    let v = match v.parse::<i64>() {
                        Ok(v) => Value::Number(v.into()),
                        Err(_) => Value::String(v.to_owned()),
                    };
                    (k.to_owned(), v)
                })
                .collect();

            tags.insert(k, Value::Object(data));
        } else if k == "id" {
            id = Some(v.to_string());
        } else if k == "tmi-sent-ts" {
            timestamp = Some(v.to_string());
        } else {
            // those twitch ids are numeric, but I want to store them as strings to avoid a 2bil issue idk
            let v = if k.ends_with("-id") {
                Value::String(v.into())
            } else {
                match v.parse::<i64>() {
                    Ok(v) => Value::Number(v.into()),
                    Err(_) => Value::String(v.into()),
                }
            };
            tags.insert(k, v);
        }
    }

    let id = Some(id.unwrap_or_else(|| Uuid::new_v4().to_string()));
    let timestamp = timestamp
        .and_then(|ts| ts.parse::<i64>().ok())
        .unwrap_or_else(|| Utc::now().timestamp_millis());

    // Coalesce display name and nick into "name"
    let display_name = tags.remove("display-name").map(|v| match v {
        Value::String(s) => s,
        _ => unreachable!(),
    });
    let irc_nick = message.prefix.as_ref().map(|p| match p {
        IRCPrefix::Full { nick, .. } => nick.clone(),
        IRCPrefix::HostOnly { host } => host.clone(), // should not happen I think?
    });
    let name = display_name.or_else(|| irc_nick.clone());

    let irc_cmd = message.command.clone();
    let channel = message
        .params
        .first()
        .and_then(|m| m.strip_prefix("#"))
        .map(|s| s.to_owned());
    let text = message.params.get(1).cloned();

    let text = text
        .or_else(|| {
            tags.get("system-msg")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
        })
        .or_else(|| {
            tags.get("msg-id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
        });

    let (commands_count, commands_only) = (irc_cmd == "PRIVMSG")
        .then_some(text.as_deref())
        .flatten()
        .map(|msg| {
            let commands = CommandMessage::parse(msg);
            let count = commands.parallel.iter().map(|seq| seq.len() as u32).sum();
            match NonZero::new(count) {
                None => (None, None),
                Some(count) => (Some(count), commands.pure.then_some(true)),
            }
        })
        .unwrap_or_default();

    let irc_extras = message.params.iter().skip(2).cloned().collect();

    Json {
        id,
        timestamp,
        name,
        tags,
        channel,
        message: text,
        irc_nick,
        irc_cmd,
        irc_extras,
        commands_count,
        commands_only,
    }
}

fn rotate(path: &Option<PathBuf>, rotation_limit: Option<usize>) -> FileRotate<AppendCount> {
    FileRotate::new(
        path.clone().unwrap_or_else(|| "twitch.log".into()),
        AppendCount::new(usize::MAX),
        ContentLimit::BytesSurpassed(rotation_limit.unwrap_or(1 << 24 /* 16 MiB */)),
        Compression::OnRotate(0),
        None,
    )
}

async fn archive(mut args: ArchiveArgs) -> Result<()> {
    let (mut receiver, client) = TwitchIRCClient::<SecureTCPTransport, _>::new(
        ClientConfig::new_simple(StaticLoginCredentials::anonymous()),
    );
    for channel in &mut args.channels {
        channel.make_ascii_lowercase();
        client.join(channel.clone())?;
    }

    let mut output: Box<dyn LogOutput> = match args.output {
        OutputFormat::Irc { file: None, .. } => Box::new(IrcLogOutput(std::io::stdout())),
        OutputFormat::Json { file: None, .. } => Box::new(JsonLogOutput(std::io::stdout())),
        OutputFormat::Irc {
            file,
            rotation_limit,
        } => Box::new(IrcLogOutput(rotate(&file, rotation_limit))),
        OutputFormat::Json {
            file,
            rotation_limit,
        } => Box::new(JsonLogOutput(rotate(&file, rotation_limit))),
        OutputFormat::Elastic {
            address,
            api_key_file,
            indices,
        } => {
            let mapping = match &indices[..] {
                [index] => args
                    .channels
                    .into_iter()
                    .map(|ch| {
                        let index = index.replace("*", &ch);
                        (ch, index)
                    })
                    .collect(),
                _ => {
                    if indices.len() != args.channels.len() {
                        bail!(
                            "Expected 1 or {} indices, got {}",
                            args.channels.len(),
                            indices.len()
                        );
                    }
                    args.channels.into_iter().zip(indices.into_iter()).collect()
                }
            };

            Box::new(ElasticLogOutput::new(&address, &api_key_file, mapping))
        }
    };

    while let Some(msg) = receiver.recv().await {
        let mut msg = msg.source().clone();
        if args.dont_filter || !IGNORED_CMDS.contains(&&*msg.command) {
            compress(&mut msg);
            output.write(&msg)?;
        }
    }

    Ok(())
}

fn backfill(args: BackfillArgs) -> Result<()> {
    let input: Box<dyn BufRead> = match args.input {
        Some(path) => Box::new(BufReader::new(std::fs::File::open(path)?)),
        None => Box::new(std::io::stdin().lock()),
    };
    let chunk_size = args.chunk_size.unwrap_or(usize::MAX);

    let mut s = String::with_capacity(1024 * 1024);
    let mut idx = 0;

    for line in input.lines() {
        let line = line?;

        let Ok(mut message) = IRCMessage::parse(&line) else {
            tracing::warn!("Failed to parse line: {line}");
            continue;
        };

        if !args.dont_filter && IGNORED_CMDS.contains(&&*message.command) {
            continue;
        }
        // we cant backfill messages without a timestamp
        if message.tags.0.iter().all(|(k, _)| *k != "tmi-sent-ts") {
            continue;
        }
        // *especially* without an id
        if message.tags.0.iter().all(|(k, _)| *k != "id") {
            continue;
        }

        compress(&mut message);

        // fixup old logs that base64-compressed uuids like that
        for (k, v) in &mut message.tags.0 {
            let Some(v) = v.as_mut() else {
                continue;
            };
            if v.len() != 36 && (*k == "reply-parent-msg-id" || *k == "reply-thread-parent-msg-id")
            {
                *v = Uuid::from_slice(&base64::prelude::BASE64_STANDARD_NO_PAD.decode(&**v)?)?
                    .to_string();
            }
        }

        let mut json = to_json(&message);

        let id = json.id.take().unwrap();

        // same as above
        let id = if id.len() != 36 {
            Uuid::from_slice(&base64::prelude::BASE64_STANDARD_NO_PAD.decode(id)?)?.to_string()
        } else {
            id
        };

        let mut appending = serde_json::to_string(&serde_json::json!({
            "create": {
                "_index": args.index,
                "_id": id,
            }
        }))?;
        appending.push('\n');
        appending.push_str(&serde_json::to_string(&json)?);
        appending.push('\n');

        if s.len() + appending.len() >= chunk_size {
            let path = args.output.replace("%", &idx.to_string());
            std::fs::write(path, std::mem::take(&mut s))?;
            idx += 1;
        }
        s.push_str(&appending);
    }
    if !s.is_empty() {
        let path = args.output.replace("%", &idx.to_string());
        std::fs::write(path, std::mem::take(&mut s))?;
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("Failed to install crypto provider"))?;

    match Args::parse() {
        Args::Archive(args) => archive(args).await,
        Args::Backfill(args) => backfill(args),
    }
}
