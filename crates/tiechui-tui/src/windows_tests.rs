use std::io;
use std::os::windows::process::CommandExt;
use std::process::Command;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::is_raw_mode_enabled;

#[test]
fn setup_initializes_console_modes() -> io::Result<()> {
    const CHILD_MODE: &str = "TIECHUI_TEST_CONSOLE_MODE";
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        // A fresh process keeps Crossterm's saved mode uninitialized, and a new
        // console lets this run under CI without changing the runner's terminal.
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        for mode in ["inline", "fullscreen"] {
            let output = Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "windows_tests::setup_initializes_console_modes",
                    "--nocapture",
                ])
                .env(CHILD_MODE, mode)
                .creation_flags(CREATE_NEW_CONSOLE)
                .output()?;
            assert!(output.status.success(), "{mode}: {output:?}");
        }
        return Ok(());
    };

    let mut out = io::stdout();
    let fullscreen = mode == "fullscreen";
    // Cover initial startup, mouse capture changes, and setup after restoration.
    for fullscreen in [fullscreen, !fullscreen, fullscreen] {
        let result = (|| -> io::Result<()> {
            super::setup(&mut out, fullscreen)?;
            assert!(is_raw_mode_enabled()?);
            execute!(out, EnableMouseCapture, DisableMouseCapture)?;
            assert!(is_raw_mode_enabled()?);
            Ok(())
        })();
        super::restore();
        result?;
        assert!(!is_raw_mode_enabled()?);
    }
    Ok(())
}
