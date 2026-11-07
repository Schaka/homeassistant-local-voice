---
name: bump-native-engine
description: Use when a pinned upstream version changes. That means the pull request of the workflow Bump pins (branch bump/pins), a Dependabot pull request, a request to update llama.cpp, ik_llama.cpp, parakeet.cpp, audio.cpp, SearXNG, Hush, the Rust toolchain or cargo-chef, an edit of LLAMACPP_BUILD, IKLLAMA_COMMIT, AUDIOCPP_COMMIT, PARAKEET_VERSION, SEARXNG_COMMIT, HUSH_COMMIT, HUSH_MODEL_REVISION, RUST_VERSION or CARGO_CHEF_VERSION in docker-bake.hcl, and a failed Image or CI run right after such a bump. Reads the release notes, checks the code that depends on the component and tests the image.
---

# Bump a pinned component

All versions are in `docker-bake.hcl`. The Dockerfile repeats them as `ARG` defaults, and `docs/configuration.md` lists them. Change all three places.

The workflow "Bump pins" does this for llama.cpp, ik_llama.cpp, audio.cpp, parakeet.cpp, SearXNG and Hush every Monday (`scripts/bump-pins.sh`). It opens one pull request on the branch `bump/pins` that changes all outdated pins together, because the image contains them together. It starts CI on the branch and test-builds the image. `scripts/bump-pins.sh --check` lists the outdated pins locally. For such a pull request, do the steps of each changed component below, then merge. If one component breaks the build, fix the cause on `main` first (for example in `src/llm/template.rs`) and run the workflow again, because each run builds the branch anew from `main`. The Rust toolchain, cargo-chef and the Hush model bundle are bumped by hand.

## llama.cpp (`LLAMACPP_BUILD`)

1. Read the release notes between the old and the new build for changes to `llama-server` flags, the Ling 3.0 parser and the chat templates. `src/llm/template.rs` patches one line of the Ling, LFM3 and MiniCPM5 templates. A warning in the log ("changed upstream") means that the patch needs a new line to find.
2. `./scripts/fetch-llama-headers.sh`, then `cargo build`. bindgen generates the bindings from the new headers. A changed struct fails the build through the layout checks, a removed function fails at runtime with a missing symbol (`dynamic_link_require_all`).
3. Record the answers of the new `llama-server` with `scripts/record-llm-fixtures.sh llamacpp BINARY MODEL`, read the diff of `tests/fixtures/llm/llamacpp/`, and run the contract tests (`tests/llm_contract.rs`). Run `scripts/eval-llm.sh` for the old and the new build, then `assistant-hub eval compare OLD.json NEW.json`.
4. Run the embedder test with the new release (`EMBED_TEST_LIB`, `EMBED_TEST_MODEL`, `cargo test real_model -- --ignored --nocapture`) and the evaluation (`src/media/eval.rs`) if you have a Jellyfin library.
5. After the image build: `assistant-hub probe llm "Turn on the kitchen light."` must show a tool call, and `llama-server --list-devices` must still print the GPU (STT and TTS choose their device from it).

## ik_llama.cpp (`IKLLAMA_COMMIT`)

1. Read the commits between the old and the new pin for changes to the server flags (`--reasoning-budget`, `--ctx-checkpoints-tolerance`, `--slot-save-path`), the slot files, the chat template engine and the parsers of Ling, MiniCPM5 and LFM3. The arguments are in `src/llm/ik.rs`.
2. `scripts/test-engine-contract.sh ik` builds the new commit and runs the contract tests. Then record the answers of the new build with `scripts/record-llm-fixtures.sh ik BINARY MODEL` and read the diff of `tests/fixtures/llm/ik/`.
3. Run `scripts/eval-llm.sh` with `LLM_ENGINE=llamacpp` and with `LLM_ENGINE=ik` for each model in `ik_architectures` (`src/llm/engine.rs`), then `assistant-hub eval compare LLAMACPP.json IK.json`. Take a model off the list if it fails the gate. Check that the tokens read again per question stay near 100.
4. Look at `llama-server --help` of the new build for the flags that `src/llm/ik.rs` uses. A flag that is gone stops the start, and the server then falls back to llama.cpp after two tries.

## parakeet.cpp (`PARAKEET_VERSION`)

1. Check the C API in `include/parakeet_capi.h` of the new release against `src/stt.rs`.
2. Check the device selection (`PARAKEET_DEVICE`, CPU fallback) in its `src/backend.cpp`.
3. After the build: `assistant-hub probe stt` with a WAV file, with and without a GPU.

## audio.cpp (`AUDIOCPP_COMMIT`)

1. The model spec comes from the same commit (`model_specs/pocket_tts.json` in the build stage). Check new or removed options in it.
2. Check the server configuration keys in `app/server/config.cpp` against `tts::child_spec` (`backend`, `device`, `threads`, models).
3. After the build: `assistant-hub probe tts`, with and without a GPU. The first request also loads the model.

## SearXNG (`SEARXNG_COMMIT`)

1. Check `searx/search/__init__.py` and `searx/search/models.py` for changes to `initialize`, `Search` and `SearchQuery`, which the bootstrap in `src/search.rs` calls.
2. After the build: `curl "http://<host>:10300/web/search?q=capital%20of%20australia"` returns results.

## Hush (`HUSH_COMMIT`, `HUSH_MODEL_REVISION`, `HUSH_MODEL_SHA256`)

1. Check the C API in `native/weya_nc_build/weya_nc/include/weya_nc.h` against `src/denoise.rs`, and the frame and delay rules in its `src/lib.rs` (160 samples at 16 kHz, one frame of delay).
2. The library builds with its own `Cargo.lock` (`cargo build --locked` in the stage `hush-builder`). If a new Rust toolchain breaks it, keep `HUSH_COMMIT` and report it upstream.
3. A new model revision: download the bundle, write its SHA-256 into `HUSH_MODEL_SHA256` and run the steps of "Check it on your rooms" in `docs/denoise.md` on recordings with a TV.
4. After the build: start the image with `STT_DENOISE=hush`. The log shows `STT cleanup loaded`, and `/health` shows `"stt_denoise": true`.

## Rust and cargo-chef (`RUST_VERSION`, `CARGO_CHEF_VERSION`)

1. Change `rust-toolchain.toml` and both variables in `docker-bake.hcl`. The cargo-chef image tag must exist: `<CARGO_CHEF_VERSION>-rust-<RUST_VERSION>-slim-trixie` on Docker Hub.
2. Fix new clippy warnings. Do not silence them without a reason in a comment.

## Every bump

1. `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, `cargo deny check all`.
2. Wait for the workflow "Image", then test the image without a GPU (see the skill add-hub-feature, step 9).
3. Update the version in `docs/configuration.md` and, if behavior changed, the page of the feature.
