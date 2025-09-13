# Editor-Server Data Contract

## Request & response types

All the messages are in `src/message.rs`. Later I’ll generate a
documentation with Typescript style type definition for request and
response bodies.

## Request & repsonse

In collab server, most requests are served async, so editor sending
the request should expect the response asynchronously too. It’s also
possible to never receive a response of a request: when there’s a
disconnect, collab server doesn’t try to return a disconnect error
response, instead it just sends a disconnect notification; when the
remote server never responds with needed info, our collab server will
naturally never send the response to editor.

All the response objects contain enough context that editor doesn’t
need to keep a record mapping request id to context.

## Share file

When sharing a file, the `filename` field should be the absolute path of the file. When sharing a buffer, the `filename` field should be a buffer name.

If file A is a symlink of file B, and editor shared file A, collab server will follow the symlink and save changes to file B. But since we don’t allow sharing the same file twice, if editor now tries to share file B, the request will fail.

There’s no way to check duplication for hard links, so if file A is a hard link of file B, collab server will allow editor to share file A and B as two files.
