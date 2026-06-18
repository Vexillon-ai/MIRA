---
title: Generate images & video
description: Have MIRA create images and short video clips from a text prompt, right in the chat.
sidebar:
  order: 7
---

MIRA can turn a text prompt into an **image** or a **short video clip** and show
the result inline in your conversation. You don't run a separate tool or open
another app — you just ask.

## Turn it on

Both generators use **OpenAI's APIs**, so they're **off until an OpenAI key is
configured**. An admin adds the key under provider settings (see
[Settings reference](../reference/settings.md)). Once a key is present, the
`image_generate` and `video_generate` tools become available to the agent.

> No OpenAI key, no image/video generation. If you'd rather use a different
> generator, you can reach one through an [MCP server](add-tools-with-mcp.md)
> instead — see [other generators](#other-generators) below.

## Generate an image

Just ask MIRA in chat:

> *"Generate an image of a red bicycle leaning against a stone wall at sunset."*

Behind the scenes MIRA calls `image_generate`, which sends your prompt to the
**OpenAI Images API** (or a compatible endpoint) and gets back a picture. The
image **renders inline** in the conversation, with **download** and **copy**
controls so you can save or reuse it.

Describe what you want as specifically as you like — subject, style, lighting,
composition. You can also ask MIRA to refine: *"make it more minimal"* or *"same
scene but in daylight"*.

## Generate a short video

Video works the same way — describe the clip:

> *"Make a 4-second clip of waves rolling onto a beach."*

MIRA calls `video_generate`, which uses **OpenAI's Videos / Sora API**. Video
rendering takes a while, so this runs **asynchronously**: MIRA **enqueues** the
job, **polls** until it's done, then returns the finished **MP4 inline** as a
video player with playback controls and download.

You can steer a few parameters by asking for them — the **model**, the **size**
(aspect ratio / resolution), and the **length in seconds** are all selectable.
Keep clips short; generation time and cost grow with length.

## Other generators

The built-in tools target OpenAI. To use a different image or video model —
Stable Diffusion, a local generator, or another provider — connect it as an
**[MCP server](add-tools-with-mcp.md)**. MCP tools that return images, audio, or
video also render inline in chat, so the experience is the same: ask, and the
result appears in the conversation.

## See also

- [Tools & MCP](../concepts/tools-and-mcp.md) — how MIRA's tools and external
  tool servers work.
- [Add tools with MCP](add-tools-with-mcp.md) — connect an alternative generator.
- [Settings reference](../reference/settings.md) — where the OpenAI provider key
  is configured.
