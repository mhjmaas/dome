//! Directory auto-activation: the `dome hook <shell>` shell integration, the `dome allow`
//! trust grant, and the hidden `dome __hook-activate` drop-in the hook invokes on a hit.
//!
//! The shell hook walks up from `$PWD` to the nearest `dome.json` in pure shell — the `dome`
//! binary is only ever exec'd once a project is found, so the prompt never blocks on a
//! directory with no project. On a hit the hook runs `dome __hook-activate <project_dir>`,
//! whose exit code tells the hook what happened (dropped in, untrusted, or skip) so it can
//! manage per-terminal-session suppression without a second `dome` call.

use std::path::Path;

use anyhow::Result;

use crate::config::{load_config, ActivateMode};

/// Exit code from `dome __hook-activate`: the developer was dropped into the guest and the
/// shell has now returned to the host. The hook suppresses re-entry for this project for the
/// rest of the terminal session.
pub(crate) const ACTIVATE_DROPPED_IN: i32 = 0;
/// Exit code: the project is untrusted (or its `dome.json` changed since `dome allow`). The
/// binary printed the one-line `dome allow` hint; the hook records that the hint was shown so
/// it is not repeated.
pub(crate) const ACTIVATE_UNTRUSTED: i32 = 10;
/// Exit code: auto-activation is a no-op here (`activate: "off"`, or a guard like an active
/// guest / `$CI` / `$DOME_NO_AUTO` fired). The hook stays quiet and does not re-invoke.
pub(crate) const ACTIVATE_SKIP: i32 = 11;

/// The sentinel env var the installed shell hook exports on `eval`. Its presence in a manual
/// `dome sandbox` session means the hook is active, so the one-time install tip is suppressed.
/// (A child `dome` process inherits the shell's environment, so it sees the export.)
pub(crate) const HOOK_SENTINEL: &str = "DOME_HOOK_INSTALLED";

/// The exact shell rc line that enables the hook for `shell`, e.g. `eval "$(dome hook zsh)"`.
/// This is the single source of truth printed by the install tip, written by `dome hook
/// --install`, and mentioned by `dome init`. Fish has no `eval "$(...)"`; its idiom is to pipe
/// the emitted hook into `source`.
fn hook_rc_line(shell: &str) -> String {
    match shell {
        "fish" => "dome hook fish | source".to_string(),
        _ => format!("eval \"$(dome hook {shell})\""),
    }
}

/// The one-time, copy-paste install tip shown after a manual `dome sandbox` session when the
/// hook is not installed. Shows the exact rc line for `shell` and points at the convenience
/// installer, and promises not to nag again.
fn install_tip(shell: &str) -> String {
    format!(
        "dome: tip — auto-activate this project's sandbox on `cd` by adding the shell hook.\n\
         Add this line to your shell rc:\n\n    {}\n\n\
         …or run `dome hook --install` to do it for you. (This tip won't show again.)",
        hook_rc_line(shell)
    )
}

/// The shell to offer hook integration for, derived from `$SHELL`, limited to shells whose
/// hook `dome` can emit. Returns `Some(shell)` for a zsh/bash/fish `$SHELL` (or when `$SHELL`
/// is unset — zsh is the macOS default), and `None` for any other shell so neither the install
/// tip nor `dome hook --install` suggests a line that would not work. Pure given the env lookup,
/// so it is unit-tested.
fn supported_shell(getenv: impl Fn(&str) -> Option<String>) -> Option<String> {
    let Some(shell) = getenv("SHELL").filter(|s| !s.is_empty()) else {
        return Some("zsh".to_string());
    };
    match Path::new(&shell).file_name().and_then(|s| s.to_str()) {
        Some("zsh") => Some("zsh".to_string()),
        Some("bash") => Some("bash".to_string()),
        Some("fish") => Some("fish".to_string()),
        _ => None,
    }
}

/// The marker file whose existence records that the one-time install tip has been shown, so it
/// never nags again. Lives in the dome data dir alongside the other per-machine state.
fn hook_tip_marker(data_dir: &str) -> String {
    format!("{data_dir}/hook-tip-shown")
}

/// Decide whether a manual `dome sandbox` session should print the one-time install tip, and for
/// which shell. Returns `Some(shell)` only when the hook is NOT installed (the [`HOOK_SENTINEL`]
/// env var is absent), the tip has not been shown before (no marker in `data_dir`), the session
/// is an interactive TTY (`is_tty`), no environment guard (`$CI`, inside a guest, `$DOME_NO_AUTO`)
/// is set, and the shell is one `dome` can emit a hook for. `None` in every other case. Pure given
/// the filesystem, the env lookup, and the TTY flag, so it is unit-tested without printing.
fn hook_tip_shell(
    data_dir: &str,
    getenv: impl Fn(&str) -> Option<String>,
    is_tty: bool,
) -> Option<String> {
    // A copy-paste tip only makes sense on an interactive terminal; never spam a piped log/CI.
    if !is_tty {
        return None;
    }
    // The same guards that block auto-activation suppress the nudge (guest / $CI / opt-out).
    if activation_blocked(&getenv).is_some() {
        return None;
    }
    // The hook exports this on eval; its presence means the hook is already installed.
    if getenv(HOOK_SENTINEL).is_some_and(|v| !v.is_empty()) {
        return None;
    }
    // Shown once: the marker makes the tip a one-time nudge.
    if Path::new(&hook_tip_marker(data_dir)).exists() {
        return None;
    }
    supported_shell(&getenv)
}

/// The shell rc file `dome hook --install` appends the hook line to, for a supported `shell`,
/// resolved relative to `home`. `None` for a shell we don't write an rc path for. Pure, so the
/// mapping is unit-tested without touching `$HOME`.
fn rc_path(shell: &str, home: &Path) -> Option<std::path::PathBuf> {
    match shell {
        "zsh" => Some(home.join(".zshrc")),
        "bash" => Some(home.join(".bashrc")),
        "fish" => Some(home.join(".config/fish/config.fish")),
        _ => None,
    }
}

/// Append the hook `line` to existing rc `contents`, returning the new contents — or `None` when
/// the line (ignoring surrounding whitespace) is already present, so re-running `dome hook
/// --install` never duplicates it. A managed comment precedes the line so it is identifiable in
/// the rc file, and a missing trailing newline is repaired first. Pure, so idempotency is
/// unit-tested without filesystem I/O.
fn append_rc_line(contents: &str, line: &str) -> Option<String> {
    if contents.lines().any(|l| l.trim() == line) {
        return None;
    }
    let mut out = contents.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("\n# dome directory auto-activation\n{line}\n"));
    Some(out)
}

/// The one-line directory-auto-activation hint `dome init` prints after readying the OS image,
/// so the feature is discoverable right after install. Mentions the exact rc line and the
/// convenience installer.
pub(crate) fn init_hook_hint() -> String {
    format!(
        "dome: tip — enable directory auto-activation (drop into a project's sandbox on `cd`) \
         by adding `{}` to your shell rc, or run `dome hook --install`.",
        hook_rc_line("zsh")
    )
}

