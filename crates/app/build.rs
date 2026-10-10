// Gives the Windows executable the Cuia icon and name (Explorer, taskbar,
// Alt+Tab). Other platforms get their icon from the window or the app bundle.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon/cuia.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon/cuia.ico");
        res.set("ProductName", "Cuia");
        res.set("FileDescription", "Cuia");
        res.compile().expect("embedding the Windows icon");
    }
}
