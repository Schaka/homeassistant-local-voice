//! Generates the Rust bindings of llama.cpp's C API (`libllama.so`, used by
//! the media index) from the headers of the llama.cpp build in the image.
//!
//! The headers come from `LLAMA_INCLUDE_DIR`. The image build sets it to the
//! headers of `LLAMACPP_BUILD`, downloaded next to the prebuilt library.
//! Without the variable, the headers are expected in
//! `target/llama.cpp-<LLAMACPP_BUILD>`, where `scripts/fetch-llama-headers.sh`
//! puts them. bindgen also writes layout checks, so a struct that changed
//! between llama.cpp versions fails at compile time, not at runtime.

use std::env;
use std::path::{Path, PathBuf};

const LLAMA_FUNCTIONS: &str = "llama_(backend_init|model_default_params|context_default_params|model_load_from_file|model_free|init_from_model|free|model_get_vocab|tokenize|model_n_embd_out|batch_init|batch_free|decode|encode|get_embeddings_seq|get_memory|memory_clear|pooling_type|n_ctx|log_set|model_has_encoder|model_has_decoder)";
const GGML_FUNCTIONS: &str = "ggml_backend_(load|dev_count|dev_get|dev_type|dev_name)";

/// `LLAMACPP_BUILD` from docker-bake.hcl, the single place of the version.
fn pinned_build(manifest: &Path) -> String {
    let bake = manifest.join("docker-bake.hcl");
    println!("cargo:rerun-if-changed={}", bake.display());
    let text = std::fs::read_to_string(&bake).expect("docker-bake.hcl");
    text.lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("variable \"LLAMACPP_BUILD\" { default = \"")
                .and_then(|r| r.strip_suffix("\" }"))
        })
        .expect("LLAMACPP_BUILD in docker-bake.hcl")
        .to_owned()
}

fn generate(header: &Path, includes: &[PathBuf], library: &str, functions: &str, out: &Path) {
    println!("cargo:rerun-if-changed={}", header.display());
    let mut builder = bindgen::Builder::default()
        .header(header.display().to_string())
        .dynamic_library_name(library)
        .dynamic_link_require_all(true)
        .allowlist_function(functions)
        .generate_comments(false)
        .use_core()
        .merge_extern_blocks(true);
    for dir in includes {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    builder
        .generate()
        .unwrap_or_else(|e| panic!("bindgen failed for {}: {e}", header.display()))
        .write_to_file(out)
        .expect("write bindings");
}

fn main() {
    println!("cargo:rerun-if-env-changed=LLAMA_INCLUDE_DIR");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dir = match env::var_os("LLAMA_INCLUDE_DIR") {
        Some(d) => PathBuf::from(d),
        None => manifest
            .join("target")
            .join(format!("llama.cpp-{}", pinned_build(&manifest))),
    };
    let llama_h = dir.join("include/llama.h");
    if !llama_h.is_file() {
        panic!(
            "llama.cpp headers not found in {}. Run scripts/fetch-llama-headers.sh, or set LLAMA_INCLUDE_DIR.",
            dir.display()
        );
    }
    let includes = [dir.join("include"), dir.join("ggml/include")];
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    generate(&llama_h, &includes, "Llama", LLAMA_FUNCTIONS, &out.join("llama_sys.rs"));
    generate(
        &dir.join("ggml/include/ggml-backend.h"),
        &includes,
        "Ggml",
        GGML_FUNCTIONS,
        &out.join("ggml_sys.rs"),
    );
}
