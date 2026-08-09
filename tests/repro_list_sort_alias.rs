use assert_cmd::Command;

#[allow(dead_code)]
#[path = "common/cli.rs"]
mod common_cli;

#[test]
fn test_list_sort_aliases_are_accepted() {
    let temp = tempfile::TempDir::new_in(common_cli::isolated_temp_root()).unwrap();
    let bin = assert_cmd::cargo::cargo_bin!("br");

    Command::new(&bin)
        .current_dir(temp.path())
        .arg("init")
        .assert()
        .success();

    Command::new(&bin)
        .current_dir(temp.path())
        .args(["list", "--sort", "created", "--json"])
        .assert()
        .success();
}
