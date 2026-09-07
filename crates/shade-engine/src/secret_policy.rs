//! Local secret detection shared by Git ingress, private state and the clean
//! filter. It returns only a decision, never matching values or fragments.
use regex::bytes::{Regex, RegexSet};
use sha2::Digest;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

const WINDOW: usize = 8192;
const OVERLAP: usize = 1024;
/// Requests larger than this keep their bytes in an anonymous private file
/// rather than in the filter's memory.
const SPILL_ABOVE: usize = 1024 * 1024;
/// The largest payload one Git packet line can carry.
const PACKET: usize = 65516;

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

/// Which hash names the repository's objects. A blob identity has to be
/// computed exactly the way Git computes it, or every comparison below fails
/// closed and the filter simply behaves as it did before.
#[derive(Clone, Copy)]
enum ObjectFormat {
    Sha1,
    Sha256,
}

/// Ask the repository the filter is running inside what it already holds.
///
/// Git starts a process filter in the top of the working tree with `GIT_DIR`
/// -- and, when the running command uses a private one, `GIT_INDEX_FILE` --
/// already in the environment, so ordinary plumbing answers about exactly the
/// index that command is reading. Its stdin is closed on purpose: this
/// process's own stdin carries the request being judged.
fn ask_git(arguments: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// Split one `-z` record into the fields before the tab and the path after it.
fn split_record(record: &[u8]) -> Option<(Vec<&[u8]>, &[u8])> {
    let tab = record.iter().position(|byte| *byte == b'\t')?;
    Some((
        record[..tab].split(|byte| *byte == b' ').collect(),
        &record[tab + 1..],
    ))
}

fn records(output: &[u8]) -> impl Iterator<Item = &[u8]> {
    output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
}

/// `git ls-files --stage` prints `<mode> SP <object> SP <stage> TAB <path>`.
/// Unmerged stages are skipped: a path mid-merge records no single blob.
fn index_blobs() -> HashMap<Vec<u8>, Vec<u8>> {
    let Some(output) = ask_git(&["ls-files", "--stage", "-z", "--full-name", "--", ":/"]) else {
        return HashMap::new();
    };
    records(&output)
        .filter_map(|record| {
            let (fields, path) = split_record(record)?;
            let [_mode, object, b"0"] = fields[..] else {
                return None;
            };
            Some((path.to_vec(), object.to_vec()))
        })
        .collect()
}

/// `git ls-tree -r` prints `<mode> SP <type> SP <object> TAB <path>`.
fn head_blobs() -> HashMap<Vec<u8>, Vec<u8>> {
    let Some(output) = ask_git(&["ls-tree", "-r", "-z", "--full-tree", "HEAD"]) else {
        return HashMap::new();
    };
    records(&output)
        .filter_map(|record| {
            let (fields, path) = split_record(record)?;
            let [_mode, b"blob", object] = fields[..] else {
                return None;
            };
            Some((path.to_vec(), object.to_vec()))
        })
        .collect()
}

fn object_format() -> Option<ObjectFormat> {
    match ask_git(&["rev-parse", "--show-object-format"])?
        .strip_suffix(b"\n")
        .unwrap_or_default()
    {
        b"sha1" => Some(ObjectFormat::Sha1),
        b"sha256" => Some(ObjectFormat::Sha256),
        _ => None,
    }
}

/// What Git already records for a path.
///
/// The clean direction decides what may *newly* enter Git, and content the
/// repository already holds under the same path is not new. It has to be
/// asked, because Git re-cleans tracked paths constantly: staging through a
/// private index read out of a tree, verifying a materialized base, and
/// rewriting an index whose files Git has just written all hand the filter
/// bytes it stored long ago. Judging those as arrivals refused a repository
/// for a credential its owner committed, at the moments its work depends on
/// most -- a woken session could not come back at all.
///
/// Each listing is read at most once per filter process, and only after the
/// scanner has actually matched, so a repository carrying nothing detectable
/// never runs one extra command.
#[derive(Default)]
struct RecordedBlobs {
    index: Option<HashMap<Vec<u8>, Vec<u8>>>,
    head: Option<HashMap<Vec<u8>, Vec<u8>>>,
    format: Option<Option<ObjectFormat>>,
}

impl RecordedBlobs {
    /// The object Git holds for `path`: the index the running command reads
    /// first, then the commit that command has checked out.
    fn blob(&mut self, path: &[u8]) -> Option<Vec<u8>> {
        if let Some(object) = self.index.get_or_insert_with(index_blobs).get(path) {
            return Some(object.clone());
        }
        self.head.get_or_insert_with(head_blobs).get(path).cloned()
    }

    fn format(&mut self) -> Option<ObjectFormat> {
        *self.format.get_or_insert_with(object_format)
    }
}

/// The bytes of one request, withheld until the whole request has passed
/// policy. Inputs above [`SPILL_ABOVE`] are held in an anonymous private file
/// instead of memory, so a large blob cannot be read out of the spool.
struct Retained<'a> {
    spool_root: &'a Path,
    memory: Vec<u8>,
    spill: Option<std::fs::File>,
    length: u64,
    kept: bool,
}

