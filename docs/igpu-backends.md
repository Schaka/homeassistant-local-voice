# Intel iGPU: Vulkan, SYCL and OpenVINO compared

The image runs the LLM with the Vulkan build of llama.cpp. llama.cpp has two more backends for Intel GPUs: SYCL and OpenVINO. A backend is the part of llama.cpp that runs the model on one type of device. This page compares the three backends on the same Intel iGPU with the same models.

Result: Vulkan stays the backend of the image. Turn off flash attention for every GPU (`LLM_EXTRA_ARGS=-fa off`). Flash attention is a faster attention method, but on the Intel iGPU it makes the prefill of Ling-3.0-tiny 3.7 times slower.

## Test setup

| Item | Value |
|---|---|
| Date | 2026-10-31 |
| iGPU | Arrow Lake-S iGPU of a Core Ultra 7 270K (64 EU, Xe-LPG, no matrix cores), kernel driver i915 |
| Memory | DDR5-6000, two channels |
| CPU reference | 6 threads on 6 P-cores (`taskset -c 0-5`, `-t 6`) |
| llama.cpp | b11966 |
| Vulkan | Prebuilt release `ubuntu-vulkan-x64`, Mesa ANV 26.2 of the host |
| SYCL | Own build from source (see [SYCL](#sycl)), oneAPI 2026.2, Intel compute runtime 26.31 |
| OpenVINO | Prebuilt release `ubuntu-openvino-2026.4.1-x64`, Intel compute runtime 26.31 |
| Models | Q4_K_M quants of Ling-3.0-tiny, Granite 4.0 H Tiny, Granite 4.0 H Micro and MiniCPM5-2B |

The server of the owner has an i5-12400T with a UHD 730 iGPU (24 EU, Xe-LP). That iGPU was not available for the test. The comparison uses the relative speed of the backends on one iGPU. The absolute values on a UHD 730 are lower.

Power was not measured, because the power counters (RAPL, i915 PMU) need root rights. The CPU load is the substitute.

## Values in the tables

| Column | Meaning |
|---|---|
| Prefill | Speed for a prompt of 4,096 tokens, in tokens/s. Like the first request with an empty prompt cache. |
| Gen. | Speed for 128 new tokens with an empty context, in tokens/s |
| Gen. 4k | Speed for 128 new tokens after 4,096 tokens of context, in tokens/s. Like a normal request with a cached Home Assistant prompt. |
| CPU | CPU load during the prefill test, in cores. One core is 1.0. |

For Home Assistant, "Gen. 4k" is the most important value. The prompt cache keeps the long prompt, so most requests prefill only a few new tokens. Prefill speed is important after a restart without a slot file. It is also important after a change of the prompt, the entities or the tools, and for long tool results such as web search results.

## Results

An empty cell was not measured. "Fails" means that llama.cpp stopped with an error.

| Model | Backend | Prefill | Gen. | Gen. 4k | CPU |
|---|---|---|---|---|---|
| Ling-3.0-tiny | CPU, 6 P-cores | 248 | 42.7 | 34.3 | 4.6 |
| Ling-3.0-tiny | Vulkan, flash attention auto (on) | 79 | 21.3 | 19.2 | 0.36 |
| Ling-3.0-tiny | Vulkan, `-fa off` | 294 | | 16.7 | |
| Ling-3.0-tiny | SYCL fp16 | 51 | 28.4 | 9.2 | 1.0 |
| Ling-3.0-tiny | SYCL fp32 | 67 | 30.8 | 11.3 | 1.0 |
| Ling-3.0-tiny | SYCL fp32, `-fa off` | 275 | | 8.2 | |
| Ling-3.0-tiny | OpenVINO | Fails | | | |
| Granite 4.0 H Tiny | CPU, 6 P-cores | 252 | 33.9 | 35.5 | 4.9 |
| Granite 4.0 H Tiny | Vulkan | 312 | 19.1 | 19.0 | 0.27 |
| Granite 4.0 H Tiny | SYCL fp16 | 214 | 28.3 | 20.0 | 1.0 |
| Granite 4.0 H Tiny | SYCL fp32 | 228 | 28.8 | 19.7 | 1.0 |
| Granite 4.0 H Tiny | OpenVINO | Fails | | | |
| Granite 4.0 H Micro | CPU, 6 P-cores | 135 | 22.0 | 21.2 | 5.4 |
| Granite 4.0 H Micro | Vulkan | 240 | 21.0 | 20.5 | 0.26 |
| Granite 4.0 H Micro | SYCL fp16 | 212 | 24.8 | 20.8 | 1.0 |
| Granite 4.0 H Micro | SYCL fp32 | 220 | 20.3 | 20.7 | 1.0 |
| Granite 4.0 H Micro | OpenVINO | Fails | | | |
| MiniCPM5-2B | CPU, 6 P-cores | 153 | 39.4 | 23.9 | 5.4 |
| MiniCPM5-2B | Vulkan | 224 | 29.3 | 23.1 | 0.25 |
| MiniCPM5-2B | SYCL fp16 | 295 | 35.0 | 23.7 | 1.0 |
| MiniCPM5-2B | SYCL fp32 | 306 | 27.5 | 22.5 | 1.0 |
| MiniCPM5-2B | OpenVINO, stateless | 352 | 26.9 | 12.9 | 1.1 |
| MiniCPM5-2B | OpenVINO, stateful | 503 | 32.2 | 23.6 | 0.9 |

The same speeds relative to the best Vulkan result (Vulkan = 1.00):

| Model | Best other backend | Prefill | Gen. 4k |
|---|---|---|---|
| Ling-3.0-tiny | SYCL fp32, `-fa off` | 0.94 | 0.49 |
| Granite 4.0 H Tiny | SYCL fp32 | 0.73 | 1.04 |
| Granite 4.0 H Micro | SYCL fp16 | 0.88 | 1.01 |
| MiniCPM5-2B | OpenVINO, stateful | 2.24 | 1.02 |

## Each backend compared with Vulkan

| | Vulkan | SYCL | OpenVINO |
|---|---|---|---|
| Models that run | All four | All four, only with an own build | Only MiniCPM5-2B |
| Prefill | Reference | 0.73 to 0.94 for Ling and Granite, 1.32 for MiniCPM5-2B | 2.24 for MiniCPM5-2B (stateful) |
| Generation, empty context | Reference | 1.2 to 1.5 times faster for all four (best of fp16 and fp32) | 1.10 for MiniCPM5-2B (stateful) |
| Generation, 4k context | Reference | Equal for Granite and MiniCPM5-2B, 0.49 for Ling | Equal (stateful), 0.56 (stateless) |
| CPU load | 0.25 to 0.36 cores | 1.0 core | 0.9 to 1.1 cores |
| Needs in the image | Mesa (already in the image) | oneAPI runtime (about 7 GB), NEO, IGC, `setvars.sh` | Its own OpenVINO libraries (the release is 334 MB), NEO, IGC |
| Device selection | `--device Vulkan0` | `--device SYCL0` | `GGML_OPENVINO_DEVICE=GPU`, not `--device` |

More limits:

1. SYCL fp32 was faster than SYCL fp16 for Ling (prefill 67 against 51, generation at 4k 11.3 against 9.2). For Granite Tiny, the two builds were within 7 % of each other. For Granite Micro and MiniCPM5-2B, fp16 generated faster with an empty context (24.8 against 20.3, 35.0 against 27.5).
2. The SYCL documentation of llama.cpp says that an iGPU with fewer than 80 EU is likely too slow for practical use. The test iGPU has 64 EU. The UHD 730 has 24 EU.
3. In stateful mode, llama-server with OpenVINO supports only one chat session.
4. When OpenVINO loads a model, it converts the Q5_K and Q6_K tensors to Q8_0_C. Then the model uses more memory than the GGUF file.
5. The OpenVINO validation in the llama.cpp documentation used a Lunar Lake iGPU. No result for Xe-LP (UHD 730) is documented.

## Flash attention

llama-server uses `-fa auto` by default. On this iGPU, `auto` turns flash attention on. For Ling-3.0-tiny on Vulkan, this has a large cost:

| Ling-3.0-tiny on Vulkan | Prefill | Gen. 4k |
|---|---|---|
| Flash attention on (`auto`) | 79 | 19.0 |
| Flash attention off (`-fa off`) | 294 | 16.7 |

With `-fa off`, the iGPU prefills Ling faster than 6 P-cores of the CPU (248 tokens/s). Generation with a 4k context is 12 % slower. For Granite 4.0 H Tiny, Granite 4.0 H Micro and MiniCPM5-2B, flash attention on or off gave the same speed (3 % difference or less).

A larger micro-batch (`-ub 1024`, `-ub 2048`) changed the Ling prefill by less than 4 % with flash attention off.

To use the GPU, set:

```bash
-e LLM_DEVICE=Vulkan0 -e LLM_EXTRA_ARGS="-fa off"
```

## SYCL

SYCL runs on Level Zero, the low-level GPU interface of Intel. It needs these parts in the image:

1. The oneAPI runtime (compiler runtime, oneMKL, oneDNN). The runtime image `intel/oneapi-runtime` is about 7 GB.
2. A current Intel compute runtime (NEO) and the graphics compiler IGC. NEO is the user-space GPU driver. The kernel driver comes from the host, but NEO comes from the image, like Mesa for Vulkan.
3. The oneAPI environment. Run `source /opt/intel/oneapi/setvars.sh` before llama-server. Without it, the Level Zero adapter does not load (`libumf.so.1` is missing), and SYCL uses OpenCL.

The NEO 23.43 of Ubuntu 24.04 does not know Arrow Lake. NEO 26.31 of Ubuntu 26.04 works. Current NEO releases support Gen12 (Xe-LP, UHD 730) and newer.

The prebuilt SYCL release of llama.cpp b11966 does not run on this iGPU. Every model stops with this error:

```
error: in kernel 'typeinfo name for void convert_unary_nc_sycl<sycl::_V1::detail::half_impl::half, sycl::_V1::ext::oneapi::bfloat16>(...)': bf conversion instruction not supported!
error: backend compiler failed build.
```

The iGPU has no bf16 conversion instructions. The UHD 730 has none either. The upstream issue is llama.cpp #31029. A build with `-fsycl-device-code-split=per_kernel` runs, because the driver then compiles each kernel only at its first use, and the bf16 kernels are not used. That build has 7,060 device modules. Add `-fsycl-max-parallel-link-jobs=20`, or the device link step takes more than 10 minutes.

SYCL keeps one CPU core at 100 % while the GPU works. Vulkan uses about a quarter of a core.

## OpenVINO

OpenVINO ran only MiniCPM5-2B. In stateful mode (`GGML_OPENVINO_STATEFUL_EXECUTION=1`), it prefilled 2.24 times faster than Vulkan, with the same generation speed at 4k context. In stateless mode (the default), generation at 4k context was only 0.56 times the Vulkan speed.

The other models did not run in any mode:

| Model | Result |
|---|---|
| Ling-3.0-tiny | Graph error: `Non-'-1' output dimensions do not evenly divide the input dimensions`. Stateful mode: `inp_pos not found in cgraph`. `GGML_OPENVINO_MOE_OP=0` did not help. |
| Granite 4.0 H Micro | Stateless: the process hangs with an idle GPU. Stateful: shape error in `transpose_shape_inference.hpp`. |
| Granite 4.0 H Tiny | Stateless: segmentation fault, then `[GPU] ProgramBuilder build failed!`. Stateful: shape error in `transpose_shape_inference.hpp`. |

The OpenVINO documentation of llama.cpp lists only dense models as validated. Ling is a MoE model, and Granite 4.0 H is a hybrid of Mamba-2 and attention layers.

## Decision

1. Vulkan stays the GPU backend of the image. It runs all models, and it uses the least CPU.
2. SYCL is not added. It needs a 7 GB runtime and its own build. It is not faster for Ling with a cached prompt, and it uses one full CPU core.
3. OpenVINO is not added. It runs none of the recommended models.
4. Turn off flash attention on a GPU (`LLM_EXTRA_ARGS=-fa off`).
5. Before you change `LLM_DEVICE` on the server, measure Ling with `-fa off` on the UHD 730. Make sure that it is faster than on the CPU of the i5-12400T.

## Result on the UHD 730 of the server

Measured on 2026-10-31 on the i5-12400T with its UHD 730 (24 EU, Xe-LP), as decision 5 asks. Each run sent the real Home Assistant prompt (about 5,800 tokens, empty prompt cache) and generated 64 tokens. Speeds come from the llama-server timings. Energy comes from the RAPL counter of the CPU package, which includes the iGPU. At idle, the package used about 25 W.

| Model | Device | Prefill | Gen. 6k | Time | Energy | Average power |
|---|---|---|---|---|---|---|
| Ling-3.0-tiny | CPU, 6 threads | 124.6 | 33.1 | 49 s | 2,502 J | 50.9 W |
| Ling-3.0-tiny | Vulkan, flash attention on | 28.5 | 10.5 | 213 s | 3,889 J | 18.3 W |
| Ling-3.0-tiny | Vulkan, `-fa off` | 92.3 | 10.0 | 70 s | 1,481 J | 21.2 W |
| MiniCPM5-2B | CPU, 6 threads | 87.6 | 16.0 | 71 s | 3,693 J | 52.1 W |
| MiniCPM5-2B | Vulkan, flash attention on | 54.2 | 8.0 | 115 s | 2,031 J | 17.7 W |
| MiniCPM5-2B | Vulkan, `-fa off` | 70.6 | 8.6 | 89 s | 1,750 J | 19.6 W |

"Gen. 6k" is the generation speed after the full prompt, in tokens/s.

1. `-fa off` is also right on the UHD 730: the Ling prefill is 3.2 times faster than with flash attention.
2. The UHD 730 generates Ling at 30 % of the CPU speed, and MiniCPM5-2B at 54 %. On the Arrow Lake iGPU, the loss was 12 %.
3. With a cached prompt, a request mostly generates tokens: thinking, a tool call and the answer, about 150 to 400 tokens. On the UHD 730, a Ling answer then takes about 15 to 40 s instead of 5 to 7 s.
4. The CPU uses about 25 W more than at idle, but only for the seconds of an answer. At 50 requests per day, this is about 2 Wh per day.

So the server keeps the LLM on the CPU (`LLM_DEVICE=none`). The iGPU is the better choice on a newer Intel GPU, like the Arrow Lake iGPU above. It also helps on a computer where the CPU is busy with other work.

## Repeat the test

The test used `llama-bench` of the release binaries, two repetitions per value:

```bash
# Prefill of 4096 tokens and 128 new tokens
llama-bench -m <model.gguf> -dev Vulkan1 -ngl 99 -p 4096 -n 128 -r 2
# 128 new tokens after 4096 tokens of context
llama-bench -m <model.gguf> -dev Vulkan1 -ngl 99 -p 0 -n 128 -d 4096 -r 2
# Flash attention off and on in one run
llama-bench -m <model.gguf> -dev Vulkan1 -ngl 99 -p 4096 -n 0 -r 1 -fa 0,1
```

`llama-bench --list-devices` shows the device names. On a system with only one GPU, the iGPU is `Vulkan0`.
