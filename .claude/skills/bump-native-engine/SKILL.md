---
name: bump-native-engine
description: Update a pinned upstream component of the image (llama.cpp, parakeet.cpp, audio.cpp, SearXNG, the Rust toolchain, cargo-chef) and check that nothing broke. Use it for version bumps and for Dependabot pull requests.
---

# Bump a pinned component

All versions are in `docker-bake.hcl`. The Dockerfile repeats them as `ARG` defaults, and `docs/configuration.md` lists them. Change all three places.

The workflow "Bump pins" does this for llama.cpp, audio.cpp, parakeet.cpp and SearXNG every Monday (`scripts/bump-pins.sh`). It opens one pull request on the branch `bump/pins` that changes all outdated pins together, because the image contains them together. It starts CI on the branch and test-builds the image. `scripts/bump-pins.sh --check` lists the outdated pins locally. For such a pull request, do the steps of each changed component below, then merge. If one component breaks the build, fix the cause on `main` first (for example in `src/llm/template.rs`) and run the workflow again, because each run builds the branch anew from `main`. The Rust toolchain and cargo-chef are bumped by hand.

## llama.cpp (`LLAMACPP_BUILD`)

1. Read the release notes between the old and the new build for changes to `llama-server` flags, the Ling 3.0 parser and the chat templates. `src/llm/template.rs` patches one line of the Ling, LFM3 and MiniCPM5 templates. A warning in the log ("changed upstream") means that the patch needs a new line to find.
2. `./scripts/fetch-llama-headers.sh`, then `cargo build`. bindgen generates the bindings from the new headers. A changed struct fails the build through the layout checks, a removed function fails at runtime with a missing symbol (`dynamic_link_require_all`).
3. Run the embedder test with the new release (`EMBED_TEST_LIB`, `EMBED_TEST_MODEL`, `cargo test real_model -- --ignored --nocapture`) and the evaluation (`src/media/eval.rs`) if you have a Jellyfin library.
4. After the image build: `assistant-hub probe llm "Turn on the kitchen light."` must show a tool call, and `llama-server --list-devices` must still print the GPU (STT and TTS choose their device from it).

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

## Rust and cargo-chef (`RUST_VERSION`, `CARGO_CHEF_VERSION`)

1. Change `rust-toolchain.toml` and both variables in `docker-bake.hcl`. The cargo-chef image tag must exist: `<CARGO_CHEF_VERSION>-rust-<RUST_VERSION>-slim-trixie` on Docker Hub.
2. Fix new clippy warnings. Do not silence them without a reason in a comment.

## Every bump

1. `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, `cargo deny check all`.
2. Wait for the workflow "Image", then test the image without a GPU (see the skill add-hub-feature, step 9).
3. Update the version in `docs/configuration.md` and, if behavior changed, the page of the feature.