/// `dome hook <shell>`: print the shell-integration hook to stdout. Supports `zsh`, `bash`, and
/// `fish`; any other shell is a clear error rather than a silent no-op the developer would `eval`
/// into nothing.
pub(crate) fn run_hook(shell: &str) -> Result<()> {
    let cmd = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "dome".to_string());
    match shell {
        "zsh" => {
            print!("{}", emit_zsh_hook(&cmd));
            Ok(())
        }
        "bash" => {
            print!("{}", emit_bash_hook(&cmd));
            Ok(())
        }
        "fish" => {
            print!("{}", emit_fish_hook(&cmd));
            Ok(())
        }
        other => anyhow::bail!("unknown shell '{other}'. Supported: zsh, bash, fish."),
    }
}

/// Print the one-time install tip before a manual `dome sandbox shell`/`run` boots its VM, when
/// the shell hook is not installed (see [`hook_tip_shell`] for the full set of conditions). Drops
/// a marker afterward so it never nags again. Never fails the command — discoverability is a
/// convenience, and a session must run even if the marker can't be written.
pub(crate) fn maybe_print_hook_tip() {
    use std::io::IsTerminal;

    let data_dir = dome_vm::default_data_dir();
    let is_tty = std::io::stderr().is_terminal();
    let Some(shell) = hook_tip_shell(&data_dir, |k| std::env::var(k).ok(), is_tty) else {
        return;
    };
    eprintln!("{}", install_tip(&shell));
    // Best effort: record that the tip has been shown so it stays a one-time nudge.
    let _ = std::fs::create_dir_all(&data_dir);
    let _ = std::fs::write(hook_tip_marker(&data_dir), b"1");
}

/// `dome hook --install`: append the hook line to the shell rc file detected from `$SHELL`, as an
/// opt-in convenience over copy-pasting it. Idempotent — re-running never duplicates the line.
/// Refuses (with the manual line to paste) when `$SHELL` is not a shell `dome` can emit a hook for
/// yet, and never modifies any rc file in that case.
pub(crate) fn run_hook_install() -> Result<()> {
    let getenv = |k: &str| std::env::var(k).ok();
    let shell = supported_shell(getenv).ok_or_else(|| {
        anyhow::anyhow!(
            "could not detect a supported shell from $SHELL (supported: zsh, bash, fish). Add \
             this to your shell rc manually:\n\n    {}",
            hook_rc_line("zsh")
        )
    })?;
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| anyhow::anyhow!("$HOME is not set — cannot locate your shell rc file"))?;
    let rc = rc_path(&shell, &home)
        .ok_or_else(|| anyhow::anyhow!("no known rc file for shell '{shell}'"))?;
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    match append_rc_line(&existing, &hook_rc_line(&shell)) {
        Some(updated) => {
            if let Some(parent) = rc.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&rc, updated)?;
            eprintln!(
                "dome: added the {shell} hook to {} — restart your shell or run `source {}`.",
                rc.display(),
                rc.display()
            );
        }
        None => eprintln!(
            "dome: the hook is already installed in {} — nothing to do.",
            rc.display()
        ),
    }
    Ok(())
}

/// `dome allow`: trust the nearest project so the hook will auto-activate it. Walks up from
/// the cwd to the nearest `dome.json` and records a trust entry pinned to that directory and
/// the current `dome.json` content. Errors when there is no project to trust.
pub(crate) fn run_allow() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project_dir = crate::config::find_nearest_dome_json(&cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "no dome.json found in {} or any parent directory — nothing to allow",
            cwd.display()
        )
    })?;
    let bytes = std::fs::read(project_dir.join("dome.json"))?;
    let data_dir = dome_vm::default_data_dir();
    // Informed re-approval: if this project was allowed before but its dome.json has changed
    // since, show what changed before re-recording — so re-approval is a deliberate review of
    // the edit, not a blind re-grant.
    if let Some(diff) = reapproval_diff(&data_dir, &project_dir, &bytes) {
        eprintln!(
            "dome: {}'s dome.json has changed since you last allowed it. Review what changed:\n",
            project_dir.display()
        );
        eprintln!("{}", diff.trim_end());
        eprintln!();
    }
    // Offer to pin a stable `sandbox` name before recording trust, so the trust record (and thus
    // auto-activation) pins to the final, possibly-just-pinned content.
    let bytes = maybe_offer_pin(&project_dir, bytes);
    crate::trust::record_trust(&data_dir, &project_dir, &bytes)?;
    eprintln!(
        "dome: trusted {} — the shell hook will now auto-activate it (edits to dome.json \
         re-lock until you run `dome allow` again).",
        project_dir.display()
    );
    Ok(())
}

/// `dome __hook-activate <project_dir>`: the hook's per-directory drop-in. Decides what to do
/// for the found project and, when trusted + activate-on, drops the developer into its
/// sandbox. The process exit code is the hook's signal (see the `ACTIVATE_*` constants).
pub(crate) fn run_hook_activate(project_dir: &str) -> Result<i32> {
    let data_dir = dome_vm::default_data_dir();
    let dir = Path::new(project_dir);
    match decide(&data_dir, dir, |k| std::env::var(k).ok())? {
        Decision::Skip => Ok(ACTIVATE_SKIP),
        // The untrusted hint is printed by the SHELL HOOK, not here: only the hook can show it
        // "at most once per terminal session" (each `__hook-activate` is a fresh process). The
        // binary just signals the state via the exit code.
        Decision::Untrusted => Ok(ACTIVATE_UNTRUSTED),
        Decision::Activate => {
            drop_in(dir)?;
            Ok(ACTIVATE_DROPPED_IN)
        }
    }
}

/// Drop into the project's sandbox shell, landing at the mapped subdirectory the developer is
/// in (falling back to the project root). Resolves the sandbox from the project's `dome.json`
/// exactly as a manual `dome sandbox shell` would: it chdirs into the project so name/config
/// resolution and the runtime project mount all anchor there, but remembers the original cwd
/// so subdir landing can be computed.
fn drop_in(project_dir: &Path) -> Result<()> {
    // Compute the guest landing from the ORIGINAL host cwd, before we chdir away. Both paths
    // are canonicalized so a subdir under the project resolves even when the host cwd reaches
    // it through a symlink (e.g. macOS `/var` → `/private/var`). A mapped subdir lands at
    // `/workspace/<sub>`; entering at the root, or from outside the project, falls back to the
    // project root `/workspace` (which the profile only cd's into if it is actually mounted).
    let host_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let project_canon =
        std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    let land = crate::vm::guest_landing_cwd(&project_canon, &host_cwd)
        .unwrap_or_else(|| crate::vm::GUEST_PROJECT_ROOT.to_string());
    // Resolve the sandbox name with the collision-proof *auto-activation* policy: an explicit
    // `sandbox` field wins, else `<slug>-<pathhash>` so two different directories with the same
    // basename never silently share one VM. (Manual `dome sandbox shell` keeps the bare cwd-slug;
    // only this auto-drop path hashes the path.) Computed from the canonical project dir before we
    // chdir, then passed explicitly so it bypasses `run_sandbox`'s manual cwd-slug fallback.
    let cfg = load_config(Some(
        project_dir.join("dome.json").to_string_lossy().as_ref(),
    ))?;
    let name = crate::sandbox::auto_activation_name(&cfg, &project_canon)?;
    // chdir into the project so `dome.json` (and thus the project mount) resolves there, matching
    // what `dome sandbox shell` does from the project root.
    std::env::set_current_dir(project_dir)?;
    crate::sandbox::run_sandbox(
        Some(name),
        &crate::cli::VmArgs::default(),
        Vec::new(),
        None,
        false,
        Some(&land),
    )?;
    Ok(())
}

