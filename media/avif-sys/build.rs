use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();
    let vendor = manifest.join("vendor");
    let dav1d = vendor.join("dav1d");
    let libavif = vendor.join("libavif");
    let dav1d_build = out.join("dav1d-build");
    let dav1d_install = out.join("dav1d-install");
    let avif_build = out.join("libavif-build");
    let rav1e_stub = out.join("librav1e.a");

    for path in [
        "vendor/libavif/include/avif/avif.h",
        "vendor/libavif/src/avif.c",
        "vendor/dav1d/meson.build",
        "vendor/dav1d/src/meson.build",
        "vendor/rav1e/rav1e.h",
        "src/shim.c",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_ROOT");

    let mut meson = Command::new(env::var_os("PROMTUZ_MESON").unwrap_or_else(|| "meson".into()));
    meson
        .arg("setup")
        .arg("--buildtype=release")
        .arg("--default-library=static")
        .arg("--prefix")
        .arg(&dav1d_install)
        .arg("--libdir=lib")
        .arg("-Denable_tools=false")
        .arg("-Denable_examples=false")
        .arg("-Denable_tests=false")
        .arg(format!("-Denable_asm={}", dav1d_asm_enabled(&target)))
        .arg(&dav1d_build)
        .arg(&dav1d);
    if target.contains("android") {
        meson.arg("--cross-file").arg(android_cross_file(&out, &target));
    }
    run(&mut meson, "Meson is required to build the vendored dav1d decoder");
    run(Command::new("ninja").arg("-C").arg(&dav1d_build), "Ninja is required to build dav1d");
    run(
        Command::new("ninja").arg("-C").arg(&dav1d_build).arg("install"),
        "Ninja could not install dav1d",
    );
    create_empty_archive(&rav1e_stub, &target);

    let mut cmake = Command::new("cmake");
    cmake
        .arg("-S")
        .arg(&libavif)
        .arg("-B")
        .arg(&avif_build)
        .arg("-DBUILD_SHARED_LIBS=OFF")
        .arg("-DAVIF_BUILD_APPS=OFF")
        .arg("-DAVIF_BUILD_EXAMPLES=OFF")
        .arg("-DAVIF_BUILD_TESTS=OFF")
        .arg("-DAVIF_CODEC_AOM=OFF")
        .arg("-DAVIF_CODEC_DAV1D=SYSTEM")
        .arg("-DAVIF_CODEC_RAV1E=SYSTEM")
        .arg("-DCMAKE_DISABLE_FIND_PACKAGE_PkgConfig=TRUE")
        .arg("-DAVIF_LIBYUV=OFF")
        .arg("-DAVIF_ZLIBPNG=OFF")
        .arg(format!("-DCMAKE_PREFIX_PATH={}", dav1d_install.display()))
        .arg(format!("-DDAV1D_INCLUDE_DIR={}", dav1d_install.join("include").display()))
        .arg(format!("-DDAV1D_LIBRARY={}", dav1d_install.join("lib/libdav1d.a").display()))
        .arg(format!("-DDAV1D_LIBRARIES={}", dav1d_install.join("lib/libdav1d.a").display()))
        .arg(format!("-DRAV1E_INCLUDE_DIR={}", vendor.join("rav1e").display()))
        .arg(format!("-DRAV1E_LIBRARY={}", rav1e_stub.display()))
        .arg(format!("-DRAV1E_LIBRARIES={}", rav1e_stub.display()));
    configure_android(&mut cmake, &target);
    run(&mut cmake, "CMake could not configure the vendored libavif decoder");
    run(Command::new("cmake").arg("--build").arg(&avif_build), "CMake could not build libavif");

    let include = libavif.join("include");
    cc::Build::new()
        .file("src/shim.c")
        .include(&include)
        .warnings(true)
        .compile("promtuz_avif_shim");
    println!("cargo:rustc-link-search=native={}", avif_build.display());
    println!("cargo:rustc-link-lib=static=avif");
    println!("cargo:rustc-link-search=native={}", dav1d_install.join("lib").display());
    println!("cargo:rustc-link-lib=static=dav1d");
}

