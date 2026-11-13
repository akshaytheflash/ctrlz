mod common;

use std::time::{Duration, Instant};

use common::Sandbox;

#[test]
#[ignore]
fn warm_mark_of_ten_thousand_files_stays_under_one_and_a_half_seconds() {
    let sandbox = Sandbox::new();
    for directory in 0..100 {
        for file in 0..100 {
            sandbox.write(
                &format!("src/dir{directory:03}/file{file:03}.txt"),
                format!("file {directory} {file}\n"),
            );
        }
    }
    sandbox.init();

    sandbox.ok(&["mark", "--label", "warmup", "--agent", "perf"]);
    let start = Instant::now();
    sandbox.ok(&["mark", "--label", "measured", "--agent", "perf"]);
    let elapsed = start.elapsed();
    eprintln!("warm mark of 10,000 files: {elapsed:?}");

    assert!(
        elapsed < Duration::from_millis(1500),
        "warm mark took {elapsed:?}"
    );
}
