#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs;
use std::os::unix::{fs::symlink, net::UnixListener};
use std::process::Command;

use elphie_tools::{Workspace, definitions};
use serde_json::{Value, json};
use tempfile::tempdir;

#[test]
fn lists_visible_entries_in_order_and_reads_nested_text() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/note"), "café\n").unwrap();
    fs::write(root.path().join("a\"b"), "").unwrap();
    fs::write(root.path().join(".config"), "").unwrap();
    fs::write(root.path().join("line\nbreak"), "").unwrap();
    let tools = Workspace::open(root.path()).unwrap();
    for _ in 0..2 {
        let list = tools
            .execute("list_directory", &json!({"path":"."}))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&list).unwrap(),
            json!([{"kind":"file","name":"a\"b"}, {"kind":"directory","name":"src"}])
        );
    }
    assert_eq!(
        tools
            .execute("read_file", &json!({"path":"./src/note"}))
            .unwrap(),
        "café\n"
    );
    for path in ["./", "src/"] {
        assert!(
            tools
                .execute("list_directory", &json!({"path":path}))
                .is_ok()
        );
    }
    assert!(
        tools
            .execute("list_directory", &json!({"path":"/"}))
            .is_err()
    );
    assert!(
        tools
            .execute("read_file", &json!({"path":"src/note/"}))
            .is_err()
    );
    let defs = definitions();
    assert_eq!(
        defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        ["list_directory", "read_file"]
    );
    assert!(
        defs.iter()
            .all(|d| d.parameters["required"] == json!(["path"])
                && d.parameters["additionalProperties"] == false)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn kernel_filesystems_cannot_be_granted_as_a_workspace() {
    for path in ["/proc", "/sys"] {
        let path = std::path::Path::new(path);
        if path
            .join(if path.ends_with("proc") {
                "self"
            } else {
                "kernel"
            })
            .exists()
        {
            assert!(Workspace::open(path).is_err(), "{path:?}");
        }
    }
}

#[test]
fn rejects_paths_and_arguments_outside_the_tool_contract() {
    let root = tempdir().unwrap();
    let tools = Workspace::open(root.path()).unwrap();
    for path in [
        "../outside",
        "a/../b",
        ".git",
        "x/.secret",
        "",
        "/etc",
        "C:/outside",
        "\\server\\share",
        "a\0b",
        "z\u{202e}txt",
    ] {
        assert!(
            tools
                .execute("list_directory", &json!({"path":path}))
                .is_err(),
            "{path:?}"
        );
    }
    for args in [
        Value::Null,
        json!({"bad":"."}),
        json!({"path":false}),
        json!({"path":".","other":1}),
        json!({"path":"a".repeat(1025)}),
        json!({"path":vec!["."; 33].join("/")}),
    ] {
        assert!(tools.execute("list_directory", &args).is_err());
    }
    assert!(tools.execute("read_file", &json!({"path":"."})).is_err());
    assert!(tools.execute("unknown", &json!({"path":"."})).is_err());
}

#[test]
fn refuses_symlinks_hard_links_and_special_files() {
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("note"), "outside").unwrap();
    fs::write(root.path().join(".config"), "hidden").unwrap();
    symlink(outside.path(), root.path().join("outside")).unwrap();
    symlink(".config", root.path().join("config")).unwrap();
    fs::hard_link(outside.path().join("note"), root.path().join("hard")).unwrap();
    let _socket = UnixListener::bind(root.path().join("socket")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(root.path().join("pipe"))
            .status()
            .unwrap()
            .success()
    );
    let tools = Workspace::open(root.path()).unwrap();
    for path in ["outside/note", "config", "hard", "pipe", "socket"] {
        assert!(
            tools.execute("read_file", &json!({"path":path})).is_err(),
            "{path}"
        );
    }
    assert!(
        tools
            .execute("list_directory", &json!({"path":"outside"}))
            .is_err()
    );
    assert_eq!(
        tools
            .execute("list_directory", &json!({"path":"."}))
            .unwrap(),
        "[]"
    );
    assert!(Workspace::open(&root.path().join("outside")).is_err());
}

#[test]
fn open_workspace_keeps_its_directory_when_the_path_is_replaced() {
    let parent = tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("note"), "first").unwrap();
    let tools = Workspace::open(&root).unwrap();
    fs::rename(&root, parent.path().join("moved")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("note"), "second").unwrap();
    assert_eq!(
        tools.execute("read_file", &json!({"path":"note"})).unwrap(),
        "first"
    );
}

#[test]
fn caps_file_bytes_and_rejects_invalid_text() {
    let root = tempdir().unwrap();
    let path = root.path().join("text");
    let tools = Workspace::open(root.path()).unwrap();
    fs::write(&path, vec![b'x'; 32768]).unwrap();
    assert_eq!(
        tools
            .execute("read_file", &json!({"path":"text"}))
            .unwrap()
            .len(),
        32768
    );
    for bytes in [vec![b'x'; 32769], vec![0xff], vec![0x1b, b'['], vec![0x00]] {
        fs::write(&path, bytes).unwrap();
        assert!(tools.execute("read_file", &json!({"path":"text"})).is_err());
    }
}

#[test]
fn caps_scan_work_visible_entries_and_encoded_listing_size() {
    let root = tempdir().unwrap();
    let tools = Workspace::open(root.path()).unwrap();
    for (dir, count, prefix) in [
        ("hidden", 1025, ".x".to_owned()),
        ("entries", 257, "x".to_owned()),
        ("output", 200, "x".repeat(200)),
    ] {
        let path = root.path().join(dir);
        fs::create_dir(&path).unwrap();
        for i in 0..count {
            fs::write(path.join(format!("{prefix}{i}")), "").unwrap();
        }
        assert!(
            tools
                .execute("list_directory", &json!({"path":dir}))
                .is_err(),
            "{dir}"
        );
    }
}
