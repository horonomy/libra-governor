//! `libra-governor statusline presentation` (HORO-1709) — the one
//! Horonom-owned record of how the user wants Libra's budget segment
//! worded.
//!
//! # Why a file of our own
//!
//! HORO-1709 is explicit that this preference must not live in
//! `~/.claude/settings.json`: that file belongs to the user and to
//! Claude Code, and a Libra wording choice has no business being written
//! into it. It equally must not live in the daemon's `config.json` — the
//! daemon reads that at startup, so a wording change would not take
//! effect until the next restart, which is an absurd cost for choosing
//! between "38% budget left" and "90,000 of 150,000 tokens left".
//!
//! So: `presentation.json` in Libra's own state directory, written by
//! this command and read by the short-lived `statusline provider`
//! process on each invocation. Nothing else reads it, nothing else
//! writes it, and deleting it restores the default.
//!
//! # Absence is not a choice
//!
//! The file is absent on every install that predates this ticket, and
//! [`BudgetDisplay::default`] is exactly the wording those installs
//! already had. That is what makes "existing users are not silently
//! changed during upgrade" true rather than merely intended: an upgrade
//! does not write this file, and the absent file means the old wording.
//!
//! A *malformed* file is treated the same way — the statusline keeps
//! working on the default wording rather than losing its budget segment
//! over a preference. The `presentation` command itself does report the
//! problem, because there the user is standing in front of it and can
//! fix it.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file, inside [`libra_governor_daemon::paths::state_dir`].
const PRESENTATION_FILE_NAME: &str = "presentation.json";

/// How much of the budget envelope Libra should put into the compact
/// label of its `budget` statusline segment (HORO-1709).
///
/// # What this does and does not govern
///
/// This governs the *label* only — the one short phrase a Clear-mode line
/// has room for. It does not gate the structured economic fields
/// (`count`/`total`/`count_label` and the `budget_*` breakdown), which
/// the provider emits whenever the ledger has authoritative figures
/// regardless of this setting. That split is what lets Clear stay
/// compact for a fresh install while Detail and `explain` still have
/// everything to show, which is the ticket's recommended default
/// ("Clear: percentage-only for fresh general-user installs. Detail:
/// both percentage and amount when authoritative monetary data exists.")
///
/// # Which route each figure takes
///
/// `remaining` and `total` have contract fields of their own
/// (`count`/`total`/`count_label`), and the contract is explicit that the
/// host formats those numbers — so asking for them adds *fields*, not
/// text, and the host decides how "57,000 of 150,000 tokens" reads in
/// its own line. `used` and `reserved` have no contract field yet, so the
/// two variants that include them have to put them in the label.
///
/// That label is capped at 48 characters by the host, and an over-long
/// label costs the *whole* provider document, not just the segment. A
/// token envelope in the millions does not fit two grouped figures in
/// what is left of 48 characters after the percentage, so the renderer
/// drops to the next shorter phrase when the longer one would not fit.
/// Each variant below names the *most* it will ever show: the preference
/// is a ceiling on detail, never a promise of it. Whatever does not fit
/// is still in the structured fields and in `statusline explain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BudgetDisplay {
    /// A share of the envelope and nothing else: `38% budget left`, with
    /// no amount fields at all. The wording every install had before
    /// HORO-1709, and so the default.
    #[default]
    #[serde(rename = "percent")]
    Percent,
    /// Adds what is still available as a contract `count`, in the
    /// envelope's own unit, for the host to render beside the share.
    #[serde(rename = "remaining")]
    Remaining,
    /// Adds the envelope that remainder is available *from*, as the
    /// contract's `total`. A bare "57,000 tokens left" is a number
    /// without a scale; this is the first level at which the user can
    /// tell a large envelope nearly spent from a small one barely
    /// touched.
    #[serde(rename = "remaining+total")]
    RemainingAndTotal,
    /// Adds what has actually been settled, in the label: `38% left,
    /// 18,000 used`.
    #[serde(rename = "used+remaining+total")]
    UsedRemainingAndTotal,
    /// Adds capacity that is held but not yet settled: `38% left, 18,000
    /// used, 75,000 held`. Reserved is never folded into used — the
    /// ticket is explicit that they are different facts — so this is the
    /// only variant that shows all four figures, and the only one that
    /// routinely runs out of characters and falls back.
    #[serde(rename = "full")]
    Full,
}

