fn main() {
    let mut build = cc::Build::new();
    build
        .compiler("clang")
        .file("src/wind_direction.c")
        .opt_level(3)
        .warnings(true)
        .flag_if_supported("-Werror")
        .flag_if_supported("-fno-math-errno")
        .flag_if_supported("-fno-trapping-math")
        .flag_if_supported("-freciprocal-math")
        .flag_if_supported("-ffp-contract=fast")
        .flag_if_supported("-fno-omit-frame-pointer");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
        build.flag_if_supported("-march=native");
    }
    build.compile("om_wind_direction");

    if std::env::var_os("CARGO_CFG_UNIX").is_some() {
        println!("cargo:rustc-link-lib=m");
    }
    println!("cargo:rerun-if-changed=src/wind_direction.c");
}
