use anyhow::{Context as _, ensure};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NpmPlatform {
    pub package: &'static str,
    pub os: &'static str,
    pub cpu: &'static str,
}

#[derive(Serialize)]
struct NativePackage<'a> {
    name: &'a str,
    version: &'a str,
    description: &'a str,
    license: &'a str,
    os: [&'a str; 1],
    cpu: [&'a str; 1],
    files: [&'a str; 1],
}

#[must_use]
pub const fn platform_for_target(target: &str) -> Option<NpmPlatform> {
    match target.as_bytes() {
        b"aarch64-apple-darwin" => Some(NpmPlatform {
            package: "tools-mcp-darwin-arm64",
            os: "darwin",
            cpu: "arm64",
        }),
        b"x86_64-apple-darwin" => Some(NpmPlatform {
            package: "tools-mcp-darwin-x64",
            os: "darwin",
            cpu: "x64",
        }),
        b"x86_64-pc-windows-msvc" => Some(NpmPlatform {
            package: "tools-mcp-win32-x64",
            os: "win32",
            cpu: "x64",
        }),
        _ => None,
    }
}

pub fn run() -> anyhow::Result<()> {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask must be inside the repository")?
        .to_path_buf();
    let release = crate::package::build()?;
    let target = mcp_agent_authority::release::current_release_target()
        .context("native npm packaging requires macOS or Windows")?;
    let platform = platform_for_target(target).context("unsupported native npm target")?;
    let version = env!("CARGO_PKG_VERSION");
    let output = repository.join("target/npm-artifacts");
    fs::create_dir_all(&output).context("create npm artifact directory")?;
    let staging = tempfile::Builder::new()
        .prefix(".tools-mcp-npm-")
        .tempdir_in(repository.join("target"))
        .context("create npm staging directory")?;
    let native = staging.path().join(platform.package);
    fs::create_dir(&native).context("create native npm package")?;
    copy_tree(&release.release_dir, &native.join("release"))?;
    let package = NativePackage {
        name: platform.package,
        version,
        description: "Platform-native tools-mcp local worker",
        license: "Apache-2.0",
        os: [platform.os],
        cpu: [platform.cpu],
        files: ["release"],
    };
    fs::write(
        native.join("package.json"),
        serde_json::to_vec_pretty(&package).context("serialize native npm manifest")?,
    )
    .context("write native npm manifest")?;
    npm_pack(&native, &output)?;
    npm_pack(&repository.join("npm/tools-mcp"), &output)?;
    println!("npm artifacts: {}", output.display());
    Ok(())
}

fn npm_pack(package: &Path, output: &Path) -> anyhow::Result<()> {
    let status = Command::new("npm")
        .args(["pack", "--silent", "--pack-destination"])
        .arg(output)
        .arg(package)
        .status()
        .context("run npm pack")?;
    ensure!(
        status.success(),
        "npm pack failed for {}",
        package.display()
    );
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(destination).context("create npm release directory")?;
    for entry in fs::read_dir(source).context("read native release")? {
        let entry = entry.context("read native release entry")?;
        let target = destination.join(entry.file_name());
        let metadata = entry.metadata().context("read native release metadata")?;
        if metadata.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            ensure!(
                metadata.is_file(),
                "native release contains a non-regular entry"
            );
            fs::copy(entry.path(), &target).context("copy native release artifact")?;
            fs::set_permissions(&target, metadata.permissions())
                .context("preserve native release mode")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{NpmPlatform, platform_for_target};

    #[test]
    fn maps_only_supported_native_release_targets() {
        assert_eq!(
            platform_for_target("aarch64-apple-darwin"),
            Some(NpmPlatform {
                package: "tools-mcp-darwin-arm64",
                os: "darwin",
                cpu: "arm64",
            })
        );
        assert_eq!(
            platform_for_target("x86_64-pc-windows-msvc")
                .expect("Windows package")
                .package,
            "tools-mcp-win32-x64"
        );
        assert_eq!(platform_for_target("x86_64-unknown-linux-gnu"), None);
    }
}
