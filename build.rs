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
    println!("cargo:rerun-if-changed=CHANNEL");
    println!("cargo:rerun-if-changed=.git/HEAD");
    // HEAD holds "ref: refs/heads/main" and does not change on a commit, so the
    // branch ref has to be watched too or the build string keeps the commit the
    // crate was last rebuilt for.
    println!("cargo:rerun-if-changed=.git/refs/heads/main");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    // The window shows this. `CHANNEL` is the release line ("Beta 6") and `REV`
    // the build number, both kept in step with VERSION.txt; the short hash goes
    // in the tooltip and identifies a build even between releases. Building from
    // a source zip without .git still works.
    let rev = std::fs::read_to_string("REV")
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let channel = std::fs::read_to_string("CHANNEL")
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let mut parts = Vec::new();
    if let Some(channel) = channel {
        parts.push(channel);
    }
    if let Some(rev) = rev {
        parts.push(format!("rev {rev}"));
    }
    let short = if parts.is_empty() {
        "development build".to_string()
    } else {
        parts.join(" \u{b7} ")
    };
    let mut full = parts;
    if let Some(hash) = git(&["rev-parse", "--short", "HEAD"]) {
        full.push(hash);
    }
    let full = if full.is_empty() {
        short.clone()
    } else {
        full.join(" \u{b7} ")
    };
    println!("cargo:rustc-env=APP_BUILD={short}");
    println!("cargo:rustc-env=APP_BUILD_FULL={full}");
}
