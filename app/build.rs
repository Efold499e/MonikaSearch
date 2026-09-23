fn main() {
    // tauri-build 会校验 bundle.resources 里的路径存在；ctxhost.exe 是本 crate 的
    // 另一个 bin，首次构建时还不存在。这里先放一个占位文件让校验通过，
    // make-dist.ps1 会在真正打包前用新构建出来的产物覆盖它。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries");
    let _ = std::fs::create_dir_all(&dir);
    let placeholder = dir.join("ctxhost.exe");
    if !placeholder.exists() {
        let _ = std::fs::write(&placeholder, []);
    }

    let windows_attrs = tauri_build::WindowsAttributes::new()
        .app_manifest(include_str!("app.manifest"));
    tauri_build::try_build(
        tauri_build::Attributes::new().windows_attributes(windows_attrs),
    )
    .expect("failed to run tauri-build");
}
