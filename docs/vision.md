# Images and vision

## Local files

The `view_image` tool reads a local PNG, JPEG or WebP file and passes its image
bytes to the configured model. For example: “Describe /tmp/screenshot.png”.
Paths are relative to Kodrik's working directory unless absolute. Files must be
regular files, at most 10 MiB, at most 32 million pixels, and at most 16384 pixels
on either axis. Invalid images are rejected. Original dimensions are preserved
for screenshot coordinates; no resizing or image generation is performed.

## Telegram photos

Telegram accepts photos from chats, selecting the largest supplied
resolution and using the caption as the prompt. Without a caption, the image is
sent without an invented request: the agent uses the current conversation and
its language, asking for clarification if the intent is unclear. This also
applies to PNG/JPEG/WebP images sent as documents. Each photo message is processed
separately; albums are not combined. Downloads are
bounded to 30 seconds and 10 MiB. Failures are reported without submitting an
empty substitute to the model. Bot-token download URLs are never sent to the
model or saved in sessions.

Photo downloads preserve message order within each chat and run independently
across chats. `/stop` and `/new` bypass downloads, cancel the current download
and discard older queued updates in that chat before reaching the agent.

## Provider requirements

The configured Responses API model/backend must support `input_image`, including
images in function-call outputs for `view_image`. Text-only requests retain their
existing format. See the [OpenAI vision documentation](https://developers.openai.com/api/docs/guides/images-vision)
and [function output documentation](https://developers.openai.com/api/docs/guides/function-calling).
There is no automatic model switch or text fallback on a provider rejection.

## Persistence

Image bytes are stored as data URLs in session JSON, so restored conversations
do not depend on temporary files or expired Telegram links. Old text sessions
remain readable. Long screenshot histories increase session size and request
size; `/new` starts a fresh conversation. Tool progress displays metadata only.

See also: [Using Kodrik](usage.md), [Configuration](configuration.md), [Storage and backups](storage.md).
