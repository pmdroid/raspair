use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn collect(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, base, out);
        } else {
            let rel = path.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
            out.push((format!("/{rel}"), path.canonicalize().unwrap()));
        }
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let release = env::var("RASPAIR_RELEASE").is_ok_and(|v| v == "1");
    println!("cargo:rerun-if-env-changed=RASPAIR_RELEASE");
    println!("cargo:rerun-if-env-changed=RASPAIR_WEB_DIST");
    println!("cargo:rerun-if-env-changed=RASPAIR_LICENSES");

    let web = env::var_os("RASPAIR_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../web/dist"));
    let watched = if web.exists() { web.clone() } else { web.parent().map(Path::to_path_buf).unwrap_or_default() };
    println!("cargo:rerun-if-changed={}", watched.display());
    let mut files = Vec::new();
    if web.join("index.html").is_file() {
        collect(&web, &web, &mut files);
    } else if release {
        panic!("RASPAIR_RELEASE=1 requires built web assets at {}", web.display());
    }
    let mut code = String::from("pub static WEB_ASSETS: &[(&str, &[u8])] = &[\n");
    for (route, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        writeln!(code, "    ({route:?}, include_bytes!({:?})),", path.display().to_string()).unwrap();
    }
    code.push_str("];\n");

    let licenses = match env::var_os("RASPAIR_LICENSES") {
        Some(p) => PathBuf::from(p).canonicalize().unwrap(),
        None if release => panic!("RASPAIR_RELEASE=1 requires RASPAIR_LICENSES"),
        None => manifest.join("LICENSES.dev.txt"),
    };
    println!("cargo:rerun-if-changed={}", licenses.display());
    writeln!(code, "pub static LICENSES: &str = include_str!({:?});", licenses.display().to_string()).unwrap();

    fs::write(PathBuf::from(env::var("OUT_DIR").unwrap()).join("embedded.rs"), code).unwrap();
}
