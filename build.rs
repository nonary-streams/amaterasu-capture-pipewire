use std::{env, process::Command};

fn main() {
    println!("cargo:rustc-check-cfg=cfg(amaterasu_pipewire)");
    println!("cargo:rustc-check-cfg=cfg(amaterasu_kwin)");
    for file in ["src/platform/adapter.c", "src/platform/adapter.h"] {
        println!("cargo:rerun-if-changed={file}");
    }
    for variable in [
        "PKG_CONFIG_PATH",
        "PKG_CONFIG_LIBDIR",
        "PKG_CONFIG_SYSROOT_DIR",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var("HOST").ok() != env::var("TARGET").ok()
    {
        return;
    }
    let Ok(output) = Command::new("pkg-config")
        .args([
            "--cflags",
            "--libs",
            "libpipewire-0.3",
            "gio-2.0",
            "gio-unix-2.0",
        ])
        .output()
    else {
        println!("cargo:warning=PipeWire capture disabled: pkg-config unavailable");
        return;
    };
    if !output.status.success() {
        println!(
            "cargo:warning=PipeWire capture disabled: system development dependencies unavailable"
        );
        return;
    }
    let mut build = cc::Build::new();
    if env::var_os("CARGO_FEATURE_LINUX_KWIN_SCREENCAST").is_some() {
        configure_kwin(&mut build);
    }
    build.file("src/platform/adapter.c").flag("-std=c11");
    for flag in String::from_utf8(output.stdout)
        .expect("pkg-config UTF-8 flags")
        .split_whitespace()
    {
        if let Some(directory) = flag.strip_prefix("-I") {
            build.include(directory);
        } else if let Some(library) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={library}");
        } else if let Some(directory) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={directory}");
        } else {
            build.flag(flag);
        }
    }
    build.warnings(true).compile("amaterasu_pipewire_adapter");
    println!("cargo:rustc-cfg=amaterasu_pipewire");
}

fn configure_kwin(build: &mut cc::Build) {
    let out = std::path::PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for name in ["zkde-screencast-unstable-v1", "xdg-output-unstable-v1"] {
        let source =
            std::path::PathBuf::from("protocols").join(format!("{name}.xml"));
        println!("cargo:rerun-if-changed={}", source.display());
        for (mode, extension) in [("client-header", "h"), ("private-code", "c")] {
            let destination = out.join(format!("{name}.{extension}"));
            assert!(
                Command::new("wayland-scanner")
                    .arg(mode)
                    .arg(&source)
                    .arg(&destination)
                    .status()
                    .expect("Wayland scanner required")
                    .success(),
                "Wayland protocol generation failed"
            );
        }
        build.file(out.join(format!("{name}.c")));
    }
    let output = Command::new("pkg-config")
        .args(["--cflags", "--libs", "wayland-client"])
        .output()
        .expect("Wayland pkg-config required");
    assert!(
        output.status.success(),
        "linux-kwin-screencast requires Wayland development files"
    );
    for flag in String::from_utf8(output.stdout).unwrap().split_whitespace() {
        if let Some(path) = flag.strip_prefix("-I") {
            build.include(path);
        } else if let Some(library) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={library}");
        } else if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else {
            build.flag(flag);
        }
    }
    build
        .include(out)
        .file("src/platform/kwin.c")
        .define("AMATERASU_KWIN", None);
    println!("cargo:rerun-if-changed=src/platform/kwin.c");
    println!("cargo:rerun-if-changed=src/platform/kwin.h");
    println!("cargo:rustc-cfg=amaterasu_kwin");
    println!("cargo:rustc-link-arg-cdylib=-Wl,--undefined=amaterasu_kwin_marker");
}
