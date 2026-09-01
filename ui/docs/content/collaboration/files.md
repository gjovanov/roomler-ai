---
title: Files
description: Share files in rooms, browse a per-room library, and search across everything — with content types resolved from the bytes rather than the uploader's claim.
tags: [collaboration, files, storage, security]
order: 3
---

Drop a file into a message and it is shared with the room and added to the
room's library.

## The library

Each room has a file list — everything shared there, newest first, with who
uploaded it and when. Files can be downloaded, previewed where the browser can
render them, and searched by name.

## Storage

Files are stored in object storage. On the hosted service that is ours; on a
self-hosted instance it is the storage container in your compose stack, which is
one of the volumes worth backing up.

Per-plan storage limits apply — see [pricing](/pricing).

## A security detail worth knowing

:::danger The stored content type comes from the bytes, not from the upload
An uploader's claimed content type is accepted and then **ignored**. The server
inspects the actual file signature and stores what the bytes really are.

This matters because the interface renders some types inline. A file that claims
to be an image and is something else entirely would otherwise be rendered as the
uploader intended rather than as what it is.
:::

For content with no recognisable signature, the fallback is deliberately narrow
and **cannot** produce an image or HTML type — those are exactly the types worth
lying about, so they must be proven by the file's own bytes. A file named
`.png` with nothing image-like inside it is stored as a generic download.

:::warning This is not a whitelist of what a client may claim
A whitelist would still be trusting the claim. The point is that the claim is
not consulted at all.
:::

## Downloads

Downloaded filenames are sanitised and correctly encoded, so a file with an
unusual name downloads as that name rather than becoming a way to influence the
browser.

## Retention

Files live as long as their room. Deleting a message removes its attachment;
deleting a room removes its library.

## Exporting

A room's messages and file list can be exported together, which is the path to
take if you are migrating away or need a record.
