fn main() {
    // Required by winfsp-rs: WinFsp is delay-loaded.
    winfsp::build::winfsp_link_delayload();

    println!("cargo:rerun-if-changed=assets/oti_link.ico");

    // Embed the application icon into the Windows EXE as resource ID 1.
    // The tray code loads the same resource, so Explorer/task manager/tray
    // all use one identity. Non-Windows builds skip the resource step.
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() == Some("windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/oti_link.ico");
        res.set("FileDescription", "OTI-Link USB3 PC-to-PC Link");
        res.set("ProductName", "OTI-Link");
        res.set("OriginalFilename", "oti_link_v10.exe");
        // Common Controls v6: modern visual styles for the settings window and dialogs.
        res.set_manifest(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#);
        res.compile().expect("failed to embed OTI-Link icon/resource");
    }
}
