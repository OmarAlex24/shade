//! Text rendering for `--human`.
//!
//! Default output is unchanged: one minified JSON value per invocation.
//! Nothing here runs unless a person asked for it, so no script and no SDK
//! sees any of it. Formatting is hand-rolled because a table this small does
//! not justify a dependency, and every renderer reads one of the small structs
//! below -- a later tier adds a column by adding a field, not by rewriting a
//! caller.

use serde_json::Value;
use std::fmt::Write as _;

/// Two spaces between columns: enough to read, cheap enough to keep a wide
/// table inside a terminal.
const GAP: &str = "  ";
/// A session id is a name a person chose and can be arbitrarily long. The
/// table stays readable; the full id is one `--session` away, and always in
/// the JSON.
const SESSION_WIDTH: usize = 28;
const STATUS_HEADERS: [&str; 8] = [
    "WORKSPACE",
    "REPO",
    "STATE",
    "SESSION",
    "AGE",
    "SIZE",
    "PARKED",
    "PATH",
];

/// One workspace, already reduced to what the table prints.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRow {
    pub workspace: String,
    /// The repository identity; `repository_label` shortens it for the column.
    pub repository: String,
    pub state: String,
    pub session: Option<String>,
    /// When the record last changed. `None` prints as `-`.
    pub updated_at_ms: Option<i64>,
    /// Disk held by this workspace alone. `None` prints as `-`: measuring it
    /// means walking the tree, which a listing must not do.
    pub bytes: Option<u64>,
    /// Absent when the workspace holds no materialized tree.
    pub path: Option<String>,
    /// Disk this workspace's suspension holds on the park volume. `None`
    /// prints as `-`: either nothing was parked, or the park that was taken
    /// no longer describes the checkpoint this workspace would wake from.
    pub parked_bytes: Option<u64>,
}

/// Everything `status --human` prints, with the clock it measures ages
/// against passed in so a test can pin it.
#[derive(Debug, Clone, Default)]
pub struct StatusReport {
    pub rows: Vec<WorkspaceRow>,
    pub now_ms: i64,
}

impl StatusReport {
    pub fn render(&self) -> String {
        if self.rows.is_empty() {
            return "no workspaces\n".to_owned();
        }
        let rows: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|row| {
                vec![
                    short_id(&row.workspace),
                    repository_label(&row.repository),
                    row.state.clone(),
                    row.session
                        .as_deref()
                        .map_or_else(dash, |session| truncate(session, SESSION_WIDTH)),
                    row.updated_at_ms
                        .map_or_else(dash, |updated| age(self.now_ms, updated)),
                    row.bytes.map_or_else(dash, bytes),
                    row.parked_bytes.map_or_else(dash, bytes),
                    row.path.clone().unwrap_or_else(dash),
                ]
            })
            .collect();
        table(&STATUS_HEADERS, &rows)
    }
}

/// A flat `key  value` block, anything that needs its own paragraph, and what
/// a person should act on. `doctor` and a single-session `status` are the same
/// shape, so they share one renderer.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub fields: Vec<(String, String)>,
    pub blocks: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

/// What a person reads first. Everything else follows in the order the daemon
/// serialized it -- alphabetical -- so a count added later appears on its own
/// without anyone touching this list.
const LEADING: [&str; 6] = [
    "state",
    "protocol",
    "root",
    "sessions",
    "workspaces",
    "operations",
];

impl Report {
    pub fn from_value(value: &Value) -> Self {
        let mut report = Self::default();
        let Some(object) = value.as_object() else {
            report.blocks.push(("result".to_owned(), scalar(value)));
            return report;
        };
        for key in LEADING {
            if let Some(value) = object.get(key) {
                report.push(key, value);
            }
        }
        for (key, value) in object {
            if !LEADING.contains(&key.as_str()) {
                report.push(key, value);
            }
        }
        report
    }

