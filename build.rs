fn main() {
    #[cfg(windows)]
    {
        let _ = embed_resource::compile("app.rc", embed_resource::NONE);
    }
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=icons/Icon_2_48_48_1.ico");
}
