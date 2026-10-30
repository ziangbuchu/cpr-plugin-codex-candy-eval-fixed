use gateway_plugin_sdk::client::{read_frame, write_frame};
use gateway_plugin_sdk::{Frame, Handshake, Message, PROTOCOL_VERSION};
use std::process::Stdio;
use tokio::process::Command;

#[tokio::test]
async fn test_plugin_hello_handshake() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codex-proxy-plugin-candy-eval"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn plugin binary");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    let manifest = codex_proxy_plugin_candy_eval::manifest().unwrap();

    let handshake = Handshake {
        protocol_version: PROTOCOL_VERSION,
        artifact_sha256: "test_digest".into(),
        plugin_id: "nmbb.codex-candy-eval".into(),
        instance_id: "test_instance".into(),
        generation: 1,
        incarnation: "test_incarnation".into(),
        configuration: serde_json::json!({}),
        contributes: manifest.contributes.clone(),
    };

    let hello_frame = Frame::control(Message::Hello {
        handshake: handshake.clone(),
    });

    write_frame(&mut stdin, &hello_frame)
        .await
        .expect("write hello frame");

    let reply = read_frame(&mut stdout).await.expect("read reply frame");

    println!("Received reply from plugin: {:?}", reply);
    match reply.message {
        Message::Ready {
            protocol_version,
            incarnation,
            ..
        } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(incarnation, "test_incarnation");
            println!("Handshake SUCCESS!");
        }
        other => panic!("Unexpected reply: {:?}", other),
    }

    let _ = child.kill().await;
}