/// Emit the zsh shell-integration hook for `eval "$(dome hook zsh)"`. `cmd` is the absolute
/// path to the `dome` binary, baked in so the hook keeps working even if `dome` later leaves
/// `$PATH`; `DOME_HOOK_CMD` overrides it (used to drive a fake shim in shell-level tests).
///
/// The hook registers a single function on both `chpwd` (directory change) and `precmd` (so a
/// terminal opened already inside a project still activates). It walks up to the nearest
/// `dome.json` in pure shell and only execs `dome` on a hit, so the prompt never blocks on a
/// non-project directory. Per-terminal-session suppression (`__dome_suppressed_root`) prevents
/// the exit→re-drop loop: after you leave the guest you stay on the host until you `cd` out of
/// the project and back in.
pub(crate) fn emit_zsh_hook(cmd: &str) -> String {
    // `cmd` is single-quoted into the script; a path with a literal `'` is pathological and
    // not worth supporting, but escape it defensively so the emitted script is always valid.
    let cmd = cmd.replace('\'', r"'\''");
    format!(
        r#"# dome directory auto-activation (zsh). Installed via: eval "$(dome hook zsh)"
# Sentinel: marks the hook as installed so a manual `dome sandbox` session can skip the
# one-time install tip. Child processes inherit it from the shell environment.
export {sentinel}=1
__dome_hook() {{
  emulate -L zsh
  # No-op guards: only ever activate an interactive terminal the developer is driving.
  [[ -o interactive ]] || return 0
  [[ -t 0 && -t 1 ]] || return 0
  [[ -n "$CI" ]] && return 0
  [[ -n "$DOME_SANDBOX" ]] && return 0
  [[ "$DOME_NO_AUTO" == "1" ]] && return 0

  # Only react when the directory actually changed (precmd fires every prompt).
  [[ "$PWD" == "$__dome_last_pwd" ]] && return 0
  __dome_last_pwd="$PWD"

  # Pure-shell walk up to the nearest dome.json. `dome` is never exec'd on a miss.
  local dir="$PWD" found=""
  while true; do
    if [[ -f "$dir/dome.json" ]]; then found="$dir"; break; fi
    [[ "$dir" == "/" ]] && break
    dir="${{dir:h}}"
  done
  if [[ -z "$found" ]]; then
    # Left every project: clear suppression so re-entering re-activates.
    __dome_suppressed_root=""
    return 0
  fi

  # Already activated this project this session (and returned to the host): stay put until
  # the developer leaves the project and comes back.
  [[ "$__dome_suppressed_root" == "$found" ]] && return 0

  "${{DOME_HOOK_CMD:-'{cmd}'}}" __hook-activate "$found"
  local rc=$?
  # Suppress re-entry for this project until the developer leaves and returns (cleared above
  # when the walk finds no project). Set for every outcome so we never re-invoke `dome` on a
  # cd *within* the same project.
  __dome_suppressed_root="$found"
  if [[ $rc -eq {dropped} ]]; then
    print -u2 "dome: back on the host — cd out of and into ${{found:t}} to re-enter"
  elif [[ $rc -eq {untrusted} ]]; then
    # Print the `dome allow` hint at most once per terminal session, even across re-entries:
    # remember which roots have been hinted (this list is never cleared on cd-out).
    if [[ " $__dome_hinted " != *" $found "* ]]; then
      __dome_hinted="$__dome_hinted $found"
      print -u2 "dome: ${{found:t}} has a dome.json but is not trusted. Run 'dome allow' to auto-activate it."
    fi
  fi
}}
autoload -Uz add-zsh-hook
add-zsh-hook chpwd __dome_hook
add-zsh-hook precmd __dome_hook
"#,
        cmd = cmd,
        sentinel = HOOK_SENTINEL,
        dropped = ACTIVATE_DROPPED_IN,
        untrusted = ACTIVATE_UNTRUSTED,
    )
}

/// Emit the bash shell-integration hook for `eval "$(dome hook bash)"`. Bash has no per-`cd`
/// hook, so the function is wired into `PROMPT_COMMAND` (it runs before every prompt) and a
/// `$PWD`-change guard makes it a no-op unless the directory actually changed — the bash analog
/// of zsh's `chpwd`+`precmd`. Detection, the no-op guard set, per-terminal-session suppression,
/// and the untrusted hint all match the zsh hook exactly; only the idioms differ (`$-`/`${x%/*}`
/// instead of `-o interactive`/`:h`). `cmd` is the baked dome path; `DOME_HOOK_CMD` overrides it
/// (used to drive a fake shim in shell-level tests).
pub(crate) fn emit_bash_hook(cmd: &str) -> String {
    let cmd = cmd.replace('\'', r"'\''");
    format!(
        r#"# dome directory auto-activation (bash). Installed via: eval "$(dome hook bash)"
# Sentinel: marks the hook as installed so a manual `dome sandbox` session can skip the
# one-time install tip. Child processes inherit it from the shell environment.
export {sentinel}=1
__dome_hook() {{
  # No-op guards: only ever activate an interactive terminal the developer is driving.
  case $- in *i*) ;; *) return 0 ;; esac
  [[ -t 0 && -t 1 ]] || return 0
  [[ -n "$CI" ]] && return 0
  [[ -n "$DOME_SANDBOX" ]] && return 0
  [[ "$DOME_NO_AUTO" == "1" ]] && return 0

  # PROMPT_COMMAND runs before every prompt; only react when the directory changed.
  [[ "$PWD" == "$__dome_last_pwd" ]] && return 0
  __dome_last_pwd="$PWD"

  # Pure-shell walk up to the nearest dome.json. `dome` is never exec'd on a miss.
  local dir="$PWD" found=""
  while [[ -n "$dir" ]]; do
    if [[ -f "$dir/dome.json" ]]; then found="$dir"; break; fi
    [[ "$dir" == "/" ]] && break
    dir="${{dir%/*}}"
    [[ -z "$dir" ]] && dir="/"
  done
  if [[ -z "$found" ]]; then
    # Left every project: clear suppression so re-entering re-activates.
    __dome_suppressed_root=""
    return 0
  fi

  # Already activated this project this session (and returned to the host): stay put until
  # the developer leaves the project and comes back.
  [[ "$__dome_suppressed_root" == "$found" ]] && return 0

  "${{DOME_HOOK_CMD:-'{cmd}'}}" __hook-activate "$found"
  local rc=$?
  # Suppress re-entry for this project until the developer leaves and returns.
  __dome_suppressed_root="$found"
  if [[ $rc -eq {dropped} ]]; then
    printf '%s\n' "dome: back on the host — cd out of and into ${{found##*/}} to re-enter" >&2
  elif [[ $rc -eq {untrusted} ]]; then
    # Print the `dome allow` hint at most once per terminal session, even across re-entries.
    if [[ " $__dome_hinted " != *" $found "* ]]; then
      __dome_hinted="$__dome_hinted $found"
      printf '%s\n' "dome: ${{found##*/}} has a dome.json but is not trusted. Run 'dome allow' to auto-activate it." >&2
    fi
  fi
}}
# Prepend the hook to PROMPT_COMMAND (once), preserving any existing value.
case "$PROMPT_COMMAND" in
  *__dome_hook*) ;;
  *) PROMPT_COMMAND="__dome_hook${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}" ;;