impl BudgetDisplay {
    /// The accepted spellings, in increasing detail. The source of truth
    /// for both the CLI's argument parsing and its own usage text, so
    /// the two cannot drift.
    pub const CHOICES: [(&'static str, BudgetDisplay); 5] = [
        ("percent", BudgetDisplay::Percent),
        ("remaining", BudgetDisplay::Remaining),
        ("remaining+total", BudgetDisplay::RemainingAndTotal),
        ("used+remaining+total", BudgetDisplay::UsedRemainingAndTotal),
        ("full", BudgetDisplay::Full),
    ];

    /// Parses one of [`CHOICES`][Self::CHOICES]. Case-sensitive and
    /// exact: a near miss is a typo worth reporting, not an invitation
    /// to guess which wording the user meant.
    pub fn parse(raw: &str) -> Option<BudgetDisplay> {
        Self::CHOICES
            .iter()
            .find(|(name, _)| *name == raw)
            .map(|(_, display)| *display)
    }

    /// The spelling this value is written and printed as.
    pub fn as_str(self) -> &'static str {
        Self::CHOICES
            .iter()
            .find(|(_, display)| *display == self)
            .map(|(name, _)| *name)
            .unwrap_or("percent")
    }
}

/// The on-disk document. One field today, and a struct rather than a
/// bare enum so adding a second presentation preference later is a
/// field rather than a file format change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Presentation {
    /// Absent when the user has never chosen. Deliberately not
    /// `#[serde(default)]`-collapsed into [`BudgetDisplay::default`] on
    /// read: "never chosen" and "chose the default" are the same
    /// rendering and different facts, and only the first one may be
    /// changed by a future default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_display: Option<BudgetDisplay>,
}

impl Presentation {
    /// The wording to render with: the user's choice, or the default.
    pub fn budget_display(&self) -> BudgetDisplay {
        self.budget_display.unwrap_or_default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PresentationError {
    #[error("could not resolve the state dir: {0}")]
    Paths(#[from] libra_governor_daemon::paths::PathsError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid presentation JSON: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// Where the preference lives, without creating anything.
pub fn path() -> Result<PathBuf, PresentationError> {
    Ok(libra_governor_daemon::paths::state_dir()?.join(PRESENTATION_FILE_NAME))
}

/// Reads the preference, distinguishing "no file" (`Ok(None)`) from "a
/// file I could not read" (`Err`).
fn read_from(path: &Path) -> Result<Option<Presentation>, PresentationError> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PresentationError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|source| PresentationError::Parse {
            path: path.to_path_buf(),
            source,
        })
}

/// The collapse rule, on a named path so it can be tested on one.
///
/// Every failure mode collapses to the default — no file, a file someone
/// broke, a spelling from a newer version. The statusline provider runs on
/// a 200 ms budget, exits 0 by contract, and has nowhere to put a
/// complaint; dropping the budget segment, or the document, over a
/// *wording preference* would be a far worse answer than rendering the
/// wording that shipped before this ticket. The `presentation` command is
/// where the same file is read strictly and the problem is reported to a
/// user who can act on it.
fn load_from(path: &Path) -> Presentation {
    read_from(path).unwrap_or_default().unwrap_or_default()
}

/// The preference as the renderer sees it: never fails, never explains.
/// [`load_from`] at its real location, with an unresolvable state
/// directory collapsing the same way an unreadable file does.
pub fn load() -> Presentation {
    path().map(|path| load_from(&path)).unwrap_or_default()
}

/// Writes the preference owner-only, via a temp file in the same
/// directory and a rename, so a reader never observes a half-written
/// document — the statusline provider reads this file on a 200 ms budget
/// and has no business retrying.
fn write(path: &Path, presentation: &Presentation) -> Result<(), PresentationError> {
    let io_err = |p: &Path| {
        let p = p.to_path_buf();
        move |source: std::io::Error| PresentationError::Io { path: p, source }
    };

    let mut json =
        serde_json::to_vec_pretty(presentation).map_err(|source| PresentationError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    json.push(b'\n');

    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp).map_err(io_err(&tmp))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(io_err(&tmp))?;
        }
        file.write_all(&json).map_err(io_err(&tmp))?;
        file.sync_all().map_err(io_err(&tmp))?;
    }
    std::fs::rename(&tmp, path).map_err(io_err(path))
}

