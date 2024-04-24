use anyhow::{bail, Result};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use clap::Parser;
use file_rotate::{compression::Compression, suffix::AppendCount, ContentLimit, FileRotate};
use irc::Message;
use std::{
    borrow::Cow,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    time::Duration,
};
use tcp_stream::{TLSConfig, TcpStream};

mod irc;

#[derive(Parser)]
struct Args {
    /// The channels to read from
    #[arg()]
    channels: Vec<String>,
    /// What nick to use for auth, defaults to an anonymous Twitch user
    #[arg(short, long)]
    nick: Option<String>,
    /// Whas password to use for auth, Twitch accepts the string
    /// "oauth:$OAUTH_TOKEN" here
    #[arg(short, long)]
    pass: Option<String>,
    /// The file to write logs to, will be rotated and compressed.
    /// By default logs are just printed to stdout.
    /// If no name is given the file will be called twitch.log
    #[arg(short)]
    output: Option<Option<PathBuf>>,
    /// The size (in bytes) that has to be surpassed for the file to be rotated
    /// Default value is 128 MiB (2^27 bytes)
    #[arg(long)]
    rotation_limit: Option<usize>,
    /// Dont filter out any messages (except PING).
    /// By default, Twitch server welcome messages and JOIN/PART are filtered
    /// away
    #[arg(long)]
    dont_filter: bool,
}

fn connect(args: &Args) -> Result<TcpStream> {
    let addr = ("irc.chat.twitch.tv", 6697);
    let stream = TcpStream::connect(addr)?;
    let mut stream = stream.into_tls(addr.0, TLSConfig::default())?;

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

fn run(args: &Args, backoff: &mut Duration) -> Result<()> {
    let mut output: Box<dyn Write> = match &args.output {
        None => Box::new(std::io::stdout()),
        Some(output) => Box::new(FileRotate::new(
            output.clone().unwrap_or_else(|| "twitch.log".into()),
            AppendCount::new(usize::MAX),
            ContentLimit::BytesSurpassed(args.rotation_limit.unwrap_or(1 << 27 /* 128 MiB */)),
            Compression::OnRotate(0),
            None,
        )),
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
            msg.write(&mut output)?;
            writeln!(output)?;
        }
        drop(msg);
        buffer.clear();
    }
    Ok(())
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

        // yep save some bytes by base64-ing the message uuids lol
        // (reply stuff for consistency)
        if k == &"id" || k == &"reply-parent-msg-id" || k == &"reply-thread-parent-msg-id" {
            if let Ok(uuid) = uuid::Uuid::parse_str(&v.0) {
                v.0 = Cow::Owned(STANDARD_NO_PAD.encode(uuid.into_bytes()))
            }
        }

        // cleanup all the tags whose absence and empty value or 0 are equivalent
        // (@badge-info=;color=;emotes=;first-msg=0;flags=;mod=0;returning-chatter=0;subscriber=0;turbo=0;user-type=)
        // etc
        !v.0.is_empty() && v.0 != "0"
    });
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut backoff = Duration::ZERO;
    loop {
        let result = run(&args, &mut backoff);
        eprintln!(
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
