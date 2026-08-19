//! Terminal UI: colour (only when the stream is a TTY *and* the environment
//! allows it, so pipes, `--json`, CI logs and `NO_COLOR` stay clean) and a
//! stderr spinner with elapsed time for long ops.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use owo_colors::OwoColorize;

/// What prose wraps to when the terminal has not said otherwise.
const DEFAULT_WIDTH: usize = 80;

/// Read an env var as text, treating "set but not UTF-8" as set: every variable
/// below is a switch, and a switch nobody can decode is still a switch somebody
/// threw.
fn env_str(key: &str) -> Option<String> {
    std::env::var_os(key).map(|v| v.to_string_lossy().into_owned())
}

/// Whether a stream that is (or is not) a terminal may carry colour.
///
/// Pure so it can be tested without a pty or a mutated environment. Precedence,
/// most specific first:
///
/// * `NO_COLOR` set to anything non-empty wins outright, per no-color.org. It is
///   what a user reaches for when a pager, a CI log or a screen reader is
///   mangling escapes, so nothing overrides it - not even `CLICOLOR_FORCE`.
/// * `CLICOLOR_FORCE` non-empty and not `0` turns colour on even off a TTY: the
///   escape hatch for `lpad ... 2>&1 | less -R`, and the only way a test can
///   reach the coloured branch without spawning a pty.
/// * `CLICOLOR=0` turns colour off.
/// * Otherwise colour follows the stream: on for a terminal, off for a pipe.
const fn color_choice(no_color: Option<&str>, force: Option<&str>, clicolor: Option<&str>, tty: bool) -> bool {
    // `const fn` cannot call `Option::is_some_and`, so these are let-chains
    // rather than the combinators you would reach for outside a `const`.
    if let Some(v) = no_color
        && !v.is_empty()
    {
        return false;
    }
    if let Some(v) = force
        && !v.is_empty()
        && !matches!(v.as_bytes(), b"0")
    {
        return true;
    }
    if let Some(v) = clicolor
        && matches!(v.as_bytes(), b"0")
    {
        return false;
    }
    tty
}

/// The one place colour is decided.
///
/// owo-colors v4 writes its escape unconditionally - `.green()` consults no
/// environment and no TTY - so every colour decision in lpad has to happen
/// before the call, and it happens here. A per-call-site check is a check
/// somebody forgets to copy into the next printer, and the failure mode is
/// escape bytes in a redirected log or a `NO_COLOR` user's terminal.
///
/// Note this governs COLOUR, not cursor control: the spinner still redraws its
/// own line on a TTY (with no colour in it, under any of the switches above),
/// because erasing a progress line is not what `NO_COLOR` is about.
fn colors(tty: bool) -> bool {
    color_choice(
        env_str("NO_COLOR").as_deref(),
        env_str("CLICOLOR_FORCE").as_deref(),
        env_str("CLICOLOR").as_deref(),
        tty,
    )
}

fn stdout_color() -> bool {
    colors(std::io::stdout().is_terminal())
}
fn stderr_color() -> bool {
    colors(std::io::stderr().is_terminal())
}
fn stderr_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// The column prose wraps at: `COLUMNS` when the shell exported it, else 80,
/// clamped to something a human can still read. lpad takes no terminal-size
/// dependency for this - a wrong guess costs one soft-wrapped line, and the
/// wrapping only happens for a terminal in the first place.
fn width() -> usize {
    env_str("COLUMNS")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map_or(DEFAULT_WIDTH, |c| c.clamp(40, 100))
}

/// The width to wrap a stderr diagnostic to, or `None` when it must not be
/// wrapped at all.
///
/// Wrapping is for the person reading the terminal. Piped output is left exactly
/// as the message was written - one diagnostic, one line - so `grep`, a test
/// asserting on a phrase, and a log line stay whole. This deliberately keys off
/// the TTY and NOT off [`colors`]: `NO_COLOR` asks for no escapes, not for
/// 200-column paragraphs.
fn wrap_to_stderr() -> Option<usize> {
    stderr_tty().then(width)
}

