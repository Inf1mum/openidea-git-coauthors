//! Git CoAuthors for OpenIDEA: Git Mob-style co-authored commits. A persisted
//! roster of teammates (`Name <email>`), a pick-many command that chooses the
//! current co-author set, the set in the status bar, and — from a command or
//! the button above the commit message (`commit.message` menu) —
//! `Co-authored-by: Name <email>` trailers appended to the commit the user is
//! preparing, via `pending-commit` + `set-commit-message`. Appending is
//! idempotent: a trailer is added only when its email is not already credited
//! anywhere in the message, and the user's own text is never otherwise
//! touched; `set-commit-message` refuses to overwrite a message the user
//! edited meanwhile (`stale`).
//!
//! State lives in the plugin's storage (roster and active emails), so both
//! survive IDE restarts and the host reclaiming the idle instance. The
//! `vcs-changed` event only re-renders the status bar item from storage.

use openidea_plugin::host::{self, storage};
use openidea_plugin::{Context, Error, Event, Invocation, LogLevel, PickItem, Plugin, StatusItem, register_plugin};
use serde::{Deserialize, Serialize};

/// Status bar item id.
const ITEM: &str = "coauthors";
/// Storage key of the teammates roster: a [`Vec<CoAuthor>`] as JSON.
const ROSTER: &str = "roster";
/// Storage key of the active co-author set: their emails as a JSON array.
const ACTIVE: &str = "active";
/// Teammates kept at most (each entry is ~50 bytes; the point is a sanity
/// cap, not a quota — storage allows 64 MiB).
const MAX_TEAMMATES: usize = 100;
/// Longest `Name` and longest `email` accepted from the dialog.
const MAX_PART: usize = 120;
/// Names shown in the status bar before it collapses to `first, second,
/// third, +N`.
const STATUS_NAMES: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoAuthor {
    pub name: String,
    pub email: String,
}

impl CoAuthor {
    /// The git trailer line for this person.
    pub fn trailer(&self) -> String {
        format!("Co-authored-by: {} <{}>", self.name, self.email)
    }
}

/// `Name <email>`, as the dialogs ask for it. The name may hold anything but
/// angle brackets, newlines or more than [`MAX_PART`] characters; the email
/// must be a single token with an `@`.
pub fn parse_person(input: &str) -> Result<CoAuthor, String> {
    let input = input.trim();
    let (open, close) = (input.rfind('<'), input.find('>'));
    let (name, email) = match (open, close) {
        (Some(o), Some(c)) if c > o => (input[..o].trim(), input[o + 1..c].trim()),
        _ => {
            return Err(format!("{input:?} is not Name <email> — for example: Ada Lovelace <ada@example.com>"));
        }
    };
    if name.is_empty() {
        return Err("the name before <email> is empty".into());
    }
    if name.len() > MAX_PART || email.len() > MAX_PART {
        return Err(format!("names and emails are at most {MAX_PART} characters"));
    }
    if name.contains(['<', '>', '\n', '\r']) {
        return Err("the name may not contain <, > or a line break".into());
    }
    if email.is_empty() || email.contains([' ', '\t', '<', '>', '\n', '\r']) || !email.contains('@') {
        return Err(format!("{email:?} is not an email (one token with an @)"));
    }
    Ok(CoAuthor { name: name.to_owned(), email: email.to_owned() })
}

/// Trailer values (`Ada Lovelace <ada@example.com>`) already in `message`:
/// every line whose `token: value` has the token `co-authored-by`
/// (case-insensitive), wherever in the message it sits.
fn existing_trailers(message: &str) -> Vec<String> {
    message
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (token, value) = line.split_once(':')?;
            token.trim().eq_ignore_ascii_case("co-authored-by").then(|| value.trim().to_owned())
        })
        .collect()
}