/// `libra-governor statusline presentation [--budget-display <choice>]`.
///
/// With no argument, prints the current setting and where it is recorded
/// — a read-only question deserves a read-only answer. With
/// `--budget-display`, records the choice and prints what it will look
/// like, so the user is not left guessing whether it took.
pub fn run(budget_display: Option<&str>) {
    let Some(raw) = budget_display else {
        return show();
    };
    let Some(choice) = BudgetDisplay::parse(raw) else {
        let choices: Vec<&str> = BudgetDisplay::CHOICES.iter().map(|(n, _)| *n).collect();
        eprintln!(
            "libra-governor statusline presentation: unknown budget display {raw:?}\n  \
             choose one of: {}",
            choices.join(", ")
        );
        std::process::exit(2);
    };

    let dir = match libra_governor_daemon::paths::ensure_state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("libra-governor statusline presentation: could not resolve state dir: {e}");
            std::process::exit(1);
        }
    };
    let path = dir.join(PRESENTATION_FILE_NAME);

    // Preserve anything already recorded: this command owns one field of
    // the document, not the document. With one field, that is not yet
    // observable from outside — `Presentation::default()` here would
    // write the same bytes, because the next line overwrites the only
    // field there is. It is written this way so that the second
    // preference is a field rather than a bug, and the rule itself is
    // asserted at the level where it can be:
    // `a_write_preserves_a_previously_recorded_choice_it_does_not_change`.
    let mut presentation = load_from(&path);
    presentation.budget_display = Some(choice);

    if let Err(e) = write(&path, &presentation) {
        eprintln!("libra-governor statusline presentation: could not record the preference: {e}");
        std::process::exit(1);
    }
    println!("budget display: {}", choice.as_str());
    println!("recorded in:    {}", path.display());
}

