//! Local secret detection shared by Git ingress, private state and the clean
//! filter. It returns only a decision, never matching values or fragments.
use regex::bytes::{Regex, RegexSet};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::OnceLock;

const WINDOW: usize = 8192;
const OVERLAP: usize = 1024;

/// Final dot-suffixes that mark a `.env`-style basename as committed template
/// content. `.env.example` and its siblings hold placeholders and are tracked
/// on purpose in most repositories, so treating them as private files rejected
/// Shade from ordinary projects.
pub(crate) const ENV_TEMPLATE_SUFFIXES: [&str; 5] =
    ["example", "sample", "template", "dist", "defaults"];

/// Glob patterns matching exactly the basenames `is_private_env_name` calls
/// templates, for the pathspec and gitattributes machinery that has to restate
/// the exception rather than call the predicate.
pub(crate) const ENV_TEMPLATE_PATTERNS: [&str; 5] = [
    ".env*.example",
    ".env*.sample",
    ".env*.template",
    ".env*.dist",
    ".env*.defaults",
];

/// Whether a basename names a private environment file, which may never enter
/// a Git object.
///
/// The name has to begin with `.env` byte for byte. `.ENV` is therefore an
/// ordinary name here and only the content scanner speaks for it, which is
/// what keeps this predicate in step with the case-sensitive `.env*` globs
/// built from it: a name this returns `true` for must also be one those globs
/// catch, or a checkpoint could stage a file the predicate called private.
/// `.envrc` and `.environment` stay private, as they have always been.
///
/// The exception is a final suffix from [`ENV_TEMPLATE_SUFFIXES`], so
/// `.env.example` and `.env.local.example` are ordinary tracked content while
/// `.env`, `.env.local`, `.env.development.local` and `.env.example.local`
/// remain private.
pub(crate) fn is_private_env_name(name: &[u8]) -> bool {
    if !name.starts_with(b".env") {
        return false;
    }
    // The leading dot is not a suffix separator: `.env` and `.envrc` have no
    // suffix of their own and are private.
    let suffix = match name.iter().rposition(|byte| *byte == b'.') {
        Some(dot) if dot > 0 => &name[dot + 1..],
        _ => return true,
    };
    !ENV_TEMPLATE_SUFFIXES
        .iter()
        .any(|template| suffix == template.as_bytes())
}

pub(crate) fn private_env_path(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| is_private_env_name(name.as_bytes()))
}

fn signatures() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new([
            r"-----BEGIN (?:[A-Z0-9]+ )?PRIVATE KEY-----",
            r"-----BEGIN PGP PRIVATE KEY BLOCK-----",
            r"(?-u:\b)gh[pousr]_[A-Za-z0-9]{30,255}",
            r"(?-u:\b)github_pat_[A-Za-z0-9_]{50,255}",
            r"(?-u:\b)(?:sk_live_|rk_live_|sk_test_|sk-proj-)[A-Za-z0-9_-]{20,255}",
            r"(?-u:\b)xox[baprs]-[A-Za-z0-9-]{20,255}",
            r"(?-u:\b)AIza[A-Za-z0-9_-]{35}",
            r#"(?i)[a-z][a-z0-9+.-]{1,15}://[^\s/@:'"]{1,64}:[^\s/@'"]{8,128}@"#,
        ])
        .expect("static secret signatures")
    })
}

fn assignments() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r#"(?i)(?-u:\b)(?<key>[a-z0-9_.-]{0,48}(?:api[_-]?key|access[_-]?key|secret[_-]?key|client[_-]?secret|password|passwd|token|private[_-]?key))["']?[ \t]{0,8}[:=][ \t]{0,8}(?:["'](?<quoted>[^"'\r\n]{8,256})["']|(?<bare>[a-z0-9_./+=:-]{12,256})(?:[ \t\r\n;,}]|$))"#).expect("static credential assignment pattern"))
}

fn credential_literal(value: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(value).to_ascii_lowercase();
    if value.contains(&b'\\')
        || [
            "example",
            "fixture",
            "placeholder",
            "changeme",
            "your_",
            "your-",
            "process.env",
            "os.environ",
            "${",
            "{{",
            "<",
        ]
        .iter()
        .any(|part| lower.contains(part))
    {
        return false;
    }
    let mut counts = [0usize; 256];
    for byte in value {
        counts[usize::from(*byte)] += 1;
    }
    let entropy = counts
        .into_iter()
        .filter(|count| *count != 0)
        .map(|count| {
            let probability = count as f64 / value.len() as f64;
            -probability * probability.log2()
        })
        .sum::<f64>();
    entropy >= 3.0
}

