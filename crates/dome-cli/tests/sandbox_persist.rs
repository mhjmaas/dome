//! Integration tests for persistent sandboxes. Boots a real VM — requires a
//! codesigned binary. All #[ignore]d by default. Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli --test sandbox_persist -- --ignored

use std::process::Command;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. just build)")
}

/// A unique sandbox name per test run so repeated runs don't collide on the global
/// sandbox namespace. Uses the test process pid plus a caller-supplied suffix.
fn sandbox_name(suffix: &str) -> String {
    format!("itest-{}-{}", std::process::id(), suffix)
}

fn sandbox_run(name: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["sandbox", "run", name, "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// Spawn a sandbox session without waiting for it, so a second session can run
/// concurrently against the same sandbox.
fn sandbox_spawn(name: &str, guest_cmd: &str) -> std::process::Child {
    Command::new(dome_bin())
        .args(["sandbox", "run", name, "--", "sh", "-c", guest_cmd])
        .spawn()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn ephemeral_run(guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["run", "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// Stop a sandbox's persistent worker (since #24 the VM outlives a session; there is no
/// user-facing `sandbox stop` until #27). SIGTERM makes the worker save the sandbox and
/// shut the VM down cleanly, releasing the persistence lock. Best-effort; waits for the
/// save + teardown to complete.
fn stop_worker(name: &str) {
    let _ = std::process::Command::new("pkill")
        .args(["-TERM", "-f", &format!("__worker {}", name)])
        .output();
    std::thread::sleep(std::time::Duration::from_secs(3));
}

fn rm_sandbox(name: &str) {
    // Best-effort cleanup independent of the binary under test. Since #24 a live worker
    // owns the VM and the persistence lock, so stop it first; then remove the index (and
    // any lock left by a crashed session) directly via the data dir so a broken `rm`
    // can never strand other tests.
    stop_worker(name);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{}/.local/share/dome/sandboxes", home);
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    // Drop any lock left by a crashed session so it can't wedge the next run.
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
}

/// Run the real `dome sandbox rm <name>` command.
fn sandbox_rm_cmd(name: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["sandbox", "rm", name])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// Run the real `dome prune` command (instance cleanup + CAS mark-and-sweep).
fn prune_cmd() -> std::process::Output {
    Command::new(dome_bin())
        .arg("prune")
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn sandbox_index_path(name: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.local/share/dome/sandboxes/{}.idx", home, name)
}

/// Number of chunk files currently in the global CAS chunk store.
fn chunk_count() -> usize {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let chunks = format!("{}/.local/share/dome/cas/chunks", home);
    std::fs::read_dir(&chunks).map(|d| d.count()).unwrap_or(0)
}

#[test]
#[ignore]
fn persistence_round_trip() {
    let name = sandbox_name("roundtrip");
    rm_sandbox(&name);

    // First session writes a file into the persistent root filesystem.
    let write = sandbox_run(&name, "echo persisted-content > /root/state.txt");
    assert!(
        write.status.success(),
        "first sandbox run should succeed; stderr: {}",
        String::from_utf8_lossy(&write.stderr)
    );

    // Second session resumes and reads it back.
    let read = sandbox_run(&name, "cat /root/state.txt");
    assert!(
        read.status.success(),
        "second sandbox run should succeed; stderr: {}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("persisted-content"),
        "resumed sandbox should see the previously written file; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&name);
}

#[test]
#[ignore]
fn ephemeral_run_does_not_see_sandbox_state() {
    let name = sandbox_name("isolation");
    rm_sandbox(&name);

    sandbox_run(&name, "echo secret > /root/state.txt");

    // A plain ephemeral run boots from the base image and must not see it.
    let read = ephemeral_run("cat /root/state.txt 2>/dev/null || echo MISSING");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("MISSING"),
        "ephemeral run must not see sandbox-persisted state; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&name);
}

#[test]
#[ignore]
fn save_happens_on_nonzero_exit() {
    let name = sandbox_name("nonzero");
    rm_sandbox(&name);

    // The command writes a file and then exits non-zero (a "failed build").
    let failed = sandbox_run(&name, "echo built > /root/artifact.txt; exit 1");
    assert_eq!(
        failed.status.code(),
        Some(1),
        "exit code should propagate from the guest command"
    );

    // Stop the worker so it flushes the sandbox to its on-disk index, then the next
    // session cold-boots from that saved state — proving a non-zero-exit session's
    // writes are durable (since #24 the save happens on worker stop, not per session).
    stop_worker(&name);
    let read = sandbox_run(&name, "cat /root/artifact.txt");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("built"),
        "state from a non-zero-exit session should still persist; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&name);
}

#[test]
#[ignore]
fn lazy_create_then_resume() {
    let name = sandbox_name("lazy");
    rm_sandbox(&name);

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let idx = format!("{}/.local/share/dome/sandboxes/{}.idx", home, name);
    assert!(
        !std::path::Path::new(&idx).exists(),
        "sandbox index should not exist before first use"
    );

    let first = sandbox_run(&name, "true");
    assert!(first.status.success());
    assert!(
        std::path::Path::new(&idx).exists(),
        "first sandbox run should lazily create the index"
    );

    let second = sandbox_run(&name, "true");
    assert!(
        second.status.success(),
        "second run should resume the sandbox"
    );

    rm_sandbox(&name);
}

/// Since #24 the ephemeral-fork model is gone: a sandbox is owned by one persistent
/// worker, and a second concurrent session attaches to the SAME live VM with full write
/// access (a shared, writable filesystem). So a write from a concurrent session is
/// immediately visible to the owner — the opposite of the old fork behaviour.
#[test]
#[ignore]
fn concurrent_session_shares_the_same_live_vm() {
    let name = sandbox_name("concurrent");
    rm_sandbox(&name);

    // Owner cold-boots the VM and keeps a session open while a second session runs.
    let mut owner = sandbox_spawn(&name, "sleep 25");
    // Give the worker time to cold-boot before the second session attaches.
    std::thread::sleep(std::time::Duration::from_secs(8));

    // A concurrent session writes a marker; it must NOT announce itself as a fork
    // (that model is gone) and its write lands on the shared live filesystem.
    let writer = sandbox_run(&name, "echo shared-write > /root/marker.txt");
    assert!(
        writer.status.success(),
        "a concurrent session should attach and run; stderr: {}",
        String::from_utf8_lossy(&writer.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&writer.stderr).contains("ephemeral fork"),
        "the ephemeral-fork model was removed in #24; stderr: {}",
        String::from_utf8_lossy(&writer.stderr)
    );

    // A third session sees the concurrent write immediately — same live VM, shared fs.
    let read = sandbox_run(&name, "cat /root/marker.txt");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("shared-write"),
        "concurrent sessions share one live writable filesystem; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    let _ = owner.wait();
    rm_sandbox(&name);
}

/// `dome sandbox rm` on a name with no index reports a clear error and fails — this
/// path needs no VM, so it is cheap even though the suite is `#[ignore]`d.
#[test]
#[ignore]
fn rm_reports_a_clear_error_for_a_missing_sandbox() {
    let name = sandbox_name("rm-missing");
    rm_sandbox(&name); // ensure it really is absent

    let out = sandbox_rm_cmd(&name);
    assert!(
        !out.status.success(),
        "removing a non-existent sandbox should fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&name) && stderr.contains("not found"),
        "error should name the missing sandbox; stderr: {}",
        stderr
    );
}

/// End-to-end of issue #10: `rm` unlinks only the index (chunks survive), and a
/// subsequent `prune` mark-and-sweep reclaims the now-orphaned chunks while a different,
/// still-referenced sandbox keeps its data and resumes intact.
#[test]
#[ignore]
fn rm_then_prune_reclaims_orphans_while_keeping_referenced() {
    let victim = sandbox_name("gc-victim");
    let keeper = sandbox_name("gc-keeper");
    rm_sandbox(&victim);
    rm_sandbox(&keeper);

    // The keeper writes a unique marker plus a few MB of unique data, then persists.
    let k = sandbox_run(
        &keeper,
        "head -c 3000000 /dev/urandom > /root/keep.bin; echo keeper-marker > /root/marker.txt",
    );
    assert!(
        k.status.success(),
        "seeding the keeper sandbox should succeed; stderr: {}",
        String::from_utf8_lossy(&k.stderr)
    );

    // The victim writes its own few MB of unique data (so it owns distinct chunks).
    let v = sandbox_run(&victim, "head -c 3000000 /dev/urandom > /root/victim.bin");
    assert!(
        v.status.success(),
        "seeding the victim sandbox should succeed; stderr: {}",
        String::from_utf8_lossy(&v.stderr)
    );

    // Since #24 the save happens on worker stop, not per session, and a live worker
    // holds the persistence lock (so `rm` would refuse). Stop both workers so their
    // writes are flushed to their on-disk indexes and the lock is released.
    stop_worker(&keeper);
    stop_worker(&victim);

    let before = chunk_count();

    // rm unlinks only the index: it succeeds, the index is gone, and — crucially — the
    // chunk store is untouched (reclamation is deferred to prune).
    let rm = sandbox_rm_cmd(&victim);
    assert!(
        rm.status.success(),
        "rm should succeed; stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    assert!(
        !std::path::Path::new(&sandbox_index_path(&victim)).exists(),
        "rm should unlink the sandbox index"
    );
    assert_eq!(
        chunk_count(),
        before,
        "rm must NOT delete chunks — that is deferred to prune"
    );

    // prune sweeps the now-unreferenced chunks: the store shrinks.
    let prune = prune_cmd();
    assert!(
        prune.status.success(),
        "prune should succeed; stderr: {}",
        String::from_utf8_lossy(&prune.stderr)
    );
    assert!(
        chunk_count() < before,
        "prune should reclaim the victim's orphaned chunks ({} -> {})",
        before,
        chunk_count()
    );

    // The keeper's referenced data survived the sweep and still resumes.
    let read = sandbox_run(&keeper, "cat /root/marker.txt");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("keeper-marker"),
        "a still-referenced sandbox must keep its data through prune; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&victim);
    rm_sandbox(&keeper);
}

/// The sandbox's central audit directory (`audit/<name>/`), which outlives the sandbox.
fn audit_sandbox_dir(name: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.local/share/dome/audit/{}", home, name)
}

/// The session directories (`audit/<name>/<session>/`) for a sandbox.
fn audit_session_dirs(name: &str) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(sessions) = std::fs::read_dir(audit_sandbox_dir(name)) {
        for session in sessions.filter_map(|e| e.ok()) {
            if session.path().is_dir() {
                dirs.push(session.path());
            }
        }
    }
    dirs
}

/// Backdate every segment file in a session directory by `age` so the age-based reaper sees
/// a stale session. (Reaping keys off the newest contained file's mtime.)
fn age_audit_session(session_dir: &std::path::Path, age: std::time::Duration) {
    let when = std::time::SystemTime::now() - age;
    if let Ok(files) = std::fs::read_dir(session_dir) {
        for f in files.filter_map(|e| e.ok()) {
            if f.path().is_file() {
                std::fs::File::options()
                    .write(true)
                    .open(f.path())
                    .and_then(|file| file.set_modified(when))
                    .expect("backdate segment mtime");
            }
        }
    }
}

/// Every `events-NNNN.jsonl` segment under a sandbox's sessions, sorted oldest-first (session
/// dir names are timestamp-led and segment names zero-padded, so a path sort is an age sort).
fn audit_segments(name: &str) -> Vec<std::path::PathBuf> {
    let mut segs = Vec::new();
    let Ok(sessions) = std::fs::read_dir(audit_sandbox_dir(name)) else {
        return segs;
    };
    for session in sessions.filter_map(|e| e.ok()) {
        let Ok(files) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for f in files.filter_map(|e| e.ok()) {
            let p = f.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl"))
            {
                segs.push(p);
            }
        }
    }
    segs.sort();
    segs
}

/// Read every audit row across all of a sandbox's sessions and segments as raw JSONL lines.
fn audit_rows(name: &str) -> Vec<String> {
    let mut rows = Vec::new();
    for segment in audit_segments(name) {
        if let Ok(text) = std::fs::read_to_string(&segment) {
            rows.extend(text.lines().map(str::to_string));
        }
    }
    rows
}

/// End-to-end spine of the egress audit log (#102): a real sandbox making egress produces
/// self-describing JSONL metadata rows under a central `audit/` root that survives the
/// sandbox. One connection is secret-bound (MITM: `postman-echo.com` carries an
/// `Authorization` secret) and one is not (blind tunnel: `example.com`); both must appear as
/// `conn_open`/`conn_close` rows stamped with the sandbox identity and carrying SNI, kind,
/// and byte counts. The proxy never decrypts the blind tunnel, so that row is metadata-only.
#[test]
#[ignore]
fn egress_audit_log_records_connection_metadata() {
    let name = sandbox_name("audit-spine");
    rm_sandbox(&name);
    // Clear any audit logs from a previous run of this test so we read only this boot's rows.
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));

    // `--allow-net` (all hosts) + a secret bound to postman-echo.com: that host is MITM'd
    // (rich, decryptable), example.com is a plain blind tunnel (metadata only). The guest's
    // own egress to both is what produces the audit rows; the guest output is irrelevant.
    let guest_cmd = "curl -sS --max-time 25 -H \"Authorization: $ECHO_TOKEN\" \
         https://postman-echo.com/get > /tmp/echo.out 2>&1; \
         curl -sS --max-time 25 https://example.com/ > /tmp/ex.out 2>&1; echo audit-done";
    let out = Command::new(dome_bin())
        .env("ECHO_TOKEN", "real-secret-value")
        .args([
            "sandbox",
            "run",
            &name,
            "--allow-net",
            "--secret",
            "ECHO_TOKEN=ECHO_TOKEN@postman-echo.com",
            "--",
            "sh",
            "-c",
            guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    assert!(
        out.status.success(),
        "the audited run should succeed; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Stop the worker so the writer drains and flushes its tail before we read the log.
    stop_worker(&name);

    let rows = audit_rows(&name);
    assert!(
        !rows.is_empty(),
        "egress should have produced audit rows under audit/{name}/"
    );
    // Every row is self-describing: stamped with the sandbox identity by the writer.
    assert!(
        rows.iter()
            .all(|r| r.contains(&format!("\"sandbox\":\"{name}\""))),
        "every row must carry the sandbox identity; rows: {rows:#?}"
    );
    assert!(
        rows.iter().all(|r| r.contains("\"session\":")),
        "every row must carry its session; rows: {rows:#?}"
    );

    // The secret-bound host was MITM'd: a conn_open row tagged mitm with its SNI.
    assert!(
        rows.iter().any(|r| r.contains("\"kind\":\"conn_open\"")
            && r.contains("\"conn_kind\":\"mitm\"")
            && r.contains("\"sni\":\"postman-echo.com\"")),
        "expected a MITM conn_open for the secret-bound host; rows: {rows:#?}"
    );
    // The non-secret host was a blind tunnel: metadata only, never decrypted.
    assert!(
        rows.iter().any(|r| r.contains("\"kind\":\"conn_open\"")
            && r.contains("\"conn_kind\":\"blind_tunnel\"")
            && r.contains("\"sni\":\"example.com\"")),
        "expected a blind-tunnel conn_open for the non-secret host; rows: {rows:#?}"
    );
    // Connections closed: at least one conn_close with byte accounting was recorded.
    assert!(
        rows.iter().any(|r| r.contains("\"kind\":\"conn_close\"")
            && r.contains("\"bytes_rx\":")
            && r.contains("\"bytes_tx\":")),
        "expected a conn_close row with byte counts; rows: {rows:#?}"
    );
    // The real secret value must never appear in the audit log.
    assert!(
        !rows.iter().any(|r| r.contains("real-secret-value")),
        "the real secret value must never enter the audit log; rows: {rows:#?}"
    );

    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));
}

/// End-to-end coverage of `dns_blocked` (#128): with a domain allowlist active, a guest
/// resolving a domain that is NOT on the allowlist has its DNS query refused at the name
/// layer — the guest never gets an IP and never opens a connection, so without this event
/// the most common kind of denial leaves zero trace. The refusal must surface as a
/// `dns_blocked` row naming the refused domain; an allowed domain must NOT produce one.
///
/// This also stands up the allowlist-active audit scaffold (`--allow-net` to enable
/// networking + `--allow-host` to restrict it) that the connection-layer slice (#129) reuses.
#[test]
#[ignore]
fn egress_audit_log_records_dns_blocked_for_disallowed_domain() {
    let name = sandbox_name("audit-dns-blocked");
    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));

    // Allowlist active: only example.com is permitted. The guest curls the allowed host
    // (resolves + connects normally) and a disallowed host (github.com), whose A query the
    // resolver refuses before resolution — a policy denial, not a SERVFAIL. The secret is
    // bound only to make the "secret never appears" invariant a real assertion.
    let guest_cmd = "curl -sS --max-time 25 https://example.com/ > /tmp/ex.out 2>&1; \
         curl -sS --max-time 25 https://github.com/ > /tmp/gh.out 2>&1; echo dns-done";
    let out = Command::new(dome_bin())
        .env("ECHO_TOKEN", "real-secret-value")
        .args([
            "sandbox",
            "run",
            &name,
            "--allow-net",
            "--allow-host",
            "example.com",
            "--secret",
            "ECHO_TOKEN=ECHO_TOKEN@example.com",
            "--",
            "sh",
            "-c",
            guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    assert!(
        out.status.success(),
        "the audited run should succeed even though one egress is blocked; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Stop the worker so the writer drains and flushes its tail before we read the log.
    stop_worker(&name);

    let rows = audit_rows(&name);
    assert!(
        !rows.is_empty(),
        "the run should have produced audit rows under audit/{name}/"
    );

    // The disallowed domain's refusal is recorded as a dns_blocked row naming that domain,
    // stamped with the sandbox identity like every other row.
    assert!(
        rows.iter().any(|r| r.contains("\"kind\":\"dns_blocked\"")
            && r.contains("\"domain\":\"github.com\"")
            && r.contains(&format!("\"sandbox\":\"{name}\""))),
        "expected a dns_blocked row naming the refused domain; rows: {rows:#?}"
    );

    // The allowed domain resolves and connects: it must NOT appear as a dns_blocked row.
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("\"kind\":\"dns_blocked\"") && r.contains("example.com")),
        "an allowed domain must not produce a dns_blocked row; rows: {rows:#?}"
    );

    // The real secret value must never appear in any audit row, including the new block row.
    assert!(
        !rows.iter().any(|r| r.contains("real-secret-value")),
        "the real secret value must never enter the audit log; rows: {rows:#?}"
    );

    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));
}

/// End-to-end coverage of `conn_blocked` (#129): with a domain allowlist active, a guest that
/// reaches the connection layer but is rejected by policy must surface as a terminal
/// `conn_blocked` row — and NOT as a `conn_open`/`conn_close` pair. The most reachable of the
/// three connection-layer denials from a guest is the literal-IP bypass: curling an IP that was
/// never DNS-pinned skips the name layer entirely (no DNS query), reaches `handle_connection`,
/// and is rejected as `ip_not_allowed`. The allowed host still opens normally, proving
/// `conn_open` keeps meaning "allowed and established".
///
/// Reuses the allowlist-active scaffold from the Slice 1 (`dns_blocked`) test.
#[test]
#[ignore]
fn egress_audit_log_records_conn_blocked_for_unpinned_literal_ip() {
    let name = sandbox_name("audit-conn-blocked");
    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));

    // Allowlist active: only example.com is permitted. The guest curls the allowed host
    // (resolves + connects normally → conn_open) and a literal IP (1.1.1.1:443) that was never
    // DNS-pinned. The literal IP bypasses the name layer, so it is rejected at the connection
    // layer as ip_not_allowed rather than producing a dns_blocked row. The secret is bound only
    // to make the "secret never appears" invariant a real assertion.
    let guest_cmd = "curl -sS --max-time 25 https://example.com/ > /tmp/ex.out 2>&1; \
         curl -sS --max-time 25 https://1.1.1.1/ > /tmp/ip.out 2>&1; echo conn-done";
    let out = Command::new(dome_bin())
        .env("ECHO_TOKEN", "real-secret-value")
        .args([
            "sandbox",
            "run",
            &name,
            "--allow-net",
            "--allow-host",
            "example.com",
            "--secret",
            "ECHO_TOKEN=ECHO_TOKEN@example.com",
            "--",
            "sh",
            "-c",
            guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    assert!(
        out.status.success(),
        "the audited run should succeed even though one egress is blocked; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Stop the worker so the writer drains and flushes its tail before we read the log.
    stop_worker(&name);

    let rows = audit_rows(&name);
    assert!(
        !rows.is_empty(),
        "the run should have produced audit rows under audit/{name}/"
    );

    // The literal-IP attempt is recorded as a terminal conn_blocked row with reason
    // ip_not_allowed, naming the destination and stamped with the sandbox identity.
    assert!(
        rows.iter().any(|r| r.contains("\"kind\":\"conn_blocked\"")
            && r.contains("\"reason\":\"ip_not_allowed\"")
            && r.contains("\"dst\":\"1.1.1.1:443\"")
            && r.contains(&format!("\"sandbox\":\"{name}\""))),
        "expected a conn_blocked row for the unpinned literal IP; rows: {rows:#?}"
    );

    // conn_blocked is terminal: the blocked attempt must NOT also produce a conn_open or
    // conn_close for that destination.
    assert!(
        !rows.iter().any(|r| (r.contains("\"kind\":\"conn_open\"")
            || r.contains("\"kind\":\"conn_close\""))
            && r.contains("1.1.1.1:443")),
        "a blocked connection must not also open or close; rows: {rows:#?}"
    );

    // The allowed host still opens normally — conn_open keeps meaning "allowed and established".
    assert!(
        rows.iter()
            .any(|r| r.contains("\"kind\":\"conn_open\"") && r.contains("\"sni\":\"example.com\"")),
        "the allowed host should still produce a conn_open; rows: {rows:#?}"
    );

    // The real secret value must never appear in any audit row, including the new block row.
    assert!(
        !rows.iter().any(|r| r.contains("real-secret-value")),
        "the real secret value must never enter the audit log; rows: {rows:#?}"
    );

    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));
}

/// Per-HTTP-request framing on secret-bound (MITM) connections (#103): the read-only framer
/// tee'd alongside the substitution relay must turn the decrypted request/response stream into
/// per-request `http_request` / `http_response` rows carrying the request line, status, sizes,
/// and timing — without headers (deferred to #107) and without ever logging the real secret.
/// A `GET` and a body-bearing `POST` on the MITM'd host exercise both no-body and
/// Content-Length framing (and reframing of the second request after the first body); the blind
/// tunnel to `example.com` stays metadata-only because the proxy never decrypts it.
#[test]
#[ignore]
fn egress_audit_log_frames_http_requests() {
    let name = sandbox_name("audit-framing");
    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));

    // A GET and a POST (with a body) to the MITM'd host, plus a blind tunnel to example.com.
    // `--keepalive-time` nudges curl to reuse one connection so the framer reframes the POST
    // after the GET on the same MITM stream where possible.
    let guest_cmd = "curl -sS --max-time 25 -H \"Authorization: $ECHO_TOKEN\" \
         https://postman-echo.com/get > /tmp/get.out 2>&1; \
         curl -sS --max-time 25 -H \"Authorization: $ECHO_TOKEN\" \
         -d hello=world https://postman-echo.com/post > /tmp/post.out 2>&1; \
         curl -sS --max-time 25 https://example.com/ > /tmp/ex.out 2>&1; echo framing-done";
    let out = Command::new(dome_bin())
        .env("ECHO_TOKEN", "real-secret-value")
        .args([
            "sandbox",
            "run",
            &name,
            "--allow-net",
            "--secret",
            "ECHO_TOKEN=ECHO_TOKEN@postman-echo.com",
            "--",
            "sh",
            "-c",
            guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    assert!(
        out.status.success(),
        "the framed run should succeed; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Stop the worker so the writer drains and flushes its tail before we read the log.
    stop_worker(&name);

    let rows = audit_rows(&name);
    assert!(
        !rows.is_empty(),
        "egress should have produced audit rows under audit/{name}/"
    );

    // The MITM'd GET was framed into an http_request row with its request line and no body.
    assert!(
        rows.iter().any(|r| r.contains("\"kind\":\"http_request\"")
            && r.contains("\"method\":\"GET\"")
            && r.contains("\"path\":\"/get\"")),
        "expected an http_request row for GET /get; rows: {rows:#?}"
    );
    // The body-bearing POST was framed too, with its Content-Length body length captured.
    assert!(
        rows.iter().any(|r| r.contains("\"kind\":\"http_request\"")
            && r.contains("\"method\":\"POST\"")
            && r.contains("\"path\":\"/post\"")
            && r.contains("\"body_bytes\":11")),
        "expected an http_request row for POST /post with body_bytes=11; rows: {rows:#?}"
    );
    // The response side was framed symmetrically into an http_response row.
    assert!(
        rows.iter()
            .any(|r| r.contains("\"kind\":\"http_response\"") && r.contains("\"status\":200")),
        "expected an http_response row with status 200; rows: {rows:#?}"
    );
    // The real secret value never appears even though the guest sent it as an Authorization
    // header (header redaction is exercised in detail by the #107 test).
    assert!(
        !rows.iter().any(|r| r.contains("real-secret-value")),
        "the real secret value must never enter the audit log; rows: {rows:#?}"
    );
    // The blind tunnel is never decrypted, so no http_request/http_response is attributable to
    // it — every framed row belongs to the MITM'd host, which only ever has /get and /post.
    assert!(
        !rows.iter().any(|r| r.contains("\"kind\":\"http_request\"")
            && !r.contains("\"path\":\"/get\"")
            && !r.contains("\"path\":\"/post\"")),
        "no http_request rows should exist beyond the MITM'd host's paths; rows: {rows:#?}"
    );

    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));
}

