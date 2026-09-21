use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::Utc;
use clap::Parser;
use file_rotate::{compression::Compression, suffix::AppendCount, ContentLimit, FileRotate};
use neca_cmd::CommandMessage;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Write as _,
    io::{BufRead, BufReader, Write},
    num::NonZero,
    os::unix::net::UnixDatagram,
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use twitch_irc::{
    login::StaticLoginCredentials,
    message::{AsRawIRC, IRCMessage, IRCPrefix, ServerMessage},
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
        /// The delay (in seconds) between attempts to retry indexing messages
        /// while ES is unavailable. Failed messages are kept in memory and
        /// retried indefinitely so that none are lost during an outage.
        #[arg(long, default_value = "5")]
        retry_interval: u64,
    },
}

#[derive(Parser)]
struct ArchiveArgs {
    /// The channels to read from
    #[arg(short, long, value_delimiter = ',')]
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
    /// How many independent connections to Twitch to keep open.
    /// Every connection joins every channel and gets the same messages, which
    /// are then deduplicated. More than one connection means that a RECONNECT
    /// request (or any other connection loss) does not lose messages, because
    /// the other connections keep receiving while the failed one comes back.
    #[arg(long, default_value = "2")]
    connections: usize,
    /// How long (in seconds) to remember a message for deduplication.
    /// A message that was not delivered by every connection inside this window
    /// is reported as missed.
    #[arg(long, default_value = "30")]
    dedup_window: u64,
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

/// How long to wait for every channel to be joined before reporting readiness
/// to the service manager anyway.
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest number of connections that fits in the bitmask of the
/// deduplicator.
const MAX_CONNECTIONS: usize = u32::BITS as usize;

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

/// A message waiting to be indexed into ES.
struct PendingMessage {
    id: String,
    index: String,
    body: String,
}

/// The outcome of a single indexing attempt.
enum IndexOutcome {
    /// The message was indexed (or is a permanent failure) and can be dropped
    /// from the backlog.
    Done,
    /// A transient failure (ES unreachable or returning 5xx/429); the message
    /// should be retried later.
    Retry,
}

/// Attempt to index a single message into ES, classifying the result as either
/// done (success/conflict/permanent failure) or a transient failure to retry.
fn try_index(client: &ureq::Agent, address: &str, pending: &PendingMessage) -> IndexOutcome {
    let endpoint = format!("{address}/{}/_create/{}", pending.index, pending.id);

    let res = match client.post(&endpoint).send(&pending.body) {
        Ok(res) => res,
        Err(e) => {
            tracing::warn!(id = pending.id, "Failed to reach ES, will retry: {e}");
            return IndexOutcome::Retry;
        }
    };

    let status = res.status();
    if status.is_success() {
        return IndexOutcome::Done;
    }
    if status == StatusCode::CONFLICT {
        tracing::info!(id = pending.id, "Message already exists in ES");
        return IndexOutcome::Done;
    }
    tracing::warn!(
        id = pending.id,
        "ES returned a error (status {status}), will retry"
    );
    IndexOutcome::Retry
}

/// Try to index the backlog in arrival order, stopping at the first transient
/// failure so that ordering is preserved while ES is unavailable.
fn drain_backlog(backlog: &mut VecDeque<PendingMessage>, client: &ureq::Agent, address: &str) {
    while let Some(front) = backlog.front() {
        match try_index(client, address, front) {
            IndexOutcome::Done => {
                backlog.pop_front();
            }
            IndexOutcome::Retry => break,
        }
    }
}

/// Background worker that owns the retry backlog and indexes messages into ES.
///
/// Messages are indexed in arrival order; whenever ES is unavailable the
/// affected messages stay queued and are retried every `retry_interval`,
/// indefinitely, so none are lost during an outage. Retries are driven by a
/// timer, independent of incoming traffic. Once the channel is closed it keeps
/// retrying until the backlog has been fully flushed before exiting.
fn run_worker(
    client: ureq::Agent,
    address: String,
    rx: Receiver<PendingMessage>,
    retry_interval: Duration,
) {
    let mut backlog: VecDeque<PendingMessage> = VecDeque::new();

    loop {
        // wait for the next message: block indefinitely when nothing is
        // pending, otherwise wake up after retry_interval to retry the backlog
        let received = if backlog.is_empty() {
            match rx.recv() {
                Ok(msg) => Some(msg),
                // channel closed and nothing left to flush
                Err(_) => return,
            }
        } else {
            match rx.recv_timeout(retry_interval) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    // shutting down: keep retrying until the backlog is empty
                    while !backlog.is_empty() {
                        drain_backlog(&mut backlog, &client, &address);
                        if backlog.is_empty() {
                            break;
                        }
                        tracing::warn!(
                            pending = backlog.len(),
                            "ES unavailable while shutting down, retrying in {}s",
                            retry_interval.as_secs()
                        );
                        std::thread::sleep(retry_interval);
                    }
                    return;
                }
            }
        };