esac
"#,
        cmd = cmd,
        sentinel = HOOK_SENTINEL,
        dropped = ACTIVATE_DROPPED_IN,
        untrusted = ACTIVATE_UNTRUSTED,
    )
}

/// Emit the fish shell-integration hook for `dome hook fish | source`. Fish fires the function
/// on every `$PWD` change via `--on-variable PWD` (its native directory-change event — the
/// analog of zsh's `chpwd`). Detection, the no-op guard set, per-terminal-session suppression,
/// and the untrusted hint all match the zsh hook exactly, translated to fish idioms (`test`,
/// `status is-interactive`, `isatty`, `dirname`). `cmd` is the baked dome path; `DOME_HOOK_CMD`
/// overrides it (used to drive a fake shim in shell-level tests).
pub(crate) fn emit_fish_hook(cmd: &str) -> String {
    let cmd = cmd.replace('\'', r"\'");
    format!(
        r#"# dome directory auto-activation (fish). Installed via: dome hook fish | source
# Sentinel: marks the hook as installed so a manual `dome sandbox` session can skip the
# one-time install tip. Child processes inherit it from the shell environment.
set -gx {sentinel} 1
function __dome_hook --on-variable PWD
  # No-op guards: only ever activate an interactive terminal the developer is driving.
  status is-interactive; or return 0
  isatty stdin; and isatty stdout; or return 0
  test -n "$CI"; and return 0
  test -n "$DOME_SANDBOX"; and return 0
  test "$DOME_NO_AUTO" = "1"; and return 0

  # --on-variable PWD only fires on a real change, but guard defensively all the same.
  test "$PWD" = "$__dome_last_pwd"; and return 0
  set -g __dome_last_pwd "$PWD"

  # Pure-shell walk up to the nearest dome.json. `dome` is never exec'd on a miss.
  set -l dir "$PWD"
  set -l found ""
  while true
    if test -f "$dir/dome.json"
      set found "$dir"
      break
    end
    test "$dir" = "/"; and break
    set dir (dirname "$dir")
  end
  if test -z "$found"
    # Left every project: clear suppression so re-entering re-activates.
    set -g __dome_suppressed_root ""
    return 0
  end

  # Already activated this project this session (and returned to the host): stay put until
  # the developer leaves the project and comes back.
  test "$__dome_suppressed_root" = "$found"; and return 0

  set -l cmd $DOME_HOOK_CMD
  test -z "$cmd"; and set cmd '{cmd}'
  "$cmd" __hook-activate "$found"
  set -l rc $status
  # Suppress re-entry for this project until the developer leaves and returns.
  set -g __dome_suppressed_root "$found"
  if test $rc -eq {dropped}
    echo "dome: back on the host — cd out of and into "(basename "$found")" to re-enter" >&2
  else if test $rc -eq {untrusted}
    # Print the `dome allow` hint at most once per terminal session, even across re-entries.
    if not contains -- "$found" $__dome_hinted
      set -g __dome_hinted $__dome_hinted "$found"
      echo "dome: "(basename "$found")" has a dome.json but is not trusted. Run 'dome allow' to auto-activate it." >&2
    end
  end
end
"#,
        cmd = cmd,
        sentinel = HOOK_SENTINEL,
        dropped = ACTIVATE_DROPPED_IN,
        untrusted = ACTIVATE_UNTRUSTED,
    )
}

/// Environment-based no-op guards the drop-in itself enforces, independent of the shell hook
/// (defense in depth: a hand-run `dome __hook-activate`, or a future bash/fish hook, hits the
/// same gate). Returns `Some(reason)` when auto-activation must not fire. The interactive /
/// TTY guards live in the shell hook; these are the ones decidable from the environment.
///
/// Generic over an env lookup so it is unit-testable without mutating the process environment.
pub(crate) fn activation_blocked(getenv: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    // Already inside a dome guest: the worker injects DOME_SANDBOX into every guest session,
    // so its presence means we are nested. Never boot a VM inside a VM.
    if getenv("DOME_SANDBOX").is_some_and(|v| !v.is_empty()) {
        return Some("already inside a dome sandbox");
    }
    // CI runners are non-interactive by intent; never hijack them into a sandbox shell.
    if getenv("CI").is_some_and(|v| !v.is_empty()) {
        return Some("$CI is set");
    }
    // Explicit per-environment opt-out.
    if getenv("DOME_NO_AUTO").as_deref() == Some("1") {
        return Some("$DOME_NO_AUTO=1");
    }
    None
}

/// The activation decision for a found project directory, factored out of the process-level
/// drop-in so it is testable without booting a VM. Resolves the guards, the `activate` field,
/// and the trust gate into the exit code the hook acts on. `Activate` means "actually drop
/// in"; the caller performs the (unmockable) VM boot.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Drop into the sandbox shell for this trusted, activate-on project.
    Activate,
    /// Untrusted or changed since `dome allow`: print the hint, do not drop in.
    Untrusted,
    /// A no-op (guard fired, or `activate: "off"`): stay quiet.
    Skip,
}

/// Decide what auto-activation should do for `project_dir` (the directory containing the
/// `dome.json` the hook found), reading trust records from `data_dir`. Pure given the
/// filesystem and the passed env lookup, so it is unit-tested directly.
pub(crate) fn decide(
    data_dir: &str,
    project_dir: &Path,
    getenv: impl Fn(&str) -> Option<String>,
) -> Result<Decision> {
    if activation_blocked(getenv).is_some() {
        return Ok(Decision::Skip);
    }
    let config_path = project_dir.join("dome.json");
    let bytes = match std::fs::read(&config_path) {
        Ok(b) => b,
        // The hook only calls us after its own walk found a dome.json; a race that deletes it
        // between the walk and here is a clean no-op rather than an error.
        Err(_) => return Ok(Decision::Skip),
    };
    let cfg = load_config(Some(config_path.to_string_lossy().as_ref()))?;
    if cfg.activate() == ActivateMode::Off {
        return Ok(Decision::Skip);
    }
    if crate::trust::is_trusted(data_dir, project_dir, &bytes) {
        Ok(Decision::Activate)
    } else {
        Ok(Decision::Untrusted)
    }
}