/// Header capture + placeholder-aware redaction + attribution on MITM connections (#107):
/// `http_request` rows now carry headers, but every header is scrubbed at capture so a raw
/// credential never reaches the log. The guest sends `Authorization: $ECHO_TOKEN`, where the
/// guest-visible value is a dome placeholder; the proxy framer must record that header as the
/// attribution tag `<secret:ECHO_TOKEN>` (naming which secret was used) while a non-sensitive
/// header like `Host` passes through verbatim. The real secret value must never appear.
#[test]
#[ignore]
fn egress_audit_log_redacts_and_attributes_headers() {
    let name = sandbox_name("audit-headers");
    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));

    // One GET to the MITM'd host carrying the secret in an Authorization header.
    let guest_cmd = "curl -sS --max-time 25 -H \"Authorization: Bearer $ECHO_TOKEN\" \
         https://postman-echo.com/get > /tmp/get.out 2>&1; echo headers-done";
    let out = Command::new(dome_bin())
        .env("ECHO_TOKEN", "real-secret-value")
        .args([
            "sandbox",
            "run",
            &name,
            "--allow-net",
            "--secret",
            "ECHO_TOKEN=ECHO_TOKEN@postman-echo.com",
            "--",
            "sh",
            "-c",
            guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    assert!(
        out.status.success(),
        "the audited run should succeed; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Stop the worker so the writer drains and flushes its tail before we read the log.
    stop_worker(&name);

    let rows = audit_rows(&name);
    assert!(
        !rows.is_empty(),
        "egress should have produced audit rows under audit/{name}/"
    );

    // The Authorization header was captured but redacted to its attribution tag: the request
    // is recorded as having used the ECHO_TOKEN secret, without the placeholder or real value.
    assert!(
        rows.iter().any(|r| r.contains("\"kind\":\"http_request\"")
            && r.contains("\"path\":\"/get\"")
            && r.contains("<secret:ECHO_TOKEN>")),
        "expected the GET /get request row to attribute its Authorization header to \
         <secret:ECHO_TOKEN>; rows: {rows:#?}"
    );
    // A non-sensitive header passes through verbatim, so the row carries real header context.
    assert!(
        rows.iter()
            .any(|r| r.contains("\"kind\":\"http_request\"") && r.contains("postman-echo.com")),
        "expected the request row to carry the verbatim Host header; rows: {rows:#?}"
    );
    // The real secret value never reaches the log, even as a redacted header.
    assert!(
        !rows.iter().any(|r| r.contains("real-secret-value")),
        "the real secret value must never enter the audit log; rows: {rows:#?}"
    );
    // The raw placeholder token must not survive in the log either — a sensitive header
    // carrying it is rewritten to the attribution tag, never the bare token.
    assert!(
        !rows.iter().any(|r| r.contains("dome_tok_")),
        "the raw placeholder token must not appear in the audit log; rows: {rows:#?}"
    );

    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));
}