impl<'a> Retained<'a> {
    fn new(spool_root: &'a Path) -> Self {
        Self {
            spool_root,
            memory: Vec::new(),
            spill: None,
            length: 0,
            kept: true,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.length += bytes.len() as u64;
        if !self.kept {
            return Ok(());
        }
        if self.spill.is_none() && self.memory.len() + bytes.len() > SPILL_ABOVE {
            let mut file = tempfile::tempfile_in(self.spool_root)?;
            file.write_all(&self.memory)?;
            self.memory = Vec::new();
            self.spill = Some(file);
        }
        match &mut self.spill {
            Some(file) => file.write_all(bytes),
            None => {
                self.memory.extend_from_slice(bytes);
                Ok(())
            }
        }
    }

    /// Stop paying for bytes that can no longer be returned to Git.
    fn forget(&mut self) {
        self.kept = false;
        self.memory = Vec::new();
        self.spill = None;
    }

    /// The object ID Git would give these bytes.
    fn blob_id(&mut self, format: ObjectFormat) -> io::Result<Vec<u8>> {
        match format {
            ObjectFormat::Sha1 => self.digest::<sha1::Sha1>(),
            ObjectFormat::Sha256 => self.digest::<sha2::Sha256>(),
        }
    }

    fn digest<D: Digest>(&mut self) -> io::Result<Vec<u8>> {
        let mut hasher = D::new();
        hasher.update(format!("blob {}\0", self.length).as_bytes());
        self.read_back(|chunk| hasher.update(chunk))?;
        Ok(hex::encode(hasher.finalize()).into_bytes())
    }

    fn write_to(&mut self, output: &mut impl io::Write) -> io::Result<()> {
        let mut written = Ok(());
        self.read_back(|chunk| {
            if written.is_ok() {
                written = write_packet(output, chunk);
            }
        })?;
        written
    }

    fn read_back(&mut self, mut visit: impl FnMut(&[u8])) -> io::Result<()> {
        let Some(file) = &mut self.spill else {
            for chunk in self.memory.chunks(PACKET) {
                visit(chunk);
            }
            return Ok(());
        };
        file.seek(SeekFrom::Start(0))?;
        let mut buffer = [0; PACKET];
        loop {
            let size = file.read(&mut buffer)?;
            if size == 0 {
                return Ok(());
            }
            visit(&buffer[..size]);
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
    let mut recorded = RecordedBlobs::default();
    while let Some(headers) = read_list(&mut input)? {
        let clean = headers.iter().any(|line| line == b"command=clean\n");
        if !clean && !headers.iter().any(|line| line == b"command=smudge\n") {
            return Err(protocol_error());
        }
        let path = headers
            .iter()
            .find_map(|line| line.strip_prefix(b"pathname="))
            .ok_or_else(protocol_error)?;
        let path = path.strip_suffix(b"\n").unwrap_or(path);
        // Only the clean direction decides what enters Git. A smudge request
        // returns bytes the object database already holds, so refusing them
        // keeps nothing out of Git and would leave a repository that already
        // carries a credential impossible to check out at all.
        let private_name = clean
            && path
                .rsplit(|byte| *byte == b'/')
                .next()
                .is_some_and(is_private_env_name);
        let mut scanner = SecretScanner::default();
        let mut retained = Retained::new(spool_root);
        // Set once the scanner matches: the object Git records for this path,
        // if it records one. A private name is never rescued by it.
        let mut identical_to = None;
        let mut judged = false;
        if private_name {
            retained.forget();
        }
        loop {
            let Some(packet) = read_packet(&mut input)? else {
                return Err(protocol_error());
            };
            let Some(bytes) = packet else {
                break;
            };
            if clean && !judged {
                scanner.feed(&bytes);
                if scanner.detected() {
                    judged = true;
                    if !private_name {
                        identical_to = recorded.blob(path);
                    }
                    if identical_to.is_none() {
                        // Nothing this request can still turn out to be.
                        retained.forget();
                    }
                }
            }
            retained.push(&bytes)?;
        }
        let admitted = match identical_to {
            // Detected content Git already holds under this path: the same
            // bytes, arriving back through the same path, are not an arrival.
            Some(object) => match recorded.format() {
                Some(format) => retained.blob_id(format)? == object,
                None => false,
            },
            None => !judged && !private_name,
        };
        if !admitted {
            write_packet(&mut output, b"status=error\n")?;
            flush_packet(&mut output)?;
            continue;
        }
        write_packet(&mut output, b"status=success\n")?;
        flush_packet(&mut output)?;
        retained.write_to(&mut output)?;
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
    fn separates_private_environment_files_from_committed_templates() {
        for name in [
            ".env",
            ".env.local",
            ".env.production",
            ".env.development.local",
            ".env.example.local",
            ".env.staging",
            // Never a template suffix, and both have always been private.
            ".envrc",
            ".environment",
            // The suffix list is matched byte for byte, so a shouted one is
            // still refused rather than quietly admitted.
            ".env.EXAMPLE",
            ".env.Example",
        ] {
            assert!(
                is_private_env_name(name.as_bytes()),
                "{name} must stay private"
            );
        }
        for name in [
            ".env.example",
            ".env.sample",
            ".env.template",
            ".env.dist",
            ".env.defaults",
            ".env.local.example",
            ".env.production.sample",
            ".envrc.example",
        ] {
            assert!(
                !is_private_env_name(name.as_bytes()),
                "{name} is a committed template"
            );
        }
        for name in [
            "env",
            "readme.env",
            "config.env.local",
            // Case-sensitive on purpose: the `.env*` globs built from this
            // predicate are case-sensitive too, and a name this called private
            // but those globs missed could be staged into a checkpoint. An
            // uppercase file with real credentials is still caught by content.
            ".ENV",
            ".ENV.local",
        ] {
            assert!(
                !is_private_env_name(name.as_bytes()),
                "{name} is not a dotenv basename"
            );
        }
    }

    #[test]
    fn reads_the_basename_of_a_nested_path() {
        for path in [
            "apps/web/.env.local",
            "/tmp/deep/nested/.env",
            "packages/api/.envrc",
        ] {
            assert!(
                private_env_path(Path::new(path)),
                "{path} must stay private"
            );
        }
        for path in [
            "apps/web/.env.example",
            "packages/api/.env.local.template",
            // A directory whose own name looks private does not make the file
            // inside it private.
            ".env.local/notes.md",
            "src/main.rs",
        ] {
            assert!(
                !private_env_path(Path::new(path)),
                "{path} is ordinary content"
            );
        }
    }

    #[test]
    fn template_globs_agree_with_the_predicate() {
        for (pattern, suffix) in ENV_TEMPLATE_PATTERNS.iter().zip(ENV_TEMPLATE_SUFFIXES) {
            assert_eq!(*pattern, format!(".env*.{suffix}"));
        }
    }

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

    /// The identity comparison is only worth anything if it agrees with Git
    /// byte for byte, in both object formats and across the spill boundary.
    #[test]
    fn computes_the_object_ids_git_computes() {
        let directory = tempfile::tempdir().unwrap();
        for (format, content, expected) in [
            (
                ObjectFormat::Sha1,
                b"shade\n".to_vec(),
                "52c5fb22a209591d8753431443dd97c09f2170db",
            ),
            (
                ObjectFormat::Sha256,
                b"shade\n".to_vec(),
                "acd798c5fd4e7b9de9f7cee2d4a8724f77294f3137358fb9869979fbcb12a310",
            ),
            (
                ObjectFormat::Sha1,
                vec![b'x'; 2_000_000],
                "33d3ac990a0bfff103ca216f232e6fb87ea5e85a",
            ),
        ] {
            let mut retained = Retained::new(directory.path());
            for chunk in content.chunks(PACKET) {
                retained.push(chunk).unwrap();
            }
            assert_eq!(
                retained.blob_id(format).unwrap(),
                expected.as_bytes(),
                "object ID for {} bytes",
                content.len()
            );
            // Reading the bytes back to hash them must not consume them: the
            // same request is still owed to Git.
            let mut returned = Vec::new();
            retained
                .read_back(|chunk| returned.extend_from_slice(chunk))
                .unwrap();
            assert_eq!(returned, content);
        }
    }

    #[test]
    fn reads_the_object_git_records_for_a_path() {
        let listing = b"100644 aaaa 0\tsrc/main.rs\x00100644 bbbb 2\tconflicted.rs\x00100644 cccc 0\ta path/with space.txt\x00";
        let entries = records(listing)
            .filter_map(split_record)
            .filter_map(|(fields, path)| {
                let [_mode, object, b"0"] = fields[..] else {
                    return None;
                };
                Some((path.to_vec(), object.to_vec()))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            vec![
                (b"src/main.rs".to_vec(), b"aaaa".to_vec()),
                (b"a path/with space.txt".to_vec(), b"cccc".to_vec()),
            ],
            "unmerged stages record no single blob and are skipped"
        );
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
