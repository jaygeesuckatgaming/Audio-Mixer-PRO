fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn main() {
    #[cfg(windows)]
    {
        let _ = embed_resource::compile("app.rc", embed_resource::NONE);
    }
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=icons/Icon_2_48_48_1.ico");
    println!("cargo:rerun-if-changed=REV");
    println!("cargo:rerun-if-changed=.git/HEAD");

    // The window title shows this. `REV` is the release number kept in step with
    // VERSION.txt; the short hash makes a build identifiable even between
    // releases. Building from a source zip without .git still works.
    let rev = std::fs::read_to_string("REV")
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let mut parts = Vec::new();
    if let Some(rev) = rev {
        parts.push(format!("rev {rev}"));
    }
    if let Some(hash) = git(&["rev-parse", "--short", "HEAD"]) {
        parts.push(hash);
    }
    let build = if parts.is_empty() {
        "development build".to_string()
    } else {
        parts.join(" \u{b7} ")
    };
    println!("cargo:rustc-env=APP_BUILD={build}");
}