/// Drop accounting / fail-open made visible (#105), end-to-end through the real boot path. The
/// proxy→writer channel is bounded and senders `try_send` (never block egress); when the guest
/// floods egress faster than the writer drains, events are dropped — and that gap must be
/// *labeled* rather than lost silently. We force a depth-1 channel via `DOME_AUDIT_CHANNEL_CAPACITY`
/// (resolved client-side, never read in the shared worker — same threading as the rotation caps)
/// and fire a thundering herd of concurrent connections so the burst of `conn_open`/`conn_close`
/// events outruns the writer. We then assert that at least one `dropped { count }` marker with a
/// positive count was materialized directly to the log, that the markers are self-describing, and
/// that the secret-capture invariant still holds (the real secret never appears) even under flood.
#[test]
#[ignore]
fn egress_audit_log_labels_dropped_events_under_overload() {
    let name = sandbox_name("audit-drops");
    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));

    // A thundering herd: 60 backgrounded curls open near-simultaneously, so a burst of
    // conn_open events slams a depth-1 channel far faster than the writer drains one at a time —
    // forcing try_send to see Full and the sink to count drops. A final secret-bound MITM curl
    // re-checks the capture invariant under load.
    let guest_cmd = "for i in $(seq 1 60); do \
         curl -sS --max-time 25 https://example.com/ > /dev/null 2>&1 & done; wait; \
         curl -sS --max-time 25 -H \"Authorization: $ECHO_TOKEN\" \
         https://postman-echo.com/get > /dev/null 2>&1; echo drops-done";
    let out = Command::new(dome_bin())
        .env("ECHO_TOKEN", "real-secret-value")
        // Depth-1 channel: the smallest bound, so even a modest concurrent burst saturates it.
        .env("DOME_AUDIT_CHANNEL_CAPACITY", "1")
        .args([
            "sandbox",
            "run",
            &name,
            "--allow-net",
            "--secret",
            "ECHO_TOKEN=ECHO_TOKEN@postman-echo.com",
            "--",
            "sh",
            "-c",
            guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    assert!(
        out.status.success(),
        "the audited run should succeed; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Stop the worker so the writer drains and labels the final gap before we read the log.
    stop_worker(&name);

    let rows = audit_rows(&name);
    assert!(
        !rows.is_empty(),
        "egress should have produced audit rows under audit/{name}/"
    );

    // The flood produced at least one labeled gap: a `dropped` record materialized directly to
    // the file (it could never have travelled the channel that overflowed) carrying a positive
    // count of events lost since the previous marker.
    let dropped: Vec<&String> = rows
        .iter()
        .filter(|r| r.contains("\"kind\":\"dropped\""))
        .collect();
    assert!(
        !dropped.is_empty(),
        "a saturated channel must label the gap with a `dropped` record; rows: {rows:#?}"
    );
    // Each marker is self-describing (stamped identity) and reports a non-zero count.
    assert!(
        dropped
            .iter()
            .all(|r| r.contains(&format!("\"sandbox\":\"{name}\"")) && r.contains("\"session\":")),
        "dropped markers must carry the sandbox/session identity; markers: {dropped:#?}"
    );
    assert!(
        dropped
            .iter()
            .all(|r| !r.contains("\"count\":0") && r.contains("\"count\":")),
        "a dropped marker is only written for a real gap (count > 0); markers: {dropped:#?}"
    );
    // Fail-open visibility never compromises the capture invariant: the real secret is absent.
    assert!(
        !rows.iter().any(|r| r.contains("real-secret-value")),
        "the real secret value must never enter the audit log; rows: {rows:#?}"
    );

    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));
}

