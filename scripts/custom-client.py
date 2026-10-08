#!/usr/bin/env python3
"""Build only Windows x64 and Android ARM64/ARMv7; never publish or install.

Run `python scripts/custom-client.py --help` for local commands. The CI workflow
installs the versions in custom-toolchains.json before calling the same commands.
Android builds use release optimization and a development signature unless a
complete signing configuration is supplied. Windows bundles are unsigned.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import zipfile


ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "build" / "custom"
VERSIONS = json.loads((ROOT / "scripts/custom-toolchains.json").read_text())
BRIDGE_FILES = (
    "src/bridge_generated.rs", "src/bridge_generated.io.rs",
    "flutter/lib/generated_bridge.dart", "flutter/lib/generated_bridge.freezed.dart",
)
ABIS = {
    "arm64-v8a": ("aarch64-linux-android", "aarch64-linux-android", "android-arm64", 183),
    "armeabi-v7a": ("armv7-linux-androideabi", "arm-linux-androideabi", "android-arm", 40),
}


def run(*args, cwd=ROOT, capture=False):
    print("+ " + " ".join(map(str, args)), flush=True)
    return subprocess.run(list(map(str, args)), cwd=cwd, check=True,
                          text=True, capture_output=capture)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def tool(name):
    found = shutil.which(name)
    require(found, f"Missing tool: {name}. Install the versions in scripts/custom-toolchains.json.")
    return found


def flutter_command():
    return tool("flutter")


def check_versions(target, collect=False):
    errors = []

    def check(description, action):
        try:
            action()
            print(f"OK: {description}")
        except (RuntimeError, OSError, subprocess.CalledProcessError) as exc:
            errors.append(f"{description}: {exc}")

    def rust_check():
        version = run(tool("rustc"), "--version", capture=True).stdout
        require(version.startswith(f"rustc {VERSIONS['rust']} "),
                f"Expected Rust {VERSIONS['rust']}; got {version.strip()}.")
        if target == "windows":
            details = run(tool("rustc"), "-vV", capture=True).stdout
            require("host: x86_64-pc-windows-msvc" in details,
                    "Use the x86_64-pc-windows-msvc host toolchain; GNU/ARM builds are unsupported.")
        tool("cargo")
        tool("rustup")

    def flutter_check():
        version = json.loads(run(flutter_command(), "--version", "--machine", capture=True).stdout)
        expected = VERSIONS["bridge_flutter" if target == "bridge" else "flutter"]
        require(version["frameworkVersion"] == expected,
                f"Expected Flutter {expected}; got {version['frameworkVersion']}.")

    check("Rust toolchain", rust_check)
    check("Flutter SDK", flutter_check)
    check("Git", lambda: tool("git"))
    check("vendored hbb_common", lambda: require(
        (ROOT / "libs/hbb_common/Cargo.toml").is_file(), "Missing libs/hbb_common sources."))
    if target != "bridge":
        def vcpkg_check():
            directory = Path(os.environ.get("VCPKG_ROOT", "__missing__"))
            executable = directory / ("vcpkg.exe" if os.name == "nt" else "vcpkg")
            require(executable.is_file(), "Set VCPKG_ROOT to a bootstrapped vcpkg checkout.")
            actual = run(tool("git"), "-C", directory, "rev-parse", "HEAD", capture=True).stdout.strip()
            require(actual == VERSIONS["vcpkg"], f"Expected vcpkg {VERSIONS['vcpkg']}; got {actual}.")
        check("vcpkg checkout", vcpkg_check)
        for name in BRIDGE_FILES:
            check(name, lambda name=name: require((ROOT / name).is_file(),
                  "Generate/restore bridge artifacts first (custom-client.py bridge)."))
    if target == "windows":
        check("Windows x64 host", lambda: require(os.name == "nt" and
              platform.machine().lower() in ("amd64", "x86_64"), "Use an x64 Windows build host."))
        check("CMake", lambda: tool("cmake"))
        check("MSBuild with Visual C++ desktop workload", lambda: tool("msbuild"))
        check("Clang / libclang", lambda: tool("clang"))
    elif target == "android":
        check("Linux x64 host", lambda: require(sys.platform == "linux" and
              platform.machine() == "x86_64", "Android native dependency scripts require Linux x64 (CI or WSL)."))
        check("Bash", lambda: tool("bash"))

        def java_check():
            result = run(tool("java"), "-version", capture=True)
            require(re.search(r'version "17[.\"]', result.stderr + result.stdout), "Use JDK 17.")
        check("JDK 17", java_check)

        def ndk_check():
            ndk = Path(os.environ.get("ANDROID_NDK_HOME", "__missing__"))
            props = ndk / "source.properties"
            require(props.is_file(), "Set ANDROID_NDK_HOME to NDK r28c.")
            require(f"Pkg.Revision = {VERSIONS['ndk_revision']}" in props.read_text(), "Use NDK r28c.")
        check("Android NDK", ndk_check)
        check("Android SDK", lambda: require(
            Path(os.environ.get("ANDROID_HOME", os.environ.get("ANDROID_SDK_ROOT", "__missing__"))).is_dir(),
            "Set ANDROID_HOME or ANDROID_SDK_ROOT."))
    if errors:
        for error in errors:
            print("ERROR: " + error, file=sys.stderr)
        if not collect:
            raise RuntimeError(f"Preflight failed ({len(errors)} checks).")
    return not errors


def package_version():
    match = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.M)
    require(match, "Missing package version in Cargo.toml.")
    return match.group(1)


def write_checksums(directory):
    lines = []
    for path in sorted(directory.iterdir()):
        if path.is_file() and path.name != "SHA256SUMS.txt":
            with path.open("rb") as file:
                digest = hashlib.file_digest(file, "sha256").hexdigest()
            lines.append(f"{digest}  {path.name}\n")
    (directory / "SHA256SUMS.txt").write_text("".join(lines))


def notes(directory, target, signing):
    shutil.copyfile(ROOT / "LICENCE", directory / "LICENCE")
    information = {
        "package": "RustDesk custom client", "version": package_version(),
        "git_commit": run(tool("git"), "rev-parse", "HEAD", capture=True).stdout.strip(),
        "working_tree_dirty": bool(run(tool("git"), "status", "--porcelain", capture=True).stdout.strip()),
        "target": target, "signing": signing, "toolchains": VERSIONS,
        "source_artifact": "custom-client-source (same workflow run)",
    }
    (directory / "BUILD-INFO.json").write_text(json.dumps(information, indent=2) + "\n")
    (directory / "BUILD-NOTES.txt").write_text(
        "Modified RustDesk client. Preserve the bundled AGPL-3.0 LICENCE and notices.\n"
        "Corresponding source and locked build inputs: custom-client-source artifact from the same run.\n"
        "Dependencies retain their upstream licenses; binary bundles retain Flutter's license asset.\n"
        f"Target: {target}. Signature: {signing}.\n"
        "Windows: extract the entire ZIP and run rustdesk.exe on Windows 10/11 x64.\n"
        "TLS 1.3 requires OS Schannel support; TLS 1.2 compatibility remains enabled.\n"
        "This is a Flutter application bundle, not a standalone EXE or MSI. No Authenticode signing.\n"
        "Android development signatures are for acceptance testing; CI-generated keys vary by run.\n"
        "A persistent private signing key is required for install-over upgrades and distribution.\n"
        "Windows uses the fixed official Flutter engine. Optional upstream external virtual display\n"
        "and printer driver downloads are omitted; this artifact does not include those drivers.\n"
        "Build/structure checks do not establish device, network, or firewall acceptance.\n",
        encoding="utf-8")


def patch_flutter():
    root = Path(flutter_command()).resolve().parent.parent
    patch = ROOT / ".github/patches/flutter_3.24.4_dropdown_menu_enableFilter.diff"
    result = subprocess.run([tool("git"), "apply", "--reverse", "--check", str(patch)],
                            cwd=root, capture_output=True)
    if result.returncode == 0:
        print("Flutter dropdown filter patch is already applied.")
    else:
        run(tool("git"), "apply", "--check", patch, cwd=root)
        run(tool("git"), "apply", patch, cwd=root)


def bridge():
    check_versions("bridge")
    run(tool("cargo"), "install", "cargo-expand", "--version", VERSIONS["cargo_expand"], "--locked")
    run(tool("cargo"), "install", "flutter_rust_bridge_codegen", "--version",
        VERSIONS["flutter_rust_bridge"], "--features", "uuid", "--locked")
    pubspec = ROOT / "flutter/pubspec.yaml"
    lock = ROOT / "flutter/pubspec.lock"
    original_pubspec, original_lock = pubspec.read_bytes(), lock.read_bytes()
    try:
        # The default upstream bridge is generated with 3.22.3; extended_text 14 needs newer Dart.
        pubspec.write_bytes(original_pubspec.replace(b"extended_text: 14.0.0", b"extended_text: 13.0.0"))
        run(flutter_command(), "pub", "get", cwd=ROOT / "flutter")
        OUTPUT.mkdir(parents=True, exist_ok=True)
        run(tool("flutter_rust_bridge_codegen"), "--rust-input", "./src/flutter_ffi.rs",
            "--dart-output", "./flutter/lib/generated_bridge.dart",
            "--c-output", OUTPUT / "bridge_generated.h")
    finally:
        pubspec.write_bytes(original_pubspec)
        lock.write_bytes(original_lock)
    for name in BRIDGE_FILES:
        require((ROOT / name).is_file(), f"Bridge generation did not produce {name}.")


def verify_windows(directory):
    for name in ("rustdesk.exe", "librustdesk.dll", "flutter_windows.dll", "dylib_virtual_display.dll",
                 "WindowInjection.dll", "data/icudtl.dat", "data/flutter_assets/AssetManifest.bin"):
        require((directory / name).is_file(), f"Incomplete Windows Flutter bundle: {name}.")
    for path in (directory / "rustdesk.exe", directory / "librustdesk.dll"):
        with path.open("rb") as file:
            require(file.read(2) == b"MZ", f"Not a PE binary: {path.name}")
            file.seek(0x3C)
            offset = struct.unpack("<I", file.read(4))[0]
            file.seek(offset)
            require(file.read(4) == b"PE\0\0" and struct.unpack("<H", file.read(2))[0] == 0x8664,
                    f"Expected x64 PE: {path.name}")
    print("Verified Windows x64 PE binaries and Flutter bundle structure.")


def verify_apk(path, abi):
    with zipfile.ZipFile(path) as apk:
        require(apk.testzip() is None, "APK ZIP integrity check failed.")
        names = set(apk.namelist())
        require("AndroidManifest.xml" in names and "classes.dex" in names, "APK lacks manifest or code.")
        expected = (f"lib/{abi}/librustdesk.so", f"lib/{abi}/libflutter.so",
                    f"lib/{abi}/libapp.so", f"lib/{abi}/libc++_shared.so")
        require(all(name in names for name in expected), f"APK lacks native libraries for {abi}.")
        packaged_abis = {name.split("/")[1] for name in names if name.startswith("lib/")}
        require(packaged_abis == {abi}, f"Unexpected packaged ABIs: {packaged_abis}.")
        binary = apk.read(expected[0])[:20]
        require(binary[:4] == b"\x7fELF" and struct.unpack("<H", binary[18:20])[0] == ABIS[abi][3],
                f"Wrong Rust library ELF architecture for {abi}.")
    sdk = Path(os.environ.get("ANDROID_HOME", os.environ.get("ANDROID_SDK_ROOT", "__missing__")))
    sign_tools = sorted((sdk / "build-tools").glob("*/apksigner"))
    require(sign_tools, "Missing Android SDK build-tools apksigner; cannot verify APK signature.")
    run(sign_tools[-1], "verify", "--verbose", "--print-certs", path)
    print(f"Verified APK signature, ZIP integrity, native libraries and {abi} ELF architecture.")


def windows():
    check_versions("windows")
    patch_flutter()
    os.environ.setdefault("LIBCLANG_PATH", str(Path(tool("clang")).parent))
    vcpkg = Path(os.environ["VCPKG_ROOT"])
    os.environ["VCPKG_DEFAULT_HOST_TRIPLET"] = "x64-windows-static"
    run(vcpkg / "vcpkg.exe", "install", "--triplet", "x64-windows-static",
        f"--x-install-root={vcpkg / 'installed'}")
    run(sys.executable, ROOT / "build.py", "--portable", "--flutter", "--skip-portable-pack", "--hwcodec", "--vram")
    bundle = ROOT / "flutter/build/windows/x64/runner/Release"
    OUTPUT.mkdir(parents=True, exist_ok=True)
    extra = OUTPUT / "topmost-window-source"
    if not extra.exists():
        run(tool("git"), "clone", "https://github.com/rustdesk-org/RustDeskTempTopMostWindow", extra)
    run(tool("git"), "checkout", "--detach", VERSIONS["topmost_window"], cwd=extra)
    run(tool("msbuild"), "WindowInjection/WindowInjection.vcxproj", "-p:Configuration=Release",
        "-p:Platform=x64", "/p:TargetVersion=Windows10", cwd=extra)
    shutil.copyfile(extra / "WindowInjection/x64/Release/WindowInjection.dll", bundle / "WindowInjection.dll")
    verify_windows(bundle)
    directory = OUTPUT / "windows-x64"
    directory.mkdir(parents=True, exist_ok=True)
    shutil.make_archive(str(directory / f"rustdesk-{package_version()}-windows-x64-unsigned"), "zip", bundle)
    # Include the corresponding source for the additional bundled DLL alongside the main source artifact.
    run(tool("git"), "archive", "--format=tar.gz", "-o", directory / "topmost-window-source.tar.gz",
        VERSIONS["topmost_window"], cwd=extra)
    notes(directory, "windows-x64", "unsigned")
    write_checksums(directory)


def android(abis):
    check_versions("android")
    patch_flutter()
    run(tool("cargo"), "install", "cargo-ndk", "--version", VERSIONS["cargo_ndk"], "--locked")
    ndk = Path(os.environ["ANDROID_NDK_HOME"])
    os.environ["ANDROID_NDK_ROOT"] = str(ndk)
    gradle = ROOT / "flutter/android/app/build.gradle"
    properties = ROOT / "flutter/android/key.properties"
    original_gradle = gradle.read_bytes()
    original_properties = properties.read_bytes() if properties.exists() else None
    key_path = OUTPUT / "private-android-signing.jks"
    key_created = False
    signing = "development signed (release optimized)"
    secret_names = ("ANDROID_SIGNING_KEY", "ANDROID_ALIAS", "ANDROID_KEY_STORE_PASSWORD", "ANDROID_KEY_PASSWORD")
    values = [os.environ.get(name, "") for name in secret_names]
    try:
        if any(values):
            require(all(values), "Provide all four ANDROID signing secrets, or none for a development package.")
            require(not key_path.exists(), "Temporary signing key already exists; remove it after reviewing its origin.")
            OUTPUT.mkdir(parents=True, exist_ok=True)
            key_path.write_bytes(base64.b64decode(values[0], validate=True))
            key_created = True
            key_path.chmod(0o600)
            def escape(value):
                return value.replace("\\", "\\\\").replace("\n", "\\n").replace("\r", "\\r")
            properties.write_text(f"storeFile={key_path.as_posix()}\nkeyAlias={escape(values[1])}\n"
                                  f"storePassword={escape(values[2])}\nkeyPassword={escape(values[3])}\n")
            properties.chmod(0o600)
            signing = "private release key"
        elif properties.exists():
            signing = "local release key (flutter/android/key.properties)"
        else:
            gradle.write_bytes(original_gradle.replace(b"signingConfig signingConfigs.release",
                                                       b"signingConfig signingConfigs.debug"))
        for abi in abis:
            target, ndk_target, flutter_target, _ = ABIS[abi]
            run(tool("rustup"), "target", "add", target)
            vcpkg = Path(os.environ["VCPKG_ROOT"])
            triplet = "arm64-android" if abi == "arm64-v8a" else "arm-neon-android"
            run(vcpkg / "vcpkg", "install", "--triplet", triplet,
                f"--x-install-root={vcpkg / 'installed'}")
            if abi == "armeabi-v7a":
                # Rust's vcpkg crate expects arm-android. Keep the original
                # installation intact so repeated/cached vcpkg builds remain valid.
                shutil.copytree(vcpkg / "installed/arm-neon-android", vcpkg / "installed/arm-android",
                                dirs_exist_ok=True)
            run(tool("cargo"), "ndk", "--platform", "21", "--target", target,
                "build", "--locked", "--release", "--features", "flutter,hwcodec", "--lib")
            libraries = ROOT / f"flutter/android/app/src/main/jniLibs/{abi}"
            libraries.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / f"target/{target}/release/liblibrustdesk.so", libraries / "librustdesk.so")
            shutil.copyfile(ndk / f"toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/{ndk_target}/libc++_shared.so",
                            libraries / "libc++_shared.so")
            run(flutter_command(), "build", "apk", "--release", "--target-platform", flutter_target,
                "--split-per-abi", cwd=ROOT / "flutter")
            apk = ROOT / f"flutter/build/app/outputs/flutter-apk/app-{abi}-release.apk"
            verify_apk(apk, abi)
            directory = OUTPUT / f"android-{abi}"
            directory.mkdir(parents=True, exist_ok=True)
            suffix = "dev-signed" if signing.startswith("development") else "release-signed"
            shutil.copyfile(apk, directory / f"rustdesk-{package_version()}-{abi}-{suffix}.apk")
            notes(directory, f"android-{abi}", signing)
            write_checksums(directory)
    finally:
        gradle.write_bytes(original_gradle)
        if original_properties is None:
            properties.unlink(missing_ok=True)
        else:
            properties.write_bytes(original_properties)
        if key_created:
            key_path.unlink(missing_ok=True)


def source():
    directory = OUTPUT / "source"
    directory.mkdir(parents=True, exist_ok=True)
    result = subprocess.run([tool("git"), "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
                            cwd=ROOT, check=True, capture_output=True)
    names = set(result.stdout.decode().split("\0")) - {""}
    names.update(name for name in BRIDGE_FILES if (ROOT / name).is_file())
    archive = directory / f"rustdesk-{package_version()}-custom-source.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        for name in sorted(names):
            path = ROOT / name
            if path.name == "key.properties" or path.suffix.lower() in (".jks", ".keystore", ".p12", ".pfx"):
                continue
            require(not path.is_symlink(), f"Review source symlink before packaging: {name}.")
            if path.is_file():
                tar.add(path, arcname="rustdesk-custom/" + name, recursive=False)
    notes(directory, "corresponding-source", "not applicable")
    write_checksums(directory)
    print(f"Packaged current source, licenses, lockfiles and any generated bridge files: {archive}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    config = commands.add_parser("config", help="Print or export pinned CI toolchain versions")
    config.add_argument("--github-env", action="store_true")
    config.add_argument("--bridge", action="store_true")
    doctor = commands.add_parser("doctor", help="Report all local build prerequisites")
    doctor.add_argument("target", choices=("bridge", "windows", "android"))
    commands.add_parser("bridge", help="Generate the shared Dart/Rust bindings")
    commands.add_parser("windows", help="Build and verify Windows x64 unsigned Flutter ZIP")
    android_parser = commands.add_parser("android", help="Build and verify ARM64 + ARMv7 APKs on Linux")
    android_parser.add_argument("--abi", choices=tuple(ABIS), action="append")
    commands.add_parser("source", help="Archive current Git source and generated bridge with licenses")
    verify = commands.add_parser("verify", help="Validate an existing bundle or signed APK")
    verify.add_argument("target", choices=("windows", *ABIS))
    verify.add_argument("path", type=Path)
    args = parser.parse_args()
    if args.command == "config":
        exported = {"CLIENT_" + key.upper() + "_VERSION": value for key, value in VERSIONS.items()}
        if args.bridge:
            exported["CLIENT_FLUTTER_VERSION"] = VERSIONS["bridge_flutter"]
        if args.github_env:
            with Path(os.environ["GITHUB_ENV"]).open("a") as file:
                file.writelines(f"{key}={value}\n" for key, value in exported.items())
        else:
            print(json.dumps(exported, indent=2))
    elif args.command == "doctor":
        return 0 if check_versions(args.target, collect=True) else 1
    elif args.command == "android":
        android(args.abi or tuple(ABIS))
    elif args.command == "verify":
        if args.target == "windows":
            verify_windows(args.path.resolve())
        else:
            verify_apk(args.path.resolve(), args.target)
    else:
        globals()[args.command]()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(1)
