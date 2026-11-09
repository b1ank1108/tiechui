use std::io;
use std::os::windows::process::CommandExt;
use std::process::Command;

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
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{mode}: {output:?}"
            );
        }
        return Ok(());
    };

    let mut out = io::stdout();
    // Cleanup before any capture must not ask Crossterm to restore an unsaved mode.
    super::restore();
    let fullscreen = mode == "fullscreen";
    // Cover initial startup, mouse capture changes, and setup after restoration.
    for fullscreen in [fullscreen, !fullscreen, fullscreen] {
        let result = (|| -> io::Result<()> {
            super::setup(&mut out, fullscreen)?;
            assert!(is_raw_mode_enabled()?);
            let mut mouse_output = Vec::new();
            super::MOUSE.enable(&mut mouse_output)?;
            assert_eq!(mouse_output, b"\x1b[?1006h");
            mouse_output.clear();
            super::MOUSE.disable(&mut mouse_output)?;
            assert_eq!(mouse_output, b"\x1b[?1006l");
            mouse_output.clear();
            super::MOUSE.disable(&mut mouse_output)?;
            assert!(mouse_output.is_empty());
            assert!(is_raw_mode_enabled()?);
            super::MOUSE.enable(&mut out)?;
            Ok(())
        })();
        super::restore();
        result?;
        assert!(!is_raw_mode_enabled()?);
    }
    Ok(())
}
