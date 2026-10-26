use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
};

#[test]
fn artifact_plan_keeps_stdout_json_and_compares_active_content() {
    let root = std::env::temp_dir().join(format!("pushit-plan-artifacts-{}", std::process::id()));
    fs::create_dir_all(root.join("repo")).unwrap();
    let repo = root.join("repo");
    fs::write(repo.join("source.wasm"), b"artifact").unwrap();
    fs::create_dir_all(repo.join("dist")).unwrap();
    fs::write(repo.join("dist/index.html"), b"artifact UI").unwrap();
    fs::write(repo.join("pushit.yaml"), "name: app\nentry: app.wasm\nenv: {}\nsecrets: []\nbuild:\n  fn: echo build-output; cp source.wasm app.wasm\n").unwrap();
    for args in [
        vec!["init"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "-m",
            "fixture",
        ],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap()
            .status
            .success());
    }
    fs::write(
        root.join("pushit.yaml"),
        format!(
            "apps:\n  cache-key:\n    repo: {}\n    env: {{}}\n",
            repo.display()
        ),
    )
    .unwrap();
    let digest = pushit_controller::digest::artifact_digest(
        &repo.join("source.wasm"),
        Some(&repo.join("dist")),
    )
    .unwrap();
    for (active, expected) in [(true, "up-to-date"), (false, "drift")] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let detail = serde_json::json!({"manifest":"name: app\nenv: {}\n", "revisions":[{"active":active,"digest":digest}]}).to_string();
        let server = std::thread::spawn(move || {
            for body in [
                "[{\"name\":\"app\"}]".to_string(),
                detail,
                "{\"keys\":[]}".into(),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                loop {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        let output = Command::new(env!("CARGO_BIN_EXE_pushit"))
            .args([
                "--server",
                &format!("http://{address}"),
                "--dev",
                "plan",
                "--artifacts",
                "--json",
            ])
            .current_dir(&root)
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(output.status.success(), "{output:?}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["apps"][0]["key"], "cache-key");
        assert_eq!(report["apps"][0]["status"], expected);
        assert_eq!(report["drift"], usize::from(!active));
        assert!(String::from_utf8_lossy(&output.stderr).contains("build-output"));
    }
    fs::remove_dir_all(root).unwrap();
}
