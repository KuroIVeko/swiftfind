//! Embeds the icon and version information into the Windows executable.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = dir.join("assets").join("swiftfind.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed=build.rs");

    let version = env!("CARGO_PKG_VERSION");
    let mut parts: Vec<u16> = version.split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    let num = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    // Icon ID 1 is what the app loads at runtime (tray::icon) and what
    // Explorer shows for the file.
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {num}
PRODUCTVERSION {num}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "SwiftFind"
      VALUE "ProductName", "SwiftFind"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "swiftfind.exe"
      VALUE "InternalName", "swiftfind"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = icon.display().to_string().replace('\\', "\\\\"),
    );
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("swiftfind.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().unwrap();
}