/// Whether two trailer values credit the same person: byte-equal, or the same
/// email in angle brackets (emails compare case-insensitively; a value
/// without `<email>` only matches itself).
fn same_person(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let email = |s: &str| {
        s.rsplit_once('<').and_then(|(_, rest)| rest.split_once('>')).map(|(e, _)| e.trim().to_ascii_lowercase())
    };
    match (email(a), email(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// `message` with one `Co-authored-by:` trailer per co-author not credited in
/// it yet. Trailers go into the message's last block when a blank line
/// separates that block from the rest of the message and it is already
/// nothing but `token: value` lines (git's trailer block — a subject is never
/// one), otherwise into a new final block; the rest of the message —
/// including its trailing whitespace when nothing is added — is never
/// touched. An empty message becomes just the trailers.
pub fn with_trailers(message: &str, coauthors: &[CoAuthor]) -> String {
    let mut seen = existing_trailers(message);
    let mut block: Vec<String> = Vec::new();
    for person in coauthors {
        let value = format!("{} <{}>", person.name, person.email);
        if seen.iter().any(|v| same_person(v, &value)) {
            continue;
        }
        seen.push(value);
        block.push(person.trailer());
    }
    if block.is_empty() {
        return message.to_owned();
    }
    let trailers = block.join("\n");
    let head = message.trim_end_matches([' ', '\t', '\r', '\n']);
    let tail = &message[head.len()..];
    if head.is_empty() {
        return trailers + tail;
    }
    // git's rule: the trailer block is the last paragraph only when a blank
    // line separates it from the rest of the message (a subject is never a
    // trailer block) and every one of its lines is a `token: value` line;
    // then we join it, else we open a new block after a blank line.
    let is_trailer_block = head.rsplit_once("\n\n").is_some_and(|(_, last_paragraph)| {
        last_paragraph.lines().all(|line| {
            matches!(line.trim().split_once(':'), Some((token, value)) if !token.trim().is_empty() && !value.trim().is_empty())
        })
    });
    if is_trailer_block { format!("{head}\n{trailers}{tail}") } else { format!("{head}\n\n{trailers}{tail}") }
}

/// The status bar text: `＋ co-authors` when nobody is picked, else the
/// picked names — all of them while there are at most [`STATUS_NAMES`], else
/// the first [`STATUS_NAMES`] and `+N` (a status bar item stays compact no
/// matter how long the names are).
pub fn status_text(active: &[CoAuthor]) -> String {
    if active.is_empty() {
        return "＋ co-authors".to_owned();
    }
    let names: Vec<&str> = active.iter().map(|p| p.name.as_str()).collect();
    if names.len() <= STATUS_NAMES {
        format!("co-authors: {}", names.join(", "))
    } else {
        format!("co-authors: {}, +{}", names[..STATUS_NAMES].join(", "), active.len() - STATUS_NAMES)
    }
}

fn names_of(active: &[CoAuthor]) -> String {
    active.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
}

/// Sorts the roster by name (then email), case-insensitively.
fn sort_roster(roster: &mut [CoAuthor]) {
    roster.sort_by(|a, b| {
        (a.name.to_lowercase(), a.email.to_lowercase()).cmp(&(b.name.to_lowercase(), b.email.to_lowercase()))
    });
}

fn load_roster() -> Vec<CoAuthor> {
    match storage::get(ROSTER) {
        Ok(Some(json)) => serde_json::from_str::<Vec<CoAuthor>>(&json).unwrap_or_else(|e| {
            host::log(LogLevel::Warn, &format!("roster in storage is not readable ({e}); starting over"));
            Vec::new()
        }),
        Ok(None) => Vec::new(),
        Err(e) => {
            host::log(LogLevel::Warn, &format!("reading the roster: {e}"));
            Vec::new()
        }
    }
}

fn save_roster(roster: &[CoAuthor]) -> Result<(), Error> {
    storage::set(ROSTER, &serde_json::to_string(roster).expect("a roster is JSON-serializable"))
}

/// The active emails not in the roster any more are dropped (a removed
/// teammate cannot stay picked).
fn load_active(roster: &[CoAuthor]) -> Vec<CoAuthor> {
    let Ok(Some(json)) = storage::get(ACTIVE) else {
        return Vec::new();
    };
    let Ok(emails) = serde_json::from_str::<Vec<String>>(&json) else {
        host::log(LogLevel::Warn, "the active co-authors in storage are not readable; none are picked");
        return Vec::new();
    };
    roster.iter().filter(|p| emails.iter().any(|e| e.eq_ignore_ascii_case(&p.email))).cloned().collect()
}

fn save_active(active: &[CoAuthor]) -> Result<(), Error> {
    let emails: Vec<&str> = active.iter().map(|p| p.email.as_str()).collect();
    storage::set(ACTIVE, &serde_json::to_string(&emails).expect("emails are JSON-serializable"))
}

fn refresh_status() -> Result<(), Error> {
    let roster = load_roster();
    let active = load_active(&roster);
    let tooltip = if active.is_empty() {
        format!(
            "No co-authors picked: the next commit is credited to you alone.\n\
             Roster: {} teammate(s). Click to pick co-authors (the dialog also adds a first teammate).",
            roster.len()
        )
    } else {
        format!(
            "Co-authored-by trailers on your next commit:\n{}\n\n\
             Roster: {} teammate(s). Click to pick co-authors.",
            active.iter().map(|p| p.trailer()).collect::<Vec<_>>().join("\n"),
            roster.len()
        )
    };
    host::set_status_item(
        ITEM,
        &StatusItem { text: status_text(&active), tooltip: Some(tooltip), command: Some("pick".into()) },
    );
    Ok(())
}

#[derive(Default)]
struct GitCoAuthors;

impl GitCoAuthors {
    /// `pick`: with an empty roster, first asks for a teammate (so one dialog
    /// is enough to get started); then pick-many over the roster decides the
    /// active set — confirming with nothing cleared picks nobody, dismissing
    /// changes nothing.
    fn pick(&mut self) -> Result<Option<String>, Error> {
        let mut roster = load_roster();
        if roster.is_empty()
            && let Some(text) = host::input(
                "No teammates yet. Add the first one, as Name <email> (for example Ada Lovelace <ada@example.com>)",
                None,
            )?
        {
            let person = parse_person(&text).map_err(Error::Invalid)?;
            roster.push(person);
            sort_roster(&mut roster);
            save_roster(&roster)?;
        }
        if roster.is_empty() {
            refresh_status()?;
            return Ok(Some("Still no teammates — Git CoAuthors: Add Teammate adds one.".to_owned()));
        }
        let items: Vec<PickItem> =
            roster.iter().map(|p| PickItem::new(&p.email, &p.name).description(&p.email)).collect();
        let Some(chosen) = host::pick_many(
            "Pick the co-authors to credit on your next commit (confirm with none checked to credit nobody)",
            &items,
        )?
        else {
            return Ok(None);
        };
        let active: Vec<CoAuthor> =
            roster.iter().filter(|p| chosen.iter().any(|k| k.eq_ignore_ascii_case(&p.email))).cloned().collect();
        save_active(&active)?;
        refresh_status()?;
        Ok(Some(if active.is_empty() {
            "No co-authors: the next commit is credited to you alone.".to_owned()
        } else {
            format!("Co-authors on the next commit: {}.", names_of(&active))
        }))
    }

    fn add_teammate(&mut self) -> Result<Option<String>, Error> {
        let Some(text) = host::input("New teammate, as Name <email>", None)? else {
            return Ok(None);
        };
        let person = parse_person(&text).map_err(Error::Invalid)?;
        let mut roster = load_roster();
        if let Some(slot) = roster.iter_mut().find(|p| p.email.eq_ignore_ascii_case(&person.email)) {
            if slot.name == person.name {
                return Ok(Some(format!("{} is already in the roster.", person.name)));
            }
            slot.name = person.name;
            let renamed = slot.name.clone();
            sort_roster(&mut roster);
            save_roster(&roster)?;
            return Ok(Some(format!("Renamed to {renamed} (same email, already in the roster).")));
        }
        if roster.len() >= MAX_TEAMMATES {
            return Err(Error::Invalid(format!(
                "the roster is full ({MAX_TEAMMATES} teammates) — remove one with Git CoAuthors: Remove Teammates first"
            )));
        }
        roster.push(person.clone());
        sort_roster(&mut roster);
        save_roster(&roster)?;
        Ok(Some(format!(
            "{} joined the roster ({} teammates). Pick co-authors with Git CoAuthors: Pick Co-authors.",
            person.name,
            roster.len()
        )))
    }

    fn remove_teammates(&mut self) -> Result<Option<String>, Error> {
        let roster = load_roster();
        if roster.is_empty() {
            return Ok(Some("The roster is empty — nothing to remove.".to_owned()));
        }
        let items: Vec<PickItem> =
            roster.iter().map(|p| PickItem::new(&p.email, &p.name).description(&p.email)).collect();
        let Some(chosen) =
            host::pick_many("Pick the teammates to remove (they also stop being picked as co-authors)", &items)?
        else {
            return Ok(None);
        };
        let removed: Vec<CoAuthor> =
            roster.iter().filter(|p| chosen.iter().any(|k| k.eq_ignore_ascii_case(&p.email))).cloned().collect();
        let roster: Vec<CoAuthor> =
            roster.into_iter().filter(|p| !removed.iter().any(|r| r.email == p.email)).collect();
        let active: Vec<CoAuthor> = load_active(&roster);
        save_roster(&roster)?;
        save_active(&active)?;
        refresh_status()?;
        Ok(Some(format!("Removed from the roster: {}.", names_of(&removed))))
    }

    fn clear(&mut self) -> Result<Option<String>, Error> {
        save_active(&[])?;
        refresh_status()?;
        Ok(Some("No co-authors: the next commit is credited to you alone.".to_owned()))
    }

    /// `apply`: appends the picked people's `Co-authored-by:` trailers to the
    /// commit being prepared in this window, via the commit surface. The user
    /// still reviews the message and commits.
    fn apply(&mut self) -> Result<Option<String>, Error> {
        let roster = load_roster();
        let active = load_active(&roster);
        if active.is_empty() {
            return Ok(Some(
                "No co-authors are picked — choose some with Git CoAuthors: Pick Co-authors first.".to_owned(),
            ));
        }
        let Some(draft) = host::pending_commit()? else {
            return Err(Error::Failed(
                "no commit is being prepared — open the Commit tool window (or the Commit dialog) and run this there"
                    .into(),
            ));
        };
        let message = with_trailers(&draft.message, &active);
        if message == draft.message {
            return Ok(Some(format!("The message already credits {}.", names_of(&active))));
        }
        match host::set_commit_message(draft.revision, &message) {
            Ok(()) => Ok(Some(format!(
                "Co-authored-by trailers added for {} — review the message and commit.",
                names_of(&active)
            ))),
            Err(Error::Stale) => Err(Error::Failed(
                "the commit changed while Git CoAuthors worked (the message or the included files moved on) — nothing was overwritten; run this again".into(),
            )),
            Err(e) => Err(e),
        }
    }
}

impl Plugin for GitCoAuthors {
    fn activate(&mut self, _ctx: &Context) -> Result<(), Error> {
        // The status bar item comes back from storage on every activation
        // (after idle reclaim or an IDE start).
        refresh_status()
    }

    fn run_command(&mut self, id: &str, _ctx: &Invocation, _arg: Option<&str>) -> Result<Option<String>, Error> {
        let result = match id {
            "pick" => self.pick(),
            "add-teammate" => self.add_teammate(),
            "remove-teammates" => self.remove_teammates(),
            "clear" => self.clear(),
            "apply" => self.apply(),
            _ => return Err(Error::NotFound(format!("command {id}"))),
        };
        if result.is_ok() {
            refresh_status()?;
        }
        result
    }

    fn handle_events(&mut self, events: &[Event], _ctx: &Invocation) {
        // Not needed for correctness (the item is kept while idle), but it
        // re-renders it after the host recreated the instance, and keeps the
        // roster/active split honest after a remove from another window.
        if events.iter().any(|e| matches!(e, Event::VcsChanged(_))) {
            let _ = refresh_status();
        }
    }
}

register_plugin!(GitCoAuthors);

#[cfg(test)]
mod tests {
    use super::*;
    use openidea_plugin::mock::MockHost;

    fn person(name: &str, email: &str) -> CoAuthor {
        CoAuthor { name: name.to_owned(), email: email.to_owned() }
    }

    fn draft(message: &str) -> openidea_plugin::CommitDraft {
        openidea_plugin::CommitDraft { revision: 7, message: message.to_owned(), amend: false, changes: Vec::new() }
    }

    /// The command ids `plugin.toml` registers, read from the manifest itself —
    /// the source of truth the dispatch has to match.
    fn manifest_command_ids() -> Vec<&'static str> {
        let mut in_commands = false;
        let mut ids = Vec::new();
        for line in include_str!("../plugin.toml").lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_commands = line == "[[contributes.commands]]";
            } else if in_commands && let Some(rest) = line.strip_prefix("id = ") {
                ids.push(rest.trim_matches('"'));
            }
        }
        ids
    }

    /// The trailers git recognizes in `message` (`git interpret-trailers
    /// --parse`) — the authority the resulting commit message must satisfy.
    fn git_parsed_trailers(message: &str) -> Vec<String> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("git")
            .args(["interpret-trailers", "--parse"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("git is available");
        child.stdin.take().unwrap().write_all(message.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git interpret-trailers failed");
        String::from_utf8(output.stdout).expect("git prints UTF-8").lines().map(str::to_owned).collect()
    }

    // ---- parse_person ----

    #[test]
    fn parses_name_and_email() {
        assert_eq!(parse_person("Ada Lovelace <ada@example.com>").unwrap(), person("Ada Lovelace", "ada@example.com"));
        assert_eq!(parse_person("  Bob <BOB@x.io>  ").unwrap(), person("Bob", "BOB@x.io"));
    }

    #[test]
    fn rejects_bad_people() {
        assert!(parse_person("").is_err());
        assert!(parse_person("no brackets at all").is_err());
        assert!(parse_person("backwards >a<").is_err());
        assert!(parse_person("Ada <ada example.com>").is_err(), "no @");
        assert!(parse_person("Ada <>").is_err(), "empty email");
        assert!(parse_person("<ada@example.com>").is_err(), "empty name");
        assert!(parse_person("Ada <a@x.io").is_err(), "unclosed bracket");
        let long = "x".repeat(MAX_PART + 1);
        assert!(parse_person(format!("{long} <a@x.io>").as_str()).is_err(), "name too long");
    }

    // ---- with_trailers ----

    #[test]
    fn appends_after_prose_with_a_blank_line() {
        let out = with_trailers("Fix the widget\n\nThe widget was sad.", &[person("Ada Lovelace", "ada@example.com")]);
        assert_eq!(out, "Fix the widget\n\nThe widget was sad.\n\nCo-authored-by: Ada Lovelace <ada@example.com>");
    }

    #[test]
    fn joins_an_existing_trailer_block_without_a_blank_line() {
        let message = "Fix the widget\n\nReviewed-by: Grace Hopper <grace@example.com>";
        let out = with_trailers(message, &[person("Ada Lovelace", "ada@example.com")]);
        assert_eq!(
            out,
            "Fix the widget\n\nReviewed-by: Grace Hopper <grace@example.com>\nCo-authored-by: Ada Lovelace <ada@example.com>"
        );
    }

    #[test]
    fn dedupes_by_email_case_insensitively_and_anywhere_in_the_message() {
        let message = "Work\n\nCo-authored-by: Original Name <ada@example.com>";
        let out = with_trailers(message, &[person("Ada Lovelace", "ADA@EXAMPLE.COM")]);
        assert_eq!(out, message, "same email, different case and name: not duplicated");
        let out = with_trailers(
            "Co-authored-by: Ada <ada@example.com>\n\nbody",
            &[person("Ada Lovelace", "ada@example.com")],
        );
        assert_eq!(out, "Co-authored-by: Ada <ada@example.com>\n\nbody", "a trailer outside the footer still counts");
    }

    #[test]
    fn leaves_the_message_untouched_when_everyone_is_already_credited() {
        let message = "Work\n\nCo-authored-by: Ada Lovelace <ada@example.com>";
        assert_eq!(with_trailers(message, &[person("Ada Lovelace", "ada@example.com")]), message);
        assert_eq!(with_trailers("untouched", &[]), "untouched", "no co-authors, no change");
    }

    #[test]
    fn dedupes_within_the_picked_set_itself() {
        let out = with_trailers("Work", &[person("Ada", "ada@example.com"), person("Ada Lovelace", "ADA@example.com")]);
        assert_eq!(out.lines().filter(|l| l.starts_with("Co-authored-by:")).count(), 1);
    }

    #[test]
    fn an_empty_message_becomes_just_the_trailers() {
        let out = with_trailers("", &[person("Ada Lovelace", "ada@example.com")]);
        assert_eq!(out, "Co-authored-by: Ada Lovelace <ada@example.com>");
    }

    #[test]
    fn a_prose_paragraph_with_colons_is_not_treated_as_trailers() {
        let message = "Note:\nthis explains the colon line";
        let out = with_trailers(message, &[person("Ada", "a@x.io")]);
        assert_eq!(out, format!("{message}\n\nCo-authored-by: Ada <a@x.io>"));
    }

    #[test]
    fn a_colon_subject_is_not_a_trailer_block() {
        let out = with_trailers("fix: bug\n", &[person("Ada Lovelace", "ada@example.com")]);
        assert_eq!(
            out, "fix: bug\n\nCo-authored-by: Ada Lovelace <ada@example.com>\n",
            "git requires a blank line before the trailer block"
        );
        let parsed = git_parsed_trailers(&out);
        assert_eq!(parsed, vec!["Co-authored-by: Ada Lovelace <ada@example.com>".to_owned()]);
    }

    #[test]
    fn trailers_of_several_people_keep_the_picked_order() {
        let out = with_trailers("Work", &[person("Ada", "a@x.io"), person("Bob", "b@x.io")]);
        assert_eq!(out, "Work\n\nCo-authored-by: Ada <a@x.io>\nCo-authored-by: Bob <b@x.io>");
    }

    // ---- status bar ----

    #[test]
    fn status_text_shows_the_picked_names() {
        assert_eq!(status_text(&[]), "＋ co-authors");
        assert_eq!(status_text(&[person("Ada", "a@x.io")]), "co-authors: Ada");
        let three = vec![person("Ada", "a@x.io"), person("Bob", "b@x.io"), person("Cid", "c@x.io")];
        assert_eq!(status_text(&three), "co-authors: Ada, Bob, Cid");
        let mut many = three.clone();
        many.push(person("Dee", "d@x.io"));
        assert_eq!(status_text(&many), "co-authors: Ada, Bob, Cid, +1");
    }

    #[test]
    fn three_long_names_are_not_collapsed() {
        let three_long = vec![
            person("A Very Long Teammate Name Indeed", "a@x.io"),
            person("Another Long Name Here", "b@x.io"),
            person("And A Third Long One", "c@x.io"),
        ];
        let joined = three_long.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
        assert_eq!(status_text(&three_long), format!("co-authors: {joined}"));
    }

    #[test]
    fn the_status_item_carries_the_picker_and_the_trailers_in_its_tooltip() {
        let host = MockHost::install();
        storage::set(ROSTER, &serde_json::to_string(&[person("Ada", "a@x.io")]).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io"]).unwrap()).unwrap();
        refresh_status().unwrap();
        let item = &host.status_items()[ITEM];
        assert_eq!(item.text, "co-authors: Ada");
        assert_eq!(item.command.as_deref(), Some("pick"));
        assert!(item.tooltip.as_deref().unwrap().contains("Co-authored-by: Ada <a@x.io>"));
    }

    // ---- commands ----

    fn roster_of(host: &MockHost) -> Vec<CoAuthor> {
        serde_json::from_str(&host.storage()[ROSTER]).unwrap()
    }

    fn active_of(host: &MockHost) -> Vec<String> {
        serde_json::from_str(&host.storage()[ACTIVE]).unwrap_or_default()
    }

    #[test]
    fn first_pick_adds_the_first_teammate_then_picks() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        host.answer_next_prompt(Some(&["Ada Lovelace <ada@example.com>"]));
        host.answer_next_prompt(Some(&["ada@example.com"]));
        let message = plugin.run_command("pick", &host.invocation(), None).unwrap().unwrap();
        assert!(message.contains("Ada Lovelace"), "{message}");
        assert_eq!(roster_of(&host), vec![person("Ada Lovelace", "ada@example.com")]);
        assert_eq!(active_of(&host), vec!["ada@example.com"]);
        assert_eq!(host.status_items()[ITEM].text, "co-authors: Ada Lovelace");
    }

    #[test]
    fn dismissing_the_picker_changes_nothing() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        storage::set(ROSTER, &serde_json::to_string(&[person("Ada", "a@x.io")]).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io"]).unwrap()).unwrap();
        let outcome = plugin.run_command("pick", &host.invocation(), None).unwrap();
        assert!(outcome.is_none(), "dismissed: no message");
        assert_eq!(active_of(&host), vec!["a@x.io"]);
    }

    #[test]
    fn confirming_an_empty_picker_clears_the_set() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        storage::set(ROSTER, &serde_json::to_string(&[person("Ada", "a@x.io")]).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io"]).unwrap()).unwrap();
        host.answer_next_prompt(Some(&[]));
        let message = plugin.run_command("pick", &host.invocation(), None).unwrap().unwrap();
        assert_eq!(message, "No co-authors: the next commit is credited to you alone.");
        assert!(active_of(&host).is_empty());
        assert_eq!(host.status_items()[ITEM].text, "＋ co-authors");
    }

    #[test]
    fn an_empty_roster_without_a_teammate_says_so() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        host.answer_next_prompt(None); // dismissed the first-teammate dialog
        let message = plugin.run_command("pick", &host.invocation(), None).unwrap().unwrap();
        assert!(message.contains("Add Teammate"), "{message}");
    }

    #[test]
    fn add_teammate_parses_sorts_and_dedupes_by_email() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        host.answer_next_prompt(Some(&["Bob <bob@x.io>"]));
        let message = plugin.run_command("add-teammate", &host.invocation(), None).unwrap().unwrap();
        assert!(message.contains("Bob"), "{message}");
        host.answer_next_prompt(Some(&["Ada Lovelace <ada@x.io>"]));
        plugin.run_command("add-teammate", &host.invocation(), None).unwrap().unwrap();
        assert_eq!(
            roster_of(&host),
            vec![person("Ada Lovelace", "ada@x.io"), person("Bob", "bob@x.io")],
            "sorted by name"
        );
        host.answer_next_prompt(Some(&["Ada Lovelace II <ADA@X.IO>"]));
        let message = plugin.run_command("add-teammate", &host.invocation(), None).unwrap().unwrap();
        assert!(message.contains("Renamed"), "{message}");
        assert_eq!(
            roster_of(&host),
            vec![person("Ada Lovelace II", "ada@x.io"), person("Bob", "bob@x.io")],
            "same email: renamed, not duplicated"
        );
        host.answer_next_prompt(Some(&["Broken <no email>"]));
        assert!(plugin.run_command("add-teammate", &host.invocation(), None).is_err(), "invalid input is an error");
        assert_eq!(roster_of(&host).len(), 2, "nothing was added by the failed parse");
    }

    #[test]
    fn a_dismissed_add_dialog_adds_nothing() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        let outcome = plugin.run_command("add-teammate", &host.invocation(), None).unwrap();
        assert!(outcome.is_none());
        assert!(!host.storage().contains_key(ROSTER));
    }

    #[test]
    fn remove_teammates_also_unpicks_them() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        let roster = vec![person("Ada", "a@x.io"), person("Bob", "b@x.io")];
        storage::set(ROSTER, &serde_json::to_string(&roster).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io", "b@x.io"]).unwrap()).unwrap();
        host.answer_next_prompt(Some(&["a@x.io"]));
        let message = plugin.run_command("remove-teammates", &host.invocation(), None).unwrap().unwrap();
        assert!(message.contains("Ada"), "{message}");
        assert_eq!(roster_of(&host), vec![person("Bob", "b@x.io")]);
        assert_eq!(active_of(&host), vec!["b@x.io"], "a removed teammate stops being picked");
        assert_eq!(host.status_items()[ITEM].text, "co-authors: Bob");
    }

    #[test]
    fn every_command_the_manifest_registers_is_dispatched() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        let ids = manifest_command_ids();
        assert!(ids.contains(&"remove-teammates"), "the manifest registers Remove Teammates");
        for id in ids {
            let outcome = plugin.run_command(id, &host.invocation(), None);
            assert!(
                !matches!(outcome, Err(Error::NotFound(_))),
                "the manifest registers {id}, but run_command does not handle it: {outcome:?}"
            );
        }
    }

    #[test]
    fn clear_unpicks_everyone() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        storage::set(ROSTER, &serde_json::to_string(&[person("Ada", "a@x.io")]).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io"]).unwrap()).unwrap();
        plugin.run_command("clear", &host.invocation(), None).unwrap().unwrap();
        assert!(active_of(&host).is_empty());
        assert_eq!(roster_of(&host).len(), 1, "clearing keeps the roster");
        assert_eq!(host.status_items()[ITEM].text, "＋ co-authors");
    }

    // ---- apply (the commit surface) ----

    #[test]
    fn apply_appends_the_trailers_to_the_pending_commit() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        let roster = vec![person("Ada Lovelace", "ada@example.com"), person("Bob", "b@x.io")];
        storage::set(ROSTER, &serde_json::to_string(&roster).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["ada@example.com"]).unwrap()).unwrap();
        host.set_commit_draft(Some(draft("Pair work on the parser\n")));
        let message = plugin.run_command("apply", &host.invocation(), None).unwrap().unwrap();
        assert!(message.contains("Ada Lovelace"), "{message}");
        let new_draft = host.commit_draft().unwrap();
        assert_eq!(
            new_draft.message, "Pair work on the parser\n\nCo-authored-by: Ada Lovelace <ada@example.com>\n",
            "the message's own trailing newline is kept after the new trailer block"
        );
        assert_eq!(new_draft.revision, 8, "the mock bumps the revision on set");
        assert!(message.contains("review the message"), "the user still commits themselves");
    }

    #[test]
    fn apply_on_a_colon_subject_produces_a_trailer_git_parses() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        let roster = vec![person("Ada Lovelace", "ada@example.com")];
        storage::set(ROSTER, &serde_json::to_string(&roster).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["ada@example.com"]).unwrap()).unwrap();
        host.set_commit_draft(Some(draft("fix: bug\n")));
        let message = plugin.run_command("apply", &host.invocation(), None).unwrap().unwrap();
        assert!(message.contains("review the message"), "{message}");
        let new_draft = host.commit_draft().unwrap();
        assert_eq!(
            new_draft.message, "fix: bug\n\nCo-authored-by: Ada Lovelace <ada@example.com>\n",
            "the subject gets a blank line of its own before the trailer block"
        );
        assert_eq!(
            git_parsed_trailers(&new_draft.message),
            vec!["Co-authored-by: Ada Lovelace <ada@example.com>".to_owned()],
            "git recognizes the co-author trailer"
        );
    }

    #[test]
    fn apply_is_idempotent() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        let roster = vec![person("Ada", "a@x.io")];
        storage::set(ROSTER, &serde_json::to_string(&roster).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io"]).unwrap()).unwrap();
        let already = draft("Work\n\nCo-authored-by: Ada <a@x.io>");
        host.set_commit_draft(Some(already.clone()));
        let message = plugin.run_command("apply", &host.invocation(), None).unwrap().unwrap();
        assert!(message.contains("already credits"), "{message}");
        assert_eq!(host.commit_draft().unwrap(), already, "nothing was rewritten");
    }

    #[test]
    fn apply_without_picked_coauthors_touches_nothing() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        storage::set(ROSTER, &serde_json::to_string(&[person("Ada", "a@x.io")]).unwrap()).unwrap();
        host.set_commit_draft(Some(draft("Work")));
        let message = plugin.run_command("apply", &host.invocation(), None).unwrap().unwrap();
        assert!(message.contains("Pick Co-authors"), "{message}");
        assert_eq!(host.commit_draft().unwrap().message, "Work");
    }

    #[test]
    fn apply_outside_the_commit_window_is_an_error() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        storage::set(ROSTER, &serde_json::to_string(&[person("Ada", "a@x.io")]).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io"]).unwrap()).unwrap();
        let err = plugin.run_command("apply", &host.invocation(), None).unwrap_err();
        assert!(err.to_string().contains("no commit is being prepared"), "{err}");
    }

    #[test]
    fn apply_honours_the_capability_gates() {
        let host = MockHost::install().with_capabilities(&["storage", "vcs.write"]);
        let mut plugin = GitCoAuthors;
        storage::set(ROSTER, &serde_json::to_string(&[person("Ada", "a@x.io")]).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io"]).unwrap()).unwrap();
        host.set_commit_draft(Some(draft("Work")));
        let err = plugin.run_command("apply", &host.invocation(), None).unwrap_err();
        assert!(err.to_string().contains("vcs.read"), "{err}");
        let host = MockHost::install().with_capabilities(&["storage", "vcs.read"]);
        let mut plugin = GitCoAuthors;
        storage::set(ROSTER, &serde_json::to_string(&[person("Ada", "a@x.io")]).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io"]).unwrap()).unwrap();
        host.set_commit_draft(Some(draft("Work")));
        let err = plugin.run_command("apply", &host.invocation(), None).unwrap_err();
        assert!(err.to_string().contains("vcs.write"), "{err}");
        assert_eq!(host.commit_draft().unwrap().message, "Work", "nothing was written");
    }

    #[test]
    fn pick_without_storage_is_denied() {
        let host = MockHost::install().with_capabilities(&["vcs.read", "vcs.write"]);
        let mut plugin = GitCoAuthors;
        host.answer_next_prompt(Some(&["Ada <a@x.io>"]));
        let err = plugin.run_command("pick", &host.invocation(), None).unwrap_err();
        assert!(err.to_string().contains("storage"), "{err}");
    }

    // ---- lifecycle ----

    #[test]
    fn activation_renders_the_item_from_storage() {
        let host = MockHost::install();
        assert!(host.status_items().is_empty());
        let mut plugin = GitCoAuthors;
        plugin.activate(&host.context("inf1mum.git-coauthors")).unwrap();
        assert_eq!(host.status_items()[ITEM].text, "＋ co-authors", "nobody picked yet");
        assert_eq!(host.status_items()[ITEM].command.as_deref(), Some("pick"));
    }

    #[test]
    fn vcs_changed_renders_the_item_again() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        storage::set(ROSTER, &serde_json::to_string(&[person("Ada", "a@x.io")]).unwrap()).unwrap();
        storage::set(ACTIVE, &serde_json::to_string(&["a@x.io"]).unwrap()).unwrap();
        plugin.handle_events(&[Event::VcsChanged("/project".into())], &host.invocation());
        assert_eq!(host.status_items()[ITEM].text, "co-authors: Ada");
    }

    #[test]
    fn an_unknown_command_is_not_found() {
        let host = MockHost::install();
        let mut plugin = GitCoAuthors;
        let err = plugin.run_command("nope", &host.invocation(), None).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }
}