    fn push(&mut self, key: &str, value: &Value) {
        // A nested object is still key/value news -- `status` merges the
        // client-side keepalive into its answer -- so it is flattened onto
        // dotted keys instead of printed back at a person as raw JSON.
        if let Value::Object(nested) = value
            && !nested.is_empty()
        {
            for (child, value) in nested {
                self.push(&format!("{key}.{child}"), value);
            }
            return;
        }
        let rendered = scalar(value);
        if rendered.contains('\n') {
            self.blocks.push((key.to_owned(), rendered));
        } else {
            self.fields.push((key.to_owned(), rendered));
        }
    }

    pub fn render(&self) -> String {
        let width = self
            .fields
            .iter()
            .map(|(key, _)| key.chars().count())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for (key, value) in &self.fields {
            let _ = writeln!(out, "{key:<width$}{GAP}{value}");
        }
        for (key, value) in &self.blocks {
            let _ = writeln!(out, "\n{key}:");
            for line in value.lines() {
                let _ = writeln!(out, "  {line}");
            }
        }
        if !self.warnings.is_empty() {
            let _ = writeln!(out, "\nwarnings:");
            for warning in &self.warnings {
                let _ = writeln!(out, "  {warning}");
            }
        }
        out
    }
}

/// `doctor` health, with the counts that ask for an action turned into
/// warnings. Silence below the block is the good news.
pub fn doctor(value: &Value) -> Report {
    let mut report = Report::from_value(value);
    report.warnings = doctor_warnings(value);
    report
}

fn doctor_warnings(value: &Value) -> Vec<String> {
    let count = |key: &str| value.get(key).and_then(Value::as_i64).unwrap_or(0);
    let mut warnings = Vec::new();
    if let Some(state) = value.get("state").and_then(Value::as_str)
        && state != "ok"
    {
        warnings.push(format!("state database integrity check reports `{state}`"));
    }
    let awaiting = count("workspaces_failed_awaiting_review");
    if awaiting > 0 {
        warnings.push(format!(
            "{awaiting} failed workspace(s) held out of collection by a pending review: \
             shade review resolve <id>"
        ));
    }
    // The two counts overlap, and saying the same disk twice reads as two
    // problems. Only the remainder is actually collectible.
    let collectible = count("workspaces_failed") - awaiting;
    if collectible > 0 {
        warnings.push(format!(
            "{collectible} failed workspace(s) queued for collection: shade gc"
        ));
    }
    let suspending = count("workspaces_suspending");
    if suspending > 0 {
        warnings.push(format!(
            "{suspending} workspace(s) stuck mid-sleep; the next daemon sweep settles them"
        ));
    }
    let unwakeable = count("suspended_without_checkpoint");
    if unwakeable > 0 {
        warnings.push(format!(
            "{unwakeable} suspended workspace(s) have no sleep checkpoint and cannot be woken"
        ));
    }
    // Not a failure and not a misconfiguration: an external volume is
    // unplugged far more often than it is wrong. It is worth one line because
    // every sleep until it comes back discards the build output it would have
    // kept, and every wake rebuilds what is sitting on the disk in the drawer.
    if value.get("park_root").is_some_and(|root| !root.is_null())
        && !value
            .get("park_mounted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        warnings.push(
            "park root configured but not mounted: sleep discards build output until it is back"
                .to_owned(),
        );
    }
    warnings
}

/// An error in `--human` mode is one line on stderr and a non-zero exit: never
/// half a table, and never JSON a person did not ask for.
pub fn error_line(code: &str, next: Option<&str>, diagnostics_id: Option<&str>) -> String {
    let mut line = format!("shade: {code}");
    if let Some(next) = next {
        let _ = write!(line, ": {next}");
    }
    if let Some(id) = diagnostics_id {
        let _ = write!(line, " (diagnostics {id})");
    }
    line
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => dash(),
        other => other.to_string(),
    }
}

fn dash() -> String {
    "-".to_owned()
}

