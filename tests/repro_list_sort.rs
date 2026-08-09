use assert_cmd::Command;

#[allow(dead_code)]
#[path = "common/cli.rs"]
mod common_cli;

#[test]
fn test_list_sort_title_case_insensitive() {
    let temp = tempfile::TempDir::new_in(common_cli::isolated_temp_root()).unwrap();
    let path = temp.path();

    // Init
    Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    // Create issues
    // "apple" (lowercase 'a')
    Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(path)
        .arg("create")
        .arg("apple")
        .assert()
        .success();

    // "Banana" (uppercase 'B')
    Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(path)
        .arg("create")
        .arg("Banana")
        .assert()
        .success();

    // List sorted by title
    let output = Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(path)
        .arg("list")
        .arg("--sort")
        .arg("title")
        .output()
        .expect("Failed to list issues");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // In case-sensitive sort: "Banana" < "apple" (B=66, a=97) -> Banana then apple
    // In case-insensitive sort: "apple" < "Banana" (a=97, b=98) -> apple then Banana

    let banana_pos = stdout.find("Banana").expect("Banana not found");
    let apple_pos = stdout.find("apple").expect("apple not found");

    // We want case-INsensitive sort, so apple should be before Banana
    assert!(
        apple_pos < banana_pos,
        "Expected 'apple' before 'Banana' (case-insensitive sort), but got:\n{stdout}"
    );
}