        if let Some(pending) = received {
            backlog.push_back(pending);
        }

        drain_backlog(&mut backlog, &client, &address);
    }
}

struct ElasticLogOutput {
    indices: HashMap<String, String>,
    sender: Option<Sender<PendingMessage>>,
    worker: Option<JoinHandle<()>>,
}

impl ElasticLogOutput {
    fn new(
        address: &str,
        api_key_file: &str,
        indices: HashMap<String, String>,
        retry_interval: Duration,
    ) -> Self {
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

        let (sender, rx) = std::sync::mpsc::channel();
        let address = address.to_owned();
        let worker = std::thread::Builder::new()
            .name("es-indexer".into())
            .spawn(move || run_worker(client, address, rx, retry_interval))
            .expect("Failed to spawn ES indexer thread");

        Self {
            indices,
            sender: Some(sender),
            worker: Some(worker),
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
            .with_context(|| format!("No index mapping for channel {channel}"))?
            .clone();
        // ^ should never happen

        let id = json.id.take().unwrap();
        let body = serde_json::to_string(&json)?;

        // hand the message off to the worker thread, which owns the backlog and
        // retries; the worker lives for the lifetime of this output, so the
        // channel only closes on drop and this send shouldn't fail
        let sender = self.sender.as_ref().expect("sender is only taken on drop");
        if sender.send(PendingMessage { id, index, body }).is_err() {
            bail!("ES indexer thread has stopped unexpectedly");
        }
        Ok(())
    }
}

impl Drop for ElasticLogOutput {
    fn drop(&mut self) {
        // close the channel so the worker flushes the backlog and exits, then
        // wait for it so no buffered message is lost on shutdown
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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
        if k == "client-nonce" || k == "display-name" && v == nick {
            return false;
        }
        // otherwise just cleanup empty tags
        !v.is_empty()
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
    action: Option<bool>,
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
        let v = v.as_str();
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

    let (action, text) = match text
        .as_deref()
        .and_then(|t| t.strip_prefix("\u{0001}ACTION "))
    {
        Some(text) => (
            Some(true),
            Some(text.strip_suffix('\u{0001}').unwrap_or(text).to_owned()),
        ),
        None => (None, text),
    };

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
        action,
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

/// Send `READY=1` to the service manager, as described in sd_notify(3).
/// Does nothing when `NOTIFY_SOCKET` is unset, which is the case outside of
/// systemd.
fn notify_ready() {
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let path = PathBuf::from(socket);
    let result = UnixDatagram::unbound().and_then(|sock| sock.send_to(b"READY=1\n", &path));
    if let Err(e) = result {
        tracing::warn!("Failed to report readiness to the service manager: {e}");
    }
}

/// The key that identifies a message across the connections.
///
/// Twitch gives every chat message a unique id, which is used when present.
/// The other messages are identified by their content: the tags are sorted,
/// because they are stored in a hash map and so their raw form has no stable
/// order.
fn dedup_key(msg: &IRCMessage) -> String {
    if let Some(id) = msg.tags.0.get("id") {
        return id.clone();
    }
    let mut tags = msg.tags.0.iter().collect::<Vec<_>>();
    tags.sort_unstable();

    let mut key = String::new();
    for (k, v) in tags {
        let _ = write!(key, "@{k}={v}");
    }
    if let Some(prefix) = &msg.prefix {
        let _ = write!(key, " :{}", prefix.as_raw_irc());
    }
    let _ = write!(key, " {}", msg.command);
    for param in &msg.params {
        let _ = write!(key, " {param}");
    }
    key
}

/// Keeps track of which connections delivered each message, so that a message
/// received over several connections is written out exactly once, and a
/// connection that misses messages is reported.
struct Dedup {
    /// How many connections are expected to deliver every message.
    connections: usize,
    /// How long a message is remembered before it is reported and dropped.
    window: Duration,
    /// Key of every remembered message, with a bitmask of the connections that
    /// delivered it.
    seen: HashMap<Rc<str>, u32>,
    /// The same keys in arrival order, with the time they first arrived.
    order: VecDeque<(Instant, Rc<str>)>,
}

impl Dedup {
    fn new(connections: usize, window: Duration) -> Self {
        Self {
            connections,
            window,
            seen: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Record that `connection` delivered the message with the given key.
    /// Returns true when this is the first connection to deliver it, which is
    /// when the message has to be written out.
    fn observe(&mut self, connection: usize, key: &str) -> bool {
        let mask = 1 << connection;
        if let Some(seen) = self.seen.get_mut(key) {
            *seen |= mask;
            return false;
        }
        let key: Rc<str> = Rc::from(key);
        self.seen.insert(Rc::clone(&key), mask);
        self.order.push_back((Instant::now(), key));
        true
    }

    /// Drop every message that is older than the window, reporting the ones
    /// that were not delivered by all of the connections.
    fn expire(&mut self) {
        while let Some((arrived, _)) = self.order.front() {
            if arrived.elapsed() < self.window {
                break;
            }
            let (_, key) = self.order.pop_front().unwrap();
            let seen = self.seen.remove(&key).unwrap_or(0);
            if seen.count_ones() as usize == self.connections {
                continue;
            }
            let missing = (0..self.connections)
                .filter(|c| seen & (1 << c) == 0)
                .collect::<Vec<_>>();
            tracing::warn!(
                ?missing,
                seen = seen.count_ones(),
                of = self.connections,
                "Message was not delivered by every connection: {key}",
            );
        }
    }
}

async fn archive(mut args: ArchiveArgs) -> Result<()> {
    if !(1..=MAX_CONNECTIONS).contains(&args.connections) {
        bail!(
            "Expected 1 to {MAX_CONNECTIONS} connections, got {}",
            args.connections
        );
    }
    for channel in &mut args.channels {
        channel.make_ascii_lowercase();
    }

    // Every connection is fully independent and joins every channel, so that
    // the messages of one connection cover the downtime of another one, for
    // example while it obeys a RECONNECT request from Twitch.
    // Their messages are merged into one stream, tagged with the index of the
    // connection they came from.
    let (merged_tx, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut clients = Vec::with_capacity(args.connections);
    for connection in 0..args.connections {
        let mut config = ClientConfig::new_simple(StaticLoginCredentials::anonymous());
        config.tracing_identifier = Some(format!("connection-{connection}").into());

        let (mut incoming, client) = TwitchIRCClient::<SecureTCPTransport, _>::new(config);
        for channel in &args.channels {
            client.join(channel.clone())?;
        }

        let merged_tx = merged_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = incoming.recv().await {
                if merged_tx.send((connection, msg)).is_err() {
                    break;
                }
            }
        });
        clients.push(client);
    }
    // only the forwarding tasks keep the stream open now
    drop(merged_tx);

    let dedup_window = Duration::from_secs(args.dedup_window);
    let mut dedup = Dedup::new(args.connections, dedup_window);
    // expiry is driven by a timer, so that a missed message is reported even
    // when no other message arrives after it
    let mut expire_interval = tokio::time::interval((dedup_window / 2).max(Duration::from_secs(1)));
    expire_interval.tick().await; // the first tick is immediate

    // Readiness is reported only once every channel is joined on every
    // connection, so that a replacement process can overlap with the one it
    // replaces instead of leaving a gap in the archive.
    let mut pending_joins: HashSet<(usize, String)> = (0..args.connections)
        .flat_map(|connection| args.channels.iter().map(move |ch| (connection, ch.clone())))
        .collect();
    let join_deadline = tokio::time::Instant::now() + JOIN_TIMEOUT;

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
            retry_interval,
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
                    args.channels.into_iter().zip(indices).collect()
                }
            };

            Box::new(ElasticLogOutput::new(
                &address,
                &api_key_file,
                mapping,
                Duration::from_secs(retry_interval),
            ))
        }
    };

    loop {
        let (connection, msg) = tokio::select! {
            msg = receiver.recv() => match msg {
                Some(msg) => msg,
                None => break,
            },
            _ = expire_interval.tick() => {
                dedup.expire();
                continue;
            }
            _ = tokio::time::sleep_until(join_deadline), if !pending_joins.is_empty() => {
                tracing::warn!(
                    channels = ?pending_joins,
                    "Not all channels were joined in time, reporting readiness anyway",
                );
                pending_joins.clear();
                notify_ready();
                continue;
            }
        };

        if let ServerMessage::Join(join) = &msg {
            let joined = (connection, join.channel_login.clone());
            if pending_joins.remove(&joined) && pending_joins.is_empty() {
                tracing::info!("Joined all channels on all connections");
                notify_ready();
            }
        }

        let msg = msg.source();
        if !args.dont_filter && IGNORED_CMDS.contains(&&*msg.command) {
            continue;
        }

        if !dedup.observe(connection, &dedup_key(msg)) {
            continue;
        }

        let mut msg = msg.clone();
        compress(&mut msg);
        output.write(&msg)?;
    }

    // the connections are only closed when their handles are dropped
    drop(clients);

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
    // the log goes to stderr, because stdout carries the archived messages
    // when no output file is given
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
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
