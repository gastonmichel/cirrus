//! Packaging tasks, in Rust so the project stays single-language.
//!
//!   cargo xtask bundle    build a release `Cirrus.app` in target/release
//!   cargo xtask install   bundle, copy to /Applications and launch
//!
//! Uses only macOS built-in tools (qlmanage, sips, iconutil, codesign, ditto, open).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const APP_NAME: &str = "Cirrus";
const BINARY: &str = "cirrus";
const BUNDLE_ID: &str = "io.github.gastonmichel.cirrus";

type Result<T = ()> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let task = std::env::args().nth(1).unwrap_or_default();
    let result = match task.as_str() {
        "bundle" => bundle().map(|app| println!("built {}", app.display())),
        "install" => bundle().and_then(|app| install(&app)),
        _ => Err("usage: cargo xtask <bundle|install>".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn run(cmd: &mut Command) -> Result {
    let status = cmd.status().map_err(|e| format!("failed to run {cmd:?}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{cmd:?} failed with {status}")) }
}

fn bundle() -> Result<PathBuf> {
    let root = root();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    run(Command::new(cargo).current_dir(&root).args(["build", "--release", "--package", BINARY]))?;

    let app = root.join("target/release").join(format!("{APP_NAME}.app"));
    let contents = app.join("Contents");
    if app.exists() {
        fs::remove_dir_all(&app).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(contents.join("MacOS")).map_err(|e| e.to_string())?;
    fs::create_dir_all(contents.join("Resources")).map_err(|e| e.to_string())?;
    fs::copy(root.join("target/release").join(BINARY), contents.join("MacOS").join(BINARY))
        .map_err(|e| format!("copying binary: {e}"))?;
    fs::write(contents.join("Info.plist"), info_plist()).map_err(|e| e.to_string())?;

    // A missing icon shouldn't block the install.
    if let Err(e) = make_icon(&root.join("assets/icon.svg"), &contents.join("Resources/AppIcon.icns")) {
        eprintln!("warning: app icon skipped: {e}");
    }

    // Ad-hoc signature: the app needs no special entitlements or permissions.
    run(Command::new("codesign").args(["--force", "--deep", "--sign", "-"]).arg(&app))?;
    Ok(app)
}

/// Renders the SVG with Quick Look, then builds an .icns with sips + iconutil.
fn make_icon(svg: &Path, icns: &Path) -> Result {
    let work = std::env::temp_dir().join(format!("cirrus-icon-{}", std::process::id()));
    let iconset = work.join("AppIcon.iconset");
    fs::create_dir_all(&iconset).map_err(|e| e.to_string())?;
    let result = (|| {
        run(Command::new("qlmanage").args(["-t", "-s", "1024", "-o"]).arg(&work).arg(svg).stdout(std::process::Stdio::null()))?;
        let png = work.join("icon.svg.png");
        if !png.exists() {
            return Err("Quick Look did not render the SVG".into());
        }
        for size in [16, 32, 128, 256, 512] {
            for (scale, suffix) in [(1, ""), (2, "@2x")] {
                let px = (size * scale).to_string();
                let out = iconset.join(format!("icon_{size}x{size}{suffix}.png"));
                run(Command::new("sips").args(["-z", &px, &px]).arg(&png).arg("--out").arg(&out).stdout(std::process::Stdio::null()))?;
            }
        }
        run(Command::new("iconutil").args(["-c", "icns"]).arg(&iconset).arg("-o").arg(icns))
    })();
    let _ = fs::remove_dir_all(&work);
    result
}

fn install(app: &Path) -> Result {
    let target = PathBuf::from("/Applications").join(format!("{APP_NAME}.app"));
    // Quit a running copy so it can be replaced.
    let _ = Command::new("pkill").args(["-x", BINARY]).status();
    if target.exists() {
        fs::remove_dir_all(&target).map_err(|e| format!("removing old {}: {e}", target.display()))?;
    }
    run(Command::new("ditto").arg(app).arg(&target))?;
    run(Command::new("open").arg(&target))?;
    println!("installed {}", target.display());
    Ok(())
}

fn info_plist() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>{APP_NAME}</string>
	<key>CFBundleName</key>
	<string>{APP_NAME}</string>
	<key>CFBundleExecutable</key>
	<string>{BINARY}</string>
	<key>CFBundleIdentifier</key>
	<string>{BUNDLE_ID}</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
"#
    )
}
