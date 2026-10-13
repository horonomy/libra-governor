//! T10(b) — a client speaking a newer protocol version must never shut
//! down the trusted-mode provider (ADR-0019 §1 "No client-triggered
//! shutdown", §3 T10, PR-2's design doc §4.3). Default mode's own
//! `serve`/`handle_connection` legitimately DO shut down in this case
//! (self-healing after a binary upgrade — see `server.rs`'s own docs on
//! `shut_down_after_reply`); this test's sensitivity control proves that
//! mechanism is real in this environment, and the two-principal run
//! proves `serve_trusted` has no such path at all.

use libra_governor_two_principal_harness::{
    assert_disposable, reset_state_dirs, run_as, ConfigSpec, DaemonProcess, TrustedConfigGuard,
    TrustedDaemon, LG_AGENT, PROVIDER_SOCKET, SAMEUID_STATE_DIR, TRUSTED_CONFIG_PATH,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn raw_client() -> &'static str {
    env!("CARGO_BIN_EXE_lg-raw-client")
}

fn send(socket: &str, json: &str) -> std::process::Output {
    run_as(
        &LG_AGENT,
        raw_client(),
        &[socket, json],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    )
}

const NEWER_PROTOCOL_ENVELOPE: &str =
    r#"{"protocol_version":4000000000,"request":{"kind":"status"}}"#;

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t10b_newer_protocol_envelope_does_not_shut_down_the_provider() {
    assert_disposable();
    reset_state_dirs();

    // Sensitivity control: the same envelope against a same-UID
    // default-mode daemon really does trigger its self-healing shutdown.
    {
        let mut daemon = DaemonProcess::start(
            &LG_AGENT,
            SAMEUID_STATE_DIR,
            "/home/lg-agent",
            std::time::Duration::from_secs(10),
        );
        let socket = format!("{SAMEUID_STATE_DIR}/daemon.sock");
        let out = send(&socket, NEWER_PROTOCOL_ENVELOPE);
        assert!(
            out.status.success(),
            "sensitivity control: lg-raw-client round trip must still succeed even though the \
             daemon shuts down right after replying"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !daemon.is_alive() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "sensitivity control: the same-UID default-mode daemon must shut down after \
                     a newer-protocol-version client connects"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    // Two-principal run: trusted mode.
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut daemon =
        TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], std::time::Duration::from_secs(10));

    let out = send(PROVIDER_SOCKET, NEWER_PROTOCOL_ENVELOPE);
    assert!(
        out.status.success(),
        "trusted mode must still answer a newer-protocol-version envelope, not drop the \
         connection: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("non-JSON reply: {e}; stdout was {:?}", out.stdout));
    assert_eq!(
        response["response"]["kind"], "error",
        "trusted mode must reply Error to a protocol-version mismatch, not shut down silently: \
         {response}"
    );

    assert!(
        daemon.is_alive(),
        "the trusted provider must still be running after the mismatched envelope"
    );

    // Prove it is not merely alive but still SERVING: a normal request
    // must still get a normal reply.
    let normal = format!(
        r#"{{"protocol_version":{},"request":{{"kind":"status"}}}}"#,
        libra_governor_protocol::PROTOCOL_VERSION
    );
    let out = send(PROVIDER_SOCKET, &normal);
    assert!(
        out.status.success(),
        "trusted provider must still serve a normal request after the mismatched envelope"
    );
    let response: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("non-JSON reply: {e}; stdout was {:?}", out.stdout));
    assert_eq!(response["response"]["kind"], "status");

    daemon.kill9_and_reap();
}
