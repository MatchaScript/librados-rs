use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = Path::new(&out_dir);

    // If librados.so does not exist in standard paths, but librados.so.2 exists,
    // create a symlink librados.so -> librados.so.2 in OUT_DIR and add OUT_DIR to link search.
    let candidate_paths = [
        "/lib64/librados.so.2",
        "/usr/lib64/librados.so.2",
        "/usr/lib/x86_64-linux-gnu/librados.so.2",
        "/usr/lib/aarch64-linux-gnu/librados.so.2",
    ];
    let link_target = candidate_paths.iter().find(|p| Path::new(p).exists());

    if let Some(target) = link_target {
        let symlink_path = out_path.join("librados.so");
        let _ = fs::remove_file(&symlink_path);
        let _ = symlink(target, &symlink_path);
        println!("cargo:rustc-link-search=native={}", out_dir);
    }

    println!("cargo:rustc-link-lib=rados");
}