/// Fold `text` so no line exceeds `width` columns, preserving the newlines and
/// the leading indentation it already has and adding `hang` more spaces to the
/// continuations of a line it had to break.
///
/// Errors here are written as a sentence plus indented follow-up lines (see
/// `network::stored`, `faucet`'s list of skipped accounts), and one of those
/// sentences prescribing an exact remedy command runs well past 80 columns. Left
/// to the terminal's own soft wrap the remedy restarts in column 0, indistinguishable
/// from a new point; folded here it stays visibly subordinate to the line it
/// belongs to.
///
/// A word is never split: an account id, a program id and a `lpad ...` command
/// line are all things the reader is about to copy, and a hyphen or a line break
/// inserted into one of them makes the copy wrong. A word longer than `width`
/// therefore overhangs on a line of its own.
///
/// A line already short enough is copied through byte for byte, so a message
/// that lined its own columns up with runs of spaces keeps them.
fn wrap(text: &str, width: usize, hang: usize) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.chars().count() <= width {
            out.push_str(line);
            continue;
        }
        let body = line.trim_start();
        let indent = &line[..line.len() - body.len()];
        let cont = format!("{indent}{}", " ".repeat(hang));
        let mut col = 0;
        for word in body.split_whitespace() {
            let w = word.chars().count();
            if col == 0 {
                out.push_str(indent);
                col = indent.chars().count() + w;
            } else if col + 1 + w > width {
                out.push('\n');
                out.push_str(&cont);
                col = cont.chars().count() + w;
            } else {
                out.push(' ');
                col += 1 + w;
            }
            out.push_str(word);
        }
    }
    out
}

/// Characters that steer a terminal instead of printing in it: every C0/C1
/// control - ESC, CR, BEL, and the single-character CSI at U+009B - plus the two
/// Unicode line separators and the bidi overrides and isolates, which reorder
/// the visible text of a line without appearing anywhere in it.
fn steers_terminal(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}'
            | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Escape everything in `s` that could do more than print.
///
/// Every string lpad shows that came off the chain is attacker-controlled.
/// `token_name`/`token_symbol` are creator-supplied and the guests bound only
/// their byte length (`MAX_METADATA_LEN`, 64); Borsh validates UTF-8 and nothing
/// else, so ESC, CR and the bidi overrides all reach a `SaleState` intact, and
/// opening a sale is permissionless. Sixty-four bytes is room for a clear-screen
/// (`ESC[2J ESC[H`), which wipes the status, price and fee lines a buyer is
/// reading before committing collateral, or for an OSC 52 clipboard write, which
/// xterm, iTerm2, kitty, wezterm and tmux honour, in a tool whose whole job is
/// moving funds to account ids the user pastes.
///
/// So the escaping lives here, at the one place text becomes terminal output,
/// rather than at each call site: every printer below routes through it, which
/// makes a call site added later safe without anyone having to remember.
/// `--json` deliberately does NOT route through it, and the reason is NOT that
/// serde_json makes it safe - this doc used to claim that and it was false.
/// serde_json escapes the C0 controls (and `"`/`\`) and nothing else, so
/// U+009B (the single-character C1 CSI), U+2028/U+2029 and the bidi overrides
/// U+202A-202E all pass through a `--json` payload verbatim. `escapes_only_c0`
/// below pins that fact rather than trusting this sentence.
///
/// The real reason is that `--json` output is data for another program: it is
/// contracted to carry `token_name` exactly as chain state stores it, so a
/// consumer can compare, re-serialise or hash it. Mangling it here would corrupt
/// that contract to protect a terminal that is not the intended reader, and
/// would do so incompletely anyway - a hostile name reaches the screen through
/// any `jq -r` regardless of what lpad escaped, because `jq` un-escapes.
/// Sanitising belongs at the point of rendering, which is what this function is
/// and what a `--json` consumer must do for itself. Residual exposure is a user
/// who pipes `--json` straight through `jq -r` into their own terminal; that is
/// the same exposure `jq -r` gives them over any JSON from anywhere, and it is
/// why the human path - the one lpad does render - escapes the full set above,
/// not just the C0 part serde_json happens to cover.
///
/// Offenders become their `\u{..}` escape rather than being dropped: a name that
/// is nothing but escape sequences should look wrong, not blank.
#[must_use]
pub fn safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if steers_terminal(c) {
            out.push_str(&c.escape_unicode().to_string());
        } else {
            out.push(c);
        }
    }
    out
}

/// Exactly what [`kv`] and [`e_kv`] print, minus the colour: the sanitised label
/// padded out to the value column, and the sanitised value.
///
/// Split out so a test can assert on the real rendering. The colour paths only
/// wrap these two finished strings, so nothing an escape could ride in on is
/// added after this point - and padding *after* sanitising is what keeps the two
/// columns aligned when the label was one of the escaped strings.
fn kv_parts(label: &str, value: &str) -> (String, String) {
    (format!("{:<20}", safe(label)), safe(value))
}

