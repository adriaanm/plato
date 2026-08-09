use std::env;

fn main() {
    let target = env::var("TARGET").unwrap();

    // Cross-compiling for the Kindle Paperwhite 3 (ezkindle fork): armv7
    // soft-float ABI, **static musl**, everything built by xbuild.py.
    //
    // Three differences from the Kobo arm above, all forced by static musl:
    //   * no libstdc++/libc++ at all -- harfbuzz is compiled -fno-exceptions
    //     -fno-rtti and its archive has no C++ runtime symbols left
    //     (verified with llvm-nm), so linking a C++ runtime would only add a
    //     dependency we do not have;
    //   * no bzip2 -- freetype is configured --with-bzip2=no, and nothing
    //     else in the EPUB path wants it;
    //   * a dylib is not an option, so every entry here is a static archive.
    if target == "armv7-unknown-linux-musleabi" {
        println!("cargo:rustc-link-search=target/mupdf_wrapper/Kindle");
        println!("cargo:rustc-link-lib=mupdf-third");
        println!("cargo:rustc-link-lib=z");
        println!("cargo:rustc-link-lib=jpeg");
        println!("cargo:rustc-link-lib=png16");
        println!("cargo:rustc-link-lib=gumbo");
        println!("cargo:rustc-link-lib=openjp2");
        println!("cargo:rustc-link-lib=jbig2dec");
        // musl has no C23 fminimum_num*/fmaximum_num*, which rustc emits for
        // f32::min / f32::max.  xbuild.py builds the four functions.
        println!("cargo:rustc-link-lib=c23compat");
        return;
    }

    // Cross-compiling for Kobo.
    if target == "arm-unknown-linux-gnueabihf" {
        println!("cargo:rustc-env=PKG_CONFIG_ALLOW_CROSS=1");
        println!("cargo:rustc-link-search=target/mupdf_wrapper/Kobo");
        println!("cargo:rustc-link-search=libs");
        println!("cargo:rustc-link-lib=dylib=stdc++");
    // Handle the Linux and macOS platforms.
    } else {
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
        match target_os.as_ref() {
            "linux" => {
                println!("cargo:rustc-link-search=target/mupdf_wrapper/Linux");
                println!("cargo:rustc-link-lib=dylib=stdc++");
            },
            "macos" => {
                println!("cargo:rustc-link-search=target/mupdf_wrapper/Darwin");
                println!("cargo:rustc-link-lib=dylib=c++");
            },
            _ => panic!("Unsupported platform: {}.", target_os),
        }

        println!("cargo:rustc-link-lib=mupdf-third");
    }

    println!("cargo:rustc-link-lib=z");
    println!("cargo:rustc-link-lib=bz2");
    println!("cargo:rustc-link-lib=jpeg");
    println!("cargo:rustc-link-lib=png16");
    println!("cargo:rustc-link-lib=gumbo");
    println!("cargo:rustc-link-lib=openjp2");
    println!("cargo:rustc-link-lib=jbig2dec");
}
