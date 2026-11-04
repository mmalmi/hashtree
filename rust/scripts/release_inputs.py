#!/usr/bin/env python3
"""Use the reviewed release input lock without resolving mutable toolchains."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import subprocess
import tarfile
import tempfile
from urllib.parse import urlparse
from urllib.request import urlopen


def run(arguments, **kwargs):
    return subprocess.run(
        arguments, check=True, text=True, capture_output=True, **kwargs
    ).stdout.strip()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def append_actions_file(name, lines):
    filename = os.environ.get(name)
    require(filename, f"{name} is required")
    for line in lines:
        require("\n" not in line and "\r" not in line, f"Invalid {name} entry")
    with open(filename, "a", encoding="utf-8", newline="\n") as output:
        output.write("\n".join(lines) + "\n")


def export_environment(values):
    append_actions_file("GITHUB_ENV", [f"{key}={value}" for key, value in values.items()])
    os.environ.update(values)


def checked_download(entry, destination, expected_filename):
    parsed = urlparse(entry["url"])
    require(parsed.scheme == "https" and parsed.hostname == "static.rust-lang.org",
            "Rust archive must use the official HTTPS distribution host")
    require(not parsed.username and not parsed.password and not parsed.query and not parsed.fragment,
            "Unexpected Rust distribution URL fields")
    require(PurePosixPath(parsed.path).name == expected_filename,
            "Rust archive filename does not match its locked version and target")
    require(re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]), "Invalid SHA-256 lock")
    digest = hashlib.sha256()
    with urlopen(entry["url"], timeout=120) as response, destination.open("xb") as output:
        while chunk := response.read(1024 * 1024):
            digest.update(chunk)
            output.write(chunk)
    require(digest.hexdigest() == entry["sha256"], f"SHA-256 mismatch for {expected_filename}")


def checked_extract(archive, destination, expected_root):
    # The archive hash is checked before examining or running its installer.
    require(hasattr(tarfile, "data_filter"), "Python with tarfile.data_filter is required")
    with tarfile.open(archive, "r:xz") as bundle:
        members = bundle.getmembers()
        for member in members:
            path = PurePosixPath(member.name)
            require(not path.is_absolute() and ".." not in path.parts and "\\" not in member.name,
                    "Unsafe archive member path")
            require(path.parts and path.parts[0] == expected_root,
                    "Archive member is outside its expected release directory")
            require(member.isfile() or member.isdir() or member.issym() or member.islnk(),
                    "Unsupported archive member type")
            if member.issym() or member.islnk():
                require(not PurePosixPath(member.linkname).is_absolute() and "\\" not in member.linkname,
                        "Unsafe archive link")
                target = path.parent / member.linkname if member.issym() else PurePosixPath(member.linkname)
                resolved = []
                for part in target.parts:
                    if part == "..":
                        require(len(resolved) > 1, "Archive link escapes its release directory")
                        resolved.pop()
                    elif part != ".":
                        resolved.append(part)
                require(resolved and resolved[0] == expected_root,
                        "Archive link escapes its release directory")
        bundle.extractall(destination, members=members, filter="data")
    installer = destination / expected_root / "install.sh"
    require(installer.is_file() and not installer.is_symlink(), "Missing regular Rust installer")
    return installer.parent


def configure_macos(sdk):
    app = Path(sdk["xcode_app"])
    require(app.is_dir() and str(app).startswith("/Applications/Xcode_") and app.suffix == ".app",
            "The locked Xcode application is unavailable")
    developer = app / "Contents" / "Developer"
    environment = dict(os.environ, DEVELOPER_DIR=str(developer))
    expected = f"Xcode {sdk['xcode_version']}\nBuild version {sdk['xcode_build']}"
    require(run(["xcodebuild", "-version"], env=environment) == expected, "Xcode version changed")
    require(run(["xcrun", "--sdk", "macosx", "--show-sdk-version"], env=environment) == sdk["sdk_version"],
            "macOS SDK version changed")
    require(run(["sw_vers", "-productVersion"]) == sdk["macos_version"], "macOS runner version changed")
    require(run(["sw_vers", "-buildVersion"]) == sdk["macos_build"], "macOS runner build changed")
    export_environment({"DEVELOPER_DIR": str(developer)})


def configure_windows(sdk):
    for key in ("msvc_tools_version", "windows_sdk_version", "visual_studio_version"):
        require(re.fullmatch(r"[0-9.]+", sdk[key]), f"Invalid {key}")
    require(os.environ.get("ImageVersion") == sdk["image_version"], "Windows runner image changed")
    program_files = Path(os.environ["ProgramFiles(x86)"])
    vswhere = program_files / "Microsoft Visual Studio" / "Installer" / "vswhere.exe"
    installations = json.loads(run([
        str(vswhere), "-products", "*", "-requires",
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-format", "json",
    ]))
    matching = [item for item in installations if item["installationVersion"] == sdk["visual_studio_version"]]
    require(len(matching) == 1, "The exact Visual Studio installation is unavailable or ambiguous")
    visual_studio = Path(matching[0]["installationPath"])
    require((visual_studio / "VC" / "Tools" / "MSVC" / sdk["msvc_tools_version"]).is_dir(),
            "The exact MSVC toolset is unavailable")
    require((program_files / "Windows Kits" / "10" / "Lib" / sdk["windows_sdk_version"]).is_dir(),
            "The exact Windows SDK is unavailable")
    script = visual_studio / "VC" / "Auxiliary" / "Build" / "vcvarsall.bat"
    require('"' not in str(script), "Unexpected compiler setup path")
    command = (f'call "{script}" amd64 {sdk["windows_sdk_version"]} '
               f'-vcvars_ver={sdk["msvc_tools_version"]} >nul && set')
    configured = dict(line.split("=", 1) for line in run(
        ["cmd.exe", "/d", "/s", "/c", command]
    ).splitlines() if "=" in line and not line.startswith("="))
    by_name = {key.upper(): value for key, value in configured.items()}
    require(by_name.get("VCTOOLSVERSION", "").rstrip("\\/") == sdk["msvc_tools_version"],
            "vcvarsall selected a different MSVC toolset")
    require(by_name.get("WINDOWSSDKVERSION", "").rstrip("\\/") == sdk["windows_sdk_version"],
            "vcvarsall selected a different Windows SDK")
    selected = ("INCLUDE", "LIB", "LIBPATH", "VCINSTALLDIR", "VCTOOLSINSTALLDIR",
                "VCTOOLSVERSION", "VSINSTALLDIR", "WINDOWSSDKDIR", "WINDOWSSDKVERSION",
                "WINDOWSSDKLIBVERSION", "UCRTVERSION", "UNIVERSALCRTSDKDIR")
    export_environment({key: by_name[key] for key in selected if key in by_name})
    old_paths = {item.casefold() for item in os.environ.get("PATH", "").split(os.pathsep)}
    added_paths = [item for item in by_name["PATH"].split(os.pathsep) if item.casefold() not in old_paths]
    append_actions_file("GITHUB_PATH", list(reversed(added_paths)))
    os.environ["PATH"] = by_name["PATH"]


def install_component(entry, archive_name, root_name, components, prefix, temporary):
    archive = temporary / archive_name
    checked_download(entry, archive, archive_name)
    directory = checked_extract(archive, temporary, root_name)
    install_prefix = str(prefix)
    if os.name == "nt":
        install_prefix = run(["bash", "-c", 'cygpath -u "$1"', "release-inputs", str(prefix)])
    subprocess.run([
        "bash", "./install.sh", f"--prefix={install_prefix}",
        f"--components={','.join(components)}", "--disable-ldconfig",
    ], cwd=directory, check=True)


def install_native(lock, host, target):
    supported = ("aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-pc-windows-msvc")
    require(host in supported and target in supported, "Unsupported native release target")
    actual_arch = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
    actual_suffix = "apple-darwin" if platform.system() == "Darwin" else "pc-windows-msvc"
    require(platform.system() in ("Darwin", "Windows") and host == f"{actual_arch}-{actual_suffix}",
            "Native Rust host does not match this runner")
    require(host.endswith("apple-darwin") == target.endswith("apple-darwin"), "Unsupported cross OS target")
    require(os.environ.get("RUNNER_TEMP"), "RUNNER_TEMP is required")
    require(os.environ.get("GITHUB_ENV") and os.environ.get("GITHUB_PATH"), "GitHub environment files are required")
    version = lock["rust_version"]
    require(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version), "Invalid Rust version")
    entry = lock["native"][host]
    if host.endswith("apple-darwin"):
        configure_macos(entry["sdk"])
    else:
        configure_windows(entry["sdk"])
    prefix = Path(os.environ["RUNNER_TEMP"]) / "pinned-rust"
    require(not prefix.exists(), "Pinned Rust prefix already exists; use a fresh runner")
    with tempfile.TemporaryDirectory(prefix="rust-release-inputs-", dir=os.environ["RUNNER_TEMP"]) as staging:
        temporary = Path(staging)
        root = f"rust-{version}-{host}"
        install_component(entry["archive"], root + ".tar.xz", root,
                          ["rustc", "cargo", f"rust-std-{host}"], prefix, temporary)
        if target != host:
            root = f"rust-std-{version}-{target}"
            install_component(entry["target_std"][target], root + ".tar.xz", root,
                              [f"rust-std-{target}"], prefix, temporary)
    suffix = ".exe" if os.name == "nt" else ""
    rustc, cargo = prefix / "bin" / f"rustc{suffix}", prefix / "bin" / f"cargo{suffix}"
    require(run([str(rustc), "--version"]).split()[:2] == ["rustc", version], "Installed Rust version mismatch")
    require(run([str(cargo), "--version"]).split()[:2] == ["cargo", version], "Installed Cargo version mismatch")
    append_actions_file("GITHUB_PATH", [str(prefix / "bin")])
    export_environment({"HASHTREE_PINNED_TOOLCHAIN": "1", "RUSTC": str(rustc),
                        "RUSTDOC": str(prefix / "bin" / f"rustdoc{suffix}")})
    print(f"Installed pinned Rust {version} for {host}, target {target}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=Path(__file__).resolve().parents[1] / "release-inputs.json")
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("linux-image", "linux-packages"):
        commands.add_parser(command).add_argument("--target", required=True)
    native = commands.add_parser("native")
    native.add_argument("--target", required=True)
    args = parser.parse_args()
    lock = json.loads(args.manifest.read_text(encoding="utf-8"))
    if args.command == "native":
        architecture = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
        suffix = "apple-darwin" if platform.system() == "Darwin" else "pc-windows-msvc"
        install_native(lock, f"{architecture}-{suffix}", args.target)
        return
    linux = lock["linux"][args.target]
    require(re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", linux["image"]), "Linux image must be digest pinned")
    if args.command == "linux-image":
        print(linux["image"])
        return
    seen = set()
    for package in linux["packages"]:
        filename = package["filename"]
        url = urlparse(package["url"])
        require(re.fullmatch(r"[A-Za-z0-9_.+\-]+\.apk", filename) and filename not in seen,
                "Invalid or duplicate APK filename")
        require(url.scheme == "https" and url.hostname == "dl-cdn.alpinelinux.org"
                and not url.username and not url.password and not url.query and not url.fragment,
                "Unexpected Alpine package URL")
        require(PurePosixPath(url.path).name == filename, "APK URL and filename differ")
        require(re.fullmatch(r"[0-9a-f]{64}", package["sha256"]), "Invalid APK SHA-256")
        require(not re.search(r"\s", package["url"]), "Whitespace in Alpine package URL")
        print(f"{package['sha256']} {filename} {package['url']}")
        seen.add(filename)


if __name__ == "__main__":
    main()
