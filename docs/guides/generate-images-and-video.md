---
title: Generate images & video
description: Have MIRA create images and short video clips from a text prompt, right in the chat — using OpenAI, or a local Stable Diffusion / ComfyUI backend with no cloud key.
sidebar:
  order: 7
---

MIRA can turn a text prompt into an **image** or a **short video clip** and show
the result inline in your conversation. You don't run a separate tool or open
another app — you just ask.

Like [voice](voice-replies.md), image and video generation are **local-first**:
a small router picks a backend for each request. You can point it at a cloud API
(OpenAI Images / Sora) or at a **local generator** running on your own
machine — Stable Diffusion or ComfyUI — that needs **no cloud key at all**.

## Choose a backend

The router resolves a backend per request: an explicit backend you name → the
configured **default backend** → the first enabled one (**local is preferred**).
So once any backend is enabled, `image_generate` and `video_generate` become
available to the agent.

Configure this under **Settings → Image & Video**, or directly in the
`[image]` / `[video]` config blocks (backend selection, endpoints, models — see
[Settings reference](../reference/settings.md)).

**Image backends:**

| Backend | Where it runs | Needs a cloud key? | Notes |
| --- | --- | --- | --- |
| **OpenAI Images** | Cloud (OpenAI or compatible) | Yes | Uses your OpenAI provider key. |
| **AUTOMATIC1111** | Local (SD WebUI / Forge) | No | Talks to `/sdapi/v1/txt2img`; start the WebUI with `--api --listen`. |
| **ComfyUI** | Local | No | Built-in default SDXL workflow, or your own `workflow_json`. |

**Video backends:**

| Backend | Where it runs | Needs a cloud key? | Notes |
| --- | --- | --- | --- |
| **OpenAI Sora** | Cloud | Yes | Async render via the OpenAI Videos API. |
| **ComfyUI** | Local | No | Bring your own Wan / AnimateDiff / SVD workflow (`workflow_json`, required). |

> No cloud key needed for the local backends. Enable AUTOMATIC1111 or ComfyUI in
> **Settings → Image & Video** and MIRA generates entirely on your own hardware.

## Generate an image

Just ask MIRA in chat:

> *"Generate an image of a red bicycle leaning against a stone wall at sunset."*

Behind the scenes MIRA calls `image_generate`, which routes your prompt to
whichever image backend is active — OpenAI, a local Stable Diffusion WebUI, or
ComfyUI. The image **renders inline** in the conversation, with **download** and
**copy** controls so you can save or reuse it.

Describe what you want as specifically as you like — subject, style, lighting,
composition. You can also ask MIRA to refine: *"make it more minimal"* or *"same
scene but in daylight"*. Parameters like the **model**, **size**, and a
**negative prompt** flow through to whichever backend is handling the request.

## Generate a short video

Video works the same way — describe the clip:

> *"Make a 4-second clip of waves rolling onto a beach."*

MIRA calls `video_generate`, routed to OpenAI Sora or a local ComfyUI video
workflow. Video rendering takes a while, so this runs **asynchronously**: MIRA
**enqueues** the job, **polls** until it's done, then returns the finished
**MP4 inline** as a video player with playback controls and download.

You can steer a few parameters by asking for them — the **model**, the **size**
(aspect ratio / resolution), and the **length in seconds**. Keep clips short;
generation time and cost grow with length.

## ComfyUI custom workflows

ComfyUI is driven by a **workflow graph**, so it's the most flexible backend.

- **Image** — MIRA ships a **built-in default SDXL txt2img workflow**, so
  ComfyUI works out of the box once the server is reachable. To use your own
  graph instead, paste an **API-format** workflow into `image.comfyui.workflow_json`.
- **Video** — there's **no universal default** (video graphs vary widely: Wan,
  AnimateDiff, SVD), so `video.comfyui.workflow_json` is **required** — the
  backend stays off until you supply one.

MIRA substitutes tokens into your workflow before submitting it, so the same
graph adapts to each request. Put these tokens where the values belong:

`{{prompt}}` · `{{negative}}` · `{{seed}}` · `{{width}}` · `{{height}}` ·
`{{steps}}` · `{{cfg}}` · `{{ckpt}}` — and for video, `{{frames}}`
(= seconds × fps) and `{{fps}}`.

Export the workflow from ComfyUI in **API format**, drop the tokens into the
nodes you want MIRA to control, and paste it into the matching `workflow_json`
setting.

## Other generators

AUTOMATIC1111 and ComfyUI are **first-class native backends** — you no longer
need an MCP workaround for Stable Diffusion or local models. Anything **beyond**
these backends (a provider MIRA doesn't speak natively) can still come in
through an **[MCP server](add-tools-with-mcp.md)**: MCP tools that return images,
audio, or video also render inline in chat.

## See also

- [Voice replies & talking to MIRA](voice-replies.md) — the same local-first
  router pattern, for speech.
- [Settings reference](../reference/settings.md) — the `[image]` / `[video]`
  config blocks and provider keys.
- [Tools & MCP](../concepts/tools-and-mcp.md) — how MIRA's tools and external
  tool servers work.
- [Add tools with MCP](add-tools-with-mcp.md) — connect a generator MIRA doesn't
  support natively.
