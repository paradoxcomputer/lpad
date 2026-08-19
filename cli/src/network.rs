//! Which sequencer this wallet talks to: the remembered choice, the first-run
//! picker, and the deployment check that follows a pick.
//!
//! Everything here writes to **stderr**. The picker can fire in the middle of
//! any command, including a `--json` one, and stdout belongs to that command's
//! own output; a menu printed there would corrupt it.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use lpad_sdk::{Deployment, LaunchpadClient, Network, ProgramStatus};

use crate::fmt::hex_program;
use crate::ui;

/// The remembered choice: a `network` file inside the wallet's own directory,
/// beside the `proof_mode` marker `detect_proof_mode` already reads, holding the
/// config form [`Network::parse`] reads back (`testnet`, `paradox`,
/// `local=<url>`, or a bare URL).
///
/// PER WALLET, deliberately, not one global setting. A wallet holds accounts,
/// shielded notes, and the ids of the sales and pools it created, and every one
/// of those is a PDA on ONE chain: point a testnet wallet at paradox and every
/// account simply reads as absent, which is why bootstrap.sh gives each network
/// its own home (`~/.lpad`, `~/.lpad-paradox`) and why scripts/test-all-cli.sh
/// refuses to run an env file against a network it was not bootstrapped for. A
/// single global choice would be a setting that is wrong for every wallet but
/// one, and switching homes with `$LEE_WALLET_HOME_DIR` would silently keep
/// aiming at the previous chain. Keeping it beside the wallet makes the wallet
/// and its chain move, copy and get deleted together.
const MARKER: &str = "network";

/// The marker path for a given wallet config: same directory, so an explicit
/// `--config` carries its own selection exactly the way it carries its own
/// `proof_mode`.
#[must_use]
pub fn marker_path(config: &Path) -> PathBuf {
    config.parent().map_or_else(|| PathBuf::from(MARKER), |d| d.join(MARKER))
}

/// The stored choice, or `None` when this wallet has never been asked.
///
/// A marker that cannot be parsed is an ERROR, never a silent fallback: the
/// whole point of the file is that the user believes it, and quietly ignoring a
/// typo would aim their next transaction at a different chain than the one they
/// think they picked - which does not fail loudly, it fails as absent accounts.
pub fn stored(config: &Path) -> Result<Option<Network>, String> {
    let path = marker_path(config);
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(None) };
    if text.trim().is_empty() {
        return Ok(None);
    }
    Network::parse(text.trim()).map(Some).map_err(|e| {
        format!(
            "{}: {e}\n  Fix or delete that file, or run `lpad network` to pick again.",
            path.display()
        )
    })
}

/// Remember `network` for this wallet. Writes the config form, which
/// [`Network::parse`] reads back to an equal value - `local=<url>` included, so
/// an edited local URL survives.
pub fn remember(config: &Path, network: &Network) -> Result<(), String> {
    let path = marker_path(config);
    std::fs::write(&path, format!("{network}\n"))
        .map_err(|e| format!("write {}: {e} - is the wallet directory writable?", path.display()))
}

/// Can we ask a human right now?
///
/// Both streams must be terminals: stdin because that is where the answer comes
/// from, stderr because that is where the question goes - a menu written into a
/// redirected stderr is a program waiting on input for a question nobody saw.
/// This is the check that keeps scripts/test-all-cli.sh and the e2e gate from
/// hanging.
#[must_use]
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// What the CLI tells a script that hit a decision only a human can make.
pub fn no_tty_error(config: &Path) -> String {
    format!(
        "no network configured for this wallet and no terminal to ask on.\n  \
         Pick one non-interactively by writing the choice next to the wallet:\n    \
         printf 'testnet\\n' > {}\n  \
         (also `paradox`, `local={}`, or any http(s) sequencer URL)\n  \
         or pass --network <choice> to override it for a single command.",
        marker_path(config).display(),
        lpad_sdk::LOCAL_SEQUENCER,
    )
}

// ---------------------------------------------------------------------------
// The picker
// ---------------------------------------------------------------------------

/// Read one line from stdin. `None` means end of input - which is not the same
/// as an empty line: an empty line is "take the default", EOF is "there is
/// nobody there", and treating the second as the first would silently pick a
/// network for a user who is not present.
fn read_line() -> Result<Option<String>, String> {
    let mut s = String::new();
    match std::io::stdin().read_line(&mut s) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(s.trim().to_owned())),
        Err(e) => Err(format!("read from stdin: {e}")),
    }
}