/// Padded columns, two spaces apart, no trailing whitespace on any line.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|head| head.chars().count()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    let mut out = String::new();
    write_row(
        &mut out,
        headers.iter().map(|head| (*head).to_owned()),
        &widths,
    );
    for row in rows {
        write_row(&mut out, row.iter().cloned(), &widths);
    }
    out
}

fn write_row(out: &mut String, cells: impl Iterator<Item = String>, widths: &[usize]) {
    let mut line = String::new();
    for (index, cell) in cells.enumerate() {
        if index > 0 {
            line.push_str(GAP);
        }
        let width = widths.get(index).copied().unwrap_or(0);
        let _ = write!(line, "{cell:<width$}");
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

/// `3m`, `2h`, `5d`. One unit is what a person needs to judge staleness, and a
/// clock that ran backwards reads as `0s` rather than as a negative age.
fn age(now_ms: i64, then_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(then_ms).max(0) / 1000;
    match seconds {
        seconds if seconds < 60 => format!("{seconds}s"),
        seconds if seconds < 3_600 => format!("{}m", seconds / 60),
        seconds if seconds < 86_400 => format!("{}h", seconds / 3_600),
        seconds => format!("{}d", seconds / 86_400),
    }
}

/// `1.2 GiB`, `4.0 KiB`, `512 B`. Scaled and rounded in integers, so the
/// printed number never drifts from the byte count it came from.
fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0_usize;
    let mut divisor = 1_u64;
    while value / divisor >= 1024 && unit + 1 < UNITS.len() {
        divisor *= 1024;
        unit += 1;
    }
    if unit == 0 {
        return format!("{value} {}", UNITS[0]);
    }
    let mut whole = value / divisor;
    let mut tenths = ((value % divisor) * 10 + divisor / 2) / divisor;
    if tenths == 10 {
        whole += 1;
        tenths = 0;
    }
    // Rounding can carry a value past its own unit. `1073741823` is 1023.99..
    // MiB, and printing `1024.0 MiB` next to a `1.0 GiB` row is the kind of
    // detail that makes a person distrust the whole table.
    if whole == 1024 && unit + 1 < UNITS.len() {
        whole = 1;
        unit += 1;
    }
    format!("{whole}.{tenths} {}", UNITS[unit])
}

/// `ws_01M1YFF5Q2GFY45338G4HX6PZA` becomes `ws_…HX6PZA`. Only a prefixed ULID
/// is shortened: the random tail is the part that distinguishes two of them,
/// and anything else -- a session id, say -- is a name a person chose.
fn short_id(id: &str) -> String {
    let Some((prefix, body)) = id.split_once('_') else {
        return id.to_owned();
    };
    if body.len() != 26
        || !body
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
    {
        return id.to_owned();
    }
    format!("{prefix}_…{}", &body[body.len() - 6..])
}

/// `ssh+scp://git@github.com/OmarAlex24/zumith-studio` becomes
/// `OmarAlex24/zumith-studio`; a `file://` identity keeps its last segment.
fn repository_label(identity: &str) -> String {
    let trimmed = identity.strip_suffix(".git").unwrap_or(identity);
    let (scheme, rest) = trimmed.split_once("://").unwrap_or(("", trimmed));
    let keep = if scheme == "file" { 1 } else { 2 };
    let mut segments: Vec<&str> = rest
        .rsplit('/')
        .filter(|segment| !segment.is_empty())
        .take(keep)
        .collect();
    if segments.is_empty() {
        return trimmed.to_owned();
    }
    segments.reverse();
    segments.join("/")
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let kept: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60_000;

    #[test]
    fn an_age_carries_one_unit_and_never_runs_backwards() {
        let now = 10 * 24 * 60 * MINUTE;
        assert_eq!(age(now, now), "0s");
        assert_eq!(age(now, now - 45_000), "45s");
        assert_eq!(age(now, now - 3 * MINUTE), "3m");
        assert_eq!(age(now, now - 59 * MINUTE), "59m");
        assert_eq!(age(now, now - 2 * 60 * MINUTE), "2h");
        assert_eq!(age(now, now - 5 * 24 * 60 * MINUTE), "5d");
        assert_eq!(age(now, now + MINUTE), "0s");
    }

    #[test]
    fn bytes_scale_to_one_decimal_without_leaving_the_integers() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(4096), "4.0 KiB");
        assert_eq!(bytes(1_288_490_188), "1.2 GiB");
        // Rounds up into the next whole unit instead of printing `1024.0`.
        assert_eq!(bytes(1_073_741_823), "1.0 GiB");
        assert_eq!(bytes(2 * 1024 * 1024 * 1024 * 1024), "2.0 TiB");
    }

    #[test]
    fn a_table_pads_every_column_but_the_last() {
        let rendered = table(
            &["A", "BB"],
            &[
                vec!["one".to_owned(), "x".to_owned()],
                vec!["2".to_owned(), "yy".to_owned()],
            ],
        );
        assert_eq!(rendered, "A    BB\none  x\n2    yy\n");
        for line in rendered.lines() {
            assert_eq!(line, line.trim_end(), "no line may carry trailing spaces");
        }
    }

    #[test]
    fn only_a_prefixed_ulid_is_shortened() {
        assert_eq!(short_id("ws_01M1YFF5Q2GFY45338G4HX6PZA"), "ws_…HX6PZA");
        assert_eq!(short_id("mig-zenith-studio-main"), "mig-zenith-studio-main");
        assert_eq!(short_id("task_42"), "task_42");
    }

    #[test]
    fn a_repository_label_keeps_the_part_a_person_recognises() {
        assert_eq!(
            repository_label("ssh+scp://git@github.com/OmarAlex24/zumith-studio"),
            "OmarAlex24/zumith-studio"
        );
        assert_eq!(
            repository_label("https://github.com/decodelabs/shade.git"),
            "decodelabs/shade"
        );
        assert_eq!(
            repository_label("file:///private/tmp/ignorefix"),
            "ignorefix"
        );
    }

    #[test]
    fn the_status_table_aligns_and_says_what_is_unknown() {
        let now = 1_000_000_000;
        let report = StatusReport {
            now_ms: now,
            rows: vec![
                WorkspaceRow {
                    workspace: "ws_01M1YFF5Q2GFY45338G4HX6PZA".into(),
                    repository: "ssh+scp://git@github.com/OmarAlex24/zumith-studio".into(),
                    state: "ready".into(),
                    session: Some("task-42".into()),
                    updated_at_ms: Some(now - 2 * 60 * MINUTE),
                    bytes: Some(1_288_490_188),
                    path: Some("~/work/ws".into()),
                    parked_bytes: None,
                },
                // The row the parked tier exists for: no tree, no size, and
                // its build output waiting on another volume.
                WorkspaceRow {
                    workspace: "ws_01M1YHFZ79H1F2FV26T0YGMCWK".into(),
                    repository: "file:///private/tmp/ignorefix".into(),
                    state: "suspended".into(),
                    parked_bytes: Some(3 * 1024 * 1024 * 1024),
                    ..WorkspaceRow::default()
                },
            ],
        };
        let rendered = report.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0],
            "WORKSPACE   REPO                      STATE      SESSION  AGE  SIZE     PARKED   PATH"
        );
        assert_eq!(
            lines[1],
            "ws_…HX6PZA  OmarAlex24/zumith-studio  ready      task-42  2h   1.2 GiB  -        ~/work/ws"
        );
        // Every column starts at the same offset as the row above it, and the
        // unknowns of a workspace with no tree are dashes, not blanks.
        assert_eq!(
            lines[2],
            "ws_…YGMCWK  ignorefix                 suspended  -        -    -        3.0 GiB  -"
        );
    }

    #[test]
    fn an_empty_inventory_says_so_instead_of_printing_a_header() {
        assert_eq!(StatusReport::default().render(), "no workspaces\n");
    }

    #[test]
    fn doctor_leads_with_the_fields_a_person_reads_first() {
        let health = serde_json::json!({
            "workspaces": 5,
            "sessions": 0,
            "state": "ok",
            "protocol": 1,
            "root": "/tmp/root",
            "operations": 0,
            "tracked_secret_matches": 10,
        });
        let rendered = doctor(&health).render();
        let keys: Vec<&str> = rendered
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(
            keys,
            [
                "state",
                "protocol",
                "root",
                "sessions",
                "workspaces",
                "operations",
                "tracked_secret_matches"
            ]
        );
        // Keys are padded to the widest one, so the values line up in a column.
        assert_eq!(rendered.lines().next(), Some("state                   ok"));
        assert!(!rendered.contains("warnings"), "a healthy root is quiet");
    }

    #[test]
    fn doctor_warns_once_per_condition_and_never_counts_the_same_disk_twice() {
        let health = serde_json::json!({
            "state": "ok",
            "workspaces_failed": 3,
            "workspaces_failed_awaiting_review": 1,
            "workspaces_suspending": 2,
            "suspended_without_checkpoint": 0,
        });
        let warnings = doctor(&health).warnings;
        assert_eq!(warnings.len(), 3);
        assert!(warnings[0].starts_with("1 failed workspace(s) held out"));
        assert!(warnings[1].starts_with("2 failed workspace(s) queued"));
        assert!(warnings[2].starts_with("2 workspace(s) stuck mid-sleep"));

        let broken = serde_json::json!({"state": "malformed index"});
        assert_eq!(
            doctor(&broken).warnings,
            ["state database integrity check reports `malformed index`"]
        );
    }

    #[test]
    fn an_unconfigured_park_root_is_a_dash_and_an_unplugged_one_is_a_warning() {
        let off = serde_json::json!({
            "state": "ok",
            "park_root": Value::Null,
            "park_mounted": false,
            "parks": 0,
            "park_bytes": 0,
        });
        let report = doctor(&off);
        assert!(
            report
                .fields
                .contains(&("park_root".to_owned(), "-".to_owned()))
        );
        assert!(
            report.warnings.is_empty(),
            "a tier nobody turned on has nothing to say"
        );

        let unplugged = serde_json::json!({
            "state": "ok",
            "park_root": "/Volumes/dev-disk/shade-park",
            "park_mounted": false,
            "parks": 2,
            "park_bytes": 8_589_934_592_i64,
        });
        assert_eq!(
            doctor(&unplugged).warnings,
            ["park root configured but not mounted: sleep discards build output until it is back"]
        );

        let mounted = serde_json::json!({
            "state": "ok",
            "park_root": "/Volumes/dev-disk/shade-park",
            "park_mounted": true,
        });
        assert!(doctor(&mounted).warnings.is_empty());
    }

    #[test]
    fn a_multi_line_value_becomes_a_block_under_the_fields() {
        let record = serde_json::json!({"code": "CLI_FAILED", "message": "one\ntwo"});
        let rendered = Report::from_value(&record).render();
        assert_eq!(rendered, "code  CLI_FAILED\n\nmessage:\n  one\n  two\n");
    }

    #[test]
    fn a_nested_object_is_flattened_onto_dotted_keys() {
        let status = serde_json::json!({
            "session": "task-42",
            "keepalive": {"running": false, "reason": "not_running"},
        });
        let fields = Report::from_value(&status).fields;
        assert_eq!(
            fields,
            [
                ("keepalive.reason".to_owned(), "not_running".to_owned()),
                ("keepalive.running".to_owned(), "false".to_owned()),
                ("session".to_owned(), "task-42".to_owned()),
            ]
        );
    }

    #[test]
    fn an_error_line_names_the_code_and_the_way_out() {
        assert_eq!(
            error_line("DAEMON_NOT_RUNNING", Some("shade install"), None),
            "shade: DAEMON_NOT_RUNNING: shade install"
        );
        assert_eq!(
            error_line("CLI_FAILED", None, Some("diag_1")),
            "shade: CLI_FAILED (diagnostics diag_1)"
        );
    }
}
