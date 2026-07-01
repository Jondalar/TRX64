//! Regression: `/mount <relative>` must resolve against the cockpit `cd` cwd (the
//! monitor FILE shell `fs_cwd`), not the daemon's process cwd. Before the fix,
//! `cd out` then `/mount lykia.crt` failed with "file read lykia.crt: No such file".

use std::path::Path;
use trx64_cli::boot_engine;

const ROMS: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";

#[test]
fn mount_resolves_relative_path_against_cd_cwd() {
    if !Path::new(ROMS).join("kernal-901227-03.bin").exists() {
        eprintln!("skip: ROMs not present at {ROMS}");
        return;
    }
    let dir = std::env::temp_dir().join(format!("trx64_cdmount_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("dummy.crt"), b"readable but not a valid CRT").unwrap();

    let e = boot_engine(Path::new(ROMS)).expect("boot");
    // `!cd` into the temp dir (sets the monitor fs_cwd via the `!` filesystem
    // namespace — CLI-FEEL S1), then mount by RELATIVE name. (A bare `cd` is now a
    // cockpit nudge to `!cd`, so the fs verb is used through its `!` prefix here.)
    e.exec_line(&format!("!cd {}", dir.display()));
    let out = e.exec_line("/mount dummy.crt").output;
    eprintln!("mount output: {out}");

    // After the fix the file is FOUND + read (resolved against the cd cwd); it then
    // fails on the bad-CRT parse — which proves the path resolved. Before the fix it
    // was "No such file or directory".
    assert!(
        !out.contains("No such file"),
        "relative `/mount` must resolve against the `cd` cwd, got: {out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