/// A minimal line-level diff of two texts, rendered with `- ` (removed), `+ ` (added), and
/// `  ` (unchanged context) prefixes — enough for `dome allow` to show what changed in a
/// `dome.json` before re-recording trust. Uses a standard longest-common-subsequence so a
/// pure insertion shows as additions only (not a wholesale replace). Not a full unified diff
/// (no hunk headers); the configs it diffs are small, so every line is shown.
fn unified_line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    // LCS length table over the two line sequences.
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    // Walk the table, emitting context for common lines and -/+ for the rest.
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push_str("  ");
            out.push_str(a[i]);
            out.push('\n');
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push_str("- ");
            out.push_str(a[i]);
            out.push('\n');
            i += 1;
        } else {
            out.push_str("+ ");
            out.push_str(b[j]);
            out.push('\n');
            j += 1;
        }
    }
    for line in &a[i..] {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    for line in &b[j..] {
        out.push_str("+ ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Offer an inline trust grant before a manual `dome sandbox shell`/`run` boots its VM: if the
/// developer is in an untrusted project (and the environment allows a prompt — see
/// [`inline_trust_target`]), ask `Trust '<name>' and auto-activate on entry? [y/N]`. A `y`
/// records the same trust record the auto-hook checks, so future terminals drop in automatically;
/// anything else runs this session once and records nothing. Never fails the command: a project
/// with no `dome.json`, an already-trusted one, or a non-interactive session is a silent no-op.
pub(crate) fn maybe_prompt_inline_trust() -> Result<()> {
    use std::io::{IsTerminal, Write};

    let data_dir = dome_vm::default_data_dir();
    let cwd = std::env::current_dir()?;
    let is_tty = std::io::stdin().is_terminal();
    let Some(project_dir) = inline_trust_target(&data_dir, &cwd, |k| std::env::var(k).ok(), is_tty)
    else {
        return Ok(());
    };

    let bytes = std::fs::read(project_dir.join("dome.json"))?;
    // Label the prompt with the sandbox name the project resolves to, matching what the hook
    // would auto-activate.
    let cfg = load_config(Some(
        project_dir.join("dome.json").to_string_lossy().as_ref(),
    ))?;
    let name = crate::sandbox::auto_activation_name(&cfg, &project_dir)
        .unwrap_or_else(|_| project_dir.display().to_string());

    eprint!("dome: trust '{name}' and auto-activate on entry? [y/N] ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("y") {
        // Offer to pin a stable name (no-op when one is already set) so the manual run that
        // follows and future auto-activations all resolve to the same sandbox.
        let bytes = maybe_offer_pin(&project_dir, bytes);
        crate::trust::record_trust(&data_dir, &project_dir, &bytes)?;
        eprintln!(
            "dome: trusted {} — future terminals will auto-activate it (edits to dome.json \
             re-lock until you run `dome allow` again).",
            project_dir.display()
        );
    } else {
        eprintln!(
            "dome: not trusted — running this session once. Run `dome allow` (or answer y next \
             time) to auto-activate on entry."
        );
    }
    Ok(())
}

/// Decide whether a manual `dome sandbox shell`/`run` should offer an inline trust grant, and
/// for which project directory. Returns the canonical project dir to offer trust for, or `None`
/// when no prompt should appear. The offer is made only when, walking up from `cwd`, there is a
/// `dome.json` whose project is untrusted, auto-activation is enabled for it, the session is an
/// interactive TTY (`is_tty`), and no environment guard (`$CI`, inside a guest, `$DOME_NO_AUTO`)
/// is set. So naturally running the sandbox by hand offers the grant — the developer never has
/// to remember `dome allow` — while trusted dirs, non-projects, and non-interactive/CI sessions
/// stay silent. Pure given the filesystem, the env lookup, and the TTY flag, so it is unit-tested
/// without prompting.
fn inline_trust_target(
    data_dir: &str,
    cwd: &Path,
    getenv: impl Fn(&str) -> Option<String>,
    is_tty: bool,
) -> Option<std::path::PathBuf> {
    // A prompt only makes sense on an interactive terminal that can answer it.
    if !is_tty {
        return None;
    }
    // The same guards that block auto-activation suppress the offer (don't nag in CI, never
    // prompt inside a guest, honor the opt-out).
    if activation_blocked(&getenv).is_some() {
        return None;
    }
    let project_dir = crate::config::find_nearest_dome_json(cwd)?;
    let config_path = project_dir.join("dome.json");
    let bytes = std::fs::read(&config_path).ok()?;
    let cfg = load_config(Some(config_path.to_string_lossy().as_ref())).ok()?;
    // With auto-activation disabled, granting trust would buy nothing, so stay silent.
    if cfg.activate() == ActivateMode::Off {
        return None;
    }
    // Already trusted (dir + current content match): run with no prompt.
    if crate::trust::is_trusted(data_dir, &project_dir, &bytes) {
        return None;
    }
    std::fs::canonicalize(&project_dir).ok()
}

/// After trust is granted for `project_dir`, offer to pin a stable `sandbox: "<slug>"` into its
/// `dome.json` when it has none. On a `y`, the field is written and the new file bytes are
/// returned so the caller records trust against the *pinned* content — which converges the manual
/// cwd-slug and the auto-activation `<slug>-<pathhash>` onto one stable, user-chosen sandbox name.
/// Returns the unchanged `current_bytes` whenever there is nothing to offer (an explicit name is
/// already set), the session can't prompt (no TTY), the user declines, or the edit can't be safely
/// applied. Never fails the surrounding command — pinning is a convenience, not a requirement.
fn maybe_offer_pin(project_dir: &Path, current_bytes: Vec<u8>) -> Vec<u8> {
    use std::io::{IsTerminal, Write};

    // A pin is an interactive yes/no; a piped/non-interactive `dome allow` just records trust.
    if !std::io::stdin().is_terminal() {
        return current_bytes;
    }
    let Ok(text) = std::str::from_utf8(&current_bytes) else {
        return current_bytes;
    };
    let Ok(cfg) = serde_json::from_str::<crate::config::DomeConfig>(text) else {
        return current_bytes;
    };
    let Some(slug) = pin_offer_slug(&cfg, project_dir) else {
        return current_bytes;
    };
    let Some(edited) = insert_sandbox_field(text, &slug) else {
        return current_bytes;
    };

    eprint!(
        "dome: pin a stable sandbox name \"{slug}\" into {}'s dome.json? \
         (otherwise auto-activation uses a path-hashed name) [y/N] ",
        project_dir.display()
    );
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() || !answer.trim().eq_ignore_ascii_case("y")
    {
        return current_bytes;
    }
    let dome_json = project_dir.join("dome.json");
    if let Err(e) = std::fs::write(&dome_json, &edited) {
        eprintln!(
            "dome: could not write {}: {e} — leaving it unpinned.",
            dome_json.display()
        );
        return current_bytes;
    }
    eprintln!("dome: pinned sandbox \"{slug}\" — manual `dome sandbox` and auto-activation now share one VM.");
    edited.into_bytes()
}

/// The stable `sandbox` name to offer pinning into a project's `dome.json`, or `None` when no
/// offer should be made. Returns the plain cwd-slug only for a project that has NOT already set
/// an explicit `sandbox` field — a project with a name is already collision-free, so there is
/// nothing to pin. Pure; the I/O wrapper [`maybe_offer_pin`] does the prompting and the write.
fn pin_offer_slug(cfg: &crate::config::DomeConfig, project_dir: &Path) -> Option<String> {
    if cfg.sandbox.as_deref().is_some_and(|s| !s.is_empty()) {
        return None;
    }
    crate::sandbox::project_slug(project_dir)
}

/// Insert a top-level `"sandbox": "<slug>"` into the raw `dome.json` text, preserving the rest
/// of the file (a textual edit rather than a parse-and-reserialize, so existing key order,
/// indentation, and the user's formatting survive). Returns `None` when the text is not a JSON
/// object we can safely edit — including when the result would not parse — so a malformed or
/// non-object `dome.json` is left untouched rather than corrupted. The caller has already
/// established (via [`pin_offer_slug`]) that there is no existing `sandbox` key.
fn insert_sandbox_field(text: &str, slug: &str) -> Option<String> {
    let open = text.find('{')?;
    let close = text.rfind('}')?;
    if close <= open {
        return None;
    }
    let field = format!("\"sandbox\": \"{slug}\"");
    // Empty object (only whitespace between the braces): write the sole field on its own line.
    let inner = &text[open + 1..close];
    let edited = if inner.trim().is_empty() {
        format!("{}\n  {field}\n{}", &text[..=open], &text[close..])
    } else {
        // Non-empty: prepend the field (with a trailing comma) right after the opening brace, so
        // it precedes the existing keys without disturbing them.
        format!("{}\n  {field},{}", &text[..=open], &text[open + 1..])
    };
    // Never hand back something that would not parse: a defensive guard against an exotic layout
    // the simple textual edit mishandles.
    serde_json::from_str::<serde_json::Value>(&edited)
        .ok()
        .filter(|v| v.is_object())
        .map(|_| edited)
}

/// Compute the informed-re-approval diff for `dome allow`: when `project_dir` was previously
/// allowed but its `dome.json` has changed since (hash mismatch), return a human-readable diff
/// of the approved content vs. the current `new_bytes`. Returns `None` when there is nothing to
/// re-approve — either the project was never allowed, or its content is unchanged (still
/// trusted). A legacy record (approved before the content was stored) has no baseline to diff,
/// so a note stands in for the old side rather than a misleading empty diff.
fn reapproval_diff(data_dir: &str, project_dir: &Path, new_bytes: &[u8]) -> Option<String> {
    let prior = crate::trust::prior_trust(data_dir, project_dir)?;
    if prior.hash == crate::trust::config_hash(new_bytes) {
        return None; // unchanged since approval — no diff to show
    }
    let new_text = String::from_utf8_lossy(new_bytes);
    Some(match prior.config {
        Some(old_text) => unified_line_diff(&old_text, &new_text),
        None => format!(
            "(the previous approval predates content recording, so the old dome.json cannot be \
             shown — current content:)\n{new_text}"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn load_config_str(s: &str) -> crate::config::DomeConfig {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn line_diff_marks_removed_added_and_keeps_context() {
        let old = "{\n  \"sandbox\": \"web\"\n}\n";
        let new = "{\n  \"sandbox\": \"web\",\n  \"allow_net\": true\n}\n";
        let diff = unified_line_diff(old, new);
        // The unchanged opening/closing braces are shown as context (two-space prefix).
        assert!(diff.contains("  {"), "context line kept; got:\n{diff}");
        // The edited sandbox line is removed and its replacement added.
        assert!(
            diff.contains("-   \"sandbox\": \"web\""),
            "old line marked removed; got:\n{diff}"
        );
        assert!(
            diff.contains("+   \"sandbox\": \"web\","),
            "new line marked added; got:\n{diff}"
        );
        // The brand-new line appears as an addition.
        assert!(
            diff.contains("+   \"allow_net\": true"),
            "added line present; got:\n{diff}"
        );
        // Exactly one line was removed (only the changed sandbox line).
        let removed = diff.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            removed, 1,
            "only the changed line is a removal; got:\n{diff}"
        );
    }

    #[test]
    fn line_diff_of_identical_input_has_no_markers() {
        let diff = unified_line_diff("a\nb\n", "a\nb\n");
        assert!(
            !diff.contains("+ ") && !diff.contains("- "),
            "identical content yields no add/remove markers; got:\n{diff}"
        );
    }

    #[test]
    fn reapproval_diff_is_none_for_a_never_allowed_project() {
        let (project, data) = fixture(r#"{"sandbox":"web"}"#);
        // No prior `dome allow`, so there is nothing to re-approve and no diff to show.
        assert!(reapproval_diff(
            data.path().to_str().unwrap(),
            project.path(),
            br#"{"sandbox":"web"}"#
        )
        .is_none());
    }

    #[test]
    fn reapproval_diff_is_none_when_config_is_unchanged() {
        let (project, data) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        crate::trust::record_trust(dd, project.path(), br#"{"sandbox":"web"}"#).unwrap();
        // Still trusted at the same content: re-running `dome allow` shows no diff.
        assert!(reapproval_diff(dd, project.path(), br#"{"sandbox":"web"}"#).is_none());
    }

    #[test]
    fn inline_offer_targets_an_untrusted_project_on_an_interactive_terminal() {
        let (project, data) = fixture(r#"{"sandbox":"web"}"#);
        let target =
            inline_trust_target(data.path().to_str().unwrap(), project.path(), no_env, true);
        assert_eq!(
            target.as_deref(),
            Some(std::fs::canonicalize(project.path()).unwrap().as_path()),
            "running the sandbox by hand in an untrusted project offers the grant"
        );
    }

    #[test]
    fn inline_offer_is_silent_for_a_trusted_project() {
        let (project, data) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        crate::trust::record_trust(dd, project.path(), br#"{"sandbox":"web"}"#).unwrap();
        // Already trusted → no prompt; the sandbox just runs.
        assert!(inline_trust_target(dd, project.path(), no_env, true).is_none());
    }

    #[test]
    fn inline_offer_is_silent_when_there_is_no_project() {
        // A directory with no dome.json (and no parent with one) has nothing to trust.
        let data = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        assert!(
            inline_trust_target(data.path().to_str().unwrap(), empty.path(), no_env, true)
                .is_none()
        );
    }

    #[test]
    fn inline_offer_is_silent_when_activation_is_off() {
        let (project, data) = fixture(r#"{"activate":"off"}"#);
        // activate:"off" means no auto-activation will ever happen, so offering trust is moot.
        assert!(
            inline_trust_target(data.path().to_str().unwrap(), project.path(), no_env, true)
                .is_none()
        );
    }

    #[test]
    fn inline_offer_is_silent_without_a_tty() {
        let (project, data) = fixture(r#"{"sandbox":"web"}"#);
        // A piped/non-interactive `dome sandbox run` cannot answer a prompt; never block on one.
        assert!(
            inline_trust_target(data.path().to_str().unwrap(), project.path(), no_env, false)
                .is_none()
        );
    }

    #[test]
    fn inline_offer_is_silent_when_a_guard_fires() {
        let (project, data) = fixture(r#"{"sandbox":"web"}"#);
        // $CI / inside-a-guest / opt-out: the same guards that block auto-activation also
        // suppress the inline offer (e.g. don't nag in CI).
        let target = inline_trust_target(
            data.path().to_str().unwrap(),
            project.path(),
            |k| (k == "CI").then(|| "true".to_string()),
            true,
        );
        assert!(target.is_none());
    }

    #[test]
    fn reapproval_diff_shows_what_changed_since_approval() {
        let (project, data) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        crate::trust::record_trust(dd, project.path(), b"{\n  \"sandbox\": \"web\"\n}\n").unwrap();
        // The developer edited dome.json since approval; `dome allow` must show the diff.
        let edited = b"{\n  \"sandbox\": \"web\",\n  \"allow_net\": true\n}\n";
        let diff = reapproval_diff(dd, project.path(), edited).expect("a changed config diffs");
        assert!(
            diff.contains("+   \"allow_net\": true"),
            "the diff names the newly added line; got:\n{diff}"
        );
    }

    #[test]
    fn pin_offer_targets_a_project_with_no_sandbox_field() {
        // A project that has not chosen a stable name is a pin candidate: suggest its cwd-slug.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("api");
        std::fs::create_dir(&project).unwrap();
        let cfg = load_config_str("{}");
        assert_eq!(pin_offer_slug(&cfg, &project).as_deref(), Some("api"));
    }

    #[test]
    fn pin_offer_is_silent_when_a_sandbox_field_is_already_set() {
        // An explicit name is already stable and collision-free — nothing to offer.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("api");
        std::fs::create_dir(&project).unwrap();
        let cfg = load_config_str(r#"{"sandbox":"web"}"#);
        assert!(pin_offer_slug(&cfg, &project).is_none());
    }

    #[test]
    fn insert_sandbox_field_adds_the_key_to_an_empty_object() {
        let out = insert_sandbox_field("{}\n", "api").expect("valid json out");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["sandbox"], "api");
    }

    #[test]
    fn insert_sandbox_field_preserves_existing_keys() {
        let original = "{\n  \"allow_net\": true\n}\n";
        let out = insert_sandbox_field(original, "api").expect("valid json out");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["sandbox"], "api");
        assert_eq!(v["allow_net"], true, "existing keys are kept");
    }

    #[test]
    fn insert_sandbox_field_rejects_non_object_json() {
        // A dome.json that is not a top-level object can't take a `sandbox` key safely.
        assert!(insert_sandbox_field("[1, 2, 3]\n", "api").is_none());
    }

    #[test]
    fn init_hook_hint_mentions_the_eval_line() {
        let hint = init_hook_hint();
        assert!(
            hint.contains("eval \"$(dome hook zsh)\""),
            "`dome init` output must mention the hook eval line; got:\n{hint}"
        );
    }

    #[test]
    fn rc_path_maps_the_shell_to_its_rc_file() {
        let home = Path::new("/home/dev");
        assert_eq!(rc_path("zsh", home), Some(home.join(".zshrc")));
    }

    #[test]
    fn append_rc_line_adds_the_line_and_is_idempotent() {
        let line = hook_rc_line("zsh");
        // First install: the line is appended to the existing rc content.
        let first =
            append_rc_line("# my rc\nexport PATH=foo\n", &line).expect("appends when absent");
        assert!(first.contains(&line), "the line is added; got:\n{first}");
        assert!(
            first.contains("export PATH=foo"),
            "existing content is preserved"
        );
        // Re-install: the line is already present, so nothing is appended (no duplicate).
        assert!(
            append_rc_line(&first, &line).is_none(),
            "re-running --install must not duplicate the line"
        );
    }

    #[test]
    fn append_rc_line_separates_from_content_missing_a_trailing_newline() {
        let line = hook_rc_line("zsh");
        let out = append_rc_line("export PATH=foo", &line).expect("appends");
        assert!(
            out.starts_with("export PATH=foo\n"),
            "a missing trailing newline is added before the appended block; got:\n{out}"
        );
        assert!(out.trim_end().ends_with(&line));
    }

    #[test]
    fn supported_shell_detects_zsh_bash_fish_and_rejects_others() {
        // A zsh $SHELL → zsh; an unset $SHELL defaults to zsh (the macOS default).
        assert_eq!(
            supported_shell(|k| (k == "SHELL").then(|| "/bin/zsh".to_string())).as_deref(),
            Some("zsh")
        );
        assert_eq!(supported_shell(no_env).as_deref(), Some("zsh"));
        // bash and fish are now at parity with zsh.
        assert_eq!(
            supported_shell(|k| (k == "SHELL").then(|| "/bin/bash".to_string())).as_deref(),
            Some("bash")
        );
        assert_eq!(
            supported_shell(|k| (k == "SHELL").then(|| "/usr/bin/fish".to_string())).as_deref(),
            Some("fish")
        );
        // An unsupported shell still suggests no line that wouldn't work.
        assert!(supported_shell(|k| (k == "SHELL").then(|| "/bin/tcsh".to_string())).is_none());
    }

    #[test]
    fn fish_rc_line_uses_the_source_idiom_not_eval() {
        // Fish has no `eval "$(...)"`; its idiom pipes the emitted hook into `source`.
        assert_eq!(hook_rc_line("fish"), "dome hook fish | source");
        assert_eq!(hook_rc_line("bash"), "eval \"$(dome hook bash)\"");
    }

    #[test]
    fn bash_hook_wires_into_prompt_command_with_the_guards_and_walk_up() {
        let script = emit_bash_hook("/opt/dome");
        // Bash's per-prompt hook is PROMPT_COMMAND (the chpwd/precmd analog).
        assert!(
            script.contains("PROMPT_COMMAND"),
            "bash hook must wire into PROMPT_COMMAND; got:\n{script}"
        );
        // Sentinel export so a manual session can detect the installed hook.
        assert!(script.contains(&format!("export {HOOK_SENTINEL}=")));
        // The full environment guard set is present.
        assert!(script.contains("DOME_SANDBOX"), "guest sentinel guard");
        assert!(script.contains("CI"), "CI guard");
        assert!(script.contains("DOME_NO_AUTO"), "opt-out guard");
        assert!(script.contains("$-"), "interactive-shell guard");
        assert!(
            script.contains("-t 0") || script.contains("-t 1"),
            "tty guard"
        );
        // Pure-shell walk-up to dome.json that only execs dome on a hit, via the baked path,
        // overridable for tests.
        assert!(script.contains("dome.json"));
        assert!(script.contains("__hook-activate"));
        assert!(script.contains("/opt/dome"));
        assert!(script.contains("DOME_HOOK_CMD"));
    }

    #[test]
    fn fish_hook_uses_the_pwd_event_with_the_guards_and_walk_up() {
        let script = emit_fish_hook("/opt/dome");
        // Fish's native directory-change event (the chpwd analog).
        assert!(
            script.contains("--on-variable PWD"),
            "fish hook must react to PWD changes; got:\n{script}"
        );
        // Sentinel export (fish idiom: `set -gx`).
        assert!(script.contains(&format!("set -gx {HOOK_SENTINEL}")));
        // The full environment guard set is present.
        assert!(script.contains("DOME_SANDBOX"), "guest sentinel guard");
        assert!(script.contains("CI"), "CI guard");
        assert!(script.contains("DOME_NO_AUTO"), "opt-out guard");
        assert!(
            script.contains("status is-interactive"),
            "interactive guard"
        );
        assert!(script.contains("isatty"), "tty guard");
        // Pure-shell walk-up to dome.json that only execs dome on a hit, via the baked path,
        // overridable for tests.
        assert!(script.contains("dome.json"));
        assert!(script.contains("__hook-activate"));
        assert!(script.contains("/opt/dome"));
        assert!(script.contains("DOME_HOOK_CMD"));
    }

    #[test]
    fn hook_tip_offered_until_the_marker_is_written() {
        let data = tempfile::tempdir().unwrap();
        let dd = data.path().to_str().unwrap();
        // Unhooked interactive session, no marker yet → the tip is offered (default zsh).
        assert_eq!(hook_tip_shell(dd, no_env, true).as_deref(), Some("zsh"));
        // After the tip has been shown (marker dropped), it never appears again.
        std::fs::write(hook_tip_marker(dd), b"1").unwrap();
        assert!(
            hook_tip_shell(dd, no_env, true).is_none(),
            "the marker must suppress the tip on subsequent runs"
        );
    }

    #[test]
    fn hook_tip_suppressed_when_the_sentinel_is_present() {
        let data = tempfile::tempdir().unwrap();
        let dd = data.path().to_str().unwrap();
        // The hook is installed (it exported the sentinel into this session) → no tip.
        let getenv = |k: &str| (k == HOOK_SENTINEL).then(|| "1".to_string());
        assert!(hook_tip_shell(dd, getenv, true).is_none());
    }

    #[test]
    fn hook_tip_suppressed_without_a_tty() {
        let data = tempfile::tempdir().unwrap();
        let dd = data.path().to_str().unwrap();
        // A piped/non-interactive session must not emit a copy-paste tip into a log.
        assert!(hook_tip_shell(dd, no_env, false).is_none());
    }

    #[test]
    fn hook_tip_suppressed_by_an_environment_guard() {
        let data = tempfile::tempdir().unwrap();
        let dd = data.path().to_str().unwrap();
        // The same guards that block auto-activation ($CI here) suppress the tip (don't nag in CI).
        let getenv = |k: &str| (k == "CI").then(|| "true".to_string());
        assert!(hook_tip_shell(dd, getenv, true).is_none());
    }

    #[test]
    fn rc_line_is_the_documented_eval() {
        assert_eq!(hook_rc_line("zsh"), "eval \"$(dome hook zsh)\"");
    }

    #[test]
    fn install_tip_shows_the_exact_rc_line() {
        let tip = install_tip("zsh");
        assert!(
            tip.contains("eval \"$(dome hook zsh)\""),
            "the tip must show the exact rc line; got:\n{tip}"
        );
    }

    #[test]
    fn zsh_hook_exports_the_install_sentinel() {
        // The hook exports a sentinel env var on eval so a manual `dome sandbox` session can
        // tell the hook is installed (and suppress the one-time install tip).
        let script = emit_zsh_hook("/usr/local/bin/dome");
        assert!(
            script.contains(&format!("export {HOOK_SENTINEL}=")),
            "the hook must export the {HOOK_SENTINEL} sentinel; got:\n{script}"
        );
    }

    #[test]
    fn zsh_hook_registers_chpwd_and_precmd() {
        let script = emit_zsh_hook("/usr/local/bin/dome");
        assert!(script.contains("add-zsh-hook chpwd"));
        assert!(script.contains("add-zsh-hook precmd"));
    }

    #[test]
    fn zsh_hook_embeds_all_environment_guards() {
        let script = emit_zsh_hook("/usr/local/bin/dome");
        // Every environment no-op guard from the spec must be present in the shell.
        assert!(script.contains("DOME_SANDBOX"), "guest sentinel guard");
        assert!(script.contains("CI"), "CI guard");
        assert!(script.contains("DOME_NO_AUTO"), "opt-out guard");
        assert!(script.contains("-o interactive"), "interactive-shell guard");
        assert!(
            script.contains("-t 0") || script.contains("-t 1"),
            "tty guard"
        );
    }

    #[test]
    fn zsh_hook_walks_up_in_pure_shell_and_only_calls_dome_on_a_hit() {
        let script = emit_zsh_hook("/opt/dome");
        // The walk references dome.json and zsh's `:h` (dirname) head modifier.
        assert!(script.contains("dome.json"));
        assert!(script.contains(":h"));
        // The binary is invoked through the baked path with the activation subcommand.
        assert!(script.contains("__hook-activate"));
        assert!(script.contains("/opt/dome"));
    }

    #[test]
    fn zsh_hook_command_path_is_overridable_for_tests() {
        // The baked path is the default; DOME_HOOK_CMD overrides it (drives the fake shim
        // in shell-level tests).
        let script = emit_zsh_hook("/opt/dome");
        assert!(script.contains("DOME_HOOK_CMD"));
    }

    #[test]
    fn guard_blocks_inside_a_guest() {
        let blocked = activation_blocked(|k| (k == "DOME_SANDBOX").then(|| "web".to_string()));
        assert_eq!(blocked, Some("already inside a dome sandbox"));
    }

    #[test]
    fn guard_blocks_on_ci() {
        let blocked = activation_blocked(|k| (k == "CI").then(|| "true".to_string()));
        assert_eq!(blocked, Some("$CI is set"));
    }

    #[test]
    fn guard_blocks_on_dome_no_auto() {
        let blocked = activation_blocked(|k| (k == "DOME_NO_AUTO").then(|| "1".to_string()));
        assert_eq!(blocked, Some("$DOME_NO_AUTO=1"));
    }

    #[test]
    fn guard_allows_a_clean_environment() {
        assert_eq!(activation_blocked(no_env), None);
        // An empty DOME_SANDBOX (rare, but possible) must not count as "inside a guest".
        let blocked = activation_blocked(|k| (k == "DOME_SANDBOX").then(String::new));
        assert_eq!(blocked, None);
    }

    /// A project dir + isolated data dir, like the trust module's fixture.
    fn fixture(contents: &str) -> (tempfile::TempDir, tempfile::TempDir) {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("dome.json"), contents).unwrap();
        let data = tempfile::tempdir().unwrap();
        (project, data)
    }

    #[test]
    fn decide_is_untrusted_for_a_never_allowed_project() {
        let (project, data) = fixture("{}");
        let d = decide(data.path().to_str().unwrap(), project.path(), no_env).unwrap();
        assert_eq!(d, Decision::Untrusted);
    }

    #[test]
    fn decide_activates_a_trusted_project() {
        let (project, data) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        crate::trust::record_trust(dd, project.path(), br#"{"sandbox":"web"}"#).unwrap();
        let d = decide(dd, project.path(), no_env).unwrap();
        assert_eq!(d, Decision::Activate);
    }

    #[test]
    fn decide_skips_when_activate_is_off_even_if_trusted() {
        let contents = r#"{"activate":"off"}"#;
        let (project, data) = fixture(contents);
        let dd = data.path().to_str().unwrap();
        crate::trust::record_trust(dd, project.path(), contents.as_bytes()).unwrap();
        let d = decide(dd, project.path(), no_env).unwrap();
        assert_eq!(d, Decision::Skip);
    }

    #[test]
    fn decide_skips_when_a_guard_fires() {
        let (project, data) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        crate::trust::record_trust(dd, project.path(), br#"{"sandbox":"web"}"#).unwrap();
        // Trusted + activate-on, but we are inside a guest → still a no-op.
        let d = decide(dd, project.path(), |k| {
            (k == "DOME_SANDBOX").then(|| "web".to_string())
        })
        .unwrap();
        assert_eq!(d, Decision::Skip);
    }
}
