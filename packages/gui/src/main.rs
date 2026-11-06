// The Flutter runner was a GUI-subsystem executable; without this every launch opens a console.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    let mut argv: Vec<String> = std::env::args().collect();
    // Makepad 1.0 opens packaged `makepad/<crate>/resources` relative to the working
    // directory; launchers (desktop entries, `paseo://` handlers, shortcuts, updater
    // relaunches) do not set one. Relative path arguments (`paseo-sessions ./project`) are
    // made absolute against the original directory first.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if option_env!("MAKEPAD_PACKAGE_DIR").is_some() {
        if let Ok(original) = std::env::current_dir() {
            for arg in argv.iter_mut().skip(1) {
                if arg.starts_with('-') || arg.contains("://") {
                    continue;
                }
                let path = std::path::Path::new(arg.as_str());
                if path.is_relative() && original.join(path).exists() {
                    *arg = original.join(path).to_string_lossy().into_owned();
                }
            }
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|p| p.to_path_buf())) {
            let _ = std::env::set_current_dir(dir);
        }
    }
    // A second launch forwards its argv (deep links, project paths) to the running primary
    // instance and exits with the code the primary reports.
    if let paseo_gui_platform::Bootstrap::Forwarded { exit_code } = paseo_gui_platform::bootstrap_process(&argv) {
        std::process::exit(exit_code);
    }
    paseo_gui::app::set_launch_args(argv);
    paseo_gui::app::app_main();
}
