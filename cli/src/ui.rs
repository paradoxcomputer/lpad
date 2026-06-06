//! Terminal UI: colour (only when the stream is a TTY, so pipes/JSON/tests stay
//! clean) and a stderr spinner with elapsed time for long ops.

use std::io::IsTerminal;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use owo_colors::OwoColorize;

fn stdout_tty() -> bool {
    std::io::stdout().is_terminal()
}
fn stderr_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// Bold section title.
pub fn header(s: &str) {
    if stdout_tty() {
        println!("\n{}", s.bold());
    } else {
        println!("\n{s}");
    }
}

/// Dimmed, left-aligned label + a bold value.
pub fn kv(label: &str, value: impl std::fmt::Display) {
    let padded = format!("{label:<20}");
    if stdout_tty() {
        println!("  {}  {}", padded.dimmed(), value.bold());
    } else {
        println!("  {padded}  {value}");
    }
}

/// Green ✓ success line (stderr, so it never mixes with piped stdout).
pub fn ok(s: &str) {
    if stderr_tty() {
        eprintln!("{} {s}", "✓".green().bold());
    } else {
        eprintln!("✓ {s}");
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
            pb.set_style(
                ProgressStyle::with_template("{spinner:.cyan} {msg} {elapsed:.dim}")
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
