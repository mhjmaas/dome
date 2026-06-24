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

/// `dome hook <shell>`: print the shell-integration hook to stdout. Only `zsh` is supported
/// in this slice (bash/fish are a follow-up); any other shell is a clear error rather than a
/// silent no-op the developer would `eval` into nothing.
pub(crate) fn run_hook(shell: &str) -> Result<()> {
    match shell {
        "zsh" => {
            let cmd = std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "dome".to_string());
            print!("{}", emit_zsh_hook(&cmd));
            Ok(())
        }
        "bash" | "fish" => anyhow::bail!(
            "`dome hook {shell}` is not available yet (zsh only for now). Use `dome hook zsh`."
        ),
        other => anyhow::bail!("unknown shell '{other}'. Supported: zsh."),
    }
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
    // chdir into the project so `dome.json` (and thus the sandbox name + project mount)
    // resolves there, matching what `dome sandbox shell` does from the project root.
    std::env::set_current_dir(project_dir)?;
    crate::sandbox::run_sandbox(
        None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
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