fn show() {
    let path = match path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("libra-governor statusline presentation: could not resolve state dir: {e}");
            std::process::exit(1);
        }
    };
    match read_from(&path) {
        Ok(Some(presentation)) => {
            println!("budget display: {}", presentation.budget_display().as_str());
            match presentation.budget_display {
                Some(_) => println!("recorded in:    {}", path.display()),
                None => println!("recorded in:    {} (no choice set)", path.display()),
            }
        }
        Ok(None) => {
            println!(
                "budget display: {} (default, nothing recorded)",
                BudgetDisplay::default().as_str()
            );
            println!("would record in: {}", path.display());
        }
        Err(e) => {
            // The statusline itself keeps rendering on the default here.
            // Say both halves: the user is being told their file is
            // broken, not that their statusline is.
            eprintln!("libra-governor statusline presentation: {e}");
            eprintln!(
                "  the statusline is using the default ({}) until this is fixed or the file \
                 is removed",
                BudgetDisplay::default().as_str()
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default must be the wording that shipped before HORO-1709.
    /// If this ever changes, every existing install's statusline changes
    /// with it without anyone having asked — which is the thing AC 2
    /// forbids.
    #[test]
    fn the_default_is_percentage_only() {
        assert_eq!(BudgetDisplay::default(), BudgetDisplay::Percent);
        assert_eq!(BudgetDisplay::CHOICES[0].1, BudgetDisplay::default());
        assert_eq!(
            Presentation::default().budget_display(),
            BudgetDisplay::Percent
        );
    }

    /// An absent file is "never chosen", which renders as the default
    /// without recording a choice. The distinction is the whole upgrade
    /// story: nothing is written, so nothing is silently changed.
    #[test]
    fn an_absent_file_is_the_default_without_recording_a_choice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRESENTATION_FILE_NAME);
        assert_eq!(read_from(&path).unwrap(), None);

        let presentation = Presentation::default();
        assert_eq!(presentation.budget_display, None);
        assert_eq!(presentation.budget_display(), BudgetDisplay::Percent);
    }

    #[test]
    fn a_recorded_choice_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRESENTATION_FILE_NAME);
        for (_, choice) in BudgetDisplay::CHOICES {
            write(
                &path,
                &Presentation {
                    budget_display: Some(choice),
                },
            )
            .unwrap();
            assert_eq!(
                read_from(&path).unwrap().unwrap().budget_display,
                Some(choice),
                "{} did not survive the file",
                choice.as_str()
            );
        }
    }

    /// The written document must be the documented grammar, not serde's
    /// idea of it: this file is the user-facing contract and a rename
    /// that only the enum knows about would be a silent format break.
    #[test]
    fn the_file_spells_the_choice_the_way_the_cli_accepts_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRESENTATION_FILE_NAME);
        write(
            &path,
            &Presentation {
                budget_display: Some(BudgetDisplay::UsedRemainingAndTotal),
            },
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"used+remaining+total\""),
            "unexpected on-disk spelling: {raw}"
        );
        assert_eq!(
            BudgetDisplay::parse("used+remaining+total"),
            Some(BudgetDisplay::UsedRemainingAndTotal)
        );
    }

    #[test]
    fn every_choice_parses_back_from_the_spelling_it_prints() {
        for (name, choice) in BudgetDisplay::CHOICES {
            assert_eq!(BudgetDisplay::parse(name), Some(choice));
            assert_eq!(choice.as_str(), name);
        }
        assert_eq!(BudgetDisplay::parse("Percent"), None);
        assert_eq!(BudgetDisplay::parse("both"), None);
        assert_eq!(BudgetDisplay::parse(""), None);
    }

    /// The statusline reads this on a hot path. A preference file someone
    /// hand-edited into invalid JSON must cost the default wording, not
    /// the budget segment.
    #[test]
    fn a_malformed_file_reads_as_the_default_rather_than_an_error_for_the_renderer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRESENTATION_FILE_NAME);
        std::fs::write(&path, b"{ this is not json").unwrap();

        assert!(
            matches!(read_from(&path), Err(PresentationError::Parse { .. })),
            "the command surface must still see the problem"
        );
        // `load()` reads the real state dir, so its rule is exercised on
        // a path of our own: an unreadable file collapses to the default
        // rather than costing the renderer its budget segment.
        assert_eq!(load_from(&path).budget_display(), BudgetDisplay::Percent);
    }

    /// An unknown spelling in the file is a parse error, not a silent
    /// fallback that would make a typo look like it took effect.
    #[test]
    fn an_unknown_spelling_in_the_file_is_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRESENTATION_FILE_NAME);
        std::fs::write(&path, br#"{"budget_display":"amount"}"#).unwrap();
        assert!(matches!(
            read_from(&path),
            Err(PresentationError::Parse { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_preference_file_is_owner_only_and_leaves_no_temp_behind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRESENTATION_FILE_NAME);
        write(
            &path,
            &Presentation {
                budget_display: Some(BudgetDisplay::Full),
            },
        )
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "preference file mode was {mode:o}");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != PRESENTATION_FILE_NAME)
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// Writing the one field this command owns must not drop the rest of
    /// the document. There is only one field today, so the guard is that
    /// a read-modify-write preserves an unrelated key rather than
    /// truncating the file to what this struct knows about.
    #[test]
    fn a_write_preserves_a_previously_recorded_choice_it_does_not_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRESENTATION_FILE_NAME);
        write(
            &path,
            &Presentation {
                budget_display: Some(BudgetDisplay::Remaining),
            },
        )
        .unwrap();

        let mut existing = read_from(&path).unwrap().unwrap();
        assert_eq!(existing.budget_display, Some(BudgetDisplay::Remaining));
        existing.budget_display = Some(BudgetDisplay::Full);
        write(&path, &existing).unwrap();
        assert_eq!(
            read_from(&path).unwrap().unwrap().budget_display,
            Some(BudgetDisplay::Full)
        );
    }

    /// An absent choice is absent from the document too — so a file
    /// written by a future version that only sets some other preference
    /// does not accidentally pin today's default as an explicit choice.
    #[test]
    fn an_unset_choice_is_not_written_into_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRESENTATION_FILE_NAME);
        write(&path, &Presentation::default()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("budget_display"),
            "an unset preference was written: {raw}"
        );
        assert_eq!(read_from(&path).unwrap().unwrap().budget_display, None);
    }
}