fn prompt(label: &str) -> Result<Option<String>, String> {
    eprint!("{label}");
    let _ = std::io::stderr().flush();
    read_line()
}

/// Ask which sequencer to use. Caller must have checked [`interactive`].
pub fn pick(first_run: bool) -> Result<Network, String> {
    if first_run {
        ui::e_header("No network configured yet. Which sequencer?");
    } else {
        ui::e_header("Which sequencer?");
    }
    let choices = Network::choices();
    for (i, n) in choices.iter().enumerate() {
        eprintln!("  {}) {:<14} {}", i + 1, n.display_name(), n.url());
    }
    eprintln!();
    // Say plainly what choice 3 commits the user to. lpad used to ship a
    // sequencer and no longer does (commit 7119197 deleted run-sequencer.sh), so
    // "Local" must not read as "lpad will start one for you".
    eprintln!("  1 and 2 are shared chains run by others. 3 is a LEZ node YOU install and run -");
    eprintln!("  lpad ships none - so the URL is yours to set, and it must be LEZ v0.2.4 to match");
    eprintln!("  the program ids this build embeds.");
    eprintln!();

    let selected = loop {
        let Some(answer) = prompt("Choice [1]: ")? else {
            return Err("no answer on stdin - re-run with a terminal, or see `lpad network --help`".into());
        };
        if answer.is_empty() {
            break choices[0].clone();
        }
        match answer.parse::<usize>() {
            Ok(n) if (1..=choices.len()).contains(&n) => break choices[n - 1].clone(),
            _ => eprintln!("  not one of 1..{} - try again", choices.len()),
        }
    };

    if !selected.is_local() {
        return Ok(selected);
    }
    // The default is only an offer; whatever the user types is what gets stored.
    loop {
        let label = format!("Local sequencer URL [{}]: ", lpad_sdk::LOCAL_SEQUENCER);
        let Some(url) = prompt(&label)? else {
            return Err("no answer on stdin - re-run with a terminal".into());
        };
        if url.is_empty() {
            return Ok(Network::local(""));
        }
        // Parse through the SDK rather than re-implementing the scheme check, so
        // a bad URL is rejected here with the same words `--network` uses.
        match Network::parse(&format!("local={url}")) {
            Ok(n) => return Ok(n),
            // Sanitised: this error quotes back the line just typed, and a
            // pasted URL is as capable of carrying escape sequences as any
            // other text lpad prints.
            Err(e) => eprintln!("  {}", ui::safe(&e)),
        }
    }
}

/// Yes/no with `yes` as the default. EOF is a NO: an absent user has not agreed
/// to anything, least of all to writing programs to a chain.
fn confirm(label: &str) -> Result<bool, String> {
    let Some(answer) = prompt(label)? else { return Ok(false) };
    Ok(matches!(answer.to_ascii_lowercase().as_str(), "" | "y" | "yes"))
}

// ---------------------------------------------------------------------------
// What follows a pick: is lpad actually on that chain?
// ---------------------------------------------------------------------------

/// Why we are looking at deployment status, which decides what a declined
/// deploy costs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// The picker fired inside another command (`lpad bc buy`, ...). Declining
    /// has to stop that command: it would be aimed at programs that are not
    /// there, and a transaction against a missing program is dropped from the
    /// mempool and reads as a timeout, not as an error.
    FirstRun,
    /// The user ran `lpad network` on purpose. Declining is just an answer.
    NetworkCmd,
}

