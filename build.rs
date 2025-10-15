fn main() {
    // Embed the icon for the Windows executable. Cargo will run this before building.
    if cfg!(target_os = "windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("src/icon.ico");
        match res.compile() {
            Ok(_) => println!("cargo:warning=Embedded icon.ico into exe"),
            Err(e) => println!("cargo:warning=Failed to embed icon.ico: {}", e),
        }
    }
}