pub(crate) fn contains_secret(bytes: &[u8]) -> bool {
    if signatures().is_match(bytes) {
        return true;
    }
    assignments().captures_iter(bytes).any(|captures| {
        captures
            .name("quoted")
            .or_else(|| captures.name("bare"))
            .is_some_and(|value| credential_literal(value.as_bytes()))
    })
}

// Only the keys of this private comparison map may leave SecretStore.
pub(crate) fn credential_pairs(bytes: &[u8]) -> std::collections::BTreeMap<String, String> {
    assignments()
        .captures_iter(bytes)
        .filter_map(|captures| {
            let value = captures.name("quoted").or_else(|| captures.name("bare"))?;
            if !credential_literal(value.as_bytes()) {
                return None;
            }
            Some((
                std::str::from_utf8(captures.name("key")?.as_bytes())
                    .ok()?
                    .to_owned(),
                std::str::from_utf8(value.as_bytes()).ok()?.to_owned(),
            ))
        })
        .collect()
}

/// Every pattern is bounded to less than OVERLAP bytes. Scanning remains
/// bounded in memory for binary files and matches spanning read boundaries.
#[derive(Default)]
pub(crate) struct SecretScanner {
    tail: Vec<u8>,
    detected: bool,
}

impl SecretScanner {
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if self.detected {
            return;
        }
        for chunk in bytes.chunks(WINDOW) {
            self.tail.extend_from_slice(chunk);
            if contains_secret(&self.tail) {
                self.detected = true;
                return;
            }
            if self.tail.len() > OVERLAP {
                self.tail.drain(..self.tail.len() - OVERLAP);
            }
        }
    }

    pub(crate) fn detected(&self) -> bool {
        self.detected
    }
}

pub(crate) fn file_contains_secret(path: &Path) -> io::Result<bool> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut scanner = SecretScanner::default();
    let mut buffer = [0; WINDOW];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            return Ok(scanner.detected());
        }
        scanner.feed(&buffer[..size]);
        if scanner.detected() {
            return Ok(true);
        }
    }
}

/// Private Git v2 process-filter entry point used by the shade executable.
/// Content is withheld until the complete request has passed policy.
pub fn run_git_filter(
    spool_root: &Path,
    input: impl Read,
    mut output: impl io::Write,
) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut input = io::BufReader::new(input);
    let hello = read_list(&mut input)?.ok_or_else(protocol_error)?;
    if hello != [b"git-filter-client\n".to_vec(), b"version=2\n".to_vec()] {
        return Err(protocol_error());
    }
    write_packet(&mut output, b"git-filter-server\n")?;
    write_packet(&mut output, b"version=2\n")?;
    flush_packet(&mut output)?;
    let capabilities = read_list(&mut input)?.ok_or_else(protocol_error)?;
    if !capabilities
        .iter()
        .any(|line| line == b"capability=clean\n")
    {
        return Err(protocol_error());
    }
    write_packet(&mut output, b"capability=clean\n")?;
    if capabilities
        .iter()
        .any(|line| line == b"capability=smudge\n")
    {
        write_packet(&mut output, b"capability=smudge\n")?;
    }
    flush_packet(&mut output)?;
    while let Some(headers) = read_list(&mut input)? {
        if !headers
            .iter()
            .any(|line| line == b"command=clean\n" || line == b"command=smudge\n")
        {
            return Err(protocol_error());
        }
        let path = headers
            .iter()
            .find_map(|line| line.strip_prefix(b"pathname="))
            .ok_or_else(protocol_error)?;
        let path = path.strip_suffix(b"\n").unwrap_or(path);
        let mut blocked = path
            .rsplit(|byte| *byte == b'/')
            .next()
            .is_some_and(is_private_env_name);
        let mut scanner = SecretScanner::default();
        let mut memory = Vec::new();
        let mut spill = None::<std::fs::File>;
        loop {
            let Some(packet) = read_packet(&mut input)? else {
                return Err(protocol_error());
            };
            let Some(bytes) = packet else {
                break;
            };
            scanner.feed(&bytes);
            blocked |= scanner.detected();
            if !blocked {
                if spill.is_none() && memory.len() + bytes.len() > 1024 * 1024 {
                    let mut file = tempfile::tempfile_in(spool_root)?;
                    file.write_all(&memory)?;
                    memory.clear();
                    spill = Some(file);
                }
                if let Some(file) = &mut spill {
                    file.write_all(&bytes)?;
                } else {
                    memory.extend_from_slice(&bytes);
                }
            }
        }
        if blocked {
            write_packet(&mut output, b"status=error\n")?;
            flush_packet(&mut output)?;
            continue;
        }
        write_packet(&mut output, b"status=success\n")?;
        flush_packet(&mut output)?;
        if let Some(mut file) = spill {
            file.seek(SeekFrom::Start(0))?;
            let mut buffer = [0; 65516];
            loop {
                let size = file.read(&mut buffer)?;
                if size == 0 {
                    break;
                }
                write_packet(&mut output, &buffer[..size])?;
            }
        } else {
            for chunk in memory.chunks(65516) {
                write_packet(&mut output, chunk)?;
            }
        }
        flush_packet(&mut output)?;
        flush_packet(&mut output)?;
    }
    Ok(())
}