fn dav1d_asm_enabled(target: &str) -> bool {
    !target.starts_with("x86_64-") || Command::new("nasm").arg("-v").output().is_ok()
}

fn create_empty_archive(path: &Path, target: &str) {
    let archive = if target.contains("android") {
        let ndk = env::var("ANDROID_NDK_HOME")
            .or_else(|_| env::var("ANDROID_NDK_ROOT"))
            .expect("ANDROID_NDK_HOME is required for Android builds");
        std::fs::read_dir(Path::new(&ndk).join("toolchains/llvm/prebuilt"))
            .expect("Android NDK LLVM toolchain is missing")
            .find_map(Result::ok)
            .map(|entry| entry.path().join("bin/llvm-ar"))
            .expect("Android NDK has no LLVM archiver")
    } else {
        PathBuf::from("ar")
    };
    run(
        Command::new(archive).arg("rcs").arg(path),
        "An archiver is required to create the rav1e ABI placeholder",
    );
}

fn run(command: &mut Command, missing_tool: &str) {
    let display = format!("{command:?}");
    match command.status() {
        Ok(status) if status.success() => {},
        Ok(status) => panic!("{missing_tool}: {display} exited {status}"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            panic!("{missing_tool}. Install it locally; native sources are already vendored.")
        },
        Err(error) => panic!("{missing_tool}: {display}: {error}"),
    }
}

fn android_cross_file(out: &Path, target: &str) -> PathBuf {
    let (triple, family, cpu) = match target {
        t if t.starts_with("aarch64-") => ("aarch64-linux-android", "aarch64", "aarch64"),
        t if t.starts_with("armv7-") => ("armv7a-linux-androideabi", "arm", "arm"),
        t if t.starts_with("x86_64-") => ("x86_64-linux-android", "x86_64", "x86_64"),
        t if t.starts_with("i686-") => ("i686-linux-android", "x86", "x86"),
        _ => panic!("unsupported Android target: {target}"),
    };
    let ndk = env::var("ANDROID_NDK_HOME")
        .or_else(|_| env::var("ANDROID_NDK_ROOT"))
        .expect("ANDROID_NDK_HOME is required for Android builds");
    let prebuilt = std::fs::read_dir(Path::new(&ndk).join("toolchains/llvm/prebuilt"))
        .expect("Android NDK LLVM toolchain is missing")
        .find_map(Result::ok)
        .map(|entry| entry.path())
        .expect("Android NDK has no LLVM host toolchain");
    let bin = prebuilt.join("bin");
    let cross = out.join(format!("dav1d-{target}.meson"));
    std::fs::write(
        &cross,
        format!(
            "[binaries]\nc = '{compiler}'\ncpp = '{compiler}++'\nar = '{ar}'\nstrip = '{strip}'\npkg-config = 'pkg-config'\n\n[properties]\nneeds_exe_wrapper = true\n\n[host_machine]\nsystem = 'android'\ncpu_family = '{family}'\ncpu = '{cpu}'\nendian = 'little'\n",
            compiler = bin.join(format!("{triple}26-clang")).display(),
            ar = bin.join("llvm-ar").display(),
            strip = bin.join("llvm-strip").display(),
            family = family,
            cpu = cpu,
        ),
    )
    .expect("could not write dav1d Android cross file");
    cross
}

fn configure_android(cmake: &mut Command, target: &str) {
    if !target.contains("android") {
        return;
    }
    let ndk = env::var("ANDROID_NDK_HOME")
        .or_else(|_| env::var("ANDROID_NDK_ROOT"))
        .expect("ANDROID_NDK_HOME is required for Android builds");
    let abi = match target {
        t if t.starts_with("aarch64-") => "arm64-v8a",
        t if t.starts_with("armv7-") => "armeabi-v7a",
        t if t.starts_with("x86_64-") => "x86_64",
        t if t.starts_with("i686-") => "x86",
        _ => panic!("unsupported Android target: {target}"),
    };
    cmake
        .arg("-DCMAKE_SYSTEM_NAME=Android")
        .arg(format!("-DCMAKE_ANDROID_NDK={ndk}"))
        .arg(format!("-DCMAKE_ANDROID_ARCH_ABI={abi}"))
        .arg("-DCMAKE_ANDROID_API=26");
}