/// The wordmark, in block capitals, six rows and 32 columns.
///
/// Kept as one `&str` rather than assembled per line so the shape is visible in
/// the source: an edit that breaks the alignment is obvious here, and would not
/// be if these were format arguments.
const WORDMARK: &str = "\
\u{2588}\u{2588}\u{2557}     \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}\u{20}
\u{2588}\u{2588}\u{2551}     \u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}
\u{2588}\u{2588}\u{2551}     \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}
\u{2588}\u{2588}\u{2551}     \u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{255d} \u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}
\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2551}     \u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d}
\u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}     \u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{255d}\u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}\u{20}";

/// The same thing for a terminal too narrow to hold the wordmark.
const WORDMARK_NARROW: &str = "L P A D";

/// How wide a terminal must be before the block wordmark is worth drawing.
/// [`WORDMARK`] is 32 columns; below a little more than that it wraps, and a
/// wrapped wordmark looks like corruption rather than like a logo.
const WORDMARK_MIN_WIDTH: usize = 34;

/// Print the wordmark, once, above a bare `lpad` or a `--help`.
///
/// **stderr, never stdout.** `--json` contracts stdout to be one parseable
/// payload and nothing else; a banner there would break every machine consumer
/// of this CLI. Putting it on stderr means it cannot, whatever flags follow.
///
/// Shown only when stderr is a terminal, for the same reason colour is: in a
/// pipe, a CI log, a file or a screen reader this is six lines of noise in front
/// of the thing the user actually asked for. `NO_COLOR` still applies to the
/// *colour* - it is a colour switch, not a "hide the title" switch - so the
/// wordmark is drawn plain rather than suppressed.
pub fn banner() {
    if !stderr_tty() {
        return;
    }
    let art = if width() >= WORDMARK_MIN_WIDTH { WORDMARK } else { WORDMARK_NARROW };
    let tag = concat!("v", env!("CARGO_PKG_VERSION"), "  -  Logos privacy-preserving token launchpad");
    if stderr_color() {
        eprintln!("\n{}", art.cyan().bold());
        eprintln!("{}\n", tag.dimmed());
    } else {
        eprintln!("\n{art}");
        eprintln!("{tag}\n");
    }
}

/// Bold section title.
pub fn header(s: &str) {
    let s = safe(s);
    if stdout_color() {
        println!("\n{}", s.bold());
    } else {
        println!("\n{s}");
    }
}

/// Dimmed, left-aligned label + a bold value.
pub fn kv(label: &str, value: impl std::fmt::Display) {
    let (padded, value) = kv_parts(label, &value.to_string());
    if stdout_color() {
        println!("  {}  {}", padded.dimmed(), value.bold());
    } else {
        println!("  {padded}  {value}");
    }
}

/// The three markers that open a line of stderr prose, and the colour each one
/// carries.
///
/// One enum rather than three copies of the same `if colour` because the
/// asymmetry it replaced was exactly that: success was green and `error:` was
/// left plain, so the one line a reader scans for was the one line that did not
/// stand out.
#[derive(Clone, Copy)]
enum Mark {
    Ok,
    Warn,
    Error,
}

impl Mark {
    const fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Warn => "!",
            Self::Error => "error:",
        }
    }

    fn painted(self) -> String {
        let g = self.glyph();
        match self {
            Self::Ok => g.green().bold().to_string(),
            Self::Warn => g.yellow().bold().to_string(),
            Self::Error => g.red().bold().to_string(),
        }
    }
}

/// The finished text of one marked stderr line: the message sanitised, the whole
/// line wrapped (continuations indented under the marker), and the marker - and
/// only the marker - painted.
///
/// Colour goes on after wrapping so no escape is ever counted as a column, and
/// on the marker alone so the message text can be diffed, grepped or pasted
/// unchanged. Split out as a pure function taking `color` and `wrap_to` because
/// the alternative way to test the coloured branch is to spawn a pty, which
/// costs a dependency.
fn marked(mark: Mark, msg: &str, color: bool, wrap_to: Option<usize>) -> String {
    let text = format!("{} {}", mark.glyph(), safe(msg));
    let text = match wrap_to {
        Some(w) => wrap(&text, w, 2),
        None => text,
    };
    if !color {
        return text;
    }
    let (_, rest) = text.split_at(mark.glyph().len());
    format!("{}{rest}", mark.painted())
}

/// Green ✓ success line (stderr, so it never mixes with piped stdout).
pub fn ok(s: &str) {
    eprintln!("{}", marked(Mark::Ok, s, stderr_color(), wrap_to_stderr()));
}

/// Yellow warning line on stderr.
pub fn warn(s: &str) {
    eprintln!("{}", marked(Mark::Warn, s, stderr_color(), wrap_to_stderr()));
}