fn protocol_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid Git filter protocol")
}

// EOF, flush, or data. Lengths exclude no terminators: Git content is binary.
fn read_packet(input: &mut impl Read) -> io::Result<Option<Option<Vec<u8>>>> {
    let mut header = [0; 4];
    if input.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut header[1..])?;
    let text = std::str::from_utf8(&header).map_err(|_| protocol_error())?;
    let length = usize::from_str_radix(text, 16).map_err(|_| protocol_error())?;
    if length == 0 {
        return Ok(Some(None));
    }
    if !(4..=65520).contains(&length) {
        return Err(protocol_error());
    }
    let mut bytes = vec![0; length - 4];
    input.read_exact(&mut bytes)?;
    Ok(Some(Some(bytes)))
}

fn read_list(input: &mut impl Read) -> io::Result<Option<Vec<Vec<u8>>>> {
    let mut list = Vec::new();
    loop {
        match read_packet(input)? {
            None if list.is_empty() => return Ok(None),
            None => return Err(protocol_error()),
            Some(None) => return Ok(Some(list)),
            Some(Some(bytes)) => {
                if list.len() >= 32 {
                    return Err(protocol_error());
                }
                list.push(bytes);
            }
        }
    }
}

fn write_packet(output: &mut impl io::Write, bytes: &[u8]) -> io::Result<()> {
    write!(output, "{:04x}", bytes.len() + 4)?;
    output.write_all(bytes)
}

fn flush_packet(output: &mut impl io::Write) -> io::Result<()> {
    output.write_all(b"0000")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_private_material_without_echoing_it() {
        for bytes in [
            ["-----BEGIN ", "OPENSSH PRIVATE KEY-----"].concat(),
            format!("{}{}", "ghp_", "aB3dE6gH9jK2mN5pQ8rS1tU4vW7xY0zA3bC6"),
            r#"{"api_key":"r3D8m2A9c6Z1q7B4v5L0n8H2"}"#.to_owned(),
            "DATABASE_PASSWORD='b7Y2n9W4q1R8d3M6'".to_owned(),
        ] {
            assert!(contains_secret(bytes.as_bytes()));
        }
        for bytes in [
            b"TOKEN=${TOKEN}".as_slice(),
            b"api_key = 'your-example-key'",
            b"let password = process.env.PASSWORD;",
            b"ordinary source",
        ] {
            assert!(!contains_secret(bytes));
        }
    }

    #[test]
    fn detects_binary_and_cross_chunk_secrets_with_bounded_memory() {
        let secret = ["-----BEGIN ", "PRIVATE KEY-----"].concat();
        for split in 0..secret.len() {
            let mut scanner = SecretScanner::default();
            scanner.feed(&[0xff; WINDOW * 2]);
            scanner.feed(&secret.as_bytes()[..split]);
            scanner.feed(&secret.as_bytes()[split..]);
            assert!(scanner.detected());
            assert!(scanner.tail.len() <= WINDOW + OVERLAP);
        }
    }
}