/// Age-based audit retention folded into `dome prune` (#106), end-to-end through the real boot
/// path. The size ceiling (#104) is the always-on bound; this is the complementary age
/// housekeeping — drop whole sessions untouched for longer than the default age, on-demand via
/// `dome prune` (no background timer). We boot a *persistent* sandbox and run an *ephemeral*
/// `dome run`, both making real egress so each writes a genuine audit session; we then backdate
/// every segment of those sessions past the age threshold, seed a *fresh* sentinel session that
/// must survive, and run the real `dome prune`. It must reap both aged sessions (ephemeral and
/// persistent under the same policy), keep the fresh one, and report the reclaimed bytes in its
/// summary alongside the chunk sweep.
#[test]
#[ignore]
fn egress_audit_log_reaps_aged_sessions_via_prune() {
    let name = sandbox_name("audit-reap");
    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));

    // A persistent sandbox makes egress (auditing rides the proxy, so `--allow-net`): a real
    // audit session lands under audit/<name>/.
    let guest_cmd =
        "curl -sS --max-time 25 https://example.com/ > /dev/null 2>&1; echo persistent-done";
    let persistent = Command::new(dome_bin())
        .args([
            "sandbox",
            "run",
            &name,
            "--allow-net",
            "--",
            "sh",
            "-c",
            guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    assert!(
        persistent.status.success(),
        "the persistent audited run should succeed; stderr: {}",
        String::from_utf8_lossy(&persistent.stderr)
    );
    // Stop the worker so the writer drains and flushes before we backdate + prune.
    stop_worker(&name);

    // An ephemeral `dome run` makes egress too: its (single-session) rows bucket under
    // audit/ephemeral/. Snapshot that bucket before/after so we age only this run's session.
    let eph_before: std::collections::HashSet<_> =
        audit_session_dirs("ephemeral").into_iter().collect();
    let ephemeral = Command::new(dome_bin())
        .args(["run", "--allow-net", "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    assert!(
        ephemeral.status.success(),
        "the ephemeral audited run should succeed; stderr: {}",
        String::from_utf8_lossy(&ephemeral.stderr)
    );
    let eph_new: Vec<_> = audit_session_dirs("ephemeral")
        .into_iter()
        .filter(|d| !eph_before.contains(d))
        .collect();
    assert!(
        !eph_new.is_empty(),
        "the ephemeral run should have written a new audit session under audit/ephemeral/"
    );

    // Backdate every aged session (persistent + the new ephemeral one) past the threshold.
    let aged = std::time::Duration::from_secs(40 * 86_400);
    let persistent_sessions = audit_session_dirs(&name);
    assert!(
        !persistent_sessions.is_empty(),
        "the persistent run should have written an audit session under audit/{name}/"
    );
    for dir in persistent_sessions.iter().chain(eph_new.iter()) {
        age_audit_session(dir, aged);
    }

    // Seed a FRESH sentinel session under the persistent sandbox: it must survive the prune.
    let fresh = audit_sandbox_dir(&name) + "/fresh-sentinel";
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::write(
        format!("{fresh}/events-0001.jsonl"),
        "{\"kind\":\"conn_open\"}\n",
    )
    .unwrap();

    // On-demand age reaping runs as part of the real `dome prune`.
    let prune = prune_cmd();
    assert!(
        prune.status.success(),
        "prune should succeed; stderr: {}",
        String::from_utf8_lossy(&prune.stderr)
    );
    let stderr = String::from_utf8_lossy(&prune.stderr);

    // The reclaimed audit bytes are reported in the prune summary (alongside the chunk sweep).
    assert!(
        stderr.contains("reaped") && stderr.contains("aged audit session"),
        "prune must report reaped audit sessions; stderr: {stderr}"
    );

    // Both aged sessions (persistent AND ephemeral) were removed under the same age policy.
    for dir in persistent_sessions.iter().chain(eph_new.iter()) {
        assert!(
            !dir.exists(),
            "aged audit session should have been reaped: {}",
            dir.display()
        );
    }
    // The fresh sentinel session survives — only aged sessions are reaped.
    assert!(
        std::path::Path::new(&fresh).exists(),
        "a fresh audit session must survive the age reap"
    );

    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));
}

