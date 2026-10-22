// A build script reports a failure by panicking.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn require_file(path: &Path) {
    assert!(
        path.is_file(),
        "required native SDK file is missing: {}",
        path.display()
    );
    println!("cargo:rerun-if-changed={}", path.display());
}

fn require_directory(path: &Path) {
    assert!(
        path.is_dir(),
        "required native SDK directory is missing: {}",
        path.display()
    );
}

fn copy_dlls(source_directory: &Path, destination_directory: &Path) {
    require_directory(source_directory);
    fs::create_dir_all(destination_directory).unwrap_or_else(|error| {
        panic!(
            "could not create native DLL staging directory {}: {error}",
            destination_directory.display()
        )
    });

    let mut dlls = fs::read_dir(source_directory)
        .unwrap_or_else(|error| {
            panic!(
                "could not enumerate native DLL directory {}: {error}",
                source_directory.display()
            )
        })
        .map(|entry| {
            entry.unwrap_or_else(|error| {
                panic!(
                    "could not read an entry from {}: {error}",
                    source_directory.display()
                )
            })
        })
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"))
        })
        .collect::<Vec<_>>();
    dlls.sort();
    assert!(
        !dlls.is_empty(),
        "native DLL directory contains no DLLs: {}",
        source_directory.display()
    );

    for source in dlls {
        println!("cargo:rerun-if-changed={}", source.display());
        let file_name = source
            .file_name()
            .expect("enumerated DLL path must have a file name");
        let destination = destination_directory.join(file_name);
        let source_metadata = fs::metadata(&source).unwrap_or_else(|error| {
            panic!("could not inspect native DLL {}: {error}", source.display())
        });
        let up_to_date = fs::metadata(&destination).is_ok_and(|destination_metadata| {
            destination_metadata.len() == source_metadata.len()
                && match (source_metadata.modified(), destination_metadata.modified()) {
                    (Ok(source_modified), Ok(destination_modified)) => {
                        destination_modified >= source_modified
                    }
                    _ => false,
                }
        });
        if !up_to_date {
            fs::copy(&source, &destination).unwrap_or_else(|error| {
                panic!(
                    "could not stage native DLL {} to {}: {error}",
                    source.display(),
                    destination.display()
                )
            });
        }
    }
}

/// The FFmpeg Windows builds link, in the source tree: the audio-only build of FFmpeg
/// c6bb22dea0 (LGPL, five DLLs) that tools/ffmpeg-audio/build.sh makes from FFmpeg's source.
const FFMPEG_VENDORED: &str = "third_party/ffmpeg";
const FFMPEG_COMMIT: &str = "c6bb22dea0";
/// Steinberg's ASIO SDK, from the address asio-sys itself uses (it serves the current SDK).
const ASIO_URL: &str = "https://www.steinberg.net/asiosdk";

/// Where downloads are kept: `<target>/player-core-downloads`, shared by every profile and kept
/// until `cargo clean`.
fn downloads() -> PathBuf {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set for build scripts"));
    out.ancestors()
        .nth(4)
        .expect("OUT_DIR has target/profile/build/package/out shape")
        .join("player-core-downloads")
}

/// A Windows system tool by its full path, so a `tar` or `curl` from another toolset on PATH
/// (Git's, MSYS's) is not taken instead.
fn system_tool(name: &str) -> PathBuf {
    let root = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join(name)
}

fn sha256_of(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes =
        fs::read(path).unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
    Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `url`'s zip unpacked into `<downloads>/<name>` once (checked against `sha256` when given), and
/// that folder. A finished folder carries `.complete`; a half-done one from an interrupted build
/// is made again. Parallel builds each work in their own staging folder and the first to finish
/// keeps its result.
fn fetch_zip(url: &str, sha256: Option<&str>, name: &str) -> PathBuf {
    let cache = downloads();
    let dest = cache.join(name);
    if dest.join(".complete").is_file() {
        return dest;
    }
    fs::create_dir_all(&cache)
        .unwrap_or_else(|error| panic!("could not create {}: {error}", cache.display()));
    let id = std::process::id();
    let zip = cache.join(format!("{name}.{id}.zip"));
    let staging = cache.join(format!("{name}.{id}"));
    println!("cargo:warning=downloading {url}");
    let status = std::process::Command::new(system_tool("curl.exe"))
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&zip)
        .arg(url)
        .status()
        .unwrap_or_else(|error| panic!("could not run curl.exe: {error}"));
    assert!(status.success(), "downloading {url} failed ({status})");
    let got = sha256_of(&zip);
    if let Some(want) = sha256 {
        assert!(got == want, "{url}: sha256 {got}, expected {want}");
    }
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .unwrap_or_else(|error| panic!("could not create {}: {error}", staging.display()));
    let status = std::process::Command::new(system_tool("tar.exe"))
        .arg("-xf")
        .arg(&zip)
        .arg("-C")
        .arg(&staging)
        .status()
        .unwrap_or_else(|error| panic!("could not run tar.exe: {error}"));
    assert!(
        status.success(),
        "unpacking {} failed ({status})",
        zip.display()
    );
    let _ = fs::remove_file(&zip);
    fs::write(staging.join(".complete"), format!("{url}\n{got}\n")).expect("write .complete");
    if fs::rename(&staging, &dest).is_err() {
        // Another build finished first, or a stale half-done folder is in the way.
        if !dest.join(".complete").is_file() {
            let _ = fs::remove_dir_all(&dest);
            fs::rename(&staging, &dest).unwrap_or_else(|error| {
                panic!("could not move {} into place: {error}", dest.display())
            });
        } else {
            let _ = fs::remove_dir_all(&staging);
        }
    }
    dest
}

