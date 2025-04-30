use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use clap::Parser;
use file_rotate::{compression::Compression, suffix::AppendCount, ContentLimit, FileRotate};
use irc::Message;
use neca_cmd::CommandMessage;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    num::NonZero,
    path::PathBuf,
    time::Duration,
};
use tcp_stream::{HandshakeError, TLSConfig, TcpStream};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use ureq::{
    http::{HeaderValue, Request, StatusCode},
    middleware::MiddlewareNext,
    SendBody,
};
use uuid::Uuid;

mod irc;

#[derive(Parser, Clone)]
enum OutputFormat {
    /// Write output in an IRCv3-compatible format, mostly what Twitch gives
    /// you with some things removed.
    Irc {
        /// The file to write logs to, will be rotated and compressed.
        /// If not specified, logs will be written to stdout.
        #[arg()]
        file: Option<PathBuf>,
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
        /// The index to index messages into.
        #[arg()]
        index: String,
    },
}

#[derive(Parser)]
struct Args {
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
    /// The size (in bytes) that has to be surpassed for the file to be rotated
    /// Default value is 16 MiB (2^24 bytes)
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

fn connect(args: &Args) -> Result<TcpStream> {
    let addr = ("irc.chat.twitch.tv", 6697);
    let stream = TcpStream::connect(addr)?;
    let mut stream = stream.into_tls(addr.0, TLSConfig::default());

    while let Err(HandshakeError::WouldBlock(mid_handshake)) = stream {
        stream = mid_handshake.handshake();
    }
    let mut stream = stream.unwrap();

    let pass = args.pass.as_deref().unwrap_or("none");
    let nick = args.nick.as_deref().unwrap_or("justinfan1337");

    write!(stream, "PASS {pass}\r\n")?;
    write!(stream, "NICK {nick}\r\n")?;
    write!(stream, "CAP REQ :twitch.tv/tags\r\n")?;
    write!(stream, "CAP REQ :twitch.tv/commands\r\n")?;
    if args.dont_filter {
        write!(stream, "CAP REQ :twitch.tv/membership\r\n")?;
    }
    for channel in &args.channels {
        let channel = channel.to_ascii_lowercase();
        write!(stream, "JOIN #{channel}\r\n")?;
    }

    Ok(stream)
}

const IGNORED_CMDS: &[&str] = &[
    "366", "001", "002", "003", "004", "375", "372", "376", "CAP", "353",
];

trait LogOutput {
    fn write(&mut self, message: &Message) -> Result<()>;
}

struct IrcLogOutput<W>(W);

impl<W: Write> LogOutput for IrcLogOutput<W> {
    fn write(&mut self, message: &Message) -> Result<()> {
        message.write(&mut self.0)?;
        writeln!(&mut self.0)?;
        Ok(())
    }
}

struct JsonLogOutput<W>(W);

impl<W: Write> LogOutput for JsonLogOutput<W> {
    fn write(&mut self, message: &Message) -> Result<()> {
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
    endpoint: String,
}

impl ElasticLogOutput {
    fn new(address: &str, api_key_file: &str, index: &str) -> Self {
        let key = std::fs::read_to_string(api_key_file)
            .expect("Failed to read ES API key from the given file");
        let key = key.trim();

        let auth_header = HeaderValue::from_str(&format!("ApiKey {key}")).unwrap();

        let client = ureq::Agent::config_builder()
            .middleware(move |mut req: Request<SendBody>, next: MiddlewareNext| {
                req.headers_mut()
                    .append("Authorization", auth_header.clone());
                req.headers_mut()
                    .append("Content-Type", HeaderValue::from_static("application/json"));
                next.handle(req)
            })
            .build()
            .new_agent();
        let endpoint = format!("{address}/{index}/_create");
        Self { client, endpoint }
    }
}

impl LogOutput for ElasticLogOutput {
    fn write(&mut self, message: &Message) -> Result<()> {
        let mut json = to_json(message);

        let id = json.id.take().unwrap();
        let endpoint = format!("{}/{id}", self.endpoint);
        let body = serde_json::to_string(&json)?;

        let res = self.client.post(&endpoint).send(&body)?;

        if !res.status().is_success() && res.status() != StatusCode::CONFLICT {
            tracing::error!(
                id,
                message = body,
                "Failed to send log to ES (status {}): {}",
                res.status(),
                res.into_body()
                    .read_to_string()
                    .unwrap_or_else(|_| "<failed to read response body>".into())
            );
        }
        Ok(())
    }
}

fn compress(msg: &mut Message) {
    // it's only twitch logins, irc user/host are redundant
    let nick = match msg.prefix {
        None => "",
        Some(ref mut prefix) => {
            if prefix.host.is_some_and(|h| h.ends_with(".tmi.twitch.tv")) {
                prefix.host = None;
                prefix.user = None;
            }
            prefix.nick
        }
    };

    // the absolute majority of commands are PRIVMSG so we "compress" only those
    if msg.command != "PRIVMSG" {
        return;
    }
    msg.tags.retain_mut(|(k, v)| {
        // room-id: ROOMSTATE gives room id for channel, and messages have channels
        // client-nonce: useless nonce that takes up 46 bytes total
        // emotes: they are still in the text, and we wont get extra metadata
        // for 7tv/ffz/bttv/etc ones anyway
        // (emotes tag only contains byteranges and emote cdn ids)
        if k == &"room-id" || k == &"client-nonce" || k == &"emotes" {
            return false;
        }
        // remove the display-name if it does nothing
        // (if it needed escaping it's not equal to the nick lol)
        if k == &"display-name" && nick == v.0 {
            return false;
        }

        // cleanup all the tags whose absence and empty value or 0 are equivalent
        // (@badge-info=;color=;emotes=;first-msg=0;flags=;mod=0;returning-chatter=0;subscriber=0;turbo=0;user-type=)
        // etc
        !v.0.is_empty() && v.0 != "0"
    });
}

#[serde_with::skip_serializing_none]
#[derive(serde::Serialize)]
struct Json<'a> {
    #[serde(rename = "_id")]
    id: Option<String>,
    #[serde(rename = "@timestamp")]
    timestamp: i64,
    channel: Option<&'a str>,
    name: Option<String>,
    message: Option<&'a str>,
    tags: serde_json::Map<String, Value>,
    #[serde(rename = "irc.nick")]
    irc_nick: Option<&'a str>,
    #[serde(rename = "irc.cmd")]
    irc_cmd: String,
    #[serde(rename = "irc.extras", skip_serializing_if = "Vec::is_empty")]
    irc_extras: Vec<String>,
    #[serde(rename = "commands.only")]
    commands_only: Option<bool>,
    #[serde(rename = "commands.count")]
    commands_count: Option<NonZero<u32>>,
}

fn to_json<'m>(message: &'m Message) -> Json<'m> {
    let mut tags = serde_json::Map::new();

    let mut id = None;
    let mut timestamp = None;

    for (k, v) in &message.tags {
        let k = (*k).to_owned();
        if k == "badges" || k == "badge-info" {
            let data = v
                .unescape()
                .split(",")
                .map(|b| {
                    let (k, v) = b.split_once("/").unwrap_or((b, ""));
                    (k.to_owned(), v.to_owned().into())
                })
                .collect();

            tags.insert(k, Value::Object(data));
        } else if k == "id" {
            id = Some(v.unescape().to_string());
        } else if k == "tmi-sent-ts" {
            timestamp = Some(v.unescape().to_string());
        } else if v.0 == "1" {
            tags.insert(k, Value::Bool(true));
        } else {
            tags.insert(k, v.unescape().into());
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
    let irc_nick = message.prefix.as_ref().map(|p| p.nick);
    let name = display_name.as_deref().or(irc_nick).map(|s| s.to_owned());

    let irc_cmd = message.command.to_owned();
    let channel = message.params.first().and_then(|m| m.strip_prefix("#"));
    let text = message.params.get(1).copied();

    let (commands_count, commands_only) = (irc_cmd == "PRIVMSG")
        .then_some(text)
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

    let irc_extras = message
        .params
        .iter()
        .skip(2)
        .map(|s| (*s).to_owned())
        .collect();

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

fn main() -> Result<()> {
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

    let args = Args::parse();

    let mut backoff = Duration::ZERO;
    loop {
        let result = run(&args, &mut backoff);
        tracing::info!(
            "disconnected from twitch, waiting for {} seconds and retrying, result was {result:?}",
            backoff.as_secs()
        );
        if backoff == Duration::ZERO {
            backoff = Duration::from_secs(1);
            continue;
        }
        std::thread::sleep(backoff);
        backoff *= 2;
        if backoff.as_secs() > 32 {
            bail!("backoff retries failed")
        }
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

fn run(args: &Args, backoff: &mut Duration) -> Result<()> {
    let mut output: Box<dyn LogOutput> = match &args.output {
        OutputFormat::Irc { file: None, .. } => Box::new(IrcLogOutput(std::io::stdout())),
        OutputFormat::Json { file: None, .. } => Box::new(JsonLogOutput(std::io::stdout())),
        OutputFormat::Irc {
            file,
            rotation_limit,
        } => Box::new(IrcLogOutput(rotate(file, *rotation_limit))),
        OutputFormat::Json {
            file,
            rotation_limit,
        } => Box::new(JsonLogOutput(rotate(file, *rotation_limit))),
        OutputFormat::Elastic {
            address,
            api_key_file,
            index,
        } => Box::new(ElasticLogOutput::new(address, api_key_file, index)),
    };

    let mut reader = BufReader::new(connect(args)?);

    // reset backoff after successful connection
    // kinda cringe that this is basically a callback, but oh well, it works
    *backoff = Duration::ZERO;

    let mut buffer = String::with_capacity(4096);
    while reader.read_line(&mut buffer)? != 0 {
        buffer.truncate(buffer.len().saturating_sub(2)); // strip crlf
        let mut msg = Message::parse(&buffer);

        if msg.command == "PING" {
            let reply = msg.params.first().unwrap_or(&"");
            write!(reader.get_mut(), "PONG :{reply}\r\n")?;
        } else if args.dont_filter || !IGNORED_CMDS.contains(&msg.command) {
            compress(&mut msg);
            output.write(&msg)?;
        }
        drop(msg);
        buffer.clear();
    }
    Ok(())
}