/// Segment rotation + inline size-ceiling retention (#104), end-to-end through the real boot
/// path. The writer rolls to `events-NNNN.jsonl` once a segment hits its cap and, immediately
/// after each roll, unlinks the oldest segments across the sandbox until under the per-sandbox
/// size ceiling — the always-on safety bound that holds without `dome prune` and can trim
/// oldest segments of even the still-running session. We force tiny caps via the
/// `DOME_AUDIT_*` env overrides (resolved client-side at boot-spec construction, never read in
/// the shared worker) so a handful of egress connections drives many rotations and a trim. We
/// then assert that
/// multiple segments exist, that the oldest was reaped, that the on-disk total stayed bounded
/// by the ceiling, and that the active writer was never disrupted (its newest rows survive,
/// still self-describing).
#[test]
#[ignore]
fn egress_audit_log_rotates_segments_and_enforces_size_ceiling() {
    let name = sandbox_name("audit-rotation");
    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));

    // Roll every 2 events and keep the sandbox under ~1.5 KB: a loop of blind-tunnel curls
    // (conn_open + conn_close = 2 events each → one segment per curl) produces far more
    // segments than the ceiling holds, forcing the oldest to be trimmed while the run is live.
    let guest_cmd = "for i in $(seq 1 12); do \
         curl -sS --max-time 20 https://example.com/ > /dev/null 2>&1; done; \
         curl -sS --max-time 25 -H \"Authorization: $ECHO_TOKEN\" \
         https://postman-echo.com/get > /dev/null 2>&1; echo rotation-done";
    let out = Command::new(dome_bin())
        .env("ECHO_TOKEN", "real-secret-value")
        .env("DOME_AUDIT_SEGMENT_MAX_BYTES", "1000000")
        .env("DOME_AUDIT_SEGMENT_MAX_EVENTS", "2")
        .env("DOME_AUDIT_SANDBOX_MAX_BYTES", "1500")
        .args([
            "sandbox",
            "run",
            &name,
            "--allow-net",
            "--secret",
            "ECHO_TOKEN=ECHO_TOKEN@postman-echo.com",
            "--",
            "sh",
            "-c",
            guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    assert!(
        out.status.success(),
        "the audited run should succeed; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Stop the worker so the writer drains and flushes its tail before we read the log.
    stop_worker(&name);

    let segs = audit_segments(&name);
    // Rotation fired: the run produced multiple `events-NNNN.jsonl` segments, not one big file.
    assert!(
        segs.len() >= 2,
        "the event cap should have rolled multiple segments; got: {segs:#?}"
    );
    // Segments are numbered, and the very first (oldest) was trimmed once over the ceiling —
    // the dir does not simply start at events-0001 and grow forever.
    assert!(
        !segs
            .iter()
            .any(|p| p.file_name().and_then(|n| n.to_str()) == Some("events-0001.jsonl")),
        "the oldest segment (events-0001.jsonl) should have been trimmed; got: {segs:#?}"
    );

    // The size ceiling is the always-on safety bound: the sandbox's audit dir stays bounded
    // (ceiling + at most one active segment's growth), nowhere near the full run.
    let total_bytes: u64 = segs
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    assert!(
        total_bytes <= 1500 + 2000,
        "the size ceiling must bound the audit dir; got {total_bytes} bytes over {segs:#?}"
    );

    // The active writer was never disrupted by trimming: the surviving newest segments still
    // hold self-describing rows stamped with the sandbox identity.
    let rows = audit_rows(&name);
    assert!(
        !rows.is_empty(),
        "the newest segments must still hold rows after trimming"
    );
    assert!(
        rows.iter()
            .all(|r| r.contains(&format!("\"sandbox\":\"{name}\""))),
        "every surviving row must carry the sandbox identity; rows: {rows:#?}"
    );
    // Trimming never compromises the guest-side capture invariant: the real secret is absent.
    assert!(
        !rows.iter().any(|r| r.contains("real-secret-value")),
        "the real secret value must never enter the audit log; rows: {rows:#?}"
    );

    rm_sandbox(&name);
    let _ = std::fs::remove_dir_all(audit_sandbox_dir(&name));
}