/// The single folder inside `dir` whose name starts with `prefix`.
fn only_folder(dir: &Path, prefix: &str) -> PathBuf {
    fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.to_ascii_lowercase().starts_with(prefix))
        })
        .unwrap_or_else(|| panic!("no {prefix}* folder in {}", dir.display()))
}

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // The FFmpeg SDK: the vendored audio-only build, or
    // `<NOVA_MEDIA_NATIVE_DIR>/ffmpeg-master-latest-win64-lgpl-shared` when a machine names
    // another build of the same ABI (avcodec-62).
    println!("cargo:rerun-if-env-changed=NOVA_MEDIA_NATIVE_DIR");
    let ffmpeg = match env::var_os("NOVA_MEDIA_NATIVE_DIR") {
        Some(dir) => PathBuf::from(dir).join("ffmpeg-master-latest-win64-lgpl-shared"),
        None => PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
            .join(FFMPEG_VENDORED),
    };
    // Where it is, for the tests that run FFmpeg's own tool as an oracle.
    println!(
        "cargo:rustc-env=PLAYER_CORE_FFMPEG_DIR={}",
        ffmpeg.display()
    );
    let ffmpeg_include = ffmpeg.join("include");

    for required in [
        ffmpeg_include.join("libavformat/avformat.h"),
        ffmpeg_include.join("libavcodec/avcodec.h"),
        ffmpeg_include.join("libavfilter/avfilter.h"),
        ffmpeg_include.join("libswresample/swresample.h"),
        ffmpeg.join("lib/avformat.lib"),
        ffmpeg.join("lib/avcodec.lib"),
        ffmpeg.join("lib/avutil.lib"),
        ffmpeg.join("lib/avfilter.lib"),
        ffmpeg.join("lib/swresample.lib"),
    ] {
        require_file(&required);
    }

    // The authored shim, one translation unit per responsibility. `player_core.h` is the public
    // ABI the Rust side binds; `player_core_internal.h` is what the units share among themselves.
    let native_sources = [
        "src/native/bn_common.c",
        "src/native/bn_demux.c",
        "src/native/bn_http.c",
        "src/native/bn_audio_decoder.c",
        "src/native/bn_upmix.c",
        "src/native/bn_tak_decoder.c",
        "src/native/bn_tags.c",
    ];
    for source in native_sources {
        println!("cargo:rerun-if-changed={source}");
    }
    println!("cargo:rerun-if-changed=src/native/player_core.h");
    println!("cargo:rerun-if-changed=src/native/player_core_internal.h");
    println!("cargo:rerun-if-changed=third_party/tak/tak_deco_lib.h");
    let mut native = cc::Build::new();
    native
        .files(native_sources)
        .include("src/native")
        .include(&ffmpeg_include)
        .warnings(true);
    #[cfg(feature = "instrument")]
    native.define("BN_INSTRUMENT", None);
    native.compile("player_core_native");

    if env::var_os("CARGO_FEATURE_ASIO").is_some() {
        println!("cargo:rerun-if-env-changed=CPAL_ASIO_DIR");
        println!("cargo:rerun-if-changed=src/native/bn_asio.cpp");
        let sdk = env::var_os("CPAL_ASIO_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| only_folder(&fetch_zip(ASIO_URL, None, "asio"), "asio"));
        require_file(&sdk.join("common/asio.h"));
        cc::Build::new()
            .cpp(true)
            .file("src/native/bn_asio.cpp")
            .include(sdk.join("common"))
            .warnings(true)
            .compile("player_core_asio");
    }

    println!(
        "cargo:rustc-link-search=native={}",
        ffmpeg.join("lib").display()
    );
    println!("cargo:rustc-link-lib=dylib=avformat");
    println!("cargo:rustc-link-lib=dylib=avcodec");
    println!("cargo:rustc-link-lib=dylib=avutil");
    println!("cargo:rustc-link-lib=dylib=avfilter");
    println!("cargo:rustc-link-lib=dylib=swresample");

    let output_directory =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set for build scripts"));
    let profile_directory = output_directory
        .ancestors()
        .nth(3)
        .expect("OUT_DIR has target/profile/build/package/out shape");
    for destination in [
        profile_directory.to_path_buf(),
        profile_directory.join("deps"),
    ] {
        copy_dlls(&ffmpeg.join("bin"), &destination);
        // What a package of these DLLs carries as FFmpeg's licence, and where they came from (a
        // release names the matching source from it).
        let _ = fs::copy(
            ffmpeg.join("LICENSE.txt"),
            destination.join("FFmpeg-LICENSE.txt"),
        );
        let origin = match env::var_os("NOVA_MEDIA_NATIVE_DIR") {
            Some(_) => ffmpeg.display().to_string(),
            None => format!("FFmpeg {FFMPEG_COMMIT}, audio-only (Player-Core tools/ffmpeg-audio)"),
        };
        let _ = fs::write(destination.join("FFmpeg-ORIGIN.txt"), origin + "\n");
        // Consumers stage this unchanged file beside their application executable as well.
        copy_dlls(Path::new("third_party/tak"), &destination);
    }
}
