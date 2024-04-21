use smallvec::SmallVec;
use std::borrow::Cow;
use std::fmt::{self, Display};
use std::io::Write;

#[repr(transparent)]
#[derive(Debug)]
pub struct TagValue<'m>(pub Cow<'m, str>);

impl<'m> TagValue<'m> {
    pub fn unescape(&self) -> Cow<str> {
        if let Cow::Owned(ref s) = self.0 {
            return Cow::Borrowed(s);
        }

        let mut iter = self.0.split('\\');

        // split never returns an empty iterator
        let first = iter.next().unwrap();

        if first.len() == self.0.len() {
            return Cow::Borrowed(&self.0);
        }

        // resulting string would be shorter because we are collapsing escapes
        let mut owned = String::with_capacity(self.0.len());
        owned += first;

        let mut skip = false;
        for part in iter {
            if skip {
                owned += part;
                skip = false;
                continue;
            }
            if let Some(control) = part.chars().next() {
                owned.push(match control {
                    ':' => ';',
                    's' => ' ',
                    'r' => '\r',
                    'n' => '\n',
                    x => x,
                });
                owned += &part[control.len_utf8()..];
            } else {
                // todo don't remember this logic, look closer
                owned.push('\\');
                skip = true;
            }
        }
        Cow::Owned(owned)
    }
}

impl<'m> Display for TagValue<'m> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.unescape())
    }
}

#[derive(Debug)]
pub struct Prefix<'m> {
    pub nick: &'m str,
    pub user: Option<&'m str>,
    pub host: Option<&'m str>,
}

impl<'m> Prefix<'m> {
    pub fn parse(mut raw: &'m str) -> Self {
        fn pop_suffix<'m>(message: &mut &'m str, sep: char) -> Option<&'m str> {
            let (rest, part) = message.rsplit_once(sep)?;
            *message = rest;
            Some(part)
        }
        Self {
            host: pop_suffix(&mut raw, '@'),
            user: pop_suffix(&mut raw, '!'),
            nick: raw,
        }
    }
}

#[derive(Debug)]
pub struct Message<'m> {
    pub tags: Vec<(&'m str, TagValue<'m>)>,
    pub prefix: Option<Prefix<'m>>,
    pub command: &'m str,
    pub params: SmallVec<[&'m str; 2]>,
}

impl<'m> Message<'m> {
    pub fn write<W: Write>(&self, mut w: W) -> std::io::Result<()> {
        if let Some(((last_k, last_v), rest)) = self.tags.split_last() {
            write!(w, "@")?;
            for (k, v) in rest {
                write!(w, "{k}={};", v.0)?;
            }
            write!(w, "{last_k}={} ", last_v.0)?;
        }
        if let Some(prefix) = &self.prefix {
            write!(w, ":{}", prefix.nick)?;
            if let Some(user) = prefix.user {
                write!(w, "!{user}")?;
            }
            if let Some(host) = prefix.host {
                write!(w, "@{host}")?;
            }
            write!(w, " ")?;
        }
        write!(w, "{}", self.command)?;
        if let Some((last, rest)) = self.params.split_last() {
            for param in rest {
                write!(w, " {param}")?;
            }

            // fixme: this is a quick hack,
            // do we really have to remember if last arg should have : prefix?
            if last.starts_with('#') {
                write!(w, " {last}")?;
            } else {
                write!(w, " :{last}")?;
            }
        }
        Ok(())
    }

    pub fn parse(mut message: &'m str) -> Self {
        fn pop_by_space<'m>(message: &mut &'m str) -> &'m str {
            let Some((part, rest)) = message.split_once(' ') else {
                return std::mem::take(message);
            };
            *message = rest;
            part
        }

        let mut part = pop_by_space(&mut message);

        let tags = match part.strip_prefix('@') {
            Some(raw) => {
                part = pop_by_space(&mut message);

                raw.split(';')
                    .map(|kv| {
                        let mut parts = kv.splitn(2, '=');
                        (
                            parts.next().unwrap(),
                            TagValue(Cow::Borrowed(parts.next().unwrap_or(""))),
                        )
                    })
                    .collect()
            }
            None => vec![],
        };
        let prefix = match part.strip_prefix(':') {
            Some(prefix) => {
                part = pop_by_space(&mut message);
                Some(Prefix::parse(prefix))
            }
            None => None,
        };

        let command = part;

        let mut params = SmallVec::new();
        while !message.is_empty() {
            if let Some(message) = message.strip_prefix(':') {
                params.push(message);
                break;
            }
            params.push(pop_by_space(&mut message));
        }

        Message {
            tags,
            prefix,
            command,
            params,
        }
    }
}