/// After a pick: check whether lpad's three programs are on that chain and act
/// on the answer. Never fatal for a reason unrelated to the pick - an
/// unreachable sequencer is reported and the command carries on to fail with
/// its own, better error.
pub fn check_deployment(
    config: &Path,
    storage: &Path,
    statistics: &Path,
    network: &Network,
    entry: Entry,
) -> Result<(), String> {
    let mut client = match LaunchpadClient::open(
        config.to_path_buf(),
        storage.to_path_buf(),
        statistics.to_path_buf(),
        Some(network),
    ) {
        Ok(c) => c,
        // Not fatal, and not diagnosed either: the wallet reports "unreachable"
        // and "unreadable config" through the same error, so claim only what is
        // certain - the check did not happen. Whatever the user actually typed
        // runs next and fails with its own, more specific error.
        Err(e) => {
            ui::warn(&format!("skipped the deployment check for {}: {e}", network.url()));
            if network.is_local() {
                eprintln!("  If your node is not running yet, start it and run `lpad network` again");
                eprintln!("  to deploy lpad's programs onto it.");
            }
            return Ok(());
        }
    };
    let status = {
        let sp = ui::Spinner::new("checking which lpad programs are on this chain", false);
        let h = sp.handle();
        client.set_progress(move |m| h.set_message(m.to_owned()));
        let s = client.lpad_deployment_status();
        sp.clear();
        s
    };
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            ui::warn(&format!("could not check what is deployed on {}: {e}", network.url()));
            eprintln!("  Re-run `lpad network` once the chain answers.");
            return Ok(());
        }
    };

    let missing: Vec<&ProgramStatus> = status.iter().filter(|s| !s.deployed()).collect();
    if missing.is_empty() {
        report_present(&status, network);
        return Ok(());
    }
    let names = missing.iter().map(|s| s.name).collect::<Vec<_>>().join(", ");
    ui::warn(&format!("{} has no record of lpad's {names} program(s).", network.url()));
    // `None` is "this chain has no record", not proof of absence - a sequencer
    // that was reset or restored can have forgotten the deploy transaction while
    // still running the program. Say so, so a re-deploy does not read as a
    // contradiction. Being wrong in this direction is free: a re-deploy of an
    // unchanged guest is a no-op.
    eprintln!("  (A chain that was reset can forget the deploy transaction while still");
    eprintln!("   running the program, so this can be a false alarm - re-deploying is a no-op.)");

    if !network.is_local() {
        // Report, never offer. An accidental deploy to a chain other people use
        // is not something a prompt should put one keystroke away, and on the
        // two named chains the operators - not this CLI - deploy. A bare URL is
        // held to the same rule: lpad has no way to know whose chain it is, and
        // `local=<url>` is how the user says "mine".
        let what = if matches!(network, Network::Custom(_)) {
            "given as a bare URL"
        } else {
            "a shared chain run by others"
        };
        eprintln!("  {} is {what}; lpad does not deploy to it from here.", network.url());
        eprintln!("  Deploying is offered only for a sequencer you told lpad you run (`local=<url>`).");
        eprintln!("  Check what is really live with:");
        eprintln!("    LPAD_NETWORK={network} bash scripts/verify-deployment.sh");
        return Ok(());
    }

    eprintln!("  A transaction against a program that is not deployed is dropped from the");
    eprintln!("  mempool and reads as a timeout, not as an error - so deploy them first.");
    if !confirm(&format!("Deploy lpad's 3 programs to {} now? [Y/n]: ", network.url()))? {
        let how = "deploy them later with `lpad network` (answer yes at the prompt)";
        return match entry {
            Entry::NetworkCmd => {
                eprintln!("  Not deployed - {how}.");
                Ok(())
            }
            // Refusing to continue is the point: the command the user actually
            // typed would be submitted against programs that are not there and
            // would look like a slow chain rather than a mistake.
            Entry::FirstRun => Err(format!(
                "lpad's programs are not deployed on {} - the command was not run.\n  {how}, \
                 then re-run it.",
                network.url()
            )),
        };
    }

    let deployed = {
        let sp = ui::Spinner::new("deploying lpad's programs", false);
        let h = sp.handle();
        client.set_progress(move |m| h.set_message(m.to_owned()));
        let d = client.deploy_lpad_programs();
        sp.clear();
        d
    }?;
    report_deployed(&deployed, network);
    Ok(())
}

/// "Everything is already here", with the block each deploy landed in.
fn report_present(status: &[ProgramStatus], network: &Network) {
    ui::e_header(&format!("lpad is deployed on {}", network.url()));
    for s in status {
        let block = s.block.map_or_else(|| "-".to_owned(), |b| b.to_string());
        ui::e_kv(s.name, format!("block {block}  {}", hex_program(s.program_id)));
    }
}

/// What a deploy run actually did, per program: its id and the block holding it.
fn report_deployed(deployed: &[Deployment], network: &Network) {
    ui::e_header(&format!("deployed to {}", network.url()));
    for (name, program) in lpad_sdk::lpad_programs() {
        let Some(d) = deployed.iter().find(|d| d.name == name) else { continue };
        // `already_deployed` names a block that may be far behind the head, so
        // it must not be reported as "landed just now".
        let note = if d.already_deployed { "  (was already on chain)" } else { "" };
        ui::e_kv(name, format!("block {}  {}{note}", d.block, hex_program(program.id())));
    }
}