/// Red `error:` line on stderr: what every failed command exits through.
///
/// Sanitised like every other printed line - an error quotes text the chain or
/// the sequencer supplied (a guest's revert string, a rejected account's
/// on-chain name), so it is as attacker-controlled as any sale name.
pub fn error(msg: &str) {
    eprintln!("{}", marked(Mark::Error, msg, stderr_color(), wrap_to_stderr()));
}

/// One indented, wrapped paragraph of prose, sanitised: the body of an
/// [`e_note`], and every follow-up line of a crash report.
///
/// Pure, so the crash report can be assembled and asserted on as a single
/// string without printing anything. The indent is two spaces and the hang is
/// zero, which puts a folded continuation directly under the first word rather
/// than under the marker - these lines sit below a heading, not beside one.
fn note_text(s: &str, wrap_to: Option<usize>) -> String {
    let text = format!("  {}", safe(s));
    match wrap_to {
        Some(w) => wrap(&text, w, 0),
        None => text,
    }
}

/// A wrapped paragraph of prose on stderr, indented two under an [`e_header`].
///
/// For the diagnostics that are a sentence rather than a value: they are the
/// ones long enough to need [`wrap`], and the ones a reader is meant to act on.
pub fn e_note(s: &str) {
    eprintln!("{}", note_text(s, wrap_to_stderr()));
}

/// `header`/`kv` on **stderr**.
///
/// Used by anything that can print in the middle of another command - the
/// network picker and the deploy report. Those fire during whatever the user
/// actually typed, including a `--json` run, and stdout there belongs to that
/// command's own machine-readable output; a menu or a deploy summary printed
/// into it would corrupt it.
pub fn e_header(s: &str) {
    let s = safe(s);
    if stderr_color() {
        eprintln!("\n{}", s.bold());
    } else {
        eprintln!("\n{s}");
    }
}

/// Stderr twin of [`kv`], same two-column shape.
pub fn e_kv(label: &str, value: impl std::fmt::Display) {
    let (padded, value) = kv_parts(label, &value.to_string());
    if stderr_color() {
        eprintln!("  {}  {}", padded.dimmed(), value.bold());
    } else {
        eprintln!("  {padded}  {value}");
    }
}

/// A live spinner on stderr with an elapsed timer. Hidden when stderr isn't a
/// TTY or when `quiet` (JSON mode), so machine output is never polluted.
pub struct Spinner(ProgressBar);

impl Spinner {
    pub fn new(msg: &str, quiet: bool) -> Self {
        let pb = ProgressBar::new_spinner();
        if quiet || !stderr_tty() {
            pb.set_draw_target(ProgressDrawTarget::hidden());
        } else {
            pb.set_draw_target(ProgressDrawTarget::stderr());
            // The spinner's own redraw (a carriage return and an erase) is not
            // colour, so it stays; the cyan and the dim do not, which is what
            // `NO_COLOR` on a terminal actually asked for.
            let template = if stderr_color() {
                "{spinner:.cyan} {msg} {elapsed:.dim}"
            } else {
                "{spinner} {msg} {elapsed}"
            };
            pb.set_style(
                ProgressStyle::with_template(template)
                    .unwrap()
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"]),
            );
            pb.enable_steady_tick(Duration::from_millis(90));
        }
        pb.set_message(msg.to_owned());
        Self(pb)
    }

    /// Cloneable handle for the SDK progress callback to update the message.
    pub fn handle(&self) -> ProgressBar {
        self.0.clone()
    }

    /// Clear the spinner line (call before printing results).
    pub fn clear(self) {
        self.0.finish_and_clear();
    }
}

// ---------------------------------------------------------------------------
// Crash reporting
// ---------------------------------------------------------------------------

/// Where a crash asks to be reported.
const ISSUES_URL: &str = "https://github.com/paradoxcomputer/lpad/issues";

/// What the process exits with when [`install_panic_hook`]'s hook fires.
///
/// The three other codes lpad can leave behind are 0 (success), 1 (an ordinary
/// failed command, `main`'s `error:` path) and 2 (clap's usage error), so a
/// crash has to be none of those or a script cannot tell "the sale is closed"
/// from "lpad fell over". 101 is Rust's own panic status, adopted rather than
/// invented: it is what an un-hooked panic already exits with, so a wrapper that
/// treats 101 as a crash keeps being right, and removing this hook one day would
/// not silently change the contract.
pub const CRASH_EXIT: i32 = 101;

