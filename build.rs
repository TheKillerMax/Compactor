use vergen::{generate_cargo_keys, ConstantsFlags};
use winres;

fn main() {
    generate_cargo_keys(ConstantsFlags::all()).expect("Unable to generate the cargo keys!");

    let mut res = winres::WindowsResource::new();
    res.set_icon("compact.ico");
    // winres 0.1 defaults to a Windows root directory and *.exe tools even
    // when its build script runs on Linux. Keep the normal MSVC path and also
    // support cross-checking the Windows sources with a real MinGW toolchain.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
        && std::env::var("HOST").ok() != std::env::var("TARGET").ok()
    {
        let architecture = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
            Ok("x86") => "i686",
            _ => "x86_64",
        };
        res.set_toolkit_path(".");
        res.set_windres_path(&format!("{}-w64-mingw32-windres", architecture));
        res.set_ar_path(&format!("{}-w64-mingw32-ar", architecture));
    }
    res.compile().expect("Unable to compile Windows resources");
}