/// The command line as typed, argv[0] reduced to its file name.
///
/// Printed back to the user and written into the crash log because the first
/// thing a bug report needs is what was run - and the reporter, mid-crash, is
/// the one person who should not have to remember. Not sanitised here: every
/// consumer below sanitises, and the log keeps the sanitised form too (see
/// [`crash_log_text`]).
///
/// No argument lpad takes is a secret - keys live in the wallet home and in
/// `LPAD_FUNDER_KEY`, never on the command line - so this is safe to persist.
/// A flag that ever does carry one must be redacted here before it is.
fn command_line() -> String {
    let mut args = std::env::args_os();
    let argv0 = args.next().map_or_else(
        || "lpad".to_owned(),
        |a| {
            std::path::Path::new(&a)
                .file_name()
                .map_or_else(|| a.to_string_lossy().into_owned(), |f| f.to_string_lossy().into_owned())
        },
    );
    std::iter::once(argv0)
        .chain(args.map(|a| a.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The one line that says what actually broke: the panic message and where it
/// was raised.
fn crash_summary(info: &std::panic::PanicHookInfo<'_>) -> String {
    // `payload_as_str` covers the `&str` and `String` payloads every `panic!`,
    // `unwrap`, `expect`, overflow check and `unimplemented!` produces. A
    // payload of some other type carries nothing printable, and saying so beats
    // printing `Any { .. }`.
    let what = info.payload_as_str().unwrap_or("panicked with a payload that is not text");
    match info.location() {
        Some(l) => format!("{what} (at {}:{}:{})", l.file(), l.line(), l.column()),
        None => what.to_owned(),
    }
}

/// The file that gets everything the screen deliberately leaves out.
///
/// Sanitised like any other rendering: a crash log exists to be read, and the
/// way a person reads one is `cat`, which hands a panic message quoting hostile
/// chain text straight to the terminal that the rest of this module spends its
/// time protecting.
fn crash_log_text(argv: &str, summary: &str, backtrace: Option<&str>) -> String {
    let mut out = format!(
        "lpad {} crash report\n\ncommand: {}\npanic:   {}\n\n",
        env!("CARGO_PKG_VERSION"),
        safe(argv),
        safe(summary),
    );
    match backtrace {
        Some(bt) => out.push_str(&format!("backtrace:\n{}\n", safe(bt))),
        None => out.push_str("backtrace: not captured - re-run with RUST_BACKTRACE=1 to add one.\n"),
    }
    out
}

/// Whether the detail reached a file, and if not, what to print instead.
enum CrashLog<'a> {
    Wrote(&'a str),
    Failed { why: &'a str, detail: &'a str },
}

/// The crash message, whole, as a pure function of everything that varies.
///
/// Pure for the same reason [`marked`] is: the alternative way to test what a
/// crash prints is to make the test binary crash under a pty, and a message
/// nobody can assert on is a message that rots. It also keeps the hook itself
/// down to "gather, render, write", which matters when the code is running
/// after something has already gone wrong.
///
/// The shape follows the rest of lpad's diagnostics - a red `error:` sentence,
/// then indented lines that each prescribe one thing to do - and it is
/// deliberately short: nobody reads a crash dump, so what is on screen is the
/// summary, the path to the detail, and where to send it.
fn crash_report(
    argv: &str,
    summary: &str,
    log: &CrashLog<'_>,
    backtrace: bool,
    color: bool,
    wrap_to: Option<usize>,
) -> String {
    let mut lines = vec![marked(
        Mark::Error,
        &format!("lpad hit a bug and stopped while running `{argv}`. This is a defect in lpad, not something you typed."),
        color,
        wrap_to,
    )];
    lines.push(note_text(&format!("What broke: {summary}"), wrap_to));
    match *log {
        CrashLog::Wrote(path) => lines.push(note_text(
            &format!("The full report was written to {path} - please open an issue at {ISSUES_URL} and attach it."),
            wrap_to,
        )),
        CrashLog::Failed { why, detail } => {
            lines.push(note_text(
                &format!(
                    "No report file could be written ({why}), so the whole of it is below - \
                     please copy it into an issue at {ISSUES_URL}."
                ),
                wrap_to,
            ));
            // Unwrapped: a backtrace is columns, not prose, and folding it at 80
            // makes the thing it exists for harder to read. Sanitised, because
            // it is still going to a terminal.
            lines.push(safe(detail));
        }
    }
    if !backtrace {
        lines.push(note_text(
            "Re-run the same command with RUST_BACKTRACE=1 to put a backtrace in that report - \
             without one a crash inside a dependency is often not traceable to a cause.",
            wrap_to,
        ));
    }
    // The one thing a crash can leave behind that a retry makes worse.
    lines.push(note_text(
        "A crash does not undo a transaction lpad had already submitted. Run `lpad status` and \
         `lpad my-sales` / `lpad my-pools` to see where the chain actually is before running the \
         command again.",
        wrap_to,
    ));
    lines.push(String::new()); // trailing newline once the lines are joined
    lines.join("\n")
}

/// Write the detail somewhere the user can retrieve it, and say where that is.
///
/// The temp dir, not the wallet home: a crash can happen before the wallet home
/// is even resolved (a bad `--config`, a panic inside clap), and a crash handler
/// that has to work out where to write is a crash handler with somewhere else to
/// fail. One file per process id, so a second run does not overwrite the report
/// of the first while it is being attached to an issue.
fn write_crash_log(text: &str) -> Result<std::path::PathBuf, String> {
    let path = std::env::temp_dir().join(format!("lpad-panic-{}.log", std::process::id()));
    match std::fs::write(&path, text) {
        Ok(()) => Ok(path),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Replace Rust's default panic output with a report a user can act on.
///
/// Install this first thing in `main`. lpad reaches a lot of code it does not
/// own - the wallet, the prover, serde - and any of it can panic; the default
/// hook answers that with `thread 'main' panicked at ...` and a note about
/// `RUST_BACKTRACE`, which tells a launchpad user nothing about what to do and
/// makes a working tool look broken. `RISC0_PROVER=bonsai` is the reachable
/// example: risc0's prover selection hits `unimplemented!()` because the feature
/// is not compiled in, and that is a mistyped environment variable, not a bug.
///
/// Nothing is swallowed: the payload, the location and (with `RUST_BACKTRACE`
/// set) the backtrace all go to a file whose path is printed. The screen gets
/// the summary and the remedy.
///
/// Two rules this hook lives by, because it runs when things are already wrong:
///
/// * It must not panic. It writes with `write_all` rather than `eprintln!`,
///   which panics on a failed write - a panic inside a panic hook aborts the
///   process, and the user gets `SIGABRT` and no message at all. A crash caused
///   by a full disk is exactly the case that would take that path.
/// * It exits rather than returning. Returning would let the unwind continue,
///   and lpad runs its online commands on a tokio runtime where a panicked
///   worker surfaces again as a `JoinError` in the caller - a second crash
///   report for one bug. Exiting here means one report and one exit code, at the
///   cost of skipping destructors, which at crash time is not a cost worth
///   paying to avoid. Rust's own buffer for stdout is not flushed by
///   `process::exit`, so that is done explicitly first: a command that had
///   already printed its JSON should not lose it because it crashed afterwards.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let argv = command_line();
        // `capture()` consults `RUST_BACKTRACE` itself and returns `Disabled`
        // when it is unset, which is exactly the default this wants: no
        // backtrace unless it was asked for.
        let bt = std::backtrace::Backtrace::capture();
        let captured = matches!(bt.status(), std::backtrace::BacktraceStatus::Captured);
        let summary = crash_summary(info);
        let detail = crash_log_text(&argv, &summary, captured.then(|| bt.to_string()).as_deref());
        let written = write_crash_log(&detail);
        let shown = match &written {
            Ok(path) => path.display().to_string(),
            Err(why) => why.clone(),
        };
        let log = match &written {
            Ok(_) => CrashLog::Wrote(&shown),
            Err(_) => CrashLog::Failed { why: &shown, detail: &detail },
        };
        let report = crash_report(&argv, &summary, &log, captured, stderr_color(), wrap_to_stderr());
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().write_all(report.as_bytes());
        std::process::exit(CRASH_EXIT);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload a malicious sale would carry in `token_name`: clear the
    /// screen and home the cursor, so the `status`, `spot price` and
    /// `fee owed to treasury` lines `bc sale-info` prints AFTER the token line
    /// never appear, then write the attacker's account id into the user's
    /// clipboard with OSC 52. Both fit inside the on-chain 64-byte bound.
    const WIPE_AND_STEAL_CLIPBOARD: &str = "\u{1b}[2J\u{1b}[H\u{1b}]52;c;YXR0YWNrZXI=\u{7}";

    /// Everything the terminal must never receive from chain state.
    fn steering_bytes(s: &str) -> Vec<char> {
        s.chars().filter(|c| steers_terminal(*c)).collect()
    }

    /// `bc sale-info` renders the token line as `kv("token", "<name> (<symbol>)")`,
    /// and that name is whatever the sale's creator stored on chain. Render the
    /// hostile version of it exactly as the command does and require that not one
    /// steering character survives into what is printed.
    #[test]
    fn a_hostile_token_name_cannot_reach_the_terminal() {
        let (label, value) = kv_parts("token", &format!("{WIPE_AND_STEAL_CLIPBOARD} (\u{1b}[31mLPAD)"));
        let line = format!("  {label}  {value}");
        assert!(
            steering_bytes(&line).is_empty(),
            "a sale name steered the terminal: {:?} survived into {line:?}",
            steering_bytes(&line)
        );
        // Escaped, not silently swallowed: a name made of escape sequences has
        // to LOOK wrong, or hiding one is as good as executing it.
        assert!(line.contains(r"\u{1b}"), "the escapes were dropped instead of shown: {line:?}");
    }

    /// The same guarantee for the discovery listings, whose row label is chain
    /// text too - `my-balance` puts the token name in the label column, where an
    /// unescaped CR would also break the two-column alignment of every row.
    #[test]
    fn a_hostile_token_name_in_the_label_column_cannot_reach_the_terminal() {
        let (label, value) = kv_parts(&format!("{WIPE_AND_STEAL_CLIPBOARD} (public)"), "1000");
        let line = format!("  {label}  {value}");
        assert!(steering_bytes(&line).is_empty(), "a holding label steered the terminal: {line:?}");
    }

    /// Bidi overrides print as nothing at all and reorder the rest of the line,
    /// so an unescaped one can make a drained sale's reserves read as full.
    #[test]
    fn bidi_overrides_cannot_reorder_a_rendered_line() {
        for c in ['\u{202e}', '\u{2066}', '\u{200f}', '\u{061c}', '\u{2028}'] {
            let (_, value) = kv_parts("token", &format!("COIN{c} (LPAD)"));
            assert!(!value.contains(c), "{c:?} survived rendering: {value:?}");
        }
    }

    /// Sanitising happens before the padding, so a label that had to be escaped
    /// still lines its value up with every other row's.
    #[test]
    fn an_escaped_label_still_lines_up_with_the_others() {
        let (plain, _) = kv_parts("token", "");
        let (escaped, _) = kv_parts("tok\u{1b}en", "");
        assert_eq!(plain.chars().count(), escaped.chars().count(), "{plain:?} vs {escaped:?}");
    }

    /// The claim the `--json` note rests on, checked instead of asserted: what
    /// serde_json escapes is C0 and nothing more.
    ///
    /// This is the sentence that was wrong before - the doc said serde_json
    /// "escapes control characters itself", which reads as "so `--json` is
    /// safe", and it is not: three classes of steering character survive. The
    /// decision to keep `--json` byte-faithful is deliberate and documented
    /// above, but it has to rest on a true statement of what a JSON consumer is
    /// actually handed. If a future serde_json escapes more (or fewer), this
    /// fails and the note gets rewritten rather than quietly becoming false
    /// again.
    #[test]
    fn serde_json_escapes_only_c0_so_the_json_note_cannot_go_stale() {
        // C0: escaped, which is the part the old note was right about.
        for c in ['\u{1b}', '\u{d}', '\u{7}', '\u{0}'] {
            let out = serde_json::to_string(&c.to_string()).unwrap();
            assert!(!out.contains(c), "serde_json stopped escaping C0 {c:?}: {out}");
        }
        // Everything else `steers_terminal` catches: NOT escaped, and therefore
        // in the payload raw. All of these are reachable in a `token_name`.
        for c in ['\u{9b}', '\u{2028}', '\u{2029}', '\u{202e}', '\u{2066}', '\u{200f}', '\u{061c}'] {
            assert!(steers_terminal(c), "test is checking a character safe() does not care about: {c:?}");
            let out = serde_json::to_string(&c.to_string()).unwrap();
            assert!(
                out.contains(c),
                "serde_json now escapes {c:?} - the `--json` note in `safe` says it does not: {out}"
            );
        }
        // And the human path escapes every one of them, which is the asymmetry
        // the note describes.
        for c in ['\u{1b}', '\u{9b}', '\u{2028}', '\u{202e}'] {
            assert!(!safe(&c.to_string()).contains(c), "safe() let {c:?} through");
        }
    }

    /// The wordmark is a fixed rectangle. A stray edit that lengthens one row
    /// only shows up as a crooked logo on somebody else's terminal, so the shape
    /// is asserted rather than eyeballed.
    #[test]
    fn the_wordmark_is_a_rectangle_that_fits_a_narrow_terminal() {
        let rows: Vec<&str> = super::WORDMARK.lines().collect();
        assert_eq!(rows.len(), 6, "six rows");
        let w = rows[0].chars().count();
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.chars().count(), w, "row {i} is a different width: {r:?}");
        }
        assert!(
            w < super::WORDMARK_MIN_WIDTH,
            "the wordmark ({w} cols) must fit inside the width it demands ({})",
            super::WORDMARK_MIN_WIDTH
        );
        // Box-drawing and block glyphs are not terminal steering, so the
        // sanitiser must leave them alone. Asserted per ROW, not on the whole
        // constant: `safe` escapes control characters and `\n` is one, so it
        // rewrites the wordmark's own line breaks by design. That is correct for
        // `safe` - it sanitises single-line values - and is why the banner
        // prints this constant directly rather than through it.
        for r in &rows {
            assert_eq!(&super::safe(r), r, "safe() rewrote a wordmark row: {r:?}");
        }
        assert_eq!(super::safe(super::WORDMARK_NARROW), super::WORDMARK_NARROW);
    }

    /// `NO_COLOR` beats everything, including `CLICOLOR_FORCE`.
    ///
    /// The precedence is the whole point of routing colour through one
    /// function, so it is asserted rather than assumed: a user reaches for
    /// `NO_COLOR` when escapes are actively breaking their pager or screen
    /// reader, and a tool that lets its own force-flag override that is worse
    /// than one with no colour at all.
    #[test]
    fn no_color_wins_over_every_other_switch() {
        assert!(!color_choice(Some("1"), Some("1"), None, true), "NO_COLOR must beat CLICOLOR_FORCE");
        assert!(!color_choice(Some("1"), None, None, true), "NO_COLOR must beat a TTY");
        // Empty means unset, per no-color.org: only a non-empty value disables.
        assert!(color_choice(Some(""), None, None, true), "empty NO_COLOR is not set");
    }

    /// The remaining precedence rungs, including the off-TTY escape hatch.
    #[test]
    fn colour_follows_force_then_clicolor_then_the_stream() {
        assert!(color_choice(None, Some("1"), None, false), "CLICOLOR_FORCE colours a pipe");
        assert!(!color_choice(None, Some("0"), None, false), "CLICOLOR_FORCE=0 is not force");
        assert!(!color_choice(None, None, Some("0"), true), "CLICOLOR=0 disables on a TTY");
        assert!(color_choice(None, None, None, true), "a bare TTY gets colour");
        assert!(!color_choice(None, None, None, false), "a bare pipe does not");
    }

    /// A crash must read as *our* bug, name the command, and carry no escapes
    /// when colour is off - the crash path is the one place a raw escape would
    /// reach a terminal that had explicitly asked for none.
    #[test]
    fn a_crash_report_blames_lpad_and_stays_plain_without_colour() {
        let report = crash_report(
            "lpad bc buy --sale 0x01",
            "attempt to subtract with overflow",
            &CrashLog::Wrote("/tmp/lpad-crash.log"),
            false,
            false,
            Some(80),
        );
        assert!(report.contains("lpad bc buy --sale 0x01"), "must echo what was run:\n{report}");
        assert!(report.contains("not something you typed"), "must not blame the user:\n{report}");
        assert!(report.contains("attempt to subtract with overflow"), "must say what broke:\n{report}");
        assert!(report.contains(ISSUES_URL), "must say where to report it:\n{report}");
        assert!(!report.contains('\u{1b}'), "no escapes when colour is off:\n{report:?}");
    }

    /// When no log file could be written the detail has to come to the screen
    /// instead, or the bug report has nothing in it.
    #[test]
    fn a_crash_that_cannot_write_its_log_prints_the_whole_thing() {
        let report = crash_report(
            "lpad status",
            "boom",
            &CrashLog::Failed { why: "permission denied", detail: "the full detail" },
            false,
            false,
            Some(80),
        );
        assert!(report.contains("permission denied"), "must say why it could not write:\n{report}");
        assert!(report.contains("the full detail"), "must fall back to printing the detail:\n{report}");
    }

    #[test]
    fn ordinary_text_is_printed_unchanged() {
        for s in ["Bob's Token (BOB)", "Café Münze", "🚀 LPAD", "a-b_c.d/e:f 0x1b"] {
            assert_eq!(safe(s), s, "safe() mangled ordinary text");
        }
        // The boundary is worth pinning separately, because `steers_terminal`
        // is a rule rather than a list and the rule is easy to widen by
        // accident. A combining acute, a zero-width joiner, a BOM and a
        // non-breaking space all read as "invisible Unicode" to a reviewer, and
        // not one of them steers a terminal: escaping them would corrupt real
        // names (every accented name normalised the decomposed way, every emoji
        // ZWJ sequence) to buy nothing.
        for benign in ['\u{0301}', '\u{200d}', '\u{feff}', '\u{00a0}'] {
            let s = benign.to_string();
            assert_eq!(safe(&s), s, "safe() escaped the benign {benign:?}");
        }
    }
}
